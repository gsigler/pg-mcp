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
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub ssl: bool,
    #[serde(default = "default_readonly")]
    pub readonly: bool,
    #[serde(default)]
    pub color: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_string: Option<String>,
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
        if path.exists() {
            match fs::read_to_string(&path) {
                Ok(contents) => match serde_json::from_str(&contents) {
                    Ok(config) => return config,
                    Err(e) => {
                        log::warn!("Failed to parse config: {}", e);
                    }
                },
                Err(e) => {
                    log::warn!("Failed to read config: {}", e);
                }
            }
        }
        Config::default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Failed to create config dir: {}", e))?;
        }
        let json =
            serde_json::to_string_pretty(self).map_err(|e| format!("Failed to serialize: {}", e))?;
        fs::write(&path, &json).map_err(|e| format!("Failed to write config: {}", e))?;

        #[cfg(unix)]
        {
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(&path, perms)
                .map_err(|e| format!("Failed to set permissions: {}", e))?;
        }

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
