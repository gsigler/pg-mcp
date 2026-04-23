use crate::config::Connection;
use native_tls::TlsConnector;
use postgres_native_tls::MakeTlsConnector;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio_postgres::config::SslMode;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Config as PgConfig, NoTls, Row};

const MAX_RESULT_ROWS: usize = 100;
const MAX_COLUMN_WIDTH: usize = 60;
const STATEMENT_TIMEOUT: &str = "30s";
const IDLE_IN_TXN_TIMEOUT: &str = "60s";

/// Best-effort keyword blocklist. This is a UX fast-fail only — real
/// readonly enforcement is server-side: `default_transaction_read_only=on`
/// is injected into the libpq startup `options` so Postgres applies it
/// before any SQL runs and resists client-side SET overrides.
const WRITE_KEYWORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "DROP", "CREATE", "ALTER", "TRUNCATE",
    "GRANT", "REVOKE", "COPY", "VACUUM", "REINDEX", "COMMENT", "SECURITY",
];

const WRITE_COMPOUND_KEYWORDS: &[&str] = &["SET ROLE", "RESET ROLE"];

/// Statements that mutate session state. Blocked on read-only connections
/// because they can relax the session's default transaction mode or wipe
/// our hardening (`application_name`, timeouts). Enforcement still lives
/// server-side — this is a clearer error than letting Postgres reject at
/// statement time, and a backstop for GUCs whose `-c` startup value can
/// be overridden by SET.
const SESSION_MUTATING_KEYWORDS: &[&str] = &["SET", "RESET", "DISCARD"];

/// Tracks queries executed in this session.
#[derive(Clone)]
pub struct QueryRecord {
    pub sql: String,
    pub timestamp: chrono::NaiveDateTime,
    pub duration_ms: u128,
    pub row_count: Option<usize>,
    pub error: Option<String>,
}

pub struct DatabaseManager {
    client: Arc<Mutex<Option<Client>>>,
    active_connection_name: Arc<Mutex<Option<String>>>,
    in_transaction: Arc<Mutex<bool>>,
    query_history: Arc<Mutex<Vec<QueryRecord>>>,
    /// Snapshot of the active connection's `redact_pii` flag. Captured
    /// at connect time and consulted when formatting data-path results.
    /// Introspection paths (describe_table, list_tables, etc.) skip
    /// redaction regardless — they only return metadata.
    redact_pii: Arc<Mutex<bool>>,
    /// Config's `readonly` flag, snapshotted at connect time. Drives the
    /// post-query DISCARD + re-harden cycle.
    readonly: Arc<Mutex<bool>>,
    /// What Postgres actually reports for `transaction_read_only` after
    /// connect + hardening. Source of truth for the connection banner
    /// icon — the config flag alone can lie if enforcement regressed.
    observed_readonly: Arc<Mutex<bool>>,
    /// `DISCARD ALL` + the full hardening SQL, memoized so post-query
    /// reset on RO connections doesn't rebuild it each call. `None` on
    /// read-write connections (no reset needed).
    reset_sql: Arc<Mutex<Option<String>>>,
}

