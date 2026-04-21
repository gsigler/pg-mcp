# CLAUDE.md

Notes for Claude Code working in this repo. User-facing docs live in `README.md`.

## What this is

A Tauri 2 desktop app that's also an MCP server. One binary, two modes:

- **No args**: launches the Svelte UI for managing Postgres connections.
- **`serve` subcommand**: runs an MCP server over stdio. This is what Claude Desktop / Claude Code invoke.

The UI writes `config.json` + keychain entries. The `serve` child process reads them. Secrets never cross the stdio pipe or land on disk unencrypted.

## Layout

```
src/                  # Svelte 5 frontend (the UI)
  lib/
    ConnectionModal.svelte      # add/edit connection form
    ConnectionList.svelte       # sidebar
    AgentSetup.svelte           # "Install in Claude Desktop/Code"
src-tauri/
  src/
    main.rs           # Tauri commands + serve dispatch + single-instance
    mcp_server.rs     # MCP JSON-RPC loop and tool handlers
    database.rs       # Postgres client, SQL execution, row formatting, hardening
    config.rs         # Connection/Config structs, load/save, migration
    secrets.rs        # OS keychain wrapper (keyring crate)
    audit.rs          # JSON-lines audit log, shared across processes
    pii.rs            # PII detection heuristics (column + value)
  tauri.conf.json     # window, CSP, bundle config
  icons/              # generated from macos-app-icons/AppIcon1024.png via `tauri icon`
macos-app-icons/      # source icon artifacts; regenerate bundle icons from here
.github/workflows/release.yml   # tag-triggered cross-platform build
```

## Build / test / run

```sh
npm install
npm run tauri dev                  # UI with Vite hot reload
npm run tauri build                # release bundle

cd src-tauri
cargo check                        # fast type check
cargo test                         # unit tests (pii heuristics, etc.)
cargo build --release              # release binary (needs frontend built first: `npm run build`)
```

On a fresh machine `cargo` may not be on PATH; Rust was installed via Homebrew's `rustup`:

```sh
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
```

## Architecture notes

### Two processes, shared config and keychain

- **UI process** runs Tauri, owns the webview, has a `DatabaseManager` used only for `test_connection_cmd`.
- **MCP child** is spawned by Claude, runs `mcp_server::McpServer::run()`, has its *own* `DatabaseManager`.

They never share memory. They share:

- `config.json` — atomic writes (tmp + chmod + rename). Reads tolerate partial writes.
- OS keychain entries under service `pg-mcp`.
- The audit log at `<config_dir>/pg-mcp/pg-mcp.log`, append-only.

### MCP tool dispatch

`mcp_server::handle_tools_call` reloads config on every invocation so active-connection changes from the UI propagate on the next tool call. `ensure_connected` reconnects when the active connection name has changed. In-flight queries finish on the old DB.

### Readonly enforcement

Defense in depth, with the server as the source of truth:

1. `check_write_query` inspects the first token of raw SQL. Fast-fail UX only; never rely on it.
2. At connect time, RO connections get `SET default_transaction_read_only = on`. Postgres rejects CTE writes, side-effecting functions, etc. at the statement level.
3. Tool-level gates: `insert_rows`/`update_rows`/`delete_rows` refuse to run on a connection flagged readonly in `config.json`.
4. `explain_query` with `analyze=true` refuses if the underlying SQL looks like a write, so `EXPLAIN ANALYZE INSERT` can't sneak past readonly.

### Identifier vs value safety

- **Identifiers** (schema, table, column names) are quoted via `quote_ident` — double quotes with embedded-quote escaping. Never bind them as parameters; Postgres doesn't accept identifiers as parameters.
- **Introspection filters** (comparing against `information_schema.columns.table_schema` etc.) use parameterized queries (`$1`, `$2`).
- **Insert / update values** are dollar-quoted via `dollar_quote`. Tag expands on collision so embedded dollar-quotes cannot break out. Unaffected by `standard_conforming_strings`.
- **WHERE clauses** to `update_rows` / `delete_rows` are raw SQL *by API contract*. Callers must trust their own inputs. The `expected_max_rows` cap is the backstop.

### Data-loss guard

