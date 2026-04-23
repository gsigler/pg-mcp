// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audit;
mod config;
mod database;
mod mcp_server;
mod pii;
mod secrets;

use config::{Config, Connection, SafeConfig};
use database::DatabaseManager;
use mcp_server::McpServer;
use std::sync::Arc;
use tauri::State;


struct AppState {
    db: Arc<DatabaseManager>,
}

// ─── Tauri Commands ─────────────────────────────────────────────

#[tauri::command]
async fn get_config() -> Result<SafeConfig, String> {
    let config = Config::load();
    Ok(SafeConfig::from(&config))
}

#[tauri::command]
async fn save_connection(connection: Connection) -> Result<SafeConfig, String> {
    let mut config = Config::load();
    let mut conn = connection;

    // Auto-assign color if empty
    if conn.color.is_empty() {
        conn.color = config.assign_color();
    }

    // Push secrets into the OS keychain *before* we persist the config,
    // so a crash between the two only leaves orphan keychain entries, not
    // a config referencing a missing secret. Empty values from the UI mean
    // "don't touch the stored secret" — the existing keychain entry stays.
    if !conn.password.is_empty() {
        if let Err(e) = secrets::save_password(&conn.name, &conn.password) {
            return Err(format!("Failed to store password in keychain: {}", e));
        }
    }
    if let Some(ref cs) = conn.connection_string {
        if !cs.is_empty() {
            if let Err(e) = secrets::save_connection_string(&conn.name, cs) {
                return Err(format!("Failed to store connection string in keychain: {}", e));
            }
        }
    }
    // Never let the plaintext values reach the serialized file.
    conn.password.clear();
    conn.connection_string = None;

    let is_new = config
        .connections
        .iter()
        .all(|c| c.name != conn.name);

    let name = conn.name.clone();
    if let Some(existing) = config.connections.iter_mut().find(|c| c.name == conn.name) {
        *existing = conn;
    } else {
        config.connections.push(conn);
    }

    // Auto-activate if this is the only connection
    if config.connections.len() == 1 {
        config.active_connection = Some(config.connections[0].name.clone());
    }

    config.save()?;

    audit::log(
        if is_new { "connection_added" } else { "connection_updated" },
        serde_json::json!({"connection": name}),
    );

    Ok(SafeConfig::from(&config))
}

#[tauri::command]
async fn delete_connection(name: String) -> Result<SafeConfig, String> {
    let mut config = Config::load();
    config.connections.retain(|c| c.name != name);

    if config.active_connection.as_deref() == Some(&name) {
        config.active_connection = config.connections.first().map(|c| c.name.clone());
    }

    config.save()?;

    // Best-effort: scrub the keychain too. A failure here is non-fatal —
    // the connection is already gone from disk and the orphaned keychain
    // entry is harmless.
    if let Err(e) = secrets::delete_all(&name) {
        log::warn!("[pg-mcp] failed to delete keychain entries for '{}': {}", name, e);
    }

    audit::log("connection_deleted", serde_json::json!({"connection": name}));

    Ok(SafeConfig::from(&config))
}

#[tauri::command]
async fn set_active(name: String, state: State<'_, AppState>) -> Result<SafeConfig, String> {
    let mut config = Config::load();

    if !config.connections.iter().any(|c| c.name == name) {
        return Err(format!("Connection '{}' not found", name));
    }

    let previous = config.active_connection.clone();
    config.active_connection = Some(name.clone());
    config.save()?;

    // Flush the UI's database connection. Running `pg-mcp serve` child
    // processes pick up the switch on their next tool call by way of the
    // `Config::load` they already do per-call — they compare
    // `active_connection` against their currently-connected name and
    // reconnect if it changed. Nothing we do here can interrupt an
    // in-flight query in those children; the switch takes effect on the
    // query *after* the one already running.
    state.db.disconnect().await;

    audit::log(
        "active_connection_changed",
        serde_json::json!({"from": previous, "to": name}),
    );

    Ok(SafeConfig::from(&config))
}