impl DatabaseManager {
    pub fn new() -> Self {
        Self {
            client: Arc::new(Mutex::new(None)),
            active_connection_name: Arc::new(Mutex::new(None)),
            in_transaction: Arc::new(Mutex::new(false)),
            query_history: Arc::new(Mutex::new(Vec::new())),
            redact_pii: Arc::new(Mutex::new(false)),
            readonly: Arc::new(Mutex::new(false)),
            observed_readonly: Arc::new(Mutex::new(false)),
            reset_sql: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn active_name(&self) -> Option<String> {
        self.active_connection_name.lock().await.clone()
    }

    pub async fn observed_readonly(&self) -> bool {
        *self.observed_readonly.lock().await
    }

    pub async fn connect(&self, conn: &Connection) -> Result<(), String> {
        self.disconnect().await;
        let client = connect_client(conn).await?;

        // Apply per-session guardrails. Returns the SQL that was applied
        // so we can replay it after a post-query DISCARD ALL.
        let hardening_sql = apply_hardening(&client, conn).await?;

        // Verify the server is actually enforcing read-only for RO
        // connections. `default_transaction_read_only` was injected via
        // the startup `options` param, so a healthy server reports 'on'
        // before any client SQL runs. Mismatch = enforcement regressed;
        // refuse rather than lie on the banner.
        let observed = query_transaction_read_only(&client).await?;
        if conn.readonly && !observed {
            return Err(
                "Refusing connection: server reports transaction_read_only=off \
                 even though this connection is configured read-only. \
                 The startup `options` injection was not honored — likely a \
                 server-side override (e.g. ALTER ROLE ... SET). Fix the role \
                 or create a dedicated read-only role."
                    .into(),
            );
        }

        let reset_sql = if conn.readonly {
            Some(format!("DISCARD ALL; {}", hardening_sql))
        } else {
            None
        };

        *self.client.lock().await = Some(client);
        *self.active_connection_name.lock().await = Some(conn.name.clone());
        *self.in_transaction.lock().await = false;
        *self.redact_pii.lock().await = conn.redact_pii;
        *self.readonly.lock().await = conn.readonly;
        *self.observed_readonly.lock().await = observed;
        *self.reset_sql.lock().await = reset_sql;
        crate::audit::log(
            "db_connected",
            serde_json::json!({
                "connection": conn.name,
                "host": conn.host,
                "database": conn.database,
                "ssl": conn.ssl,
                "readonly": conn.readonly,
                "observed_readonly": observed,
                "redact_pii": conn.redact_pii,
            }),
        );
        Ok(())
    }

    pub async fn disconnect(&self) {
        *self.client.lock().await = None;
        *self.active_connection_name.lock().await = None;
        *self.in_transaction.lock().await = false;
        *self.redact_pii.lock().await = false;
        *self.readonly.lock().await = false;
        *self.observed_readonly.lock().await = false;
        *self.reset_sql.lock().await = None;
    }

    /// On read-only connections outside a user transaction, run DISCARD
    /// ALL and re-apply the hardening SQL. Catches session-state mutation
    /// that the classifier can't see (e.g. `SELECT set_config(...)` or
    /// function side effects). No-op on RW or inside a user txn — DISCARD
    /// isn't valid inside a transaction block.
    async fn reset_if_readonly_outside_txn(&self) {
        let readonly = *self.readonly.lock().await;
        if !readonly {
            return;
        }
        if *self.in_transaction.lock().await {
            return;
        }
        let sql = match self.reset_sql.lock().await.clone() {
            Some(s) => s,
            None => return,
        };
        let guard = self.client.lock().await;
        if let Some(client) = guard.as_ref() {
            if let Err(e) = client.batch_execute(&sql).await {
                log::warn!("[pg-mcp] post-query session reset failed: {}", e);
            }
        }
    }

    async fn get_client(&self) -> Result<tokio::sync::MutexGuard<'_, Option<Client>>, String> {
        let guard = self.client.lock().await;
        if guard.is_none() {
            return Err("No active connection. Open the pg-mcp UI and activate a connection.".into());
        }
        Ok(guard)
    }

    pub fn check_write_query(sql: &str) -> bool {
        let normalized = sql.trim().to_uppercase();
        for kw in WRITE_COMPOUND_KEYWORDS {
            if normalized.starts_with(kw) {
                return true;
            }
        }
        if normalized.starts_with("BEGIN") || normalized.starts_with("COMMIT")
            || normalized.starts_with("ROLLBACK") || normalized.starts_with("SAVEPOINT")
            || normalized.starts_with("RELEASE") {
            return false;
        }
        if let Some(first_token) = normalized.split_whitespace().next() {
            WRITE_KEYWORDS.contains(&first_token)
        } else {
            false
        }
    }

    /// True if the statement is a session-state mutation (`SET`, `RESET`,
    /// `DISCARD`, including `SET SESSION` / `SET LOCAL`). These aren't
    /// writes in the table-data sense, so they bypass `check_write_query`,
    /// but on a read-only connection they're privileged — they can weaken
    /// `default_transaction_read_only` or clear our hardening.
    pub fn check_session_mutating(sql: &str) -> bool {
        let normalized = sql.trim().to_uppercase();
        if let Some(first_token) = normalized.split_whitespace().next() {
            SESSION_MUTATING_KEYWORDS.contains(&first_token)
        } else {
            false
        }
    }

    // ─── Core query execution ──────────────────────────────────────

    pub async fn execute_query(
        &self,
        sql: &str,
        readonly: bool,
    ) -> Result<String, String> {
        // Fast-fail UX checks. Real enforcement is the startup-options
        // `default_transaction_read_only=on` and the post-query session
        // reset; these errors just produce clearer messages than letting
        // Postgres reject at statement time.
        if readonly && Self::check_write_query(sql) {
            return Err(
                "Write operation blocked. This connection is read-only.\n\
                 To enable writes, open pg-mcp UI and disable read-only mode for this connection."
                    .into(),
            );
        }
        if readonly && Self::check_session_mutating(sql) {
            return Err(
                "Session-mutating statement (SET/RESET/DISCARD) blocked on read-only \
                 connections. These can weaken the guardrail they exist to enforce. \
                 If you need to change a session parameter, switch to a read-write \
                 connection in the pg-mcp UI."
                    .into(),
            );
        }
        self.execute_parameterized(sql, &[]).await
    }

    async fn execute_parameterized(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<String, String> {
        let start = Instant::now();
        let redact = *self.redact_pii.lock().await;

        // Scope the client guard so it's dropped before the post-query
        // reset reacquires the client mutex. `tokio::sync::Mutex` is not
        // reentrant — holding it across the reset call would deadlock.
        let result = {
            let guard = self.get_client().await?;
            let client = guard.as_ref().unwrap();
            client.query(sql, params).await
        };
        let duration_ms = start.elapsed().as_millis();

        let response = match result {
            Ok(rows) => {
                let row_count = rows.len();
                self.record_query(sql, duration_ms, Some(row_count), None).await;
                Ok(format_rows_opt(&rows, redact))
            }
            Err(e) => {
                let err = format!("Query error: {}", e);
                self.record_query(sql, duration_ms, None, Some(err.clone())).await;
                Err(err)
            }
        };

        self.reset_if_readonly_outside_txn().await;
        response
    }

    /// Execute an UPDATE/DELETE (expected to return rows via RETURNING *)
    /// inside a transaction or savepoint, and only commit if the affected
    /// row count is ≤ `expected_max_rows`. If the statement would touch
    /// more rows than the caller declared, roll back and return an error
    /// naming the actual count — data is left untouched.
    ///
    /// When the caller is already inside a user-managed transaction
    /// (`begin_transaction`), we use a SAVEPOINT so the outer transaction
    /// is preserved. Otherwise we open our own BEGIN/COMMIT pair.
    async fn execute_write_capped(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
        expected_max_rows: u64,
    ) -> Result<String, String> {
        let start = Instant::now();
        let redact = *self.redact_pii.lock().await;
        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();
        let in_user_txn = *self.in_transaction.lock().await;
        let savepoint = "pgmcp_write_cap";

        // Open scope
        let open_res = if in_user_txn {
            client
                .batch_execute(&format!("SAVEPOINT {}", savepoint))
                .await
        } else {
            client.batch_execute("BEGIN").await
        };
        open_res.map_err(|e| format!("Failed to open write guard: {}", e))?;

        let result = client.query(sql, params).await;
        let duration_ms = start.elapsed().as_millis();

        let undo_sql = if in_user_txn {
            format!(
                "ROLLBACK TO SAVEPOINT {sp}; RELEASE SAVEPOINT {sp}",
                sp = savepoint
            )
        } else {
            "ROLLBACK".to_string()
        };
        let commit_sql = if in_user_txn {
            format!("RELEASE SAVEPOINT {}", savepoint)
        } else {
            "COMMIT".to_string()
        };

        match result {
            Ok(rows) => {
                let row_count = rows.len() as u64;
                if row_count > expected_max_rows {
                    let _ = client.batch_execute(&undo_sql).await;
                    let err = format!(
                        "Blocked: statement would affect {} rows, but expected_max_rows = {}. \
                         No changes were committed. Re-issue the call with \
                         expected_max_rows >= {} (or tighten the WHERE clause) to proceed.",
                        row_count, expected_max_rows, row_count
                    );
                    self.record_query(sql, duration_ms, None, Some(err.clone())).await;
                    Err(err)
                } else {
                    client
                        .batch_execute(&commit_sql)
                        .await
                        .map_err(|e| format!("Failed to commit write: {}", e))?;
                    self.record_query(sql, duration_ms, Some(row_count as usize), None)
                        .await;
                    Ok(format_rows_opt(&rows, redact))
                }
            }
            Err(e) => {
                let _ = client.batch_execute(&undo_sql).await;
                let err = format!("Query error: {}", e);
                self.record_query(sql, duration_ms, None, Some(err.clone())).await;
                Err(err)
            }
        }
    }

    /// Execute query with pagination support
    pub async fn execute_query_paginated(
        &self,
        sql: &str,
        readonly: bool,
        limit: usize,
        offset: usize,
    ) -> Result<String, String> {
        // Clamp to sensible bounds; values are integers so safe to inline.
        let limit = limit.min(10_000);
        let offset = offset.min(10_000_000);
        let paginated = format!(
            "SELECT * FROM ({}) AS __pgmcp_paged LIMIT {} OFFSET {}",
            sql.trim().trim_end_matches(';'),
            limit,
            offset
        );
        let result = self.execute_query(&paginated, readonly).await?;
        Ok(format!("{}\n(page: offset={}, limit={})\n", result, offset, limit))
    }

    // ─── Transaction support ───────────────────────────────────────

    pub async fn begin_transaction(&self, readonly: bool) -> Result<String, String> {
        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();

        if *self.in_transaction.lock().await {
            return Err("Already in a transaction. COMMIT or ROLLBACK first.".into());
        }

        let sql = if readonly {
            "BEGIN TRANSACTION READ ONLY"
        } else {
            "BEGIN TRANSACTION"
        };
        client.execute(sql, &[]).await
            .map_err(|e| format!("Failed to begin transaction: {}", e))?;

        *self.in_transaction.lock().await = true;
        Ok("Transaction started.".into())
    }

    pub async fn commit_transaction(&self) -> Result<String, String> {
        if !*self.in_transaction.lock().await {
            return Err("No active transaction to commit.".into());
        }

        {
            let guard = self.get_client().await?;
            let client = guard.as_ref().unwrap();
            client.execute("COMMIT", &[]).await
                .map_err(|e| format!("Failed to commit: {}", e))?;
        }

        *self.in_transaction.lock().await = false;
        // Post-txn reset: non-LOCAL `SET`s that somehow landed inside the
        // txn remain in session scope after COMMIT. DISCARD ALL clears them.
        self.reset_if_readonly_outside_txn().await;
        Ok("Transaction committed.".into())
    }

    pub async fn rollback_transaction(&self) -> Result<String, String> {
        if !*self.in_transaction.lock().await {
            return Err("No active transaction to rollback.".into());
        }

        {
            let guard = self.get_client().await?;
            let client = guard.as_ref().unwrap();
            client.execute("ROLLBACK", &[]).await
                .map_err(|e| format!("Failed to rollback: {}", e))?;
        }

        *self.in_transaction.lock().await = false;
        self.reset_if_readonly_outside_txn().await;
        Ok("Transaction rolled back.".into())
    }

    // ─── CRUD helpers (parameterized) ──────────────────────────────

    pub async fn insert_rows(
        &self,
        table: &str,
        columns: &[String],
        rows: &[Vec<String>],
    ) -> Result<String, String> {
        if rows.is_empty() {
            return Err("No rows provided.".into());
        }
        if columns.is_empty() {
            return Err("No columns provided.".into());
        }

        let (schema_q, tbl_q) = parse_qualified(table);
        let col_list = columns
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");

        let mut value_groups: Vec<String> = Vec::new();
        for row in rows {
            if row.len() != columns.len() {
                return Err(format!(
                    "Row has {} values but {} columns were declared",
                    row.len(),
                    columns.len()
                ));
            }
            let rendered: Vec<String> = row
                .iter()
                .map(|v| match literal_sentinel(v) {
                    Some(lit) => lit.to_string(),
                    None => dollar_quote(v),
                })
                .collect();
            value_groups.push(format!("({})", rendered.join(", ")));
        }

        // Values are dollar-quoted literals so PG's usual unknown→column
        // type coercion applies (same behavior as before the rewrite), but
        // injection via embedded quotes/backslashes is impossible.
        let sql = format!(
            "INSERT INTO {}.{} ({}) VALUES {} RETURNING *",
            schema_q,
            tbl_q,
            col_list,
            value_groups.join(", ")
        );
        self.execute_parameterized(&sql, &[]).await
    }

    pub async fn update_rows(
        &self,
        table: &str,
        set_columns: &HashMap<String, String>,
        conditions: &str,
        expected_max_rows: u64,
    ) -> Result<String, String> {
        if set_columns.is_empty() {
            return Err("No columns to update.".into());
        }
        if conditions.trim().is_empty() {
            return Err("WHERE condition is required for safety. Use 'TRUE' to update all rows.".into());
        }

        let (schema_q, tbl_q) = parse_qualified(table);

        let mut keys: Vec<&String> = set_columns.keys().collect();
        keys.sort();
        let set_parts: Vec<String> = keys
            .into_iter()
            .map(|col| {
                let val = &set_columns[col];
                let col_q = quote_ident(col);
                match literal_sentinel(val) {
                    Some(lit) => format!("{} = {}", col_q, lit),
                    None => format!("{} = {}", col_q, dollar_quote(val)),
                }
            })
            .collect();

        // NOTE: `conditions` is raw SQL by API contract; callers are
        // responsible for trusting the source of the WHERE clause.
        let sql = format!(
            "UPDATE {}.{} SET {} WHERE {} RETURNING *",
            schema_q,
            tbl_q,
            set_parts.join(", "),
            conditions
        );
        self.execute_write_capped(&sql, &[], expected_max_rows).await
    }

    pub async fn delete_rows(
        &self,
        table: &str,
        conditions: &str,
        expected_max_rows: u64,
    ) -> Result<String, String> {
        if conditions.trim().is_empty() {
            return Err("WHERE condition is required for safety. Use 'TRUE' to delete all rows.".into());
        }

        let (schema_q, tbl_q) = parse_qualified(table);
        let sql = format!(
            "DELETE FROM {}.{} WHERE {} RETURNING *",
            schema_q, tbl_q, conditions
        );
        self.execute_write_capped(&sql, &[], expected_max_rows).await
    }

    // ─── Explain / dry-run ─────────────────────────────────────────

    pub async fn explain_query(
        &self,
        sql: &str,
        analyze: bool,
        readonly: bool,
    ) -> Result<String, String> {
        // If the connection is read-only, refuse EXPLAIN ANALYZE of a
        // suspected write. Plain EXPLAIN is harmless. Server-side
        // default_transaction_read_only will also block writes here, but
        // the early refusal is clearer.
        if analyze && readonly && Self::check_write_query(sql) {
            return Err(
                "EXPLAIN ANALYZE would execute this write statement. \
                 Blocked by read-only mode."
                    .into(),
            );
        }
        let prefix = if analyze { "EXPLAIN ANALYZE" } else { "EXPLAIN" };
        let explain_sql = format!("{} {}", prefix, sql);
        self.execute_parameterized(&explain_sql, &[]).await
    }

    // ─── Enhanced describe_table ───────────────────────────────────

    pub async fn describe_table(&self, table: &str, brief: bool) -> Result<String, String> {
        if brief {
            return self.describe_table_brief(table).await;
        }
        let (schema, tbl) = split_qualified(table);

        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();

        // --- Row count ---
        let count_sql =
            "SELECT n_live_tup::bigint AS approx_rows \
             FROM pg_stat_user_tables \
             WHERE schemaname = $1 AND relname = $2";
        let count_rows = client.query(count_sql, &[&schema, &tbl]).await.unwrap_or_default();
        let approx_rows: i64 = count_rows
            .first()
            .and_then(|r| r.try_get(0).ok())
            .unwrap_or(0);

        let mut output = format!("{}.{} (~{} rows)\n\n", schema, tbl, approx_rows);

        // --- Columns ---
        let col_sql =
            "SELECT c.column_name, c.data_type, c.is_nullable, c.column_default, \
                    pgd.description AS column_comment \
             FROM information_schema.columns c \
             LEFT JOIN pg_catalog.pg_statio_all_tables st \
                ON st.schemaname = c.table_schema AND st.relname = c.table_name \
             LEFT JOIN pg_catalog.pg_description pgd \
                ON pgd.objoid = st.relid AND pgd.objsubid = c.ordinal_position \
             WHERE c.table_schema = $1 AND c.table_name = $2 \
             ORDER BY c.ordinal_position";
        let col_rows = client
            .query(col_sql, &[&schema, &tbl])
            .await
            .map_err(|e| format!("Query error: {}", e))?;

        output.push_str("Columns:\n");
        output.push_str(&format_rows(&col_rows));

        // --- JSONB column inspection ---
        let jsonb_cols: Vec<String> = col_rows
            .iter()
            .filter(|r| {
                let dt: String = r.try_get::<_, String>(1).unwrap_or_default();
                dt == "jsonb" || dt == "json"
            })
            .filter_map(|r| r.try_get::<_, String>(0).ok())
            .collect();

        if !jsonb_cols.is_empty() {
            output.push_str("\nJSONB Column Keys (sampled from first 100 rows):\n");
            let schema_q = quote_ident(&schema);
            let tbl_q = quote_ident(&tbl);
            for col_name in &jsonb_cols {
                let col_q = quote_ident(col_name);
                // Identifiers are safely quoted; no user-controlled strings
                // are interpolated as SQL fragments here.
                let sample_sql = format!(
                    "SELECT DISTINCT jsonb_object_keys({col}) AS key \
                     FROM (SELECT {col} FROM {schema}.{tbl} WHERE {col} IS NOT NULL LIMIT 100) sub \
                     ORDER BY key",
                    col = col_q,
                    schema = schema_q,
                    tbl = tbl_q,
                );
                match client.query(&sample_sql, &[]).await {
                    Ok(key_rows) => {
                        let keys: Vec<String> = key_rows
                            .iter()
                            .filter_map(|r| r.try_get::<_, String>(0).ok())
                            .collect();
                        if !keys.is_empty() {
                            output.push_str(&format!("  {}: {}\n", col_name, keys.join(", ")));
                        }
                    }
                    Err(_) => {}
                }
            }
        }

        // --- Indexes ---
        let idx_sql =
            "SELECT indexname, indexdef FROM pg_indexes \
             WHERE schemaname = $1 AND tablename = $2";
        let idx_rows = client
            .query(idx_sql, &[&schema, &tbl])
            .await
            .map_err(|e| format!("Query error: {}", e))?;
        if !idx_rows.is_empty() {
            output.push_str("\nIndexes:\n");
            output.push_str(&format_rows(&idx_rows));
        }

        // --- Outgoing foreign keys ---
        let fk_sql =
            "SELECT tc.constraint_name, kcu.column_name, \
                    ccu.table_schema || '.' || ccu.table_name AS foreign_table, \
                    ccu.column_name AS foreign_column \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
                ON tc.constraint_name = kcu.constraint_name \
                AND tc.table_schema = kcu.table_schema \
             JOIN information_schema.constraint_column_usage ccu \
                ON ccu.constraint_name = tc.constraint_name \
                AND ccu.table_schema = tc.table_schema \
             WHERE tc.constraint_type = 'FOREIGN KEY' \
                AND tc.table_schema = $1 AND tc.table_name = $2";
        let fk_rows = client
            .query(fk_sql, &[&schema, &tbl])
            .await
            .map_err(|e| format!("Query error: {}", e))?;
        if !fk_rows.is_empty() {
            output.push_str("\nOutgoing Foreign Keys:\n");
            output.push_str(&format_rows(&fk_rows));
        }

        // --- Incoming foreign keys ---
        let rfk_sql =
            "SELECT tc.table_schema || '.' || tc.table_name AS referencing_table, \
                    kcu.column_name AS referencing_column, \
                    tc.constraint_name \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
                ON tc.constraint_name = kcu.constraint_name \
                AND tc.table_schema = kcu.table_schema \
             JOIN information_schema.constraint_column_usage ccu \
                ON ccu.constraint_name = tc.constraint_name \
                AND ccu.table_schema = tc.table_schema \
             WHERE tc.constraint_type = 'FOREIGN KEY' \
                AND ccu.table_schema = $1 AND ccu.table_name = $2";
        let rfk_rows = client
            .query(rfk_sql, &[&schema, &tbl])
            .await
            .map_err(|e| format!("Query error: {}", e))?;
        if !rfk_rows.is_empty() {
            output.push_str("\nIncoming Foreign Keys (tables referencing this one):\n");
            output.push_str(&format_rows(&rfk_rows));
        }

        Ok(output)
    }

    /// Brief-mode describe: one row-count line + one compact column line
    /// with PK/FK markers. Drops indexes, JSONB sampling, FK deep listing.
    /// For "I just need column names" lookups and as the per-table piece
    /// of `describe_tables`.
    async fn describe_table_brief(&self, table: &str) -> Result<String, String> {
        let (schema, tbl) = split_qualified(table);
        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();

        let meta = client
            .query_opt(
                "SELECT COALESCE(s.n_live_tup, 0)::bigint AS approx_rows, \
                        obj_description(c.oid) AS description, \
                        c.relkind \
                 FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 LEFT JOIN pg_stat_user_tables s ON s.relid = c.oid \
                 WHERE n.nspname = $1 AND c.relname = $2 \
                   AND c.relkind IN ('r','v','m','p','f')",
                &[&schema, &tbl],
            )
            .await
            .map_err(|e| format!("Query error: {}", e))?;

        let (approx_rows, description, relkind): (i64, Option<String>, i8) = match meta {
            Some(row) => (row.get(0), row.get(1), row.get::<_, i8>(2)),
            None => {
                return Err(format!(
                    "Relation '{}.{}' not found. Check spelling or try search_schema.",
                    schema, tbl
                ))
            }
        };

        let kind_label = match relkind as u8 as char {
            'v' => " [view]",
            'm' => " [matview]",
            'p' => " [partitioned]",
            'f' => " [foreign]",
            _ => "",
        };

        let cols = client
            .query(
                "SELECT a.attname, \
                        format_type(a.atttypid, a.atttypmod) AS type, \
                        EXISTS ( \
                            SELECT 1 FROM pg_constraint k \
                            WHERE k.conrelid = a.attrelid \
                              AND k.contype = 'p' \
                              AND a.attnum = ANY(k.conkey) \
                        ) AS is_pk, \
                        ( \
                            SELECT cn.nspname || '.' || cc.relname || '.' || att.attname \
                            FROM pg_constraint k \
                            JOIN pg_class cc ON cc.oid = k.confrelid \
                            JOIN pg_namespace cn ON cn.oid = cc.relnamespace \
                            JOIN pg_attribute att \
                                ON att.attrelid = k.confrelid \
                                AND att.attnum = k.confkey[array_position(k.conkey, a.attnum)] \
                            WHERE k.conrelid = a.attrelid \
                              AND k.contype = 'f' \
                              AND a.attnum = ANY(k.conkey) \
                            LIMIT 1 \
                        ) AS fk_target \
                 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 \
                   AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&schema, &tbl],
            )
            .await
            .map_err(|e| format!("Query error: {}", e))?;

        let mut output = format!("{}.{}{} (~{} rows)", schema, tbl, kind_label, approx_rows);
        if let Some(d) = description.filter(|s| !s.is_empty()) {
            let first = d.lines().next().unwrap_or(&d);
            output.push_str(&format!(": {}", first));
        }
        output.push('\n');

        if cols.is_empty() {
            output.push_str("  (no columns)\n");
        } else {
            let parts: Vec<String> = cols
                .iter()
                .map(|r| {
                    let cn: String = r.get(0);
                    let ct: String = r.get(1);
                    let is_pk: bool = r.get(2);
                    let fk: Option<String> = r.get(3);
                    let mut s = format!("{}:{}", cn, ct);
                    if is_pk {
                        s.push_str(" PK");
                    }
                    if let Some(t) = fk {
                        s.push_str(&format!(" →{}", t));
                    }
                    s
                })
                .collect();
            output.push_str(&format!("  {}\n", parts.join(", ")));
        }
        Ok(output)
    }

    /// Describe multiple tables in one call, each in brief form. Errors
    /// on individual tables are emitted inline instead of aborting the
    /// whole call — one bad name shouldn't waste the rest of the batch.
    pub async fn describe_tables(&self, tables: &[String]) -> Result<String, String> {
        if tables.is_empty() {
            return Err("No tables provided.".into());
        }
        let mut output = String::new();
        for (i, table) in tables.iter().enumerate() {
            if i > 0 {
                output.push('\n');
            }
            match self.describe_table_brief(table).await {
                Ok(s) => output.push_str(&s),
                Err(e) => output.push_str(&format!("{}: ERROR {}\n", table, e)),
            }
        }
        Ok(output)
    }

    // ─── Enhanced list_tables ──────────────────────────────────────

    pub async fn list_tables(
        &self,
        schema: Option<&str>,
        include_counts: bool,
    ) -> Result<String, String> {
        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();

        let (schema_pred, params_owned): (&str, Vec<String>) = match schema {
            Some(s) => ("= $1", vec![s.to_string()]),
            None => ("NOT IN ('pg_catalog', 'information_schema')", Vec::new()),
        };

        let sql = if include_counts {
            format!(
                "SELECT t.schemaname, t.tablename AS name, 'table' AS type, \
                        COALESCE(s.n_live_tup, 0)::bigint AS approx_rows, \
                        obj_description(c.oid) AS description \
                 FROM pg_tables t \
                 LEFT JOIN pg_stat_user_tables s \
                    ON s.schemaname = t.schemaname AND s.relname = t.tablename \
                 LEFT JOIN pg_class c \
                    ON c.relname = t.tablename \
                 LEFT JOIN pg_namespace n \
                    ON n.oid = c.relnamespace AND n.nspname = t.schemaname \
                 WHERE t.schemaname {pred} \
                 UNION ALL \
                 SELECT v.schemaname, v.viewname AS name, 'view' AS type, \
                        0::bigint AS approx_rows, \
                        obj_description(c.oid) AS description \
                 FROM pg_views v \
                 LEFT JOIN pg_class c \
                    ON c.relname = v.viewname \
                 LEFT JOIN pg_namespace n \
                    ON n.oid = c.relnamespace AND n.nspname = v.schemaname \
                 WHERE v.schemaname {pred} \
                 ORDER BY schemaname, name",
                pred = schema_pred
            )
        } else {
            format!(
                "SELECT schemaname, tablename AS name, 'table' AS type FROM pg_tables \
                 WHERE schemaname {pred} \
                 UNION ALL \
                 SELECT schemaname, viewname AS name, 'view' AS type FROM pg_views \
                 WHERE schemaname {pred} \
                 ORDER BY schemaname, name",
                pred = schema_pred
            )
        };

        // Both occurrences of `schemaname {pred}` share the same param list
        // when `= $1` is used. Postgres expects one $1 binding regardless
        // of how many times it appears.
        let params: Vec<&(dyn ToSql + Sync)> =
            params_owned.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        let rows = client
            .query(&sql, &params)
            .await
            .map_err(|e| format!("Query error: {}", e))?;
        Ok(format_rows(&rows))
    }

    // ─── Other tools ───────────────────────────────────────────────

    pub async fn test_connection(conn: &Connection) -> Result<(String, u128, bool), String> {
        let start = Instant::now();
        let client = connect_client(conn).await?;
        let row = client
            .query_one("SELECT version()", &[])
            .await
            .map_err(|e| format!("Query failed: {}", e))?;
        let version: String = row.get(0);
        let latency = start.elapsed().as_millis();
        let observed = query_transaction_read_only(&client).await.unwrap_or(false);
        Ok((version, latency, observed))
    }

    pub async fn list_schemas(&self) -> Result<String, String> {
        let sql = "SELECT n.nspname AS schema_name, \
                   COUNT(DISTINCT c.relname) FILTER (WHERE c.relkind = 'r') AS tables, \
                   COUNT(DISTINCT c.relname) FILTER (WHERE c.relkind = 'v') AS views \
                   FROM pg_namespace n \
                   LEFT JOIN pg_class c ON c.relnamespace = n.oid \
                   WHERE n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast') \
                     AND n.nspname NOT LIKE 'pg_temp%' \
                   GROUP BY n.nspname \
                   ORDER BY n.nspname";
        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();
        let rows = client
            .query(sql, &[])
            .await
            .map_err(|e| format!("Query error: {}", e))?;
        Ok(format_rows(&rows))
    }

    pub async fn get_table_sample(
        &self,
        table: &str,
        limit: Option<usize>,
    ) -> Result<String, String> {
        let limit = limit.unwrap_or(5).min(50);
        let (schema_q, tbl_q) = parse_qualified(table);
        let sql = format!("SELECT * FROM {}.{} LIMIT {}", schema_q, tbl_q, limit);
        self.execute_query(&sql, true).await
    }

    pub async fn get_schema_diagram(&self, schema: Option<&str>) -> Result<String, String> {
        let schema = schema.unwrap_or("public").to_string();
        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();

        let col_sql =
            "SELECT c.table_name, c.column_name, c.data_type, \
                    CASE WHEN pk.column_name IS NOT NULL THEN 'PK' ELSE '' END AS is_pk \
             FROM information_schema.columns c \
             LEFT JOIN ( \
                SELECT kcu.table_name, kcu.column_name \
                FROM information_schema.table_constraints tc \
                JOIN information_schema.key_column_usage kcu \
                    ON tc.constraint_name = kcu.constraint_name \
                WHERE tc.constraint_type = 'PRIMARY KEY' AND tc.table_schema = $1 \
             ) pk ON c.table_name = pk.table_name AND c.column_name = pk.column_name \
             WHERE c.table_schema = $1 \
             ORDER BY c.table_name, c.ordinal_position";
        let col_rows = client
            .query(col_sql, &[&schema])
            .await
            .map_err(|e| format!("Query error: {}", e))?;

        let fk_sql =
            "SELECT tc.table_name, kcu.column_name, \
                    ccu.table_name AS foreign_table, ccu.column_name AS foreign_column \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
                ON tc.constraint_name = kcu.constraint_name \
             JOIN information_schema.constraint_column_usage ccu \
                ON ccu.constraint_name = tc.constraint_name \
             WHERE tc.constraint_type = 'FOREIGN KEY' AND tc.table_schema = $1";
        let fk_rows = client
            .query(fk_sql, &[&schema])
            .await
            .map_err(|e| format!("Query error: {}", e))?;

        let mut tables: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
        for row in &col_rows {
            let table_name: String = row.get(0);
            let col_name: String = row.get(1);
            let data_type: String = row.get(2);
            let is_pk: String = row.get(3);
            tables
                .entry(table_name)
                .or_default()
                .push((col_name, data_type, is_pk));
        }

        let mut diagram = format!("Schema: {}\n\n", schema);
        let mut table_names: Vec<&String> = tables.keys().collect();
        table_names.sort();

        for name in &table_names {
            diagram.push_str(&format!("┌─ {} ─────────────────────────────┐\n", name));
            if let Some(cols) = tables.get(*name) {
                for (col_name, data_type, is_pk) in cols {
                    let pk_marker = if is_pk == "PK" { " [PK]" } else { "" };
                    diagram.push_str(&format!("│  {}{} : {}\n", col_name, pk_marker, data_type));
                }
            }
            diagram.push_str("└────────────────────────────────────┘\n\n");
        }

        if !fk_rows.is_empty() {
            diagram.push_str("Relationships:\n");
            for row in &fk_rows {
                let table: String = row.get(0);
                let col: String = row.get(1);
                let ftable: String = row.get(2);
                let fcol: String = row.get(3);
                diagram.push_str(&format!("  {}.{} ──> {}.{}\n", table, col, ftable, fcol));
            }
        }

        Ok(diagram)
    }

    // ─── Orientation tools ─────────────────────────────────────────

    /// Single-call orientation package. Bundles what an agent otherwise
    /// gets from `list_schemas` + `list_tables --include-counts` +
    /// `describe_table` (xN) + `get_table_sample` (xN) into one response.
    /// Sample rows respect the connection's PII redaction setting.
    pub async fn db_overview(&self, top_n: usize) -> Result<String, String> {
        let top_n = top_n.clamp(1, 50) as i64;
        let redact = *self.redact_pii.lock().await;
        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();

        let mut output = String::new();

        let hdr = client
            .query_one("SELECT current_database(), version()", &[])
            .await
            .map_err(|e| format!("Query error: {}", e))?;
        let db_name: String = hdr.get(0);
        let version: String = hdr.get(1);
        let short_ver = version.split(" on ").next().unwrap_or(&version);
        output.push_str(&format!("DATABASE: {}\n  Version: {}\n", db_name, short_ver));

        let schemas = client
            .query(
                "SELECT n.nspname, \
                        COUNT(*) FILTER (WHERE c.relkind = 'r')::int8 AS tables, \
                        COUNT(*) FILTER (WHERE c.relkind = 'v')::int8 AS views \
                 FROM pg_namespace n \
                 LEFT JOIN pg_class c ON c.relnamespace = n.oid \
                 WHERE n.nspname NOT IN ('pg_catalog','information_schema','pg_toast') \
                   AND n.nspname NOT LIKE 'pg_temp%' \
                   AND n.nspname NOT LIKE 'pg_toast_temp%' \
                 GROUP BY n.nspname \
                 ORDER BY n.nspname",
                &[],
            )
            .await
            .map_err(|e| format!("Query error: {}", e))?;
        if !schemas.is_empty() {
            let parts: Vec<String> = schemas
                .iter()
                .map(|r| {
                    let n: String = r.get(0);
                    let t: i64 = r.get(1);
                    let v: i64 = r.get(2);
                    format!("{}({}t,{}v)", n, t, v)
                })
                .collect();
            output.push_str(&format!("  Schemas: {}\n\n", parts.join(", ")));
        } else {
            output.push('\n');
        }

        let top_tables = client
            .query(
                "SELECT n.nspname AS schema, c.relname AS name, \
                        COALESCE(s.n_live_tup, 0)::bigint AS approx_rows, \
                        pg_size_pretty(pg_total_relation_size(c.oid)) AS size, \
                        obj_description(c.oid) AS description \
                 FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 LEFT JOIN pg_stat_user_tables s ON s.relid = c.oid \
                 WHERE c.relkind = 'r' \
                   AND n.nspname NOT IN ('pg_catalog','information_schema','pg_toast') \
                   AND n.nspname NOT LIKE 'pg_temp%' \
                 ORDER BY pg_total_relation_size(c.oid) DESC NULLS LAST \
                 LIMIT $1",
                &[&top_n],
            )
            .await
            .map_err(|e| format!("Query error: {}", e))?;

        if top_tables.is_empty() {
            output.push_str("No user tables found.\n");
            return Ok(output);
        }

        output.push_str(&format!("TOP {} TABLES BY SIZE:\n", top_tables.len()));

        for tbl in &top_tables {
            let schema: String = tbl.get(0);
            let name: String = tbl.get(1);
            let approx_rows: i64 = tbl.get(2);
            let size: String = tbl.get(3);
            let description: Option<String> = tbl.get(4);

            output.push_str(&format!(
                "\n  {}.{} ({}, ~{} rows)\n",
                schema, name, size, approx_rows
            ));
            if let Some(d) = description.filter(|s| !s.is_empty()) {
                let first = d.lines().next().unwrap_or(&d);
                output.push_str(&format!("    note: {}\n", first));
            }

            let cols = client
                .query(
                    "SELECT a.attname, \
                            format_type(a.atttypid, a.atttypmod) AS type, \
                            EXISTS ( \
                                SELECT 1 FROM pg_constraint k \
                                WHERE k.conrelid = a.attrelid \
                                  AND k.contype = 'p' \
                                  AND a.attnum = ANY(k.conkey) \
                            ) AS is_pk, \
                            ( \
                                SELECT cn.nspname || '.' || cc.relname || '.' || att.attname \
                                FROM pg_constraint k \
                                JOIN pg_class cc ON cc.oid = k.confrelid \
                                JOIN pg_namespace cn ON cn.oid = cc.relnamespace \
                                JOIN pg_attribute att \
                                    ON att.attrelid = k.confrelid \
                                    AND att.attnum = k.confkey[array_position(k.conkey, a.attnum)] \
                                WHERE k.conrelid = a.attrelid \
                                  AND k.contype = 'f' \
                                  AND a.attnum = ANY(k.conkey) \
                                LIMIT 1 \
                            ) AS fk_target \
                     FROM pg_attribute a \
                     JOIN pg_class c ON c.oid = a.attrelid \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND c.relname = $2 \
                       AND a.attnum > 0 AND NOT a.attisdropped \
                     ORDER BY a.attnum",
                    &[&schema, &name],
                )
                .await
                .map_err(|e| format!("Query error: {}", e))?;

            if !cols.is_empty() {
                let parts: Vec<String> = cols
                    .iter()
                    .map(|r| {
                        let cn: String = r.get(0);
                        let ct: String = r.get(1);
                        let is_pk: bool = r.get(2);
                        let fk: Option<String> = r.get(3);
                        let mut s = format!("{}:{}", cn, ct);
                        if is_pk {
                            s.push_str(" PK");
                        }
                        if let Some(t) = fk {
                            s.push_str(&format!(" →{}", t));
                        }
                        s
                    })
                    .collect();
                output.push_str(&format!("    cols: {}\n", parts.join(", ")));
            }

            let schema_q = quote_ident(&schema);
            let tbl_q = quote_ident(&name);
            let sample_sql = format!("SELECT * FROM {}.{} LIMIT 3", schema_q, tbl_q);
            if let Ok(rows) = client.query(&sample_sql, &[]).await {
                if !rows.is_empty() {
                    output.push_str("    sample:\n");
                    for line in format_rows_opt(&rows, redact).lines() {
                        output.push_str("      ");
                        output.push_str(line);
                        output.push('\n');
                    }
                }
            }
        }

        let fks = client
            .query(
                "SELECT n1.nspname || '.' || c1.relname AS tbl, \
                        a1.attname AS col, \
                        n2.nspname || '.' || c2.relname AS ref_tbl, \
                        a2.attname AS ref_col \
                 FROM pg_constraint k \
                 JOIN pg_class c1 ON c1.oid = k.conrelid \
                 JOIN pg_namespace n1 ON n1.oid = c1.relnamespace \
                 JOIN pg_class c2 ON c2.oid = k.confrelid \
                 JOIN pg_namespace n2 ON n2.oid = c2.relnamespace \
                 JOIN pg_attribute a1 \
                    ON a1.attrelid = k.conrelid AND a1.attnum = k.conkey[1] \
                 JOIN pg_attribute a2 \
                    ON a2.attrelid = k.confrelid AND a2.attnum = k.confkey[1] \
                 WHERE k.contype = 'f' \
                   AND n1.nspname NOT IN ('pg_catalog','information_schema') \
                 ORDER BY c1.relname, a1.attname \
                 LIMIT 30",
                &[],
            )
            .await
            .map_err(|e| format!("Query error: {}", e))?;
        if !fks.is_empty() {
            output.push_str("\nRELATIONSHIPS (first 30):\n");
            for row in &fks {
                let t: String = row.get(0);
                let c: String = row.get(1);
                let rt: String = row.get(2);
                let rc: String = row.get(3);
                output.push_str(&format!("  {}.{} → {}.{}\n", t, c, rt, rc));
            }
        }

        drop(guard);
        self.reset_if_readonly_outside_txn().await;
        Ok(output)
    }

    /// Ranked ILIKE search across table names, column names, and their
    /// comments. Lets an agent locate identifiers by concept instead of
    /// guessing names + describing until it finds them.
    pub async fn search_schema(&self, query: &str, limit: usize) -> Result<String, String> {
        let query = query.trim();
        if query.is_empty() {
            return Err("search query is empty".into());
        }
        let limit = limit.clamp(1, 100) as i64;
        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();

        let comment_pat = format!("%{}%", query);

        let sql = "\
            WITH hits AS ( \
                SELECT 'table'::text AS kind, \
                       n.nspname AS schema, \
                       c.relname AS object, \
                       NULL::text AS column, \
                       obj_description(c.oid) AS comment, \
                       (CASE WHEN lower(c.relname) = lower($1) THEN 100 \
                             WHEN c.relname ILIKE $1 || '%' THEN 50 \
                             WHEN c.relname ILIKE '%' || $1 || '%' THEN 20 \
                             ELSE 0 END) \
                     + (CASE WHEN obj_description(c.oid) ILIKE $2 THEN 5 ELSE 0 END) AS score \
                FROM pg_class c \
                JOIN pg_namespace n ON n.oid = c.relnamespace \
                WHERE c.relkind IN ('r', 'v') \
                  AND n.nspname NOT IN ('pg_catalog','information_schema','pg_toast') \
                UNION ALL \
                SELECT 'column'::text, \
                       n.nspname, \
                       c.relname, \
                       a.attname, \
                       col_description(c.oid, a.attnum::int), \
                       (CASE WHEN lower(a.attname) = lower($1) THEN 80 \
                             WHEN a.attname ILIKE $1 || '%' THEN 40 \
                             WHEN a.attname ILIKE '%' || $1 || '%' THEN 15 \
                             ELSE 0 END) \
                     + (CASE WHEN col_description(c.oid, a.attnum::int) ILIKE $2 THEN 3 ELSE 0 END) \
                FROM pg_class c \
                JOIN pg_namespace n ON n.oid = c.relnamespace \
                JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
                WHERE c.relkind IN ('r', 'v') \
                  AND n.nspname NOT IN ('pg_catalog','information_schema','pg_toast') \
            ) \
            SELECT kind, schema, object, column, comment, score \
            FROM hits \
            WHERE score > 0 \
            ORDER BY score DESC, kind, schema, object, column \
            LIMIT $3";

        let rows = client
            .query(sql, &[&query, &comment_pat, &limit])
            .await
            .map_err(|e| format!("Query error: {}", e))?;

        if rows.is_empty() {
            return Ok(format!(
                "No matches for '{}'. Try a shorter or different term.\n",
                query
            ));
        }

        let mut output = format!("Matches for '{}' ({} result{}):\n", query, rows.len(),
            if rows.len() == 1 { "" } else { "s" });
        for row in &rows {
            let kind: String = row.get(0);
            let schema: String = row.get(1);
            let object: String = row.get(2);
            let column: Option<String> = row.get(3);
            let comment: Option<String> = row.get(4);

            match kind.as_str() {
                "column" => {
                    let c = column.unwrap_or_default();
                    output.push_str(&format!("  column  {}.{}.{}", schema, object, c));
                }
                _ => {
                    output.push_str(&format!("  table   {}.{}", schema, object));
                }
            }
            if let Some(c) = comment.filter(|s| !s.is_empty()) {
                let first = c.lines().next().unwrap_or(&c);
                let trimmed = if first.len() > 80 {
                    format!("{}…", &first[..80])
                } else {
                    first.to_string()
                };
                output.push_str(&format!(" — {}", trimmed));
            }
            output.push('\n');
        }
        Ok(output)
    }

    // ─── Query history ─────────────────────────────────────────────

    async fn record_query(
        &self,
        sql: &str,
        duration_ms: u128,
        row_count: Option<usize>,
        error: Option<String>,
    ) {
        let conn_name = self.active_connection_name.lock().await.clone();

        // Persist to the shared audit log first so a crash between this
        // call and in-memory insertion still leaves a trail.
        crate::audit::log(
            if error.is_some() { "query_error" } else { "query" },
            serde_json::json!({
                "connection": conn_name,
                // Truncate very long SQL so the log file doesn't balloon
                // on a pathological insert. Full text still lives in the
                // in-memory ring for `query_history`.
                "sql": truncate_for_log(sql, 4000),
                "duration_ms": duration_ms as u64,
                "rows": row_count,
                "error": error,
            }),
        );

        let record = QueryRecord {
            sql: sql.to_string(),
            timestamp: chrono::Utc::now().naive_utc(),
            duration_ms,
            row_count,
            error,
        };
        let mut history = self.query_history.lock().await;
        history.push(record);
        if history.len() > 200 {
            let excess = history.len() - 200;
            history.drain(0..excess);
        }
    }

    pub async fn get_query_history(&self, limit: usize) -> String {
        let history = self.query_history.lock().await;
        if history.is_empty() {
            return "No queries in session history.\n".into();
        }

        let start = if history.len() > limit { history.len() - limit } else { 0 };
        let mut output = format!("Last {} queries:\n\n", history.len().min(limit));

        for (i, record) in history[start..].iter().enumerate() {
            let status = if let Some(ref err) = record.error {
                format!("ERROR: {}", err)
            } else {
                format!("{} rows, {}ms", record.row_count.unwrap_or(0), record.duration_ms)
            };
            output.push_str(&format!(
                "{}. [{}] {}\n   {}\n\n",
                i + 1,
                record.timestamp.format("%H:%M:%S"),
                status,
                record.sql
            ));
        }

        output
    }
}

// ─── Connection helpers ─────────────────────────────────────────────

async fn connect_client(conn: &Connection) -> Result<Client, String> {
    let cfg = build_connect_config(conn)?;

    // Attempt TLS whenever the UI toggle is on *or* the parsed sslmode
    // demands it. The second half matters for hosted Postgres like AWS
    // RDS: users often paste a `postgresql://…?sslmode=require` URL
    // without flipping the SSL toggle, and tokio-postgres rejects
    // `Require` + NoTls with a terse "no tls" error.
    let use_tls = conn.ssl || cfg.get_ssl_mode() == SslMode::Require;

    let client = if use_tls {
        let tls = TlsConnector::builder()
            .build()
            .map_err(|e| format!("TLS setup failed: {}", e))?;
        let tls = MakeTlsConnector::new(tls);
        let (c, h) = cfg
            .connect(tls)
            .await
            .map_err(|e| format!("Connection failed (TLS): {}", hint_ssl_error(&e.to_string(), conn)))?;
        tokio::spawn(async move {
            if let Err(e) = h.await {
                log::error!("Connection error: {}", e);
            }
        });
        c
    } else {
        let (c, h) = cfg
            .connect(NoTls)
            .await
            .map_err(|e| format!("Connection failed: {}", hint_ssl_error(&e.to_string(), conn)))?;
        tokio::spawn(async move {
            if let Err(e) = h.await {
                log::error!("Connection error: {}", e);
            }
        });
        c
    };

    Ok(client)
}

/// Append an actionable hint when the underlying error is the classic
/// "server requires SSL but the client didn't offer it" shape. Hosted
/// Postgres (AWS RDS, GCP Cloud SQL, Azure) rejects plaintext with this
/// error and new users have no way to know the UI SSL toggle is the fix.
fn hint_ssl_error(msg: &str, conn: &Connection) -> String {
    let lower = msg.to_ascii_lowercase();
    let looks_like_ssl_required = lower.contains("no encryption")
        || lower.contains("ssl connection is required")
        || lower.contains("ssl off")
        || lower.contains("no tls")
        || lower.contains("no pg_hba.conf entry");
    if looks_like_ssl_required && !conn.ssl {
        format!(
            "{msg}\n\nThis server appears to require SSL. Enable the SSL \
             toggle on this connection (or add `sslmode=require` to your \
             connection string) and try again."
        )
    } else {
        msg.to_string()
    }
}

/// Build the `tokio_postgres::Config` we'll connect with. Either parses
/// the user's connection string or assembles a Config from the discrete
/// fields, then layers on our overrides — notably, for read-only
/// connections, `default_transaction_read_only=on` in the libpq startup
/// `options` parameter.
///
/// Putting readonly in `options` rather than as a post-connect `SET` means
/// Postgres applies it before the first client statement runs. It's also
/// harder for a later SET to flip: `-c` values become the session default,
/// and `RESET default_transaction_read_only` brings you back to `on`
/// rather than `off`. A malicious `SET` can still flip it temporarily
/// (the GUC is USERSET), which is why the classifier blocks SET and why
/// we run DISCARD ALL + re-harden after each query.
fn build_connect_config(conn: &Connection) -> Result<PgConfig, String> {
    let mut cfg: PgConfig = if let Some(ref cs) = conn.connection_string {
        // AWS RDS docs and the `psql` CLI commonly use `sslmode=verify-ca`
        // or `sslmode=verify-full`, but tokio-postgres 0.7 only accepts
        // `disable|prefer|require`. Rewrite the stricter modes to
        // `require` before parsing — `native-tls` already performs full
        // CA + hostname verification by default, so the effective
        // security level is unchanged.
        let normalized = normalize_sslmode_param(cs);
        normalized
            .parse::<PgConfig>()
            .map_err(|e| format!("Invalid connection string: {}", e))?
    } else {
        let mut c = PgConfig::new();
        c.host(&conn.host)
            .port(conn.port)
            .dbname(&conn.database)
            .user(&conn.user)
            .password(&conn.password);
        c
    };

    if cfg.get_connect_timeout().is_none() {
        cfg.connect_timeout(std::time::Duration::from_secs(10));
    }

    if conn.readonly {
        let existing = cfg.get_options().unwrap_or("").trim().to_string();
        let injection = "-c default_transaction_read_only=on";
        let combined = if existing.is_empty() {
            injection.to_string()
        } else if existing.contains("default_transaction_read_only") {
            // Leave a user-supplied setting alone — they may be doing
            // something unusual on purpose. The connect-time SHOW check
            // will catch it if the resulting state disagrees with our flag.
            existing
        } else {
            format!("{} {}", existing, injection)
        };
        cfg.options(&combined);
    }

    Ok(cfg)
}

/// Rewrite `sslmode=verify-ca` / `sslmode=verify-full` to `sslmode=require`
/// inside a connection string, leaving everything else untouched.
/// tokio-postgres 0.7 only accepts `disable|prefer|require` and rejects
/// the verify-* variants with an "Invalid value" error, even though
/// AWS RDS and the libpq CLI treat them as the canonical "secure" modes.
/// `native-tls` already performs full CA + hostname verification by
/// default, so mapping to `require` preserves the user's intent.
fn normalize_sslmode_param(cs: &str) -> String {
    let lower = cs.to_ascii_lowercase();
    let key = "sslmode=";
    let mut out = String::with_capacity(cs.len());
    let mut copied = 0;
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find(key) {
        let key_start = search_from + rel;
        let value_start = key_start + key.len();
        let value_end = cs[value_start..]
            .find(|c: char| c == ' ' || c == '\t' || c == '&' || c == '\n' || c == '\r')
            .map(|p| value_start + p)
            .unwrap_or(cs.len());
        let value = &cs[value_start..value_end];
        if value.eq_ignore_ascii_case("verify-ca") || value.eq_ignore_ascii_case("verify-full") {
            out.push_str(&cs[copied..value_start]);
            out.push_str("require");
            copied = value_end;
        }
        search_from = value_end;
    }
    out.push_str(&cs[copied..]);
    out
}

/// Apply per-session guardrails after connect:
/// - application_name: tags the connection in `pg_stat_activity` as
///   `pg-mcp/<session-uuid>/<connection-name>` so operators can tell
///   concurrent agents apart
/// - statement_timeout: bounds any single query
/// - idle_in_transaction_session_timeout: prevents dangling txns locking rows
///
/// Returns the SQL that was executed so callers can replay it after a
/// `DISCARD ALL` (which resets `application_name` and the timeouts along
/// with everything else).
///
/// Server-side readonly enforcement is set at connect time via the startup
/// `options` param (see `build_connect_config`) rather than here, so it
/// applies before any SQL runs.
async fn apply_hardening(client: &Client, conn: &Connection) -> Result<String, String> {
    let app_name_raw = format!(
        "pg-mcp/{}/{}",
        crate::audit::session_id(),
        conn.name
    );
    let app_name_escaped = app_name_raw.replace('\'', "''");

    // For RO connections we also re-assert `default_transaction_read_only`
    // here so a `RESET` or `DISCARD ALL` followed by this SQL puts us
    // back into the enforced state regardless of what the session default
    // resolves to.
    let ro_reassert = if conn.readonly {
        " SET default_transaction_read_only = on;"
    } else {
        ""
    };

    let sql = format!(
        "SET application_name = '{app}'; \
         SET statement_timeout = '{st}'; \
         SET idle_in_transaction_session_timeout = '{it}';{ro}",
        app = app_name_escaped,
        st = STATEMENT_TIMEOUT,
        it = IDLE_IN_TXN_TIMEOUT,
        ro = ro_reassert,
    );
    client
        .batch_execute(&sql)
        .await
        .map_err(|e| format!("Failed to apply session settings: {}", e))?;
    Ok(sql)
}

async fn query_transaction_read_only(client: &Client) -> Result<bool, String> {
    let row = client
        .query_one("SHOW transaction_read_only", &[])
        .await
        .map_err(|e| format!("Failed to verify transaction_read_only: {}", e))?;
    let val: String = row.get(0);
    Ok(val.eq_ignore_ascii_case("on"))
}

// ─── Identifier helpers ─────────────────────────────────────────────

/// Quote a SQL identifier. Doubles any embedded double-quote.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Split "schema.table" or "table" and return owned un-quoted parts for
/// use as query *parameters* (not interpolated into SQL).
fn split_qualified(table: &str) -> (String, String) {
    if let Some((s, t)) = table.split_once('.') {
        (s.to_string(), t.to_string())
    } else {
        ("public".to_string(), table.to_string())
    }
}

/// Parse "schema.table" or "table" and return quoted identifiers safe to
/// interpolate into SQL.
fn parse_qualified(table: &str) -> (String, String) {
    let (s, t) = split_qualified(table);
    (quote_ident(&s), quote_ident(&t))
}

/// Wrap a value in a PostgreSQL dollar-quoted string literal. The tag is
/// expanded until it does not appear in the value, making injection via
/// crafted input impossible — the closing delimiter is always unique.
/// Unlike single-quote escaping, this is unaffected by
/// `standard_conforming_strings`.
fn dollar_quote(v: &str) -> String {
    let mut tag = String::from("pgmcp");
    while v.contains(&format!("${}$", tag)) {
        tag.push('_');
    }
    format!("${tag}${v}${tag}$", tag = tag, v = v)
}

/// Recognize a narrow set of literal SQL sentinels that callers may want
/// to emit unquoted in INSERT/UPDATE value positions. Anything else is
/// treated as a bind parameter.
fn literal_sentinel(v: &str) -> Option<&'static str> {
    match v.trim().to_uppercase().as_str() {
        "NULL" => Some("NULL"),
        "DEFAULT" => Some("DEFAULT"),
        "TRUE" => Some("TRUE"),
        "FALSE" => Some("FALSE"),
        "NOW()" => Some("NOW()"),
        "CURRENT_TIMESTAMP" => Some("CURRENT_TIMESTAMP"),
        "CURRENT_DATE" => Some("CURRENT_DATE"),
        "CURRENT_TIME" => Some("CURRENT_TIME"),
        _ => None,
    }
}

