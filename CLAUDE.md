# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is Shard?

A workspace manager for parallel coding agents. Manages repos, git worktrees (workspaces), and PTY sessions with a detached supervisor architecture. Windows-native, with future Mac portability via platform traits. Inspired by [penberg/swarm](https://github.com/penberg/swarm) (clean-room reimplementation).

## Build & Test Commands

```bash
cargo check                        # Type-check entire workspace
cargo test --workspace             # Run all tests (protocol + git URL parsing)
cargo build -p shard-cli           # Build CLI → target/debug/shardctl.exe
cargo build -p shard-app           # Build Tauri app (runs frontend build automatically)
cargo run -p shard-cli -- <args>   # Run CLI directly

# Frontend (in crates/shard-app/frontend/)
bun install                        # Install frontend deps
bun run dev                        # Vite dev server on :5173
bun run tsc --noEmit               # TypeScript type check
bun run build                      # Production build → dist/

# Dev launch (app): start Vite dev server, then cargo run -p shard-app
```

## Architecture

Five-crate Cargo workspace. Dependency flow:

```
shard-cli ──┐
shard-app ──┤→ shard-supervisor → shard-transport
            └→ shard-core       ↗
```

**shard-core** — Data model, SQLite (rusqlite), git operations. No platform deps. `ShardPaths` resolves all directories (`SHARD_DATA_DIR` env override or `%LOCALAPPDATA%\shard\data\`). `ShardError` is the unified error type.

**shard-transport** — `SessionTransport` async trait + framed protocol. Windows: named pipes (`\\.\pipe\shard-session-{id}`). Wire format: `[u32 len][u8 type][payload]` with 7 frame types (TerminalOutput, TerminalInput, Resize, StopGraceful, StopForce, Status, Resume).

**shard-supervisor** — `ProcessControl` trait for platform process lifecycle. `PtySession` wraps portable-pty. Event loop (`event_loop.rs`) runs 4 concurrent tokio tasks: PTY reader fan-out, pipe accept loop, resize handler, child wait/shutdown. Job Object prevents orphaned processes on Windows.

**shard-cli** — `shardctl` binary. Commands: repo, workspace, session, prune. The `session serve` hidden subcommand IS the supervisor process — spawned detached by `session create`.

**shard-app** — Tauri v2 desktop app. TypeScript frontend with xterm.js (WebGL). Backend exposes 16 IPC commands. Connects to supervisors as a transport client (same as CLI attach). Frontend in `crates/shard-app/frontend/`.

## Session Supervisor Model

`session create` → spawns detached `shardctl session serve` → supervisor spawns PTY + named pipe server → writes ready file → creator returns. Clients (CLI attach or Tauri app) connect to the pipe, send Resume frame to replay from log, then stream live. Stop is via RPC (StopGraceful/StopForce frames), never external kill.

## Local vs Remote Repos

**Local repos** (added by filesystem path): no bare clone. The original checkout is the git source. Default workspace (`is_base=true`) points directly to the checkout. Additional workspaces create git worktrees in `.shard/` next to the repo. `.shard/` is auto-added to `.git/info/exclude`.

**Remote repos** (added by URL): bare-cloned to AppData. All workspaces are git worktrees from the bare clone.

## Platform Portability

Platform-specific code is behind two traits: `SessionTransport` (IPC) and `ProcessControl` (process lifecycle). Windows implementations are in `transport_windows.rs` and `process_windows.rs`. Adding Mac support means implementing `transport_unix.rs` (~50 lines, Unix sockets) and `process_unix.rs` (~30 lines, POSIX signals).

## SQLite Concurrency

WAL mode + 5s busy_timeout on every connection (`db::open_connection`). Multiple processes (supervisors, CLI, app) access `repo.db` concurrently. Short transactions only.

## Playwright MCP for Tauri UI Inspection

Playwright MCP can attach to the running Tauri WebView via CDP (Chrome DevTools Protocol). This is the only way to get screenshots, accessibility snapshots, and DOM interaction with the live app. **It does NOT work by navigating to the Vite dev URL** — that creates a separate context without `window.__TAURI__`, breaking IPC.

### Setup

1. **Launch the app with CDP enabled** — set the env var before `cargo run`:
   ```bash
   WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9222" cargo run -p shard-app
   ```

2. **Playwright MCP is configured** with `--cdp-endpoint http://localhost:9222` (already in Claude Code MCP config). It connects to the existing WebView rather than launching its own browser.

3. **Verify CDP is live** before using Playwright tools:
   ```bash
   curl -s http://localhost:9222/json/list
   ```
   Should return a JSON array with a page entry titled "Shard".

### Gotchas

- **App relaunch = Playwright reconnect required.** When the Tauri app restarts, the CDP WebSocket changes. Playwright MCP caches the old connection and the next call returns "Target page, context or browser has been closed." Two ways to recover, in order of preference:
  1. **In-tool reattach (no MCP restart needed):** call `browser_close` then `browser_tabs list`. This makes Playwright re-enumerate CDP targets and pick up the new "Shard" page. Then call `browser_snapshot` normally.
  2. **Fallback:** run `/mcp` to fully reconnect.

  Do **not** call `browser_navigate http://localhost:5173/` to recover — that creates a fresh CDP context without `window.__TAURI__`, which empties the sidebar and breaks IPC for the rest of the session. Recovery requires killing and restarting the app.

- **`tauri.conf.json` changes require full relaunch.** Changes like `decorations: false` are compiled into the Rust binary. Vite HMR won't pick them up — you must kill and re-run `cargo run -p shard-app`.

- **Vite module cache can serve stale code.** If you rewrite a file completely (not incremental edit), Vite's transform cache may keep serving the old version. Fix: delete `node_modules/.vite/`, `touch` all changed files, then relaunch the app. Verify with:
  ```js
  // In browser_evaluate:
  fetch('/src/main.ts').then(r => r.text()).then(t => t.slice(0, 300))
  ```

- **`browser_evaluate` runs in a CDP-isolated context.** `window.__TAURI__` will be `undefined` in evaluate calls even though it works fine in the actual app. Don't rely on it for diagnostics.

- **Clean up artifacts.** Playwright MCP dumps screenshots, snapshots, and console logs into `.playwright-mcp/` in the working directory. Clean these up before committing — they're not useful to keep.
