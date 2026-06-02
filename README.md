# pg-mcp

A PostgreSQL MCP server with a native desktop config UI.

Runs as a Tauri app on macOS and Windows. You manage connections in the UI; Claude Desktop, Claude Code, and other MCP clients talk to the server over stdio and see whichever database you've marked active — no `.mcp.json` juggling, no per-project credentials, no guessing which environment the agent is pointed at.

## Highlights

- **Deliberate DB selection.** The agent can list connections and sees a banner on every tool response naming the active one, but it cannot switch databases. You pick, always.
- **Secrets in the OS keychain.** Passwords and connection strings live in macOS Keychain or Windows Credential Manager. `config.json` never holds secrets.
- **Server-enforced read-only.** Read-only connections open with `default_transaction_read_only = on` in the libpq startup packet, catching CTE writes, side-effecting functions, and `pg_terminate_backend` — things a first-token keyword check would miss.
- **TLS that means it.** The `ssl` toggle wires up `postgres-native-tls` for end-to-end encryption against remote databases.
- **Data-loss guard on destructive ops.** `update_rows` and `delete_rows` require an `expected_max_rows` argument; if the affected count exceeds it, the statement is rolled back. A mistyped WHERE doesn't wipe a table.
- **Optional PII redaction.** A per-connection toggle replaces cells whose column name or value looks like PII (email, phone, SSN, card-shaped digits, name-ish columns) with `[REDACTED]` before the response leaves the server.
- **Audit log.** Every query from every agent lands in `pg-mcp.log` tagged with a session UUID. `tail -f` to watch what your agents are doing.
- **`application_name` tagged.** Agents show up in `pg_stat_activity` as `pg-mcp/<session>/<connection>`, so DBAs can tell them apart.

## Install

### From a release

Grab the macOS `.dmg` or Windows `.msi` from the [latest release](https://github.com/gsigler/pg-mcp/releases) and run the installer.

### Register with your agent

Open pg-mcp, add a connection, then use the **Agent Setup** panel for one-click install into:

- **Claude Desktop** — `~/Library/Application Support/Claude/claude_desktop_config.json`
- **Claude Code** — `~/.claude.json` (user scope)
- **Cursor** — `~/.cursor/mcp.json`
- **VS Code** — `~/Library/Application Support/Code/User/mcp.json` (VS Code uses `servers` rather than `mcpServers`, and entries need `"type": "stdio"`)
- **Codex** — `~/.codex/config.toml` (one file covers the Codex CLI, IDE extension, and desktop app)

Restart the target after installing. For Zed, Cline, or any other client, expand **Manual setup / other clients** in the panel for copy-paste snippets.

### Build from source

```sh
# Prereqs: Node 20+, Rust stable (rustup or Homebrew), Xcode CLI tools on macOS
git clone https://github.com/gsigler/pg-mcp.git
cd pg-mcp
npm install
npm run tauri build   # or: npm run tauri dev
```

The binary does double duty: with no arguments it launches the UI; with `serve` it speaks MCP over stdio.

## Tool reference

Every tool response begins with a banner naming the active connection, its host, read/write mode, and whether PII redaction is on.

| Tool | What it does |
|---|---|
| `list_connections` | All configured connections, marking the active one. |
| `db_overview` | Single-call orientation: version, schemas, top N tables by size with column summary and 3-row samples, FK summary. Call this first on an unfamiliar DB. |
| `search_schema` | Ranked search over table and column names and comments. Use when you know *what* you're looking for but not *where* it lives. |
| `query` | Run raw SQL. Write statements are blocked on read-only connections. Supports `limit` and `offset` for pagination. |
| `list_tables` | Tables and views by schema. Optional approximate row counts. |
| `list_schemas` | Non-system schemas with table and view counts. |
| `describe_table` | Columns (with comments), indexes, incoming and outgoing foreign keys, approximate row count, JSONB key sampling. `format: "brief"` collapses output to a two-line `col:type PK →fk` summary. |
| `describe_tables` | Brief-mode describe for an array of tables in one call. |
| `get_table_sample` | Up to 50 sample rows. |
| `get_schema_diagram` | Text ER diagram. |
| `test_connection` | Version and latency probe. |
| `insert_rows` | Structured insert. Values are dollar-quoted so cell content can't inject SQL. |
| `update_rows` | Structured update. Requires `expected_max_rows`; exceeding it rolls back. |
| `delete_rows` | Structured delete. Requires `expected_max_rows`; exceeding it rolls back. |
| `begin_transaction` / `commit` / `rollback` | Explicit transaction control. |
| `explain_query` | `EXPLAIN` or `EXPLAIN ANALYZE`. `ANALYZE` of a suspected write is refused on read-only connections. |
| `query_history` | In-memory ring of the last 200 queries from the current MCP process. |

## Security model

pg-mcp is built to avoid being a foot-gun on top of whatever you already do at the database level. Database-side hygiene is still on you:

- **Connect as a least-privilege role**, not a superuser. Nothing pg-mcp does can save you from `DROP DATABASE` if the role has that privilege.
- **Prefer a read replica for analytics access.** Read-only mode is enforced server-side via `default_transaction_read_only = on`, but a role with `BYPASSRLS` still bypasses row-level security.
- **Enable TLS on remote connections** with the SSL toggle.
- **Rotate credentials** via the keychain or your cloud provider's IAM. pg-mcp holds static secrets only — it doesn't auto-refresh tokens.

## Concurrency

Each agent session spawns its own `pg-mcp serve` child process. Concurrency between agents is by process isolation — they don't share in-memory state. Two agents querying the same database is fine; Postgres handles it. The audit log captures every query from every agent tagged by session UUID.

If you flip the active connection in the UI while a child is mid-query, the current query finishes on the old DB and the next tool call picks up the new one. The UI is single-instance: relaunching focuses the open window.

## Paths

|  | macOS | Windows |
|---|---|---|
| Config (no secrets, 0600 on Unix) | `~/Library/Application Support/pg-mcp/config.json` | `%APPDATA%\pg-mcp\config.json` |
| Secrets | Keychain, service `pg-mcp` | Credential Manager, service `pg-mcp` |
| Audit log (append-only JSON lines) | `~/Library/Application Support/pg-mcp/pg-mcp.log` | `%APPDATA%\pg-mcp\pg-mcp.log` |

## Development

```sh
npm run tauri dev            # UI with hot reload
cd src-tauri && cargo test   # Rust unit tests (PII heuristics, etc.)
cd src-tauri && cargo check  # Quick type check
```

## Release process

Release builds are tag-driven. A version bump PR by itself does not start a release; the GitHub Actions workflow runs when a `v*` tag is pushed or when the workflow is dispatched manually.

1. Bump the version in all app metadata:
   - `package.json`
   - `package-lock.json`
   - `src-tauri/Cargo.toml`
   - `src-tauri/Cargo.lock`
   - `src-tauri/tauri.conf.json`
2. Build and check locally:
   ```sh
   npm run build
   cd src-tauri && cargo check
   ```
3. Merge the version bump to `main`.
4. Tag the release commit and push the tag:
   ```sh
   git tag vX.Y.Z
   git push origin main vX.Y.Z
   ```
5. Wait for the **Release** workflow to finish. It builds macOS universal and Windows x64 artifacts, then attaches them to a draft GitHub release.
6. Review the draft release notes and assets in GitHub, then publish the release manually.

## License

MIT. See [LICENSE](LICENSE).
