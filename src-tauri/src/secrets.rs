//! Secret storage backed by the OS keychain.
//!
//! * macOS — Keychain Services
//! * Windows — Credential Manager
//! * Linux — Secret Service (GNOME Keyring, KWallet, etc.)
//!
//! Connection secrets are written here instead of the plaintext
//! `config.json`. On platforms where the keychain is unavailable (headless
//! Linux with no running secret service, for instance) every function
//! returns `Err(SecretError::Unavailable(..))`; callers degrade gracefully
//! to the legacy plaintext path.
//!
//! Key scheme:
//!   service  = `pg-mcp`
//!   account  = `<connection-name>`          → connection password
//!   account  = `<connection-name>.connstr`  → full connection string
//!
//! Keychain entries are tied to the connection *name*. `rename()` handles
//! moves, `delete()` removes both entries.
use keyring::{Entry, Error as KeyringError};

const SERVICE: &str = "pg-mcp";

#[derive(Debug)]
pub enum SecretError {
    /// The backend is not available on this system. Callers should fall
    /// back to the plaintext config path and warn the user.
    Unavailable(String),
    /// An actual error from the backend (permission denied, etc.).
    Backend(String),
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretError::Unavailable(msg) => write!(f, "keychain unavailable: {}", msg),
            SecretError::Backend(msg) => write!(f, "keychain error: {}", msg),
        }
    }
}

impl From<KeyringError> for SecretError {
    fn from(e: KeyringError) -> Self {
        match e {
            KeyringError::PlatformFailure(ref inner) => {
                SecretError::Unavailable(format!("{}", inner))
            }
            KeyringError::NoStorageAccess(ref inner) => {
                SecretError::Unavailable(format!("{}", inner))
            }
            other => SecretError::Backend(format!("{}", other)),
        }
    }
}

fn password_account(name: &str) -> String {
    name.to_string()
}

fn connstr_account(name: &str) -> String {
    format!("{}.connstr", name)
}

fn set(account: &str, value: &str) -> Result<(), SecretError> {
    let entry = Entry::new(SERVICE, account)?;
    entry.set_password(value)?;
    Ok(())
}

fn get(account: &str) -> Result<Option<String>, SecretError> {
    let entry = Entry::new(SERVICE, account)?;
    match entry.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(KeyringError::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn remove(account: &str) -> Result<(), SecretError> {
    let entry = Entry::new(SERVICE, account)?;
    match entry.delete_credential() {
        Ok(_) => Ok(()),
        Err(KeyringError::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ── Passwords ──────────────────────────────────────────────────────

pub fn save_password(connection_name: &str, password: &str) -> Result<(), SecretError> {
    if password.is_empty() {
        return delete_password(connection_name);
    }
    set(&password_account(connection_name), password)
}

pub fn load_password(connection_name: &str) -> Result<Option<String>, SecretError> {
    get(&password_account(connection_name))
}

pub fn delete_password(connection_name: &str) -> Result<(), SecretError> {
    remove(&password_account(connection_name))
}

// ── Connection strings ─────────────────────────────────────────────

pub fn save_connection_string(connection_name: &str, value: &str) -> Result<(), SecretError> {
    if value.is_empty() {
        return delete_connection_string(connection_name);
    }
    set(&connstr_account(connection_name), value)
}

pub fn load_connection_string(connection_name: &str) -> Result<Option<String>, SecretError> {
    get(&connstr_account(connection_name))
}

pub fn delete_connection_string(connection_name: &str) -> Result<(), SecretError> {
    remove(&connstr_account(connection_name))
}

// ── Bulk ops ───────────────────────────────────────────────────────

pub fn delete_all(connection_name: &str) -> Result<(), SecretError> {
    delete_password(connection_name)?;
    delete_connection_string(connection_name)?;
    Ok(())
}

#[allow(dead_code)]
pub fn rename(old_name: &str, new_name: &str) -> Result<(), SecretError> {
    if old_name == new_name {
        return Ok(());
    }
    if let Some(pw) = load_password(old_name)? {
        save_password(new_name, &pw)?;
        delete_password(old_name)?;
    }
    if let Some(cs) = load_connection_string(old_name)? {
        save_connection_string(new_name, &cs)?;
        delete_connection_string(old_name)?;
    }
    Ok(())
}

/// Probe whether the backend is reachable at all. Used on startup so we
/// can warn once instead of on every operation.
#[allow(dead_code)]
pub fn is_available() -> bool {
    match Entry::new(SERVICE, "__pgmcp_probe__") {
        Ok(entry) => match entry.get_password() {
            Ok(_) => true,
            Err(KeyringError::NoEntry) => true,
            Err(KeyringError::PlatformFailure(_)) => false,
            Err(KeyringError::NoStorageAccess(_)) => false,
            Err(_) => true,
        },
        Err(KeyringError::PlatformFailure(_)) => false,
        Err(KeyringError::NoStorageAccess(_)) => false,
        Err(_) => true,
    }
}