#[tauri::command]
async fn test_connection_cmd(name: String) -> Result<String, String> {
    let config = Config::load();
    let mut conn = config
        .connections
        .iter()
        .find(|c| c.name == name)
        .cloned()
        .ok_or_else(|| format!("Connection '{}' not found", name))?;

    conn.hydrate_secrets();
    let (version, latency, observed_ro) = DatabaseManager::test_connection(&conn).await?;
    let ro_note = if conn.readonly && !observed_ro {
        " [WARNING: read-only not honored by server]"
    } else if observed_ro {
        " [server read-only enforced]"
    } else {
        ""
    };
    Ok(format!("OK — {} ({}ms){}", version, latency, ro_note))
}

#[tauri::command]
async fn get_connection_for_edit(name: String) -> Result<Connection, String> {
    // Never send the plaintext password or full connection string to the
    // webview. save_connection re-uses the stored password when the
    // supplied password is empty, so the UI can edit other fields without
    // re-entering the secret.
    let config = Config::load();
    let mut conn = config
        .connections
        .iter()
        .find(|c| c.name == name)
        .cloned()
        .ok_or_else(|| format!("Connection '{}' not found", name))?;
    conn.password.clear();
    conn.connection_string = None;
    Ok(conn)
}

/// Write JSON to disk atomically: write to a temp sibling, then rename.
/// Prevents corruption if the process dies mid-write.
fn atomic_write_json(path: &std::path::Path, value: &serde_json::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create config dir: {}", e))?;
    }
    let tmp = path.with_extension("pgmcp.tmp");
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| format!("Failed to serialize: {}", e))?;
    std::fs::write(&tmp, json).map_err(|e| format!("Failed to write temp file: {}", e))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Failed to commit config: {}", e))?;
    Ok(())
}

/// TOML sibling of `atomic_write_json` for Codex's `~/.codex/config.toml`.
/// Uses `toml::Value` so unrelated tables (profiles, keys, other mcp_servers
/// entries) round-trip unchanged.
fn atomic_write_toml(path: &std::path::Path, value: &toml::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create config dir: {}", e))?;
    }
    let tmp = path.with_extension("pgmcp.tmp");
    let body = toml::to_string_pretty(value)
        .map_err(|e| format!("Failed to serialize TOML: {}", e))?;
    std::fs::write(&tmp, body).map_err(|e| format!("Failed to write temp file: {}", e))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Failed to commit config: {}", e))?;
    Ok(())
}

/// Decide what key to use for our entry, avoiding clobbering an unrelated
/// "postgres" MCP the user may already have configured.
fn pg_mcp_key(existing: Option<&serde_json::Value>, binary: &str) -> String {
    match existing {
        Some(obj) if obj.get("postgres").and_then(|e| e.get("command"))
            .and_then(|c| c.as_str())
            .map_or(true, |cmd| cmd == binary) => "postgres".into(),
        Some(_) => "pg-mcp".into(),
        None => "postgres".into(),
    }
}

#[tauri::command]
async fn get_binary_path() -> Result<String, String> {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .map_err(|e| format!("Failed to get binary path: {}", e))
}

#[tauri::command]
async fn add_to_claude_desktop() -> Result<String, String> {
    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .map_err(|e| format!("Failed to get binary path: {}", e))?;

    let config_path = dirs::home_dir()
        .ok_or("Cannot find home directory")?
        .join("Library/Application Support/Claude/claude_desktop_config.json");

    let mut config: serde_json::Value = if config_path.exists() {
        let contents = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("Failed to read config: {}", e))?;
        serde_json::from_str(&contents).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if config.get("mcpServers").is_none() {
        config["mcpServers"] = serde_json::json!({});
    }

    let key = pg_mcp_key(config.get("mcpServers"), &binary);
    config["mcpServers"][&key] = serde_json::json!({
        "command": binary,
        "args": ["serve"]
    });

    atomic_write_json(&config_path, &config)?;
    Ok(format!(
        "Added to Claude Desktop as '{}'. Restart Claude Desktop to connect.",
        key
    ))
}

