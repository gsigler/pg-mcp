# AGENTS.md

Notes for cloud agents working in this repo. User-facing docs: `README.md`, `CLAUDE.md`.

## Cursor Cloud specific instructions

### What runs here

Single Tauri 2 + Svelte 5 app (`pg-mcp`). Two modes from one binary:

- **UI**: `npm run tauri dev` (Vite on port **1420** + webview)
- **MCP**: `src-tauri/target/debug/pg-mcp serve` (stdio JSON-RPC)

There is no in-repo docker-compose or separate API service. **PostgreSQL is external** (optional for `cargo test`; required for live DB / MCP E2E).

### VM prerequisites (not in update script)

On Linux cloud VMs, install once (or ensure image has):

1. **Rust**: stable **≥ 1.85** (older 1.83 fails on `edition2024` crates). Use `rustup default stable` and `source /usr/local/cargo/env` before `cargo` commands.
2. **Tauri Linux deps** (Debian/Ubuntu):

```sh
sudo apt-get install -y libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev libgtk-3-dev patchelf pkg-config
```

3. **GUI**: `DISPLAY` must be set (e.g. `:1`) for `tauri dev` / the desktop window.
4. **Postgres (optional E2E)**: e.g. `sudo apt-get install postgresql`, `sudo pg_ctlcluster 16 main start`, create a DB. Config lives at `~/.config/pg-mcp/config.json`; passwords belong in the keychain when available, or temporarily in `config.json` for headless tests (see `config.rs` migration).

### Standard commands

| Task | Command |
|------|---------|
| Install JS deps | `npm install` |
| Frontend dev only | `npm run dev` → http://localhost:1420 |
| Full desktop dev | `npm run tauri dev` |
| Frontend production build | `npm run build` (required once before `cargo check` on fresh clone) |
| Rust unit tests | `cd src-tauri && cargo test` |
| Rust typecheck | `cd src-tauri && cargo check` |
| Release bundle | `npm run tauri build` |

No ESLint/Prettier scripts are configured. CI runs the Release workflow on every merge to `main` (and on manual dispatch); it builds macOS + Windows, not Linux.

### Gotchas

- **`tauri::generate_context!` needs `dist/`** — run `npm run build` before first `cargo check` / `cargo build` if `dist/` is missing.
- **Port 1420**: `tauri dev` runs `beforeDevCommand` (`npm run dev`). If Vite is already bound to 1420, stop the other process first.
- **Linux keychain**: `keyring` uses Secret Service; headless VMs may log keychain warnings and keep plaintext secrets in config until migrated.
- **Long-running dev**: use tmux for `npm run tauri dev` (see portal tmux config under `/exec-daemon/tmux.portal.conf`).

### MCP smoke test (no GUI)

With config + Postgres reachable:

```sh
source /usr/local/cargo/env
BIN=src-tauri/target/debug/pg-mcp
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"test_connection","arguments":{}}}' \
  | "$BIN" serve
```

Expect a connection banner and `Connection OK` in the `test_connection` result.
