use crate::config::Connection;
use native_tls::TlsConnector;
use postgres_native_tls::MakeTlsConnector;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Config as PgConfig, NoTls, Row};

const MAX_RESULT_ROWS: usize = 100;
const MAX_COLUMN_WIDTH: usize = 60;
const STATEMENT_TIMEOUT: &str = "30s";
const IDLE_IN_TXN_TIMEOUT: &str = "60s";

/// Best-effort keyword blocklist. This is a UX fast-fail only — real
/// readonly enforcement happens server-side via
/// `SET default_transaction_read_only = on` applied at connect time.
const WRITE_KEYWORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "DROP", "CREATE", "ALTER", "TRUNCATE",
    "GRANT", "REVOKE", "COPY", "VACUUM", "REINDEX", "COMMENT", "SECURITY",
];

const WRITE_COMPOUND_KEYWORDS: &[&str] = &["SET ROLE", "RESET ROLE"];

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
}

impl DatabaseManager {
    pub fn new() -> Self {
        Self {
            client: Arc::new(Mutex::new(None)),
            active_connection_name: Arc::new(Mutex::new(None)),
            in_transaction: Arc::new(Mutex::new(false)),
            query_history: Arc::new(Mutex::new(Vec::new())),
            redact_pii: Arc::new(Mutex::new(false)),
        }
    }

    pub async fn active_name(&self) -> Option<String> {
        self.active_connection_name.lock().await.clone()
    }

    pub async fn connect(&self, conn: &Connection) -> Result<(), String> {
        self.disconnect().await;
        let client = connect_client(conn).await?;
        *self.client.lock().await = Some(client);
        *self.active_connection_name.lock().await = Some(conn.name.clone());
        *self.in_transaction.lock().await = false;
        *self.redact_pii.lock().await = conn.redact_pii;
        crate::audit::log(
            "db_connected",
            serde_json::json!({
                "connection": conn.name,
                "host": conn.host,
                "database": conn.database,
                "ssl": conn.ssl,
                "readonly": conn.readonly,
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

    // ─── Core query execution ──────────────────────────────────────

    pub async fn execute_query(
        &self,
        sql: &str,
        readonly: bool,
    ) -> Result<String, String> {
        // Fast-fail UX check. Real enforcement is via session-level
        // default_transaction_read_only applied at connect time.
        if readonly && Self::check_write_query(sql) {
            return Err(
                "Write operation blocked. This connection is read-only.\n\
                 To enable writes, open pg-mcp UI and disable read-only mode for this connection."
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
        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();

        let result = client.query(sql, params).await;
        let duration_ms = start.elapsed().as_millis();

        match result {
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
        }
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
        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();

        if !*self.in_transaction.lock().await {
            return Err("No active transaction to commit.".into());
        }

        client.execute("COMMIT", &[]).await
            .map_err(|e| format!("Failed to commit: {}", e))?;

        *self.in_transaction.lock().await = false;
        Ok("Transaction committed.".into())
    }

    pub async fn rollback_transaction(&self) -> Result<String, String> {
        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();

        if !*self.in_transaction.lock().await {
            return Err("No active transaction to rollback.".into());
        }

        client.execute("ROLLBACK", &[]).await
            .map_err(|e| format!("Failed to rollback: {}", e))?;

        *self.in_transaction.lock().await = false;
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

    pub async fn describe_table(&self, table: &str) -> Result<String, String> {
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

    pub async fn test_connection(conn: &Connection) -> Result<(String, u128), String> {
        let start = Instant::now();
        let client = connect_client(conn).await?;
        let row = client
            .query_one("SELECT version()", &[])
            .await
            .map_err(|e| format!("Query failed: {}", e))?;
        let version: String = row.get(0);
        let latency = start.elapsed().as_millis();
        Ok((version, latency))
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
    let client = if conn.ssl {
        let tls = TlsConnector::builder()
            .build()
            .map_err(|e| format!("TLS setup failed: {}", e))?;
        let tls = MakeTlsConnector::new(tls);

        if let Some(ref cs) = conn.connection_string {
            let (c, h) = tokio_postgres::connect(cs, tls)
                .await
                .map_err(|e| format!("Connection failed (TLS): {}", e))?;
            tokio::spawn(async move {
                if let Err(e) = h.await {
                    log::error!("Connection error: {}", e);
                }
            });
            c
        } else {
            let cfg = build_pg_config(conn);
            let (c, h) = cfg
                .connect(tls)
                .await
                .map_err(|e| format!("Connection failed (TLS): {}", e))?;
            tokio::spawn(async move {
                if let Err(e) = h.await {
                    log::error!("Connection error: {}", e);
                }
            });
            c
        }
    } else {
        if let Some(ref cs) = conn.connection_string {
            let (c, h) = tokio_postgres::connect(cs, NoTls)
                .await
                .map_err(|e| format!("Connection failed: {}", e))?;
            tokio::spawn(async move {
                if let Err(e) = h.await {
                    log::error!("Connection error: {}", e);
                }
            });
            c
        } else {
            let cfg = build_pg_config(conn);
            let (c, h) = cfg
                .connect(NoTls)
                .await
                .map_err(|e| format!("Connection failed: {}", e))?;
            tokio::spawn(async move {
                if let Err(e) = h.await {
                    log::error!("Connection error: {}", e);
                }
            });
            c
        }
    };

    harden_session(&client, conn).await?;
    Ok(client)
}

fn build_pg_config(conn: &Connection) -> PgConfig {
    let mut cfg = PgConfig::new();
    cfg.host(&conn.host)
        .port(conn.port)
        .dbname(&conn.database)
        .user(&conn.user)
        .password(&conn.password)
        .connect_timeout(std::time::Duration::from_secs(10));
    cfg
}

/// Apply per-session guardrails after connect:
/// - application_name: tags the connection in `pg_stat_activity` as
///   `pg-mcp/<session-uuid>/<connection-name>` so operators can tell
///   concurrent agents apart
/// - statement_timeout: bounds any single query
/// - idle_in_transaction_session_timeout: prevents dangling txns locking rows
/// - default_transaction_read_only: server-side readonly enforcement
///   (catches CTE writes, side-effecting functions, etc. that the client-side
///   keyword blocklist cannot)
async fn harden_session(client: &Client, conn: &Connection) -> Result<(), String> {
    let app_name_raw = format!(
        "pg-mcp/{}/{}",
        crate::audit::session_id(),
        conn.name
    );
    let app_name_escaped = app_name_raw.replace('\'', "''");

    let base = format!(
        "SET application_name = '{app}'; \
         SET statement_timeout = '{st}'; \
         SET idle_in_transaction_session_timeout = '{it}';",
        app = app_name_escaped,
        st = STATEMENT_TIMEOUT,
        it = IDLE_IN_TXN_TIMEOUT,
    );
    client
        .batch_execute(&base)
        .await
        .map_err(|e| format!("Failed to apply session settings: {}", e))?;

    if conn.readonly {
        client
            .batch_execute("SET default_transaction_read_only = on;")
            .await
            .map_err(|e| format!("Failed to apply readonly mode: {}", e))?;
    }
    Ok(())
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
