// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod database;
mod mcp_server;

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

    // Update existing or insert new
    if let Some(existing) = config.connections.iter_mut().find(|c| c.name == conn.name) {
        // Preserve password if not provided on edit
        if conn.password.is_empty() {
            conn.password = existing.password.clone();
        }
        if conn.connection_string.is_none() {
            conn.connection_string = existing.connection_string.clone();
        }
        *existing = conn;
    } else {
        config.connections.push(conn);
    }

    // Auto-activate if this is the only connection
    if config.connections.len() == 1 {
        config.active_connection = Some(config.connections[0].name.clone());
    }

    config.save()?;
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
    Ok(SafeConfig::from(&config))
}

#[tauri::command]
async fn set_active(name: String, state: State<'_, AppState>) -> Result<SafeConfig, String> {
    let mut config = Config::load();

    if !config.connections.iter().any(|c| c.name == name) {
        return Err(format!("Connection '{}' not found", name));
    }

    config.active_connection = Some(name);
    config.save()?;

    // Flush the database connection so the MCP server picks up the change
    state.db.disconnect().await;

    Ok(SafeConfig::from(&config))
}

#[tauri::command]
async fn test_connection_cmd(name: String) -> Result<String, String> {
    let config = Config::load();
    let conn = config
        .connections
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| format!("Connection '{}' not found", name))?;

    let (version, latency) = DatabaseManager::test_connection(conn).await?;
    Ok(format!("OK — {} ({}ms)", version, latency))
}

#[tauri::command]
async fn get_connection_for_edit(name: String) -> Result<Connection, String> {
    let config = Config::load();
    config
        .connections
        .iter()
        .find(|c| c.name == name)
        .cloned()
        .ok_or_else(|| format!("Connection '{}' not found", name))
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
        // Create directory if needed
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config dir: {}", e))?;
        }
        serde_json::json!({})
    };

    // Ensure mcpServers object exists
    if config.get("mcpServers").is_none() {
        config["mcpServers"] = serde_json::json!({});
    }

    // Add/update the postgres entry
    config["mcpServers"]["postgres"] = serde_json::json!({
        "command": binary,
        "args": ["serve"]
    });

    let json = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("Failed to serialize: {}", e))?;
    std::fs::write(&config_path, json)
        .map_err(|e| format!("Failed to write config: {}", e))?;

    Ok("Added to Claude Desktop. Restart Claude Desktop to connect.".into())
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

    // Claude Code stores MCP servers under projects.<home_dir>.mcpServers
    let home = dirs::home_dir()
        .ok_or("Cannot find home directory")?
        .display().to_string();

    if config.get("projects").is_none() {
        config["projects"] = serde_json::json!({});
    }
    if config["projects"].get(&home).is_none() {
        config["projects"][&home] = serde_json::json!({});
    }
    if config["projects"][&home].get("mcpServers").is_none() {
        config["projects"][&home]["mcpServers"] = serde_json::json!({});
    }

    config["projects"][&home]["mcpServers"]["postgres"] = serde_json::json!({
        "type": "stdio",
        "command": binary,
        "args": ["serve"],
        "env": {}
    });

    let json = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("Failed to serialize: {}", e))?;
    std::fs::write(&config_path, json)
        .map_err(|e| format!("Failed to write config: {}", e))?;

    Ok("Added to Claude Code. Restart Claude Code to connect.".into())
}

#[tauri::command]
async fn check_agent_status() -> Result<serde_json::Value, String> {
    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    // Check Claude Desktop
    let desktop_path = dirs::home_dir()
        .map(|h| h.join("Library/Application Support/Claude/claude_desktop_config.json"));
    let desktop_installed = desktop_path.as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("mcpServers")?.get("postgres")?.get("command").cloned())
        .map_or(false, |cmd| cmd.as_str() == Some(&binary));

    // Check Claude Code
    let code_path = dirs::home_dir().map(|h| h.join(".claude.json"));
    let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
    let code_installed = code_path.as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("projects")?.get(&home)?.get("mcpServers")?.get("postgres")?.get("command").cloned())
        .map_or(false, |cmd| cmd.as_str() == Some(&binary));

    Ok(serde_json::json!({
        "claudeDesktop": desktop_installed,
        "claudeCode": code_installed,
    }))
}

// ─── Main ───────────────────────────────────────────────────────

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();

    // "serve" subcommand: run MCP server over stdio (no UI)
    if args.get(1).map(|s| s.as_str()) == Some("serve") {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
        rt.block_on(async {
            let server = McpServer::new();
            if let Err(e) = server.run().await {
                eprintln!("MCP server error: {}", e);
                std::process::exit(1);
            }
        });
        return;
    }

    // Default: launch the Tauri UI
    let state = AppState {
        db: Arc::new(DatabaseManager::new()),
    };

    tauri::Builder::default()
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
            check_agent_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
