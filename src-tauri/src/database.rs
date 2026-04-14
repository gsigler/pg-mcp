use crate::config::Connection;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio_postgres::{Client, Config as PgConfig, NoTls, Row};

const MAX_RESULT_ROWS: usize = 100;
const MAX_COLUMN_WIDTH: usize = 60;

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
}

impl DatabaseManager {
    pub fn new() -> Self {
        Self {
            client: Arc::new(Mutex::new(None)),
            active_connection_name: Arc::new(Mutex::new(None)),
            in_transaction: Arc::new(Mutex::new(false)),
            query_history: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn active_name(&self) -> Option<String> {
        self.active_connection_name.lock().await.clone()
    }

    pub async fn connect(&self, conn: &Connection) -> Result<(), String> {
        self.disconnect().await;

        let client = if let Some(ref conn_str) = conn.connection_string {
            let (client, connection) = tokio_postgres::connect(conn_str, NoTls)
                .await
                .map_err(|e| format!("Connection failed: {}", e))?;
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    log::error!("Connection error: {}", e);
                }
            });
            client
        } else {
            let mut config = PgConfig::new();
            config
                .host(&conn.host)
                .port(conn.port)
                .dbname(&conn.database)
                .user(&conn.user)
                .password(&conn.password)
                .connect_timeout(std::time::Duration::from_secs(10));

            let (client, connection) = config
                .connect(NoTls)
                .await
                .map_err(|e| format!("Connection failed: {}", e))?;
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    log::error!("Connection error: {}", e);
                }
            });
            client
        };

        *self.client.lock().await = Some(client);
        *self.active_connection_name.lock().await = Some(conn.name.clone());
        *self.in_transaction.lock().await = false;
        Ok(())
    }

    pub async fn disconnect(&self) {
        *self.client.lock().await = None;
        *self.active_connection_name.lock().await = None;
        *self.in_transaction.lock().await = false;
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
        // Transaction control statements are not "writes" per se
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
        if readonly && Self::check_write_query(sql) {
            return Err(
                "Write operation blocked. This connection is read-only.\n\
                 To enable writes, open pg-mcp UI and disable read-only mode for this connection."
                    .into(),
            );
        }

        let start = Instant::now();
        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();

        let in_txn = *self.in_transaction.lock().await;

        // Only wrap in read-only transaction if not already in a user-managed transaction
        if readonly && !in_txn {
            client
                .execute("BEGIN TRANSACTION READ ONLY", &[])
                .await
                .map_err(|e| format!("Failed to begin transaction: {}", e))?;
        }

        let result = client.query(sql, &[]).await;

        if readonly && !in_txn {
            let _ = client.execute("COMMIT", &[]).await;
        }

        let duration_ms = start.elapsed().as_millis();

        match result {
            Ok(rows) => {
                let row_count = rows.len();
                self.record_query(sql, duration_ms, Some(row_count), None).await;
                Ok(format_rows(&rows))
            }
            Err(e) => {
                if readonly && !in_txn {
                    let _ = client.execute("ROLLBACK", &[]).await;
                }
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
        // Wrap the query with LIMIT/OFFSET
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

    // ─── CRUD helpers ──────────────────────────────────────────────

    pub async fn insert_rows(
        &self,
        table: &str,
        columns: &[String],
        rows: &[Vec<String>],
    ) -> Result<String, String> {
        if rows.is_empty() {
            return Err("No rows provided.".into());
        }

        let cols = columns.iter().map(|c| format!("\"{}\"", c)).collect::<Vec<_>>().join(", ");
        let mut value_groups = Vec::new();
        for row in rows {
            let vals = row.iter().map(|v| {
                if v.eq_ignore_ascii_case("NULL") || v.eq_ignore_ascii_case("DEFAULT")
                    || v.starts_with("NOW(") || v.starts_with("now(")
                    || v.eq_ignore_ascii_case("TRUE") || v.eq_ignore_ascii_case("FALSE") {
                    v.to_string()
                } else {
                    // Escape single quotes for safety
                    format!("'{}'", v.replace('\'', "''"))
                }
            }).collect::<Vec<_>>().join(", ");
            value_groups.push(format!("({})", vals));
        }

        let sql = format!(
            "INSERT INTO {} ({}) VALUES {} RETURNING *",
            table, cols, value_groups.join(", ")
        );
        self.execute_query(&sql, false).await
    }

    pub async fn update_rows(
        &self,
        table: &str,
        set_columns: &HashMap<String, String>,
        conditions: &str,
    ) -> Result<String, String> {
        if set_columns.is_empty() {
            return Err("No columns to update.".into());
        }
        if conditions.trim().is_empty() {
            return Err("WHERE condition is required for safety. Use 'TRUE' to update all rows.".into());
        }

        let set_clause = set_columns.iter().map(|(col, val)| {
            if val.eq_ignore_ascii_case("NULL") || val.eq_ignore_ascii_case("DEFAULT")
                || val.starts_with("NOW(") || val.starts_with("now(")
                || val.eq_ignore_ascii_case("TRUE") || val.eq_ignore_ascii_case("FALSE") {
                format!("\"{}\" = {}", col, val)
            } else {
                format!("\"{}\" = '{}'", col, val.replace('\'', "''"))
            }
        }).collect::<Vec<_>>().join(", ");

        let sql = format!(
            "UPDATE {} SET {} WHERE {} RETURNING *",
            table, set_clause, conditions
        );
        self.execute_query(&sql, false).await
    }

    pub async fn delete_rows(
        &self,
        table: &str,
        conditions: &str,
    ) -> Result<String, String> {
        if conditions.trim().is_empty() {
            return Err("WHERE condition is required for safety. Use 'TRUE' to delete all rows.".into());
        }

        let sql = format!(
            "DELETE FROM {} WHERE {} RETURNING *",
            table, conditions
        );
        self.execute_query(&sql, false).await
    }

    // ─── Explain / dry-run ─────────────────────────────────────────

    pub async fn explain_query(
        &self,
        sql: &str,
        analyze: bool,
    ) -> Result<String, String> {
        let prefix = if analyze { "EXPLAIN ANALYZE" } else { "EXPLAIN" };
        let explain_sql = format!("{} {}", prefix, sql);
        // EXPLAIN is always safe to run (read-only), unless ANALYZE on a write
        self.execute_query(&explain_sql, false).await
    }

    // ─── Enhanced describe_table ───────────────────────────────────

    pub async fn describe_table(&self, table: &str) -> Result<String, String> {
        let (schema, tbl) = if table.contains('.') {
            let parts: Vec<&str> = table.splitn(2, '.').collect();
            (parts[0], parts[1])
        } else {
            ("public", table)
        };

        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();

        // --- Row count (#5) ---
        let count_sql = format!(
            "SELECT n_live_tup::bigint AS approx_rows \
             FROM pg_stat_user_tables \
             WHERE schemaname = '{}' AND relname = '{}'",
            schema, tbl
        );
        let count_rows = client.query(&count_sql, &[]).await.unwrap_or_default();
        let approx_rows: i64 = count_rows.first()
            .and_then(|r| r.try_get(0).ok())
            .unwrap_or(0);

        let mut output = format!("{}.{} (~{} rows)\n\n", schema, tbl, approx_rows);

        // --- Columns ---
        let col_sql = format!(
            "SELECT c.column_name, c.data_type, c.is_nullable, c.column_default, \
             pgd.description AS column_comment \
             FROM information_schema.columns c \
             LEFT JOIN pg_catalog.pg_statio_all_tables st \
                ON st.schemaname = c.table_schema AND st.relname = c.table_name \
             LEFT JOIN pg_catalog.pg_description pgd \
                ON pgd.objoid = st.relid AND pgd.objsubid = c.ordinal_position \
             WHERE c.table_schema = '{}' AND c.table_name = '{}' \
             ORDER BY c.ordinal_position",
            schema, tbl
        );
        let col_rows = client.query(&col_sql, &[]).await
            .map_err(|e| format!("Query error: {}", e))?;

        output.push_str("Columns:\n");
        output.push_str(&format_rows(&col_rows));

        // --- JSONB column inspection (#6) ---
        let jsonb_cols: Vec<String> = col_rows.iter()
            .filter(|r| {
                let dt: String = r.try_get::<_, String>(1).unwrap_or_default();
                dt == "jsonb" || dt == "json"
            })
            .filter_map(|r| r.try_get::<_, String>(0).ok())
            .collect();

        if !jsonb_cols.is_empty() {
            output.push_str("\nJSONB Column Keys (sampled from first 100 rows):\n");
            for col_name in &jsonb_cols {
                let sample_sql = format!(
                    "SELECT DISTINCT jsonb_object_keys(\"{}\") AS key \
                     FROM (SELECT \"{}\" FROM {}.{} WHERE \"{}\" IS NOT NULL LIMIT 100) sub \
                     ORDER BY key",
                    col_name, col_name, schema, tbl, col_name
                );
                match client.query(&sample_sql, &[]).await {
                    Ok(key_rows) => {
                        let keys: Vec<String> = key_rows.iter()
                            .filter_map(|r| r.try_get::<_, String>(0).ok())
                            .collect();
                        if !keys.is_empty() {
                            output.push_str(&format!("  {}: {}\n", col_name, keys.join(", ")));
                        }
                    }
                    Err(_) => {} // Skip if column isn't actually jsonb
                }
            }
        }

        // --- Indexes ---
        let idx_sql = format!(
            "SELECT indexname, indexdef FROM pg_indexes \
             WHERE schemaname = '{}' AND tablename = '{}'",
            schema, tbl
        );
        let idx_rows = client.query(&idx_sql, &[]).await
            .map_err(|e| format!("Query error: {}", e))?;
        if !idx_rows.is_empty() {
            output.push_str("\nIndexes:\n");
            output.push_str(&format_rows(&idx_rows));
        }

        // --- Outgoing foreign keys ---
        let fk_sql = format!(
            "SELECT tc.constraint_name, kcu.column_name, \
             ccu.table_schema || '.' || ccu.table_name AS foreign_table, \
             ccu.column_name AS foreign_column \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
                ON tc.constraint_name = kcu.constraint_name AND tc.table_schema = kcu.table_schema \
             JOIN information_schema.constraint_column_usage ccu \
                ON ccu.constraint_name = tc.constraint_name AND ccu.table_schema = tc.table_schema \
             WHERE tc.constraint_type = 'FOREIGN KEY' \
                AND tc.table_schema = '{}' AND tc.table_name = '{}'",
            schema, tbl
        );
        let fk_rows = client.query(&fk_sql, &[]).await
            .map_err(|e| format!("Query error: {}", e))?;
        if !fk_rows.is_empty() {
            output.push_str("\nOutgoing Foreign Keys:\n");
            output.push_str(&format_rows(&fk_rows));
        }

        // --- Incoming foreign keys (#7) ---
        let rfk_sql = format!(
            "SELECT tc.table_schema || '.' || tc.table_name AS referencing_table, \
             kcu.column_name AS referencing_column, \
             tc.constraint_name \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
                ON tc.constraint_name = kcu.constraint_name AND tc.table_schema = kcu.table_schema \
             JOIN information_schema.constraint_column_usage ccu \
                ON ccu.constraint_name = tc.constraint_name AND ccu.table_schema = tc.table_schema \
             WHERE tc.constraint_type = 'FOREIGN KEY' \
                AND ccu.table_schema = '{}' AND ccu.table_name = '{}'",
            schema, tbl
        );
        let rfk_rows = client.query(&rfk_sql, &[]).await
            .map_err(|e| format!("Query error: {}", e))?;
        if !rfk_rows.is_empty() {
            output.push_str("\nIncoming Foreign Keys (tables referencing this one):\n");
            output.push_str(&format_rows(&rfk_rows));
        }

        Ok(output)
    }

    // ─── Enhanced list_tables (#8) ─────────────────────────────────

    pub async fn list_tables(&self, schema: Option<&str>, include_counts: bool) -> Result<String, String> {
        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();

        let schema_filter = if let Some(s) = schema {
            format!("= '{}'", s)
        } else {
            "NOT IN ('pg_catalog', 'information_schema')".to_string()
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
                 WHERE t.schemaname {} \
                 UNION ALL \
                 SELECT v.schemaname, v.viewname AS name, 'view' AS type, \
                 0::bigint AS approx_rows, \
                 obj_description(c.oid) AS description \
                 FROM pg_views v \
                 LEFT JOIN pg_class c \
                    ON c.relname = v.viewname \
                 LEFT JOIN pg_namespace n \
                    ON n.oid = c.relnamespace AND n.nspname = v.schemaname \
                 WHERE v.schemaname {} \
                 ORDER BY schemaname, name",
                schema_filter, schema_filter
            )
        } else {
            format!(
                "SELECT schemaname, tablename AS name, 'table' AS type FROM pg_tables \
                 WHERE schemaname {} \
                 UNION ALL \
                 SELECT schemaname, viewname AS name, 'view' AS type FROM pg_views \
                 WHERE schemaname {} \
                 ORDER BY schemaname, name",
                schema_filter, schema_filter
            )
        };

        let rows = client.query(&sql, &[]).await
            .map_err(|e| format!("Query error: {}", e))?;
        Ok(format_rows(&rows))
    }

    // ─── Other tools (unchanged) ───────────────────────────────────

    pub async fn test_connection(conn: &Connection) -> Result<(String, u128), String> {
        let start = Instant::now();

        let client = if let Some(ref conn_str) = conn.connection_string {
            let (client, connection) = tokio_postgres::connect(conn_str, NoTls)
                .await
                .map_err(|e| format!("Connection failed: {}", e))?;
            tokio::spawn(async move { let _ = connection.await; });
            client
        } else {
            let mut config = PgConfig::new();
            config
                .host(&conn.host)
                .port(conn.port)
                .dbname(&conn.database)
                .user(&conn.user)
                .password(&conn.password)
                .connect_timeout(std::time::Duration::from_secs(10));
            let (client, connection) = config
                .connect(NoTls)
                .await
                .map_err(|e| format!("Connection failed: {}", e))?;
            tokio::spawn(async move { let _ = connection.await; });
            client
        };

        let row = client.query_one("SELECT version()", &[]).await
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
        let rows = client.query(sql, &[]).await
            .map_err(|e| format!("Query error: {}", e))?;
        Ok(format_rows(&rows))
    }

    pub async fn get_table_sample(
        &self,
        table: &str,
        limit: Option<usize>,
    ) -> Result<String, String> {
        let limit = limit.unwrap_or(5).min(50);
        let sql = format!("SELECT * FROM {} LIMIT {}", table, limit);
        self.execute_query(&sql, true).await
    }

    pub async fn get_schema_diagram(&self, schema: Option<&str>) -> Result<String, String> {
        let schema = schema.unwrap_or("public");
        let guard = self.get_client().await?;
        let client = guard.as_ref().unwrap();

        let col_sql = format!(
            "SELECT c.table_name, c.column_name, c.data_type, \
             CASE WHEN pk.column_name IS NOT NULL THEN 'PK' ELSE '' END AS is_pk \
             FROM information_schema.columns c \
             LEFT JOIN ( \
                SELECT kcu.table_name, kcu.column_name \
                FROM information_schema.table_constraints tc \
                JOIN information_schema.key_column_usage kcu \
                    ON tc.constraint_name = kcu.constraint_name \
                WHERE tc.constraint_type = 'PRIMARY KEY' AND tc.table_schema = '{}' \
             ) pk ON c.table_name = pk.table_name AND c.column_name = pk.column_name \
             WHERE c.table_schema = '{}' \
             ORDER BY c.table_name, c.ordinal_position",
            schema, schema
        );
        let col_rows = client.query(&col_sql, &[]).await
            .map_err(|e| format!("Query error: {}", e))?;

        let fk_sql = format!(
            "SELECT tc.table_name, kcu.column_name, \
             ccu.table_name AS foreign_table, ccu.column_name AS foreign_column \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
                ON tc.constraint_name = kcu.constraint_name \
             JOIN information_schema.constraint_column_usage ccu \
                ON ccu.constraint_name = tc.constraint_name \
             WHERE tc.constraint_type = 'FOREIGN KEY' AND tc.table_schema = '{}'",
            schema
        );
        let fk_rows = client.query(&fk_sql, &[]).await
            .map_err(|e| format!("Query error: {}", e))?;

        let mut tables: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
        for row in &col_rows {
            let table_name: String = row.get(0);
            let col_name: String = row.get(1);
            let data_type: String = row.get(2);
            let is_pk: String = row.get(3);
            tables.entry(table_name).or_default().push((col_name, data_type, is_pk));
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

    // ─── Query history (#10) ───────────────────────────────────────

    async fn record_query(&self, sql: &str, duration_ms: u128, row_count: Option<usize>, error: Option<String>) {
        let record = QueryRecord {
            sql: sql.to_string(),
            timestamp: chrono::Utc::now().naive_utc(),
            duration_ms,
            row_count,
            error,
        };
        let mut history = self.query_history.lock().await;
        history.push(record);
        // Keep last 200 queries
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

// ─── Row formatting ────────────────────────────────────────────────

fn format_rows(rows: &[Row]) -> String {
    if rows.is_empty() {
        return "(0 rows)\n".to_string();
    }

    let columns = rows[0].columns();
    let headers: Vec<String> = columns.iter().map(|c| c.name().to_string()).collect();

    let mut data: Vec<Vec<String>> = Vec::new();
    for row in rows.iter().take(MAX_RESULT_ROWS) {
        let mut row_data = Vec::new();
        for (i, col) in columns.iter().enumerate() {
            let val = get_cell_string(row, i, col.type_());
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
    let header_line: Vec<String> = headers.iter().enumerate()
        .map(|(i, h)| format!("{:width$}", h, width = widths[i]))
        .collect();
    output.push_str(&header_line.join(" | "));
    output.push('\n');

    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    output.push_str(&sep.join("-+-"));
    output.push('\n');

    for row_data in &data {
        let line: Vec<String> = row_data.iter().enumerate()
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

/// BUG FIX #1: Handle all PostgreSQL types properly so RETURNING clauses
/// with timestamps, UUIDs, numerics, etc. don't show as NULL.
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
        Type::JSON | Type::JSONB => {
            match row.try_get::<_, Option<serde_json::Value>>(idx) {
                Ok(Some(v)) => v.to_string(),
                Ok(None) => "NULL".into(),
                Err(_) => "NULL".into(),
            }
        }
        Type::TIMESTAMP => {
            match row.try_get::<_, Option<chrono::NaiveDateTime>>(idx) {
                Ok(Some(v)) => v.format("%Y-%m-%d %H:%M:%S%.f").to_string(),
                Ok(None) => "NULL".into(),
                Err(_) => "NULL".into(),
            }
        }
        Type::TIMESTAMPTZ => {
            match row.try_get::<_, Option<chrono::DateTime<chrono::Utc>>>(idx) {
                Ok(Some(v)) => v.format("%Y-%m-%d %H:%M:%S%.f%z").to_string(),
                Ok(None) => "NULL".into(),
                Err(_) => "NULL".into(),
            }
        }
        Type::DATE => {
            match row.try_get::<_, Option<chrono::NaiveDate>>(idx) {
                Ok(Some(v)) => v.format("%Y-%m-%d").to_string(),
                Ok(None) => "NULL".into(),
                Err(_) => "NULL".into(),
            }
        }
        Type::TIME => {
            match row.try_get::<_, Option<chrono::NaiveTime>>(idx) {
                Ok(Some(v)) => v.format("%H:%M:%S%.f").to_string(),
                Ok(None) => "NULL".into(),
                Err(_) => "NULL".into(),
            }
        }
        Type::UUID => {
            match row.try_get::<_, Option<uuid::Uuid>>(idx) {
                Ok(Some(v)) => v.to_string(),
                Ok(None) => "NULL".into(),
                Err(_) => "NULL".into(),
            }
        }
        Type::BYTEA => {
            match row.try_get::<_, Option<Vec<u8>>>(idx) {
                Ok(Some(v)) => format!("\\x{}", hex::encode(&v)),
                Ok(None) => "NULL".into(),
                Err(_) => "NULL".into(),
            }
        }
        Type::CHAR_ARRAY | Type::TEXT_ARRAY | Type::VARCHAR_ARRAY => {
            match row.try_get::<_, Option<Vec<String>>>(idx) {
                Ok(Some(v)) => format!("{{{}}}", v.join(",")),
                Ok(None) => "NULL".into(),
                Err(_) => "NULL".into(),
            }
        }
        Type::INT4_ARRAY => {
            match row.try_get::<_, Option<Vec<i32>>>(idx) {
                Ok(Some(v)) => format!("{{{}}}", v.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")),
                Ok(None) => "NULL".into(),
                Err(_) => "NULL".into(),
            }
        }
        _ => {
            // Generic fallback: try as String (works for text, varchar, char, citext, enums, etc.)
            match row.try_get::<_, Option<String>>(idx) {
                Ok(Some(v)) => v,
                Ok(None) => "NULL".into(),
                Err(_) => "NULL".into(),
            }
        }
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