#[tauri::command]
async fn add_to_claude_code() -> Result<String, String> {
    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .map_err(|e| format!("Failed to get binary path: {}", e))?;

    let config_path = dirs::home_dir()
        .ok_or("Cannot find home directory")?
        .join(".claude.json");

    let mut config: serde_json::Value = if config_path.exists() {
        let contents = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("Failed to read config: {}", e))?;
        serde_json::from_str(&contents).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // Register at USER scope (top-level `mcpServers`) so the server is
    // available regardless of which directory the user runs `claude`
    // from. A prior version of this command wrote to
    // `projects.<home>.mcpServers`, which is project scope and only
    // worked when claude was invoked from the home directory — silently
    // broken everywhere else. Scrub that old entry if it's ours.
    if config.get("mcpServers").is_none() {
        config["mcpServers"] = serde_json::json!({});
    }
    let key = pg_mcp_key(config.get("mcpServers"), &binary);
    config["mcpServers"][&key] = serde_json::json!({
        "type": "stdio",
        "command": binary,
        "args": ["serve"],
        "env": {}
    });

    // Clean up any previous project-scoped install pointing at our binary
    // so the user doesn't end up with duplicate registrations.
    if let Some(home) = dirs::home_dir().map(|h| h.display().to_string()) {
        if let Some(mcps) = config
            .get_mut("projects")
            .and_then(|p| p.get_mut(&home))
            .and_then(|h| h.get_mut("mcpServers"))
            .and_then(|m| m.as_object_mut())
        {
            mcps.retain(|_k, v| {
                v.get("command").and_then(|c| c.as_str()) != Some(&binary)
            });
        }
    }

    atomic_write_json(&config_path, &config)?;
    Ok(format!(
        "Added to Claude Code as '{}' (user scope). Restart Claude Code to connect.",
        key
    ))
}

#[tauri::command]
async fn add_to_cursor() -> Result<String, String> {
    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .map_err(|e| format!("Failed to get binary path: {}", e))?;

    // Cursor reads `~/.cursor/mcp.json` at user scope — same path on macOS
    // and Windows. Schema matches Claude Desktop / Claude Code: a flat
    // `mcpServers` object with `command` + `args` + optional `env`.
    let config_path = dirs::home_dir()
        .ok_or("Cannot find home directory")?
        .join(".cursor")
        .join("mcp.json");

    let mut config: serde_json::Value = if config_path.exists() {
        let contents = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("Failed to read config: {}", e))?;
        serde_json::from_str(&contents).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if config.get("mcpServers").is_none() {
        config["mcpServers"] = serde_json::json!({});
    }
    let key = pg_mcp_key(config.get("mcpServers"), &binary);
    config["mcpServers"][&key] = serde_json::json!({
        "command": binary,
        "args": ["serve"]
    });

    atomic_write_json(&config_path, &config)?;
    Ok(format!(
        "Added to Cursor as '{}'. Restart Cursor to connect.",
        key
    ))
}

#[tauri::command]
async fn add_to_vscode() -> Result<String, String> {
    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .map_err(|e| format!("Failed to get binary path: {}", e))?;

    // VS Code's user-scope MCP file is different from everyone else: the
    // top-level key is `servers` (not `mcpServers`) and each entry needs
    // an explicit `type: "stdio"`. Path is
    // `~/Library/Application Support/Code/User/mcp.json` on macOS.
    let config_path = dirs::home_dir()
        .ok_or("Cannot find home directory")?
        .join("Library/Application Support/Code/User/mcp.json");

    let mut config: serde_json::Value = if config_path.exists() {
        let contents = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("Failed to read config: {}", e))?;
        serde_json::from_str(&contents).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if config.get("servers").is_none() {
        config["servers"] = serde_json::json!({});
    }
    let key = pg_mcp_key(config.get("servers"), &binary);
    config["servers"][&key] = serde_json::json!({
        "type": "stdio",
        "command": binary,
        "args": ["serve"]
    });

    atomic_write_json(&config_path, &config)?;
    Ok(format!(
        "Added to VS Code as '{}' (user scope). Reload VS Code to connect.",
        key
    ))
}

