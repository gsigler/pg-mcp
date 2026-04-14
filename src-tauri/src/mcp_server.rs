use crate::config::{Config, Connection};
use crate::database::DatabaseManager;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct McpServer {
    db: Arc<DatabaseManager>,
    config: Arc<Mutex<Config>>,
}

impl McpServer {
    pub fn new() -> Self {
        Self {
            db: Arc::new(DatabaseManager::new()),
            config: Arc::new(Mutex::new(Config::load())),
        }
    }

    pub async fn run(&self) -> Result<(), String> {
        let stdin = io::stdin();
        let mut stdout = io::stdout();

        for line in stdin.lock().lines() {
            let line = line.map_err(|e| format!("Read error: {}", e))?;
            if line.trim().is_empty() {
                continue;
            }

            let request: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[pg-mcp] Failed to parse JSON: {} — input: {}", e, line);
                    continue;
                }
            };

            let response = self.handle_request(&request).await;

            // Notifications (no "id" field) don't get responses
            if response.is_null() {
                continue;
            }

            let response_str =
                serde_json::to_string(&response).map_err(|e| format!("Serialize error: {}", e))?;
            stdout.write_all(response_str.as_bytes())
                .map_err(|e| format!("Write error: {}", e))?;
            stdout.write_all(b"\n")
                .map_err(|e| format!("Write error: {}", e))?;
            stdout.flush()
                .map_err(|e| format!("Flush error: {}", e))?;
        }

        Ok(())
    }

    async fn handle_request(&self, request: &Value) -> Value {
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");

        // Notifications have no "id" — never respond
        if id.is_none() || id.as_ref() == Some(&Value::Null) {
            if method == "initialized" || method.starts_with("notifications/") {
                return Value::Null;
            }
        }

        let id = id.unwrap_or(Value::Null);

        match method {
            "initialize" => self.handle_initialize(&id),
            "tools/list" => self.handle_tools_list(&id),
            "tools/call" => self.handle_tools_call(&id, request).await,
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {}", method) }
            }),
        }
    }

    fn handle_initialize(&self, id: &Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "pg-mcp", "version": "1.1.0" }
            }
        })
    }

    fn handle_tools_list(&self, id: &Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": [
                    {
                        "name": "list_connections",
                        "description": "Lists all configured database connections. Shows which is active and each connection's read/write mode.",
                        "inputSchema": { "type": "object", "properties": {}, "required": [] }
                    },
                    {
                        "name": "query",
                        "description": "Executes a SQL query against the active database. Returns results as a formatted text table. Blocked by read-only enforcement if the query is a write operation. Supports pagination with limit/offset.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "sql": { "type": "string", "description": "The SQL query to execute" },
                                "limit": { "type": "number", "description": "Max rows to return (for pagination). Defaults to 100." },
                                "offset": { "type": "number", "description": "Row offset (for pagination). Defaults to 0." }
                            },
                            "required": ["sql"]
                        }
                    },
                    {
                        "name": "list_tables",
                        "description": "Lists all tables and views grouped by schema. Optionally includes approximate row counts and table descriptions/comments.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "schema": { "type": "string", "description": "Filter to a specific schema. Defaults to all non-system schemas." },
                                "include_counts": { "type": "boolean", "description": "Include approximate row counts and table descriptions. Default false." }
                            },
                            "required": []
                        }
                    },
                    {
                        "name": "describe_table",
                        "description": "Returns column definitions (with comments), indexes, outgoing AND incoming foreign keys, approximate row count, and JSONB column key inspection. Accepts schema-qualified names like 'public.users'.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "table": { "type": "string", "description": "The table name, optionally schema-qualified (e.g. 'public.users')" }
                            },
                            "required": ["table"]
                        }
                    },
                    {
                        "name": "list_schemas",
                        "description": "Lists all non-system schemas with their table and view counts.",
                        "inputSchema": { "type": "object", "properties": {}, "required": [] }
                    },
                    {
                        "name": "test_connection",
                        "description": "Tests connectivity to the active database. Returns server version and latency.",
                        "inputSchema": { "type": "object", "properties": {}, "required": [] }
                    },
                    {
                        "name": "get_table_sample",
                        "description": "Returns sample rows from a table (default 5, max 50).",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "table": { "type": "string", "description": "The table name to sample from" },
                                "limit": { "type": "number", "description": "Number of rows (default 5, max 50)" }
                            },
                            "required": ["table"]
                        }
                    },
                    {
                        "name": "get_schema_diagram",
                        "description": "Generates a text-based entity-relationship diagram showing tables, columns, types, and foreign key relationships.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "schema": { "type": "string", "description": "The schema to diagram. Defaults to 'public'." }
                            },
                            "required": []
                        }
                    },
                    // ── New tools ──────────────────────────────────────
                    {
                        "name": "insert_rows",
                        "description": "Insert rows into a table with structured inputs. Safer than raw SQL — values are escaped automatically. Returns inserted rows via RETURNING *. Requires read-write mode.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "table": { "type": "string", "description": "Target table name" },
                                "columns": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Column names to insert into"
                                },
                                "rows": {
                                    "type": "array",
                                    "items": {
                                        "type": "array",
                                        "items": { "type": "string" }
                                    },
                                    "description": "Array of row arrays. Each row is an array of string values in column order. Use 'NULL', 'DEFAULT', 'NOW()' for special values."
                                }
                            },
                            "required": ["table", "columns", "rows"]
                        }
                    },
                    {
                        "name": "update_rows",
                        "description": "Update rows in a table with structured inputs. Requires a WHERE condition for safety. Returns updated rows via RETURNING *. Requires read-write mode.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "table": { "type": "string", "description": "Target table name" },
                                "set": {
                                    "type": "object",
                                    "additionalProperties": { "type": "string" },
                                    "description": "Column-value pairs to set. Use 'NULL', 'NOW()' etc. for special values."
                                },
                                "where": { "type": "string", "description": "WHERE condition (required). Use 'TRUE' to update all rows." }
                            },
                            "required": ["table", "set", "where"]
                        }
                    },
                    {
                        "name": "delete_rows",
                        "description": "Delete rows from a table. Requires a WHERE condition for safety. Returns deleted rows via RETURNING *. Requires read-write mode.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "table": { "type": "string", "description": "Target table name" },
                                "where": { "type": "string", "description": "WHERE condition (required). Use 'TRUE' to delete all rows." }
                            },
                            "required": ["table", "where"]
                        }
                    },
                    {
                        "name": "begin_transaction",
                        "description": "Start a transaction. Subsequent query/insert/update/delete calls will run within this transaction until commit or rollback.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "readonly": { "type": "boolean", "description": "Start a read-only transaction. Default false." }
                            },
                            "required": []
                        }
                    },
                    {
                        "name": "commit",
                        "description": "Commit the current transaction.",
                        "inputSchema": { "type": "object", "properties": {}, "required": [] }
                    },
                    {
                        "name": "rollback",
                        "description": "Rollback the current transaction, undoing all changes since BEGIN.",
                        "inputSchema": { "type": "object", "properties": {}, "required": [] }
                    },
                    {
                        "name": "explain_query",
                        "description": "Run EXPLAIN on a query to see the execution plan without executing it. Useful for verifying write operations before committing, or understanding query performance.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "sql": { "type": "string", "description": "The SQL query to explain" },
                                "analyze": { "type": "boolean", "description": "Run EXPLAIN ANALYZE (actually executes the query to get real timing). Default false." }
                            },
                            "required": ["sql"]
                        }
                    },
                    {
                        "name": "query_history",
                        "description": "Show the last N queries executed in this session, with timing, row counts, and errors.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "limit": { "type": "number", "description": "Number of recent queries to show. Default 20." }
                            },
                            "required": []
                        }
                    }
                ]
            }
        })
    }

    async fn handle_tools_call(&self, id: &Value, request: &Value) -> Value {
        let params = request.get("params").cloned().unwrap_or(json!({}));
        let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        // Reload config for every tool call to pick up UI changes
        let config = Config::load();
        *self.config.lock().await = config.clone();

        let result = match tool_name {
            "list_connections" => self.tool_list_connections(&config).await,
            "query" => self.tool_query(&config, &arguments).await,
            "list_tables" => self.tool_list_tables(&config, &arguments).await,
            "describe_table" => self.tool_describe_table(&config, &arguments).await,
            "list_schemas" => self.tool_list_schemas(&config).await,
            "test_connection" => self.tool_test_connection(&config).await,
            "get_table_sample" => self.tool_get_table_sample(&config, &arguments).await,
            "get_schema_diagram" => self.tool_get_schema_diagram(&config, &arguments).await,
            "insert_rows" => self.tool_insert_rows(&config, &arguments).await,
            "update_rows" => self.tool_update_rows(&config, &arguments).await,
            "delete_rows" => self.tool_delete_rows(&config, &arguments).await,
            "begin_transaction" => self.tool_begin_transaction(&config, &arguments).await,
            "commit" => self.tool_commit(&config).await,
            "rollback" => self.tool_rollback(&config).await,
            "explain_query" => self.tool_explain_query(&config, &arguments).await,
            "query_history" => self.tool_query_history(&arguments).await,
            _ => Err(format!("Unknown tool: {}", tool_name)),
        };

        match result {
            Ok(text) => json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "content": [{ "type": "text", "text": text }] }
            }),
            Err(err) => json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "content": [{ "type": "text", "text": err }], "isError": true }
            }),
        }
    }

    // ─── Helpers ────────────────────────────────────────────────────

    fn connection_banner(conn: &Connection) -> String {
        let mode = if conn.readonly { "\u{1f512} READ-ONLY" } else { "\u{1f513} READ-WRITE" };
        let bar = "\u{25a0}".repeat(51);
        format!(
            "{bar}\n\u{25a0} \u{1f4e6} {name}\n\u{25a0} \u{1f517} {host}:{port}/{db}\n\u{25a0} {mode}\n{bar}\n",
            bar = bar, name = conn.name, host = conn.host, port = conn.port,
            db = conn.database, mode = mode,
        )
    }

    async fn ensure_connected(&self, config: &Config) -> Result<Connection, String> {
        let conn = config.get_active_connection()
            .ok_or_else(|| "No active connection.\nOpen the pg-mcp UI and activate a connection, or run:\n  pg-mcp activate <connection-name>".to_string())?
            .clone();
        let needs_connect = self.db.active_name().await.as_deref() != Some(&conn.name);
        if needs_connect {
            self.db.connect(&conn).await?;
        }
        Ok(conn)
    }

    // ─── Original tools ────────────────────────────────────────────

    async fn tool_list_connections(&self, config: &Config) -> Result<String, String> {
        if config.connections.is_empty() {
            return Ok("No connections configured.\nOpen pg-mcp UI to add a database connection.".into());
        }
        let mut output = String::new();
        for conn in &config.connections {
            let active = config.active_connection.as_deref().map_or(false, |a| a == conn.name);
            let mode = if conn.readonly { "RO" } else { "RW" };
            let marker = if active { " (active)" } else { "" };
            output.push_str(&format!("- {}{} [{}] {}:{}/{}\n", conn.name, marker, mode, conn.host, conn.port, conn.database));
        }
        Ok(output)
    }

    async fn tool_query(&self, config: &Config, args: &Value) -> Result<String, String> {
        let conn = self.ensure_connected(config).await?;
        let sql = args.get("sql").and_then(|s| s.as_str()).ok_or("Missing required parameter: sql")?;

        let limit = args.get("limit").and_then(|l| l.as_u64()).map(|l| l as usize);
        let offset = args.get("offset").and_then(|o| o.as_u64()).map(|o| o as usize);

        let banner = Self::connection_banner(&conn);
        let result = if limit.is_some() || offset.is_some() {
            self.db.execute_query_paginated(sql, conn.readonly, limit.unwrap_or(100), offset.unwrap_or(0)).await?
        } else {
            self.db.execute_query(sql, conn.readonly).await?
        };
        Ok(format!("{}\n{}", banner, result))
    }

    async fn tool_list_tables(&self, config: &Config, args: &Value) -> Result<String, String> {
        let conn = self.ensure_connected(config).await?;
        let schema = args.get("schema").and_then(|s| s.as_str());
        let include_counts = args.get("include_counts").and_then(|b| b.as_bool()).unwrap_or(false);
        let banner = Self::connection_banner(&conn);
        let result = self.db.list_tables(schema, include_counts).await?;
        Ok(format!("{}\n{}", banner, result))
    }

    async fn tool_describe_table(&self, config: &Config, args: &Value) -> Result<String, String> {
        let conn = self.ensure_connected(config).await?;
        let table = args.get("table").and_then(|s| s.as_str()).ok_or("Missing required parameter: table")?;
        let banner = Self::connection_banner(&conn);
        let result = self.db.describe_table(table).await?;
        Ok(format!("{}\n{}", banner, result))
    }

    async fn tool_list_schemas(&self, config: &Config) -> Result<String, String> {
        let conn = self.ensure_connected(config).await?;
        let banner = Self::connection_banner(&conn);
        let result = self.db.list_schemas().await?;
        Ok(format!("{}\n{}", banner, result))
    }

    async fn tool_test_connection(&self, config: &Config) -> Result<String, String> {
        let conn = config.get_active_connection().ok_or("No active connection")?.clone();
        let banner = Self::connection_banner(&conn);
        let (version, latency) = DatabaseManager::test_connection(&conn).await?;
        Ok(format!("{}\nConnection OK\nServer: {}\nLatency: {}ms", banner, version, latency))
    }

    async fn tool_get_table_sample(&self, config: &Config, args: &Value) -> Result<String, String> {
        let conn = self.ensure_connected(config).await?;
        let table = args.get("table").and_then(|s| s.as_str()).ok_or("Missing required parameter: table")?;
        let limit = args.get("limit").and_then(|l| l.as_u64()).map(|l| l as usize);
        let banner = Self::connection_banner(&conn);
        let result = self.db.get_table_sample(table, limit).await?;
        Ok(format!("{}\n{}", banner, result))
    }

    async fn tool_get_schema_diagram(&self, config: &Config, args: &Value) -> Result<String, String> {
        let conn = self.ensure_connected(config).await?;
        let schema = args.get("schema").and_then(|s| s.as_str());
        let banner = Self::connection_banner(&conn);
        let result = self.db.get_schema_diagram(schema).await?;
        Ok(format!("{}\n{}", banner, result))
    }

    // ─── New CRUD tools (#2) ───────────────────────────────────────

    async fn tool_insert_rows(&self, config: &Config, args: &Value) -> Result<String, String> {
        let conn = self.ensure_connected(config).await?;
        if conn.readonly {
            return Err("Write operation blocked. This connection is read-only.".into());
        }

        let table = args.get("table").and_then(|s| s.as_str()).ok_or("Missing: table")?;
        let columns: Vec<String> = args.get("columns")
            .and_then(|a| a.as_array())
            .ok_or("Missing: columns")?
            .iter().filter_map(|v| v.as_str().map(String::from)).collect();
        let rows: Vec<Vec<String>> = args.get("rows")
            .and_then(|a| a.as_array())
            .ok_or("Missing: rows")?
            .iter().filter_map(|r| {
                r.as_array().map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            }).collect();

        let banner = Self::connection_banner(&conn);
        let result = self.db.insert_rows(table, &columns, &rows).await?;
        Ok(format!("{}\n{}", banner, result))
    }

    async fn tool_update_rows(&self, config: &Config, args: &Value) -> Result<String, String> {
        let conn = self.ensure_connected(config).await?;
        if conn.readonly {
            return Err("Write operation blocked. This connection is read-only.".into());
        }

        let table = args.get("table").and_then(|s| s.as_str()).ok_or("Missing: table")?;
        let set_obj = args.get("set").and_then(|o| o.as_object()).ok_or("Missing: set")?;
        let mut set_columns: HashMap<String, String> = HashMap::new();
        for (k, v) in set_obj {
            set_columns.insert(k.clone(), v.as_str().unwrap_or("NULL").to_string());
        }
        let conditions = args.get("where").and_then(|s| s.as_str()).ok_or("Missing: where")?;

        let banner = Self::connection_banner(&conn);
        let result = self.db.update_rows(table, &set_columns, conditions).await?;
        Ok(format!("{}\n{}", banner, result))
    }

    async fn tool_delete_rows(&self, config: &Config, args: &Value) -> Result<String, String> {
        let conn = self.ensure_connected(config).await?;
        if conn.readonly {
            return Err("Write operation blocked. This connection is read-only.".into());
        }

        let table = args.get("table").and_then(|s| s.as_str()).ok_or("Missing: table")?;
        let conditions = args.get("where").and_then(|s| s.as_str()).ok_or("Missing: where")?;

        let banner = Self::connection_banner(&conn);
        let result = self.db.delete_rows(table, conditions).await?;
        Ok(format!("{}\n{}", banner, result))
    }

    // ─── Transaction tools (#3) ────────────────────────────────────

    async fn tool_begin_transaction(&self, config: &Config, args: &Value) -> Result<String, String> {
        let conn = self.ensure_connected(config).await?;
        let readonly = args.get("readonly").and_then(|b| b.as_bool()).unwrap_or(false);
        if !readonly && conn.readonly {
            return Err("Cannot start a read-write transaction on a read-only connection.".into());
        }
        let banner = Self::connection_banner(&conn);
        let result = self.db.begin_transaction(readonly).await?;
        Ok(format!("{}\n{}", banner, result))
    }

    async fn tool_commit(&self, config: &Config) -> Result<String, String> {
        let conn = self.ensure_connected(config).await?;
        let banner = Self::connection_banner(&conn);
        let result = self.db.commit_transaction().await?;
        Ok(format!("{}\n{}", banner, result))
    }

    async fn tool_rollback(&self, config: &Config) -> Result<String, String> {
        let conn = self.ensure_connected(config).await?;
        let banner = Self::connection_banner(&conn);
        let result = self.db.rollback_transaction().await?;
        Ok(format!("{}\n{}", banner, result))
    }

    // ─── Explain tool (#4) ─────────────────────────────────────────

    async fn tool_explain_query(&self, config: &Config, args: &Value) -> Result<String, String> {
        let conn = self.ensure_connected(config).await?;
        let sql = args.get("sql").and_then(|s| s.as_str()).ok_or("Missing: sql")?;
        let analyze = args.get("analyze").and_then(|b| b.as_bool()).unwrap_or(false);
        let banner = Self::connection_banner(&conn);
        let result = self.db.explain_query(sql, analyze).await?;
        Ok(format!("{}\n{}", banner, result))
    }

    // ─── Query history tool (#10) ──────────────────────────────────

    async fn tool_query_history(&self, args: &Value) -> Result<String, String> {
        let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(20) as usize;
        Ok(self.db.get_query_history(limit).await)
    }
}
