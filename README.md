# pg-mcp

A PostgreSQL MCP server with a native desktop config UI.

Runs as a Tauri app on macOS and Windows. You manage connections in the UI;
Claude Desktop, Claude Code, and other MCP clients talk to the server over
stdio and see whichever database you've marked active — no `.mcp.json`
juggling, no per-project credentials, no guessing which environment the
agent is pointed at.

## Why this, vs connecting an agent directly

- **Deliberate DB selection.** The agent can list connections and sees a banner on every tool response saying which one is active, but it cannot switch DBs. You pick, always.
- **Secrets live in the OS keychain.** Passwords and connection strings are stored via macOS Keychain / Windows Credential Manager. `config.json` contains no secrets.
- **Server-enforced read-only.** Read-only connections get `SET default_transaction_read_only = on` at the session level, which catches CTE writes, side-effecting functions, and `pg_terminate_backend` — things a first-token keyword check wouldn't.
- **TLS flag that actually means something.** The `ssl` toggle wires up `postgres-native-tls`. Earlier dev builds forced `NoTls`.
- **Data-loss guard on destructive ops.** `update_rows` and `delete_rows` require an `expected_max_rows` argument; if the affected count exceeds it, the statement is rolled back. A mistyped WHERE doesn't wipe a table.
- **Optional PII redaction.** Per-connection toggle replaces cells whose column name or value looks like PII (email, phone, SSN, card-shaped digits, name-ish columns) with `[REDACTED]` before the MCP response leaves the server.
- **Audit log.** Every query from every agent lands in `~/Library/Application Support/pg-mcp/pg-mcp.log` tagged with a session UUID, so you can `tail -f` to see what's happening across sessions.
- **`application_name` tagged.** Agents show up in `pg_stat_activity` as `pg-mcp/<session>/<connection>` — DBAs can tell them apart.

## Install

### From a release

Grab the macOS `.dmg` or Windows `.msi` from the latest [release](https://github.com/gsigler/pg-mcp/releases) and run the installer.

### Register with your agent

Open pg-mcp, configure a connection, then use the Agent Setup panel to one-click install into:

- **Claude Desktop** → `~/Library/Application Support/Claude/claude_desktop_config.json`
- **Claude Code** → `~/.claude.json` (user scope)
- **Cursor** → `~/.cursor/mcp.json`
- **VS Code** → `~/Library/Application Support/Code/User/mcp.json` (note: VS Code uses `servers`, not `mcpServers`, and entries need `"type": "stdio"`)
- **Codex** → `~/.codex/config.toml` (one file covers the Codex CLI, IDE extension, and standalone desktop app)

Restart the target after installing. For Zed, Cline, or anything else, expand **Manual setup / other clients** for copy-paste snippets.

### Build from source

```sh
# Prereqs: Node 20+, Rust stable (via rustup or homebrew), Xcode CLI tools
git clone https://github.com/gsigler/pg-mcp.git
cd pg-mcp
npm install
npm run tauri build   # or: npm run tauri dev
```

The built binary serves double duty: with no arguments it launches the UI; with `serve` it speaks MCP over stdio.

## Tool reference

Every response begins with a banner naming the active connection, its host, read/write mode, and whether PII redaction is on.

| Tool | What it does |
|---|---|
| `list_connections` | All configured connections, marking the active one. |
| `db_overview` | Single-call orientation: version, schemas, top N tables by size with column summary + 3-row samples, FK summary. Call this first on an unfamiliar DB. |
| `search_schema` | Ranked search over table and column names + comments. Use when you know WHAT you're looking for but not WHERE it lives. |
| `query` | Run raw SQL. Blocked on read-only connections for write statements. Supports `limit` / `offset` for pagination. |
| `list_tables` | Tables and views by schema. Optional approximate row counts. |
| `list_schemas` | Non-system schemas with table/view counts. |
| `describe_table` | Columns (with comments), indexes, outgoing + incoming foreign keys, approximate row count, JSONB key sampling. `format: "brief"` collapses to a two-line `col:type PK →fk` summary. |
| `describe_tables` | Brief-mode describe for an array of tables in one call. Batch sibling of `describe_table`. |
| `get_table_sample` | Up to 50 sample rows. |
| `get_schema_diagram` | Text ER diagram. |
| `test_connection` | Version + latency probe. |
| `insert_rows` | Structured insert. Values are dollar-quoted so SQL injection from cell content is not possible. |
| `update_rows` | Structured update. Requires `expected_max_rows`; exceeding it rolls back. |
| `delete_rows` | Structured delete. Requires `expected_max_rows`; exceeding it rolls back. |
| `begin_transaction` / `commit` / `rollback` | Explicit transaction control. |
| `explain_query` | EXPLAIN or EXPLAIN ANALYZE. ANALYZE of a suspected write is refused on read-only connections. |
| `query_history` | In-memory ring of the last 200 queries from the current MCP process. |

## Security model

The investments in this repo are about keeping *pg-mcp itself* from being a foot-gun. Operational security around the database is your responsibility:

- **Connect as a least-privilege Postgres role**, not as a superuser. Nothing pg-mcp does can save you from `DROP DATABASE` if the connecting role has that privilege.
- **Prefer a read replica for analytics access.** Read-only mode in pg-mcp is enforced server-side via `default_transaction_read_only = on`, but a misconfigured DB role that has BYPASSRLS will still bypass row-level security.
- **Require TLS on remote connections** by flipping the SSL toggle. Future versions will reject plaintext to non-loopback hosts automatically.
- **Rotate credentials** via the keychain or your cloud provider's IAM. pg-mcp doesn't auto-refresh tokens.

## Concurrency

Each Claude session spawns its own `pg-mcp serve` child process. Concurrency between agents is by process isolation — they don't share in-memory state. Two agents running queries on the same database is fine; Postgres handles it. The audit log shows every query from every agent tagged by session UUID.

If you flip the active connection in the UI while an MCP child is mid-query, the current query finishes on the old DB and the next tool call picks up the new one. The UI is single-instance: relaunching focuses the open window.

## Paths

- Config: `~/Library/Application Support/pg-mcp/config.json` (macOS), `%APPDATA%\pg-mcp\config.json` (Windows). 0600 on Unix. No secrets.
- Secrets: macOS Keychain / Windows Credential Manager, service `pg-mcp`.
- Audit log: `~/Library/Application Support/pg-mcp/pg-mcp.log`. 0600 on Unix. Append-only JSON lines.

## Development

```sh
npm run tauri dev            # UI with hot reload
cd src-tauri && cargo test   # Rust unit tests (PII heuristics etc.)
cd src-tauri && cargo check  # Quick type check
```

Releases are cut by pushing a `v*` tag; the GitHub Actions workflow builds a universal macOS `.dmg` and a Windows `.msi` and attaches them to a draft release. See `.github/workflows/release.yml`.

## License

TBD.
