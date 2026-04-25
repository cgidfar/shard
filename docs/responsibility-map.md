# Shard Responsibility Map

Who owns what across the running system. There are **six** distinct roles at runtime, not four — `shardctl` is a single binary that plays two very different roles (user-facing CLI vs. detached supervisor vs. detached daemon), and Vite is strictly dev-only.

---

## High-level topology

```
┌──────────────────────────────────────────────────────────────────────┐
│                        DEVELOPMENT ONLY                              │
│  ┌────────────────┐                                                  │
│  │ Vite dev server│  :5173 (HMR :5174)                               │
│  │  (bun/node)    │  Serves TS/CSS/HTML to the WebView during dev.   │
│  └────────────────┘  Not present in production — Tauri bundles dist. │
└──────────────────────────────────────────────────────────────────────┘

╔══════════════════════════════════════════════════════════════════════╗
║                      USER-FACING PROCESSES                           ║
╠══════════════════════════════════════════════════════════════════════╣
║                                                                      ║
║  ┌────────────────────┐        ┌──────────────────────────────────┐  ║
║  │  Tauri Frontend    │        │  shard-cli (shardctl)            │  ║
║  │  (WebView + xterm) │        │  User's terminal invocation      │  ║
║  │  TypeScript only   │        │  repo/workspace/session/prune    │  ║
║  └─────────┬──────────┘        └─────────────────┬────────────────┘  ║
║            │ Tauri IPC                           │ Control-pipe RPC  ║
║            │ (invoke + channels)                 │                   ║
║            ▼                                     │                   ║
║  ┌────────────────────┐                          │                   ║
║  │  Tauri Backend     │                          │                   ║
║  │  (shard-app, Rust) │                          │                   ║
║  └─────────┬──────────┘                          │                   ║
║            │ Control-pipe RPC                    │                   ║
║            └────────────────┬─────────────────── ┘                   ║
║                             ▼                                        ║
╠══════════════════════════════════════════════════════════════════════╣
║                   DETACHED BACKGROUND PROCESSES                      ║
╠══════════════════════════════════════════════════════════════════════╣
║                                                                      ║
║          ┌───────────────────────────────────────┐                   ║
║          │  Daemon  (shardctl daemon start)      │                   ║
║          │  - Tray icon, orchestrates lifecycle  │                   ║
║          │  - Spawns & job-objects supervisors   │                   ║
║          └──────────────────┬────────────────────┘                   ║
║                             │ process spawn (detached)               ║
║                             ▼                                        ║
║          ┌───────────────────────────────────────┐                   ║
║          │  Supervisor × N                        │                  ║
║          │  (shardctl session serve --id <uuid>) │                   ║
║          │  - Owns ONE PTY + ONE named pipe      │                   ║
║          └──────────────────┬────────────────────┘                   ║
║                             │ stdio                                  ║
║                             ▼                                        ║
║                       ┌───────────┐                                  ║
║                       │Shell/Agent│                                  ║
║                       └───────────┘                                  ║
╠══════════════════════════════════════════════════════════════════════╣
║                         SHARED STATE                                 ║
║                                                                      ║
║        SQLite  repo.db  (WAL mode, 5s busy_timeout)                  ║
║        Filesystem: ready files, session logs, .shard/ worktrees      ║
║                                                                      ║
║    Readers/writers: Tauri Backend, Daemon, CLI                       ║
║    Supervisors do NOT write the DB.                                  ║
╚══════════════════════════════════════════════════════════════════════╝
```

---

## Per-component responsibilities

### 1. Vite dev server  — dev-only frontend bundler
- **Owns:** HMR, TypeScript compilation, module graph for `crates/shard-app/frontend/src/`.
- **Talks to:** the WebView during `bun run dev` — that's it.
- **Does NOT:** exist in production builds, talk to Rust, touch the DB, or participate in any session plumbing. Production ships the static `dist/` bundled into the Tauri binary.

### 2. Tauri Frontend  (`crates/shard-app/frontend/`)
- **Owns:** the entire UI — xterm.js terminal, sidebar tree, dialogs, title bar, layout state.
- **Talks to:** the Tauri Backend ONLY, via two mechanisms:
  - `invoke(...)` for request/response IPC (the `#[tauri::command]` handlers).
  - Tauri event listeners (`sidebar-changed`, `workspace-status-changed`, `session-activity`) and Tauri channels (binary PTY output stream).
- **Does NOT:** connect to named pipes directly, spawn processes, read SQLite, or run git. Every side effect is an `invoke` call.

### 3. Tauri Backend  (`crates/shard-app/src/`)
The orchestrator / translator between the WebView and the rest of the system.
- **Owns:**
  - The ~15 `#[tauri::command]` handlers in `commands/{repo,workspace,session}.rs`.
  - `AppState`: in-memory registry of active `SessionConnection`s (monitor task + attach reader per session).
  - Best-effort Claude Code hook installation on first session.
- **Talks to:**
  - **Frontend** — replies to `invoke`, emits events/channel frames.
  - **Daemon** — control-protocol RPC over named pipe: `SpawnSession`, `StopSession`, `ListSessions`. Spawns the daemon if it isn't running.
  - **Supervisors directly** — session transport pipe. Opens TWO connections per attached session: a *monitor* (Status/ActivityUpdate) and an *attach reader* (TerminalOutput); sends TerminalInput/Resize on the same pipe.
  - **SQLite** via `shard-core` stores, for all list/add/remove reads and writes of repo & workspace records.
