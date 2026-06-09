use crate::secrets;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const CONNECTION_COLORS: &[&str] = &[
    "#22c55e", "#3b82f6", "#f59e0b", "#ef4444", "#a855f7", "#06b6d4", "#f97316", "#ec4899",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub name: String,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub database: String,
    #[serde(default = "default_user")]
    pub user: String,
    /// Not persisted to disk — the config.json serialization skips this
    /// field entirely. Passwords live in the OS keychain. Populated in
    /// memory when (a) the user submits a new value from the UI, or
    /// (b) `Connection::hydrate_secrets` has pulled it from the keychain
    /// just before a connect attempt.
    #[serde(default, skip_serializing)]
    pub password: String,
    #[serde(default)]
    pub ssl: bool,
    #[serde(default = "default_readonly")]
    pub readonly: bool,
    /// Extra MCP-side gate for operations that can modify a read-write
    /// database. Defaults off even when `readonly` is false.
    #[serde(default)]
    pub allow_agent_writes: bool,
    /// When true, PII in result rows is replaced with `[REDACTED]`
    /// before the MCP response is sent back — so the LLM on the other
    /// side of the pipe never sees the raw values.
    #[serde(default)]
    pub redact_pii: bool,
    #[serde(default)]
    pub color: String,
    /// Same treatment as `password` — never persisted to disk, since
    /// connection strings routinely embed `user:pass@host` credentials.
    #[serde(default, skip_serializing)]
    pub connection_string: Option<String>,
}

impl Connection {
    pub fn settings_signature(&self) -> String {
        serde_json::json!({
            "name": self.name,
            "host": self.host,
            "port": self.port,
            "database": self.database,
            "user": self.user,
            "ssl": self.ssl,
            "readonly": self.readonly,
            "allow_agent_writes": self.allow_agent_writes,
            "redact_pii": self.redact_pii,
            "color": self.color,
        })
        .to_string()
    }

    /// Fill `password` and `connection_string` from the OS keychain if they
    /// aren't already set in memory. Call this right before attempting a
    /// connect. Any keychain error is logged and the field stays empty;
    /// the caller will get the usual "auth failed" response from Postgres.
    pub fn hydrate_secrets(&mut self) {
        if self.password.is_empty() {
            match secrets::load_password(&self.name) {
                Ok(Some(pw)) => self.password = pw,
                Ok(None) => {}
                Err(e) => log::warn!("[pg-mcp] keychain read failed for '{}': {}", self.name, e),
            }
        }
        if self.connection_string.is_none() {
            match secrets::load_connection_string(&self.name) {
                Ok(Some(cs)) => self.connection_string = Some(cs),
                Ok(None) => {}
                Err(e) => log::warn!(
                    "[pg-mcp] keychain read failed (connstr) for '{}': {}",
                    self.name,
                    e
                ),
            }
        }
    }
}

fn default_host() -> String {
    "localhost".to_string()
}
fn default_port() -> u16 {
    5432
}
fn default_user() -> String {
    "postgres".to_string()
}
fn default_readonly() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub connections: Vec<Connection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_connection: Option<String>,
    #[serde(default = "default_ui_port")]
    pub ui_port: u16,
}

fn default_ui_port() -> u16 {
    5488
}

impl Default for Config {
    fn default() -> Self {
        Self {
            connections: Vec::new(),
            active_connection: None,
            ui_port: default_ui_port(),
        }
    }
}

impl Config {
    pub fn config_path() -> PathBuf {
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("pg-mcp");
        config_dir.join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        let mut cfg = if path.exists() {
            match fs::read_to_string(&path) {
                Ok(contents) => match serde_json::from_str::<Config>(&contents) {
                    Ok(config) => config,
                    Err(e) => {
                        log::warn!("Failed to parse config: {}", e);
                        Config::default()
                    }
                },
                Err(e) => {
                    log::warn!("Failed to read config: {}", e);
                    Config::default()
                }
            }
        } else {
            Config::default()
        };

        // One-shot migration: if the on-disk file still carries plaintext
        // passwords/connection strings from a pre-keychain build, move
        // them into the keychain and rewrite the file without the
        // secrets. If the keychain is unavailable we leave them in place
        // and let the operator see the warning.
        if cfg.migrate_plaintext_secrets() {
            if let Err(e) = cfg.save() {
                log::warn!("Failed to rewrite config after migration: {}", e);
            }
        }

        cfg
    }

