# pg-mcp

pg-mcp gives AI coding agents a safer, friendlier way to work with PostgreSQL.

It is a small desktop app for managing database connections plus an MCP server that Claude, Cursor, VS Code, Codex, and other MCP clients can call over stdio. Pick the active database in the app, then your agent gets schema tools, query tools, and clear guardrails without copying credentials into project config files.

## Why use it?

- **You stay in control.** Agents can see which connection is active, but they cannot switch databases for you.
- **Credentials stay local.** Passwords and connection strings are stored in macOS Keychain or Windows Credential Manager.
- **Read-only mode is enforced.** Read-only connections are opened with PostgreSQL's transaction read-only setting enabled from the start.
- **Destructive writes have a row cap.** `update_rows` and `delete_rows` require `expected_max_rows` so a bad filter can be rolled back before it does too much damage.
- **PII can be redacted.** A per-connection toggle hides cells that look like emails, phone numbers, SSNs, credit-card-like values, or name-ish fields.
- **Every query is auditable.** The local audit log records agent queries with session IDs.
- **Remote databases can use TLS.** Turn on SSL for encrypted Postgres connections.

## Install

1. Download the macOS `.dmg` or Windows `.msi` from the [latest release](https://github.com/gsigler/pg-mcp/releases).
2. Open the installer and move pg-mcp into your Applications folder on macOS, or follow the Windows installer prompts.
3. Launch pg-mcp and add your PostgreSQL connection.
4. Open **Agent Setup** and install pg-mcp into your MCP client.
5. Restart the client so it picks up the new MCP server.

The Agent Setup panel can install pg-mcp for Claude Desktop, Claude Code, Cursor, VS Code, and Codex. It also includes copy-paste config for other MCP clients.

### macOS: opening the unsigned build

The macOS build is not yet signed or notarized by Apple, so Gatekeeper may block it the first time you open it.

If macOS says the app cannot be opened:

1. Make sure you downloaded pg-mcp from the official release page.
2. In Finder, Control-click `pg-mcp.app` and choose **Open**.
3. Click **Open** again in the confirmation dialog.

If macOS only shows a warning in System Settings:

1. Open **System Settings** > **Privacy & Security**.
2. Scroll to the security message about pg-mcp.
3. Click **Open Anyway**, then confirm.

You should only need to do this once for each downloaded build.

## How it works

pg-mcp has two modes in one app:

- Launch it normally to manage connections in the desktop UI.
- MCP clients launch the same binary with `serve` to talk to PostgreSQL over stdio.

Every tool response starts with a banner showing the active connection, host, read/write mode, and PII redaction state. If you change the active connection in the UI, the next MCP tool call uses the new selection.

## What agents can do

pg-mcp gives agents practical Postgres tools without making them memorize your schema:

- List configured connections and identify the active one.
- Get a database overview with schemas, large tables, columns, samples, and foreign keys.
- Search table and column names.
- Describe schemas, tables, indexes, relationships, and JSONB keys.
- Run SQL queries with pagination.
- Sample table rows.
- Generate a text ER diagram.
- Test connection health.
- Insert, update, and delete rows with guardrails.
- Run `EXPLAIN` and `EXPLAIN ANALYZE`.
- Review recent query history for the current MCP process.

## A few safety tips

- Use a least-privilege Postgres role instead of a superuser.
- Prefer read-only roles or read replicas for agent analysis work.
- Turn on SSL for remote databases.
- Rotate credentials in your normal password or cloud-secret workflow.

pg-mcp adds helpful guardrails, but Postgres permissions are still the source of truth.

## Build from source

You need Node 20+, Rust stable, and the normal Tauri platform prerequisites.

```sh
git clone https://github.com/gsigler/pg-mcp.git
cd pg-mcp
npm install
npm run tauri build
```

For development:

```sh
npm run tauri dev
cd src-tauri && cargo test
cd src-tauri && cargo check
```

## License

MIT. See [LICENSE](LICENSE).