`execute_write_capped` wraps UPDATE/DELETE in `BEGIN` (or `SAVEPOINT pgmcp_write_cap` when inside a user transaction). Runs the statement with `RETURNING *`, counts rows, commits only if count ≤ `expected_max_rows`. Otherwise rolls back.

### PII redaction

`format_rows_opt(rows, redact)` is the chokepoint. Only the data-path methods (`execute_parameterized`, `execute_write_capped`) read the current `redact_pii` flag (captured on connect) and pass it through. Introspection paths pass the default `false`. Redaction is whole-cell: `[REDACTED]` replaces both the raw value and anything the truncation code would have produced.

### Secrets migration

`Config::load` runs a one-shot migration: any plaintext password / connection string left over from a pre-keychain build is moved into the keychain on startup, the file is rewritten without it, and the in-memory struct is left with blank fields (hydrated on demand by `Connection::hydrate_secrets`).

`ensure_connected` only calls `hydrate_secrets` when reconnecting — **not** on every tool call. Hitting the macOS keychain per query produces a prompt-per-query storm.

## Adding a new MCP tool

1. Append an entry to the `"tools"` array in `McpServer::handle_tools_list`. Every property needs a JSONSchema `type`; required fields go in `required`.
2. Add a match arm in `McpServer::handle_tools_call`.
3. Implement `async fn tool_<name>(&self, config: &Config, args: &Value) -> Result<String, String>` on `McpServer`. Start with `let conn = self.ensure_connected(config).await?;` unless the tool doesn't need a DB.
4. Add the banner: `Ok(format!("{}\n{}", Self::connection_banner(&conn), result))`.
5. If the tool returns user data rows, call through `execute_parameterized` / `execute_write_capped` so PII redaction applies. If it returns metadata only, call `client.query(...)` + `format_rows(...)` directly.

## Adding a field to `Connection`

1. Add the field on `Connection` in `config.rs` with `#[serde(default)]` so old config files parse.
2. Add it to `SafeConnection` and the `From<&Connection>` impl (the webview sees this, not `Connection`, except on edit).
3. Thread through UI: state var in `ConnectionModal.svelte`, bound input, include in `buildConnection()`.
4. If it affects DB behavior, snapshot it on `DatabaseManager` at connect time (see `redact_pii` as the template).

## Release flow

```sh
git tag vX.Y.Z
git push origin main vX.Y.Z
```

GitHub Actions builds macOS universal and Windows x64 and attaches installers to a draft release. Manually publish via the GitHub UI after reviewing release notes.

To regenerate icons after replacing `macos-app-icons/AppIcon1024.png`:

```sh
npx @tauri-apps/cli icon macos-app-icons/AppIcon1024.png
# then prune non-macOS/Windows variants:
cd src-tauri/icons && rm -rf android ios Square*.png StoreLogo.png 64x64.png icon.png
```

## Common pitfalls

- **Keychain re-prompts on every rebuild.** Unsigned binaries get path+inode-based ACLs; `cargo build` changes the inode. Click "Always Allow" once per build. Real fix is code signing in CI — not yet done.
- **Claude Code install scope.** We install at user scope (`mcpServers` at the top of `~/.claude.json`). Earlier builds wrote to project scope under `~`; the current install command scrubs that automatically.
- **`tauri::generate_context!` panics without `dist/`.** Run `npm run build` before `cargo check` / `cargo build` on a fresh clone.
- **CSP.** Set in `tauri.conf.json`. Adding a new asset source (e.g. remote image) requires updating it or the webview blocks the resource.
- **Dollar-quote tag collision.** `dollar_quote` expands the tag by appending `_` until absent from the value. Don't assume the tag is always `pgmcp` when writing tests or log expectations.

## Non-goals

- **Letting the agent select the active DB.** The tool's value prop is deliberate, visible DB selection. `list_connections` is read-only on purpose.
- **Auto-refreshing IAM tokens.** Out of scope — the keychain holds static secrets only.
- **Multi-DB concurrency within a single MCP session.** Each Claude session already gets its own MCP child process; that's the unit of concurrency. A `deadpool-postgres` pool would unlock intra-session parallelism but no common MCP client sends concurrent tool calls per session yet.