// ─── Row formatting ────────────────────────────────────────────────

/// Format rows for the MCP response. When `redact` is true, PII cells
/// are replaced with `[REDACTED]` *before* truncation, so partial
/// content cannot leak via the width-clamp path.
fn format_rows(rows: &[Row]) -> String {
    format_rows_opt(rows, false)
}

fn format_rows_opt(rows: &[Row], redact: bool) -> String {
    if rows.is_empty() {
        return "(0 rows)\n".to_string();
    }

    let columns = rows[0].columns();
    let headers: Vec<String> = columns.iter().map(|c| c.name().to_string()).collect();

    let mut data: Vec<Vec<String>> = Vec::new();
    for row in rows.iter().take(MAX_RESULT_ROWS) {
        let mut row_data = Vec::new();
        for (i, col) in columns.iter().enumerate() {
            let raw = get_cell_string(row, i, col.type_());
            let val = if redact {
                crate::pii::redact(col.name(), &raw)
            } else {
                raw
            };
            let truncated = if val.len() > MAX_COLUMN_WIDTH {
                format!("{}...", &val[..MAX_COLUMN_WIDTH - 3])
            } else {
                val
            };
            row_data.push(truncated);
        }
        data.push(row_data);
    }

    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row_data in &data {
        for (i, val) in row_data.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(val.len());
            }
        }
    }

    let mut output = String::new();
    let header_line: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{:width$}", h, width = widths[i]))
        .collect();
    output.push_str(&header_line.join(" | "));
    output.push('\n');

    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    output.push_str(&sep.join("-+-"));
    output.push('\n');

    for row_data in &data {
        let line: Vec<String> = row_data
            .iter()
            .enumerate()
            .map(|(i, v)| format!("{:width$}", v, width = widths.get(i).copied().unwrap_or(0)))
            .collect();
        output.push_str(&line.join(" | "));
        output.push('\n');
    }

    let total = rows.len();
    if total > MAX_RESULT_ROWS {
        output.push_str(&format!("\n(showing {} of {} rows)\n", MAX_RESULT_ROWS, total));
    } else {
        output.push_str(&format!("({} rows)\n", total));
    }

    output
}

