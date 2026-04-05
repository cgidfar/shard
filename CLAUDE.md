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