- **Does NOT:** spawn supervisors itself (that's the daemon), run PTYs, or own the tray.

### 4. shard-cli  (`shardctl`, user-invoked)
- **Owns:** CLI UX — `repo`, `workspace`, `session`, `prune` subcommands.
- **Talks to:** the Daemon via control-protocol RPC (same as the Tauri backend), and SQLite for read-only listings.
- **Does NOT:** attach to PTY pipes for terminal I/O — there's no `shardctl session attach` that streams bytes. Terminal I/O is a Tauri-backend-only path. CLI is strictly for lifecycle and inspection.

### 5. Daemon  (`shardctl daemon start`, detached singleton)
The lifecycle manager. One per machine/user session.
- **Owns:**
  - Windows tray icon (Open Shard / Quit / session count).
  - The control-protocol pipe server that both the Tauri backend and CLI call.
  - A Windows **Job Object** that every spawned supervisor is assigned to — if the daemon dies, the OS kills all supervisors. If a supervisor crashes, the Job Object kills its child PTY.
  - Graceful shutdown sequencing: snapshot sessions → StopGraceful each supervisor in parallel (≤3s budget) → force-kill stragglers → mark DB rows `stopped`.
  - Stale-PID pruning (heartbeat) to handle Windows PID reuse.
- **Talks to:**
  - **Tauri backend & CLI** via control-protocol RPC (inbound).
  - **Supervisors** by spawning them as detached child processes and by opening their session pipes to send StopGraceful/StopForce during shutdown.
  - **SQLite** — writes session status on shutdown; reads when pruning.
- **Does NOT:** handle terminal I/O, speak the session transport protocol for data frames, or know anything about repos/workspaces beyond session rows.

### 6. Supervisor  (`shardctl session serve --id <uuid>`, one per session)
- **Owns:**
  - Exactly one PTY (via `portable-pty`) and exactly one named pipe server at `\\.\pipe\shard-session-<id>`.
  - The replay ring buffer (offset-tagged TerminalOutput frames for Resume).
  - Activity state machine (Active / Idle / Blocked) — fed by harness hooks reaching back via `SHARD_PIPE_ADDR`.
  - `supervisor.log` and `session.log`; the `ready` file that signals the daemon it's listening.
- **Talks to:**
  - **The PTY** — reads output, writes input, handles resize/exit.
  - **Any connected client** (Tauri backend, or hook scripts) over the session pipe: accepts `Resume`/`TerminalInput`/`Resize`/`StopGraceful`/`StopForce`, emits `TerminalOutput`/`Status`/`ActivityUpdate`.
- **Does NOT:** write the DB, spawn other processes, accept control-protocol RPCs, or know where the daemon is.

---

## Two pipes, two protocols (don't conflate them)

| Pipe | Server | Client | Protocol | Purpose |
|---|---|---|---|---|
| `\\.\pipe\shard-daemon` (control) | Daemon | Tauri backend, CLI | Control RPC (SpawnSession, StopSession, ListSessions, Shutdown) | Lifecycle |
| `\\.\pipe\shard-session-<id>` (transport) | Supervisor | Tauri backend, hooks | Framed session protocol (7 frame types in `shard-transport`) | Terminal I/O + activity |

The Tauri backend is the only client that speaks **both** protocols.

---

## Session-creation cascade (the canonical flow)

```
Frontend          Tauri Backend        Daemon             Supervisor        PTY
   │                   │                  │                   │              │
   │ invoke(create)    │                  │                   │              │
   ├──────────────────▶│                  │                   │              │
   │                   │ SpawnSession RPC │                   │              │
   │                   ├─────────────────▶│                   │              │
   │                   │                  │ spawn detached    │              │
   │                   │                  ├──────────────────▶│              │
   │                   │                  │                   │ open pipe    │
   │                   │                  │                   │ spawn PTY    │
   │                   │                  │                   ├─────────────▶│
   │                   │                  │   write ready file│              │
   │                   │                  │◀──────────────────┤              │
   │                   │  SpawnAck        │                   │              │
   │                   │◀─────────────────┤                   │              │
   │  Session record   │                  │                   │              │
   │◀──────────────────┤                  │                   │              │
   │                   │                  │                   │              │
   │ invoke(attach)    │                  │                   │              │
   ├──────────────────▶│  Resume frame    │                   │              │
   │                   ├──────────────────┼──────────────────▶│              │
   │                   │  TerminalOutput frames                │              │
   │◀──────────────────┤◀──────────────────┼───────────────────┤              │
```

---

## Surprising / easy-to-miss

- **Frontend never touches a named pipe.** All terminal bytes flow Frontend ↔ Tauri-backend ↔ Supervisor. The backend is an active relay, not a pass-through.
- **CLI can't stream terminal I/O.** It creates/stops/lists; terminal UX is Tauri-only by design.
- **Daemon is the only process that spawns supervisors.** Even when Tauri creates a session, it asks the daemon to spawn — this keeps every supervisor in the daemon's Job Object.
- **Supervisors are write-blind to SQLite.** They only consume paths; DB writes for session status come from the daemon (on shutdown) and Tauri backend (on user action).
- **Vite is never in the loop at runtime.** If the app feels "live-reload-y" in prod, that's only Tauri event emission, not HMR.