fn get_cell_string(row: &Row, idx: usize, ty: &tokio_postgres::types::Type) -> String {
    use tokio_postgres::types::Type;

    match *ty {
        Type::BOOL => try_get_opt::<bool>(row, idx),
        Type::INT2 => try_get_opt::<i16>(row, idx),
        Type::INT4 | Type::OID => try_get_opt::<i32>(row, idx),
        Type::INT8 => try_get_opt::<i64>(row, idx),
        Type::FLOAT4 => try_get_opt::<f32>(row, idx),
        Type::FLOAT8 => try_get_opt::<f64>(row, idx),
        Type::NUMERIC => try_get_opt::<rust_decimal::Decimal>(row, idx),
        Type::JSON | Type::JSONB => match row.try_get::<_, Option<serde_json::Value>>(idx) {
            Ok(Some(v)) => v.to_string(),
            Ok(None) => "NULL".into(),
            Err(_) => "NULL".into(),
        },
        Type::TIMESTAMP => match row.try_get::<_, Option<chrono::NaiveDateTime>>(idx) {
            Ok(Some(v)) => v.format("%Y-%m-%d %H:%M:%S%.f").to_string(),
            Ok(None) => "NULL".into(),
            Err(_) => "NULL".into(),
        },
        Type::TIMESTAMPTZ => match row.try_get::<_, Option<chrono::DateTime<chrono::Utc>>>(idx) {
            Ok(Some(v)) => v.format("%Y-%m-%d %H:%M:%S%.f%z").to_string(),
            Ok(None) => "NULL".into(),
            Err(_) => "NULL".into(),
        },
        Type::DATE => match row.try_get::<_, Option<chrono::NaiveDate>>(idx) {
            Ok(Some(v)) => v.format("%Y-%m-%d").to_string(),
            Ok(None) => "NULL".into(),
            Err(_) => "NULL".into(),
        },
        Type::TIME => match row.try_get::<_, Option<chrono::NaiveTime>>(idx) {
            Ok(Some(v)) => v.format("%H:%M:%S%.f").to_string(),
            Ok(None) => "NULL".into(),
            Err(_) => "NULL".into(),
        },
        Type::UUID => match row.try_get::<_, Option<uuid::Uuid>>(idx) {
            Ok(Some(v)) => v.to_string(),
            Ok(None) => "NULL".into(),
            Err(_) => "NULL".into(),
        },
        Type::BYTEA => match row.try_get::<_, Option<Vec<u8>>>(idx) {
            Ok(Some(v)) => format!("\\x{}", hex::encode(&v)),
            Ok(None) => "NULL".into(),
            Err(_) => "NULL".into(),
        },
        Type::CHAR_ARRAY | Type::TEXT_ARRAY | Type::VARCHAR_ARRAY => {
            match row.try_get::<_, Option<Vec<String>>>(idx) {
                Ok(Some(v)) => format!("{{{}}}", v.join(",")),
                Ok(None) => "NULL".into(),
                Err(_) => "NULL".into(),
            }
        }
        Type::INT4_ARRAY => match row.try_get::<_, Option<Vec<i32>>>(idx) {
            Ok(Some(v)) => format!(
                "{{{}}}",
                v.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
            ),
            Ok(None) => "NULL".into(),
            Err(_) => "NULL".into(),
        },
        _ => match row.try_get::<_, Option<String>>(idx) {
            Ok(Some(v)) => v,
            Ok(None) => "NULL".into(),
            Err(_) => "NULL".into(),
        },
    }
}

fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Respect char boundaries so we don't produce invalid UTF-8.
        let end = s
            .char_indices()
            .take_while(|(i, _)| *i < max)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!("{}…", &s[..end])
    }
}

fn try_get_opt<'a, T: std::fmt::Display + tokio_postgres::types::FromSql<'a>>(
    row: &'a Row,
    idx: usize,
) -> String {
    match row.try_get::<_, Option<T>>(idx) {
        Ok(Some(v)) => v.to_string(),
        Ok(None) => "NULL".into(),
        Err(_) => "NULL".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_blocks_writes() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "  update t set x=1  ",
            "DELETE FROM t",
            "DROP TABLE t",
            "CREATE TABLE t ()",
            "set role admin",
        ] {
            assert!(DatabaseManager::check_write_query(sql), "should block: {}", sql);
        }
    }

    #[test]
    fn classifier_allows_reads_and_txn() {
        for sql in [
            "SELECT 1",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "BEGIN",
            "COMMIT",
            "ROLLBACK",
            "SAVEPOINT foo",
            "RELEASE SAVEPOINT foo",
        ] {
            assert!(!DatabaseManager::check_write_query(sql), "should allow: {}", sql);
        }
    }

    #[test]
    fn classifier_blocks_session_mutating() {
        for sql in [
            "SET default_transaction_read_only = off",
            "set local statement_timeout = '0'",
            "  SET SESSION x = 1",
            "RESET ALL",
            "DISCARD ALL",
            "discard plans",
        ] {
            assert!(DatabaseManager::check_session_mutating(sql), "should block: {}", sql);
        }
    }

    #[test]
    fn classifier_allows_non_session_statements() {
        for sql in ["SELECT 1", "INSERT INTO t VALUES (1)", "BEGIN"] {
            assert!(!DatabaseManager::check_session_mutating(sql), "should not flag: {}", sql);
        }
    }

    #[test]
    fn normalize_sslmode_rewrites_verify_variants() {
        let url = "postgresql://u:p@mydb.xyz.us-east-1.rds.amazonaws.com:5432/d?sslmode=verify-full";
        assert_eq!(
            normalize_sslmode_param(url),
            "postgresql://u:p@mydb.xyz.us-east-1.rds.amazonaws.com:5432/d?sslmode=require"
        );

        let kv = "host=rds sslmode=verify-ca user=x";
        assert_eq!(normalize_sslmode_param(kv), "host=rds sslmode=require user=x");

        // Case-insensitive key, preserves casing of surrounding params.
        let mixed = "HOST=rds SSLMODE=Verify-Full USER=x";
        assert_eq!(normalize_sslmode_param(mixed), "HOST=rds SSLMODE=require USER=x");

        // Multiple query params, trailing sslmode.
        let multi = "postgresql://h/d?connect_timeout=10&sslmode=verify-ca";
        assert_eq!(
            normalize_sslmode_param(multi),
            "postgresql://h/d?connect_timeout=10&sslmode=require"
        );
    }

    #[test]
    fn normalize_sslmode_leaves_supported_modes_alone() {
        for cs in [
            "postgresql://h/d?sslmode=require",
            "postgresql://h/d?sslmode=prefer",
            "postgresql://h/d?sslmode=disable",
            "postgresql://h/d",
            "host=h user=u",
        ] {
            assert_eq!(normalize_sslmode_param(cs), cs, "should pass through: {}", cs);
        }
    }

    #[test]
    fn normalize_sslmode_result_parses() {
        let rewritten =
            normalize_sslmode_param("postgresql://u:p@h:5432/d?sslmode=verify-full");
        rewritten
            .parse::<PgConfig>()
            .expect("rewritten connection string should parse");
    }
}