#[tauri::command]
async fn add_to_codex() -> Result<String, String> {
    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .map_err(|e| format!("Failed to get binary path: {}", e))?;

    // `~/.codex/config.toml` is shared by the Codex CLI, the Codex IDE
    // extension, and the standalone Codex desktop app — one write covers
    // all three. TOML, with per-server tables under `[mcp_servers.<name>]`.
    // Note: the table key is snake_case (`mcp_servers`), unlike the
    // camelCase `mcpServers` used by every JSON-based client.
    let config_path = dirs::home_dir()
        .ok_or("Cannot find home directory")?
        .join(".codex")
        .join("config.toml");

    let mut root: toml::Value = if config_path.exists() {
        let contents = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("Failed to read config: {}", e))?;
        contents
            .parse::<toml::Value>()
            .unwrap_or_else(|_| toml::Value::Table(toml::value::Table::new()))
    } else {
        toml::Value::Table(toml::value::Table::new())
    };

    // Ensure `[mcp_servers]` exists and is a table.
    let root_table = root
        .as_table_mut()
        .ok_or("Codex config is not a TOML table")?;
    let servers = root_table
        .entry("mcp_servers".to_string())
        .or_insert_with(|| toml::Value::Table(toml::value::Table::new()))
        .as_table_mut()
        .ok_or("`mcp_servers` in Codex config is not a table")?;

    // Match the `pg_mcp_key` JSON helper's intent: prefer "postgres",
    // but don't clobber an unrelated entry pointing at a different binary.
    let key = match servers.get("postgres") {
        Some(existing) => {
            let same_binary = existing
                .get("command")
                .and_then(|c| c.as_str())
                .map_or(false, |c| c == binary);
            if same_binary {
                "postgres".to_string()
            } else {
                "pg-mcp".to_string()
            }
        }
        None => "postgres".to_string(),
    };

    let mut entry = toml::value::Table::new();
    entry.insert(
        "command".into(),
        toml::Value::String(binary.clone()),
    );
    entry.insert(
        "args".into(),
        toml::Value::Array(vec![toml::Value::String("serve".into())]),
    );
    servers.insert(key.clone(), toml::Value::Table(entry));

    atomic_write_toml(&config_path, &root)?;
    Ok(format!(
        "Added to Codex as '{}'. Covers the CLI, IDE extension, and desktop app — restart whichever you use.",
        key
    ))
}