    /// Move any plaintext `password` / `connection_string` values into the
    /// OS keychain, blanking the in-memory fields so they are not
    /// re-persisted on the next `save()`. Returns true iff at least one
    /// field was migrated and the caller should re-save the config.
    fn migrate_plaintext_secrets(&mut self) -> bool {
        let mut changed = false;
        for conn in &mut self.connections {
            if !conn.password.is_empty() {
                match secrets::save_password(&conn.name, &conn.password) {
                    Ok(_) => {
                        log::info!("[pg-mcp] migrated password for '{}' to keychain", conn.name);
                        conn.password.clear();
                        changed = true;
                    }
                    Err(e) => {
                        log::warn!(
                            "[pg-mcp] keychain unavailable, leaving plaintext password for '{}': {}",
                            conn.name,
                            e
                        );
                    }
                }
            }
            if let Some(ref cs) = conn.connection_string.clone() {
                if !cs.is_empty() {
                    match secrets::save_connection_string(&conn.name, cs) {
                        Ok(_) => {
                            log::info!(
                                "[pg-mcp] migrated connection string for '{}' to keychain",
                                conn.name
                            );
                            conn.connection_string = None;
                            changed = true;
                        }
                        Err(e) => {
                            log::warn!(
                                "[pg-mcp] keychain unavailable, leaving plaintext connection string for '{}': {}",
                                conn.name,
                                e
                            );
                        }
                    }
                }
            }
        }
        changed
    }

    /// Persist the config to disk atomically: write to a sibling temp
    /// file, chmod it to 0600 on Unix, then rename over the target. A
    /// crash between `write` and `rename` leaves the previous config
    /// intact, rather than the zero-byte file that a non-atomic
    /// `fs::write` would produce after `O_TRUNC`.
    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config dir: {}", e))?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize: {}", e))?;

        let tmp = path.with_extension("pgmcp.tmp");
        fs::write(&tmp, &json).map_err(|e| format!("Failed to write temp config: {}", e))?;

        // Set perms on the temp file *before* the rename so there is
        // never a window where the permanent file exists with default
        // umask permissions.
        #[cfg(unix)]
        {
            let perms = fs::Permissions::from_mode(0o600);
            if let Err(e) = fs::set_permissions(&tmp, perms) {
                let _ = fs::remove_file(&tmp);
                return Err(format!("Failed to set permissions: {}", e));
            }
        }

        fs::rename(&tmp, &path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("Failed to commit config: {}", e)
        })?;

        Ok(())
    }

    pub fn get_active_connection(&self) -> Option<&Connection> {
        self.active_connection
            .as_ref()
            .and_then(|name| self.connections.iter().find(|c| &c.name == name))
    }

    pub fn assign_color(&self) -> String {
        let used: Vec<&str> = self.connections.iter().map(|c| c.color.as_str()).collect();
        CONNECTION_COLORS
            .iter()
            .find(|c| !used.contains(c))
            .unwrap_or(&CONNECTION_COLORS[0])
            .to_string()
    }
}

/// Returns a sanitized config for the frontend (passwords masked).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeConnection {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password_set: bool,
    pub ssl: bool,
    pub readonly: bool,
    pub allow_agent_writes: bool,
    pub redact_pii: bool,
    pub color: String,
    pub connection_string_set: bool,
}

impl From<&Connection> for SafeConnection {
    fn from(c: &Connection) -> Self {
        Self {
            name: c.name.clone(),
            host: c.host.clone(),
            port: c.port,
            database: c.database.clone(),
            user: c.user.clone(),
            password_set: !c.password.is_empty(),
            ssl: c.ssl,
            readonly: c.readonly,
            allow_agent_writes: c.allow_agent_writes,
            redact_pii: c.redact_pii,
            color: c.color.clone(),
            connection_string_set: c.connection_string.is_some(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeConfig {
    pub connections: Vec<SafeConnection>,
    pub active_connection: Option<String>,
    pub ui_port: u16,
}

impl From<&Config> for SafeConfig {
    fn from(c: &Config) -> Self {
        Self {
            connections: c.connections.iter().map(SafeConnection::from).collect(),
            active_connection: c.active_connection.clone(),
            ui_port: c.ui_port,
        }
    }
}
