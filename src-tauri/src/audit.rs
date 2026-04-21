//! Append-only JSON-lines audit log.
//!
//! One file, shared between the UI process and every `pg-mcp serve`
//! child. Each process gets a fresh session UUID at startup so you can
//! grep the log by agent session (`session=`) or by process kind
//! (`process=ui` vs `process=mcp`). Writes use `O_APPEND` so concurrent
//! appends from multiple processes don't interleave within a line.
//!
//! Path: `<config_dir>/pg-mcp/pg-mcp.log`, 0600 on Unix.
//!
//! Schema: every line is a JSON object containing at minimum
//! `{ts, kind, session, process, pid}`. Callers add event-specific
//! fields via the second argument to `log`.
//!
//! Intentionally does not rotate. Operators rotate via logrotate or an
//! equivalent; we just append.
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

struct Audit {
    writer: Option<File>,
    session_id: String,
    process: &'static str,
    pid: u32,
}

static AUDIT: OnceLock<Mutex<Audit>> = OnceLock::new();

fn audit() -> &'static Mutex<Audit> {
    AUDIT.get_or_init(|| {
        Mutex::new(Audit {
            writer: None,
            session_id: uuid::Uuid::new_v4().to_string(),
            process: "unknown",
            pid: std::process::id(),
        })
    })
}

/// Initialise the audit log. Call once per process at startup.
/// `process` should be a stable short identifier like `"ui"` or `"mcp"`.
pub fn init(process: &'static str) {
    let path = log_path();
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();

    #[cfg(unix)]
    if file.is_some() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    {
        let mut a = audit().lock().unwrap();
        a.writer = file;
        a.process = process;
    }

    log(
        "start",
        json!({
            "version": env!("CARGO_PKG_VERSION"),
        }),
    );
}

/// Append an event to the audit log. Silently drops if the log file
/// could not be opened — audit must never be a hard dependency.
pub fn log(kind: &str, fields: Value) {
    let mut a = audit().lock().unwrap();
    let mut obj = json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "kind": kind,
        "session": a.session_id,
        "process": a.process,
        "pid": a.pid,
    });
    if let Value::Object(ref mut m) = obj {
        if let Value::Object(f) = fields {
            for (k, v) in f {
                m.insert(k, v);
            }
        }
    }
    if let Some(w) = a.writer.as_mut() {
        let _ = writeln!(w, "{}", obj);
    }
}

/// The current process's session UUID. Stable for the lifetime of the
/// process. Useful for tagging pg_stat_activity rows.
pub fn session_id() -> String {
    audit().lock().unwrap().session_id.clone()
}

/// Filesystem location of the audit log.
pub fn log_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("pg-mcp")
        .join("pg-mcp.log")
}