#[tauri::command]
async fn check_agent_status() -> Result<serde_json::Value, String> {
    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    // True if any entry under `mcpServers` (at the given JSON path) has a
    // `command` that matches our binary.
    fn has_our_binary(map: Option<&serde_json::Value>, binary: &str) -> bool {
        map.and_then(|v| v.as_object())
            .map(|obj| {
                obj.values().any(|entry| {
                    entry
                        .get("command")
                        .and_then(|c| c.as_str())
                        .map_or(false, |cmd| cmd == binary)
                })
            })
            .unwrap_or(false)
    }

    let desktop_path = dirs::home_dir()
        .map(|h| h.join("Library/Application Support/Claude/claude_desktop_config.json"));
    let desktop_cfg: Option<serde_json::Value> = desktop_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok());
    let desktop_installed =
        has_our_binary(desktop_cfg.as_ref().and_then(|v| v.get("mcpServers")), &binary);

    // Claude Code: we install at user scope (top-level `mcpServers`),
    // but earlier builds wrote to project scope under the home directory.
    // Treat either location as "installed".
    let code_path = dirs::home_dir().map(|h| h.join(".claude.json"));
    let code_cfg: Option<serde_json::Value> = code_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok());
    let home = dirs::home_dir()
        .map(|h| h.display().to_string())
        .unwrap_or_default();
    let code_user_scope = has_our_binary(
        code_cfg.as_ref().and_then(|v| v.get("mcpServers")),
        &binary,
    );
    let code_project_scope = has_our_binary(
        code_cfg
            .as_ref()
            .and_then(|v| v.get("projects"))
            .and_then(|p| p.get(&home))
            .and_then(|h| h.get("mcpServers")),
        &binary,
    );
    let code_installed = code_user_scope || code_project_scope;

    let cursor_path = dirs::home_dir().map(|h| h.join(".cursor/mcp.json"));
    let cursor_cfg: Option<serde_json::Value> = cursor_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok());
    let cursor_installed = has_our_binary(
        cursor_cfg.as_ref().and_then(|v| v.get("mcpServers")),
        &binary,
    );

    // VS Code uses `servers` as the top-level key instead of `mcpServers`.
    let vscode_path =
        dirs::home_dir().map(|h| h.join("Library/Application Support/Code/User/mcp.json"));
    let vscode_cfg: Option<serde_json::Value> = vscode_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok());
    let vscode_installed =
        has_our_binary(vscode_cfg.as_ref().and_then(|v| v.get("servers")), &binary);

    // Codex stores config as TOML in `~/.codex/config.toml`, with tables
    // at `[mcp_servers.<name>]`. Parse separately from the JSON-based ones.
    let codex_path = dirs::home_dir().map(|h| h.join(".codex/config.toml"));
    let codex_installed = codex_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.parse::<toml::Value>().ok())
        .and_then(|v| {
            v.get("mcp_servers")
                .and_then(|m| m.as_table())
                .map(|t| {
                    t.values().any(|entry| {
                        entry
                            .get("command")
                            .and_then(|c| c.as_str())
                            .map_or(false, |cmd| cmd == binary)
                    })
                })
        })
        .unwrap_or(false);

    Ok(serde_json::json!({
        "claudeDesktop": desktop_installed,
        "claudeCode": code_installed,
        "cursor": cursor_installed,
        "vscode": vscode_installed,
        "codex": codex_installed,
    }))
}

// ─── Main ───────────────────────────────────────────────────────

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();

    // "serve" subcommand: run MCP server over stdio (no UI)
    if args.get(1).map(|s| s.as_str()) == Some("serve") {
        audit::init("mcp");
        let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
        rt.block_on(async {
            let server = McpServer::new();
            if let Err(e) = server.run().await {
                audit::log("fatal", serde_json::json!({"error": e.clone()}));
                eprintln!("MCP server error: {}", e);
                std::process::exit(1);
            }
        });
        return;
    }

    // Default: launch the Tauri UI
    audit::init("ui");

    let state = AppState {
        db: Arc::new(DatabaseManager::new()),
    };

    tauri::Builder::default()
        // Single-instance must be the *first* plugin so the secondary
        // process exits before loading anything else. When a second
        // launch is attempted (e.g. double-clicking the app in Finder)
        // the callback fires in the already-running primary and we
        // raise its window.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            use tauri::Manager;
            audit::log("second_instance_focused", serde_json::json!({}));
            // Raise every visible window — robust against the default
            // label changing across Tauri versions.
            for (_label, window) in app.webview_windows() {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_shell::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_connection,
            delete_connection,
            set_active,
            test_connection_cmd,
            get_connection_for_edit,
            get_binary_path,
            add_to_claude_desktop,
            add_to_claude_code,
            add_to_cursor,
            add_to_vscode,
            add_to_codex,
            check_agent_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
