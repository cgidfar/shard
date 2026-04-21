# Daemon-as-Broker Migration Plan

**Status:** Phase 0 + Phase 1 + Phase 2 + Phase 3 + Phase 4 landed on branch `daemon-broker-migration`. Phase 4 adds Batch C session lifecycle tail (RemoveSession, RenameSession, DetachSession, FindSessionById); `SpawnSession` / `StopSession` / `ListSessions` existed from earlier phases. Phase 4 Codex round 1 done — 2 mediums + 1 low applied. 137 tests pass (22 new in Phase 4: 15 session-lifecycle integration + 7 wire roundtrips), zero Phase 4-introduced warnings.
**Related:** SHA-55 (workspace delete failure), `docs/responsibility-map.md`

## Goals

1. **Single owner for state mutations.** Every DB write, filesystem mutation, and process spawn goes through a daemon RPC. `shard-core` stores are only called by the daemon.
2. **Eliminate coordination bugs** of the SHA-55 class. Today, workspace delete is split across the Tauri backend (issues the call), `shard-core` (does git + DB), and the daemon (owns watchers). The split produces race conditions. One actor owns the full sequence.
3. **Make CLI = GUI equivalence trivial.** `shardctl` and the Tauri frontend both speak the same daemon RPC. Agent-driven actions via CLI are indistinguishable from human-driven actions via GUI.
4. **Set up for multi-window** (same Tauri process). The Tauri backend subscribes once per repo/session; all windows share that subscription through in-process Tauri events.

## Non-goals

- Rebuilding wire framing. The existing `[u32 len][u8 type][payload]` binary codec is fine and stays.
- Routing terminal I/O (TerminalOutput / TerminalInput / Resize) through the daemon. Byte-level PTY streaming stays direct from Tauri backend ↔ supervisor pipe.
- Routing `ActivityUpdate` frames through the daemon. Supervisor fans out to each connected client with bounded mpsc; that path is efficient and keeps activity latency low.
- Cross-process multi-window. Same-Tauri-process is the target; cross-process would require additional daemon broadcast work and is out of scope.
- Base-workspace asymmetry cleanup. `is_base=true` workspaces referring to the user's original checkout is a pre-existing design choice; leave alone for this migration.
- Feature-flagging the migration. We cut over per batch — the codebase has one consumer (the author) and Git history gives us rollback.

## Current state

See `docs/responsibility-map.md` for the topology. Quick recap of what changes:

| Concern | Today | After |
| --- | --- | --- |
| Repo CRUD | Tauri backend + CLI each call `RepositoryStore` directly | Both call daemon RPC |
| Workspace CRUD | Same | Same → daemon coordinates watcher drop + session stop + git + DB as one atomic op |
| Session CRUD (except create) | Same | Daemon owns (`create` already routes through daemon) |
| Reads: `*.get` / immutable lookups | Direct SQLite | Unchanged (direct SQLite) |
| Reads: `*.list` / `list_branch_info` / `find_by_id` | Direct SQLite / git | Daemon RPC (consistent with event stream) |
| Workspace status events | Daemon broadcasts via existing `Subscribe` | Unchanged |
| Session lifecycle events (`SessionsChanged`) | Daemon broadcasts | Extend to finer-grained events (create / rename / stop) |
| `session-activity` | Supervisor fans out to each Tauri-backend connection | Unchanged |
| Terminal I/O | Direct supervisor ↔ Tauri channel | Unchanged |

## Target architecture

```
Frontend  ──Tauri IPC──▶  Tauri Backend ──┬──control RPC──▶  Daemon ──owns──▶  SQLite + FS + git + watchers
                                          │                          │
                                          │                          └──spawns──▶ Supervisor (one per session)
                                          │
                                          └──Subscribe pipe──▶  Daemon events (repo/workspace/session changes)
                                          
                                          └──session pipe──▶   Supervisor directly (terminal I/O + activity)

CLI  ───────control RPC────────▶  Daemon (same surface as Tauri Backend)
```

The Tauri backend is a thin translator in two directions:
- **Tauri IPC → daemon RPC** for every mutation and for `list`-style reads.
- **Daemon events → Tauri events** re-broadcast to the WebView (all windows).

## Decisions taken

### D1. Keep current framing; extend `ControlFrame` enum

Agent A confirmed the existing codec in `crates/shard-transport/src/control_protocol.rs` supports everything needed. Each new RPC is ~16 lines of codec boilerplate (enum variant + type byte + encode arm + decode arm). Not worth moving to serde JSON.

### D2. Use a dedicated subscription connection for events

`run_state_subscriber` in `crates/shard-app/src/daemon_ipc.rs:67-87` already does this. Keep it. Don't mux push events onto the request/response connection.

### D3. Activity + terminal I/O stay on the session pipe, direct

Routing through the daemon adds hops without solving a real problem. For multi-window-same-process, the Tauri backend subscribes once per session and re-emits to all WebView windows via Tauri events.

### D4. Reads split: direct for immutable lookups, RPC for live views

Migrate to daemon RPC:
- `repo.list` — pairs with topology events
- `workspace.list` — pairs with workspace status events
- `workspace.list_branch_info` — git-backed, needs monitor coordination
- `session.list` — pairs with session lifecycle events
- `session.find_by_id` — daemon owns the global session index

Stay direct (read from SQLite in-process):
- `repo.get(alias)` — immutable post-creation
- `workspace.get(repo, name)` — path/branch/is_base immutable
- `session.get(repo, id)` — short-lived lookups for attach/stop

### D5. Each mutation RPC is a serialized workflow, not just a sequenced handler

The client-visible RPC is still one call, but internally it's a workflow that must be serialized against concurrent mutations on the same object and must wait for real quiescence at each step. The accept loop spawns per-client handlers concurrently (`daemon.rs:1007`), so "run these five steps in order" in one task is not enough — another connection can land `SpawnSession` between step 1 and step 3.

The daemon handler for `remove_workspace` does:

1. **Acquire per-workspace lifecycle gate.** Transition the workspace's in-memory state from `active` → `deleting` under a per-repo mutex. Concurrent `SpawnSession` / `CreateWorkspace` / second `RemoveWorkspace` on the same target either wait on the gate or fail fast with a typed "workspace is being deleted" error. See D12 for the state machine.
2. **Stop-and-wait all sessions bound to the workspace.** Existing `StopSession` (`daemon.rs:1429`) writes a stop frame and returns `StopAck` without waiting for exit — that's not enough here because the child shell's CWD still pins the directory. Use the `stop_one_graceful` pattern at `daemon.rs:652` (write stop frame, read until `Status` frame or pipe EOF) with a timeout + force-kill fallback via the Job Object. Phase 0 extracts this as `stop_session_and_wait`.
3. **Drop the watcher and await the on-task drop.** Send `MonitorCommand::DropRepoWorkspace { alias, name, ack }`. The ack means "the `Debouncer` was actually dropped on the monitor task," not "the command was queued" (see D9).
4. **Run `git worktree remove` / fallback `fs::remove_dir_all`.**
5. **Delete the DB row.**
6. **Transition state to `gone` and release the gate.**
7. **Broadcast `WorkspaceRemoved { repo, name }`.**

Any step that fails transitions the state back to a recoverable position (`active` if nothing on disk changed, `broken` if partial cleanup happened) and releases the gate. No half-held locks, no stuck `deleting` state on panic.

"Atomic" here means **serialized + idempotent + reversible on failure** — not "one uninterruptible transaction." That's the right level of guarantee for a workflow that spans four subsystems (PTY lifecycle, Windows handles, git, SQLite).

### D6. Partial-failure policy: rollback where possible, monitor-reconcile where not

For mutations with multiple side effects (git + DB), the daemon wraps the DB write in a transaction that only commits after the filesystem op succeeds. If git fails mid-operation (e.g., `worktree_add` creates the directory then crashes), the monitor's reconcile tick marks the row Broken and the next user-triggered remove cleans it. Acceptable because reconcile already exists.

### D7. Hard cutover per batch, no feature flags

Each batch migrates all callers atomically. Revert = `git revert`. Greenfield project, one consumer, no staged rollout needed.

### D8. Eager read migration per batch

When a mutation migrates to daemon RPC, its corresponding `list` / live-view reads migrate in the same batch. Avoids a mid-migration window where writes and reads see different sources of truth. Per-batch PRs grow, but the inconsistency window is avoidable and worth it.

### D9. Monitor coordination via typed command channel

Daemon mutation handlers communicate with `WorkspaceMonitor` through a typed `MonitorCommand` mpsc (e.g., `DropRepoWorkspace { alias, name, ack: oneshot<()> }`), not through a shared `Arc<Mutex<Monitor>>`. Preserves the monitor's actor shape, avoids holding a lock during a potentially slow filesystem op, and makes the ack point explicit.

**Ack semantics are load-bearing:** the oneshot must be sent only *after* the `Debouncer` / `RecommendedWatcher` has been dropped on the monitor task — not when the command is pulled off the channel, not when the monitor "acknowledges receipt." If the ack fires before the drop, the mutation handler proceeds to `RemoveDirectoryW` while the `ReadDirectoryChangesW` HANDLE is still live, and the bug we're fixing reappears. Concretely: the monitor's command handler removes the debouncer from its map, `drop`s the value, *then* sends the ack.

### D10. Coarse event surface for now

Keep today's coarse broadcasts (`TopologyChanged`, `SessionsChanged`, per-repo `StateSnapshot`). The frontend re-fetches on each. Move to finer events (`WorkspaceRemoved`, `SessionCreated`, etc.) only if a concrete UX problem forces it — e.g., sidebar flicker from over-invalidation.

### D11. Base-workspace asymmetry preserved; tracked separately

`WorkspaceStore::remove` skips the git/fs step for `is_base=true` workspaces today (`crates/shard-core/src/workspaces.rs:362`). Correct for local repos (the checkout belongs to the user), arguably wrong for remote repos (the bare-clone-derived base IS a worktree that should be reaped). Preserve the behavior for this migration, add a code comment marking it intentional, and file a Linear issue to revisit. (Filed as SHA-56.)

### D12. Explicit per-workspace lifecycle state

Every workspace has an in-memory lifecycle state owned by the daemon (per-repo map, guarded by a per-repo mutex):

```
active ──(RemoveWorkspace)──▶ deleting ──(cleanup ok)──▶ gone
  │                              │
  │                              └──(cleanup failed partway)──▶ broken
  ▼
stopping (not currently used for workspaces; reserved for future "quiesce all sessions" op)
```

Semantics:
- **`active`** — normal state. Mutations on this workspace are accepted.
- **`deleting`** — a `RemoveWorkspace` is in flight. New `SpawnSession` / `CreateWorkspace` targeting this workspace fail fast with a typed error. A second `RemoveWorkspace` is idempotent: it joins the existing delete (await its completion) rather than starting a duplicate.
- **`broken`** — partial cleanup (e.g., `git worktree remove` succeeded but `RemoveDirectoryW` failed because a handle leaked). The monitor's reconcile tick surfaces this; a retried `RemoveWorkspace` picks up where the previous attempt left off.
- **`gone`** — removed from the map once the broadcast has fired. A further `RemoveWorkspace` returns a typed "workspace not found" error.

Sessions get a parallel state (`active | stopping | stopped`) used by D5 step 2's stop-and-wait — `stopping` prevents a concurrent `StopSession` from firing a duplicate stop frame while the first is still draining.

This replaces "the plan relies on sequencing prose." Every RPC handler reads the state, transitions it, and commits — it doesn't just sequence side effects.

## RPC catalog

Grouped by batch. Each RPC is a new `ControlFrame` variant with a request and a typed response. Error responses use the existing `Error { message }` frame.

### Batch A — Repo CRUD

| RPC | Args | Response | Notes |
| --- | --- | --- | --- |
| `AddRepo` | `{ url, alias? }` | `{ repo: Repository }` | Auto-creates base workspace; remote → bare clone |
| `RemoveRepo` | `{ alias }` | `{}` | Walks workspaces; drops watchers; cleans `.shard/`; DB delete |
| `SyncRepo` | `{ alias }` | `{}` | `git fetch --all --prune`; no DB mutation |
| `ListRepos` | `{}` | `{ repos: Vec<Repository> }` | |
| `FindSessionById` | `{ prefix }` | `{ repo, session }` or `Error` | Walks all DBs; daemon owns the index |

### Batch B — Workspace CRUD (fixes SHA-55)

| RPC | Args | Response | Notes |
| --- | --- | --- | --- |
| `CreateWorkspace` | `{ repo, name?, mode, branch?, is_base }` | `{ workspace: Workspace }` | |
| `RemoveWorkspace` | `{ repo, name }` | `{}` | Atomic: stop sessions → drop watcher → git remove → DB delete → broadcast. Fixes SHA-55. |
| `ListWorkspaces` | `{ repo }` | `{ workspaces: Vec<WorkspaceWithStatus> }` | Daemon enriches with live status |
| `ListBranchInfo` | `{ repo }` | `{ branches: Vec<BranchInfo> }` | Git-backed; monitor-aware |

### Batch C — Session lifecycle (minus terminal I/O)

| RPC | Args | Response | Notes |
| --- | --- | --- | --- |
| `SpawnSession` | existing | existing | Already exists; no change |
| `StopSession` | existing | existing | Already exists; no change |
| `RemoveSession` | `{ repo, id }` | `{}` | Guards on status; cleans session dir |
| `RenameSession` | `{ repo, id, label? }` | `{}` | Label-only |
| `ListSessions` | existing | existing | Already exists; no change |
| `DetachSession` | `{ id }` | `{}` | Current Tauri command; trivial daemon move |

### Batch D — Polish

| RPC | Args | Response | Notes |
| --- | --- | --- | --- |
| `InstallHarnessHooks` | `{ harness: "claude-code" \| ... }` | `{ installed: bool, skipped_reason? }` | Currently best-effort per-session; centralize in daemon |

## Migration phases

Each phase is a self-contained PR. All tests pass at the end of each phase; none in between is expected to ship to users.

### Phase 0 — Groundwork (no behavior change) ✅ LANDED

- ~~Factor the daemon's mutation handler plumbing~~ Deferred — handlers are still flat matches in `dispatch_request`; `handle_remove_workspace` became the first end-to-end workflow, but a shared helper wasn't extracted. Worth revisiting in Phase 2 once the second mutation workflow lands and the shape stabilizes.
- ✅ `MonitorCommand` typed mpsc replacing the old `topology_tx`. `DropRepoWorkspace { alias, workspace_name, ack }` drops the old `Debouncer` first, rebuilds the watcher over remaining workspaces (filtered to exclude `workspace_name`), then acks. `workspace_monitor.rs::handle_drop_repo_workspace`.
- ✅ `DaemonConnection::request_typed<T>` + `DaemonError` enum in `shard-transport/daemon_client.rs`. Folds `Error { message }` responses into `DaemonError::Reported`.
- ✅ `LifecycleRegistry` in `crates/shard-cli/src/cmd/lifecycle.rs`. States: `Active | Deleting { completion: Arc<Notify> } | Broken`. `DeleteGuard` RAII with `commit_gone / commit_broken / rollback`; Drop-without-commit rolls back. `BeginDelete::{Started, AlreadyDeleting(notifier)}` — atomic over absent/Active/Broken → Deleting (Codex review simplification; no separate `NotFound`).
- ✅ `stop_and_drain` (timeout-aware drain) + `stop_session_and_wait` (drain + force-kill fallback) in `daemon.rs`. The force path uses `force_kill_pid_checked(pid, creation_time)` — new in `shard-supervisor/process_windows.rs`. `creation_time = 0` is a documented sentinel meaning "don't guard." Legacy `stop_one_graceful` now delegates to `stop_and_drain` (no force fallback — the tray-quit path relies on the Job Object).
- ✅ Integration test harness in `crates/shard-cli/tests/common/mod.rs`: `TestHarness` with `TempDir`-backed `ShardPaths::from_data_dir` (new constructor), per-test unique pipe name, `state: Arc<DaemonState>` exposed for test injection, plus `setup_local_repo` + `setup_workspace` helpers. `build_headless_state` + `run_headless_daemon_with_state` separate state construction from the control-loop task so tests keep a handle.

**Phase 0 discoveries the plan didn't anticipate:**

- `CONTROL_PIPE_NAME` was a compile-time const in `shard-transport`. Runtime-configurable pipe name required a new `DaemonConfig` struct + `connect_to(pipe_name)` in `daemon_client`. `shard-cli` gained a `lib.rs` target so integration tests can import daemon internals.
- Per-PID force-kill did not exist. `DaemonJobGuard` only handles "kill everything on daemon exit." Added `force_kill_pid_checked` with FILETIME creation-time comparison to avoid killing recycled PIDs.
- Watcher granularity is per-repo, not per-workspace. `debouncers: HashMap<String, Debouncer>` is keyed by repo alias. `DropRepoWorkspace` therefore rebuilds the whole repo's watcher, filtered to exclude the workspace being deleted.
- `WorkspaceStore` and `RepositoryStore::remove` were calling `ShardPaths::new()` internally for nested store lookups, ignoring the injected paths. Fixed to use `self.paths.clone()` so the test harness's `TempDir`-backed paths actually apply end-to-end.
- `DaemonState`, `ShutdownMode`, `LiveSession`, `LifecycleRegistry` became `pub #[doc(hidden)]` to support test injection (`test_inject_live_session`, `test_lifecycle_check`, etc.). Not part of the supported external API; production should route everything through the RPC layer.

### Phase 1 — Fix SHA-55 via Batch B workspace-remove slice ✅ LANDED

1. ✅ `ControlFrame::RemoveWorkspace { repo, name }`, `RemoveWorkspaceAck`, `WorkspaceRemoved { repo, name }` (type bytes `0x91–0x93`).
2. ✅ `daemon.rs::handle_remove_workspace` implements the D5 workflow. Changes vs the original plan: the gate-acquire is an outer `loop { begin_delete }` that handles the `AlreadyDeleting` + notifier-wait case — after wake-up, re-entering the loop either Starts the retry (if the first caller rolled back) or finds the entry absent (first caller committed Gone) and re-Starts as the owner of a best-effort verify path. `ws_store.get` returning `WorkspaceNotFound` then commits Gone idempotently.
3. ✅ Tauri `commands/workspace.rs::remove_workspace` — 4-line handler that calls `daemon_ipc::remove_workspace(repo, name)`. The helper consumes `request_typed` for the connect/handshake/extract-ack boilerplate. This is the template every future batch copies.
4. ✅ CLI `shardctl workspace remove` routes through the daemon RPC via `tokio::runtime::Runtime::new()` per-command (matches the existing `stop_daemon` pattern).
5. ✅ `spawn_topology_poke` removed from the Tauri remove path. The daemon broadcasts `ChangeKind::WorkspaceRemoved` via the subscribe channel, which `run_subscribe_loop` maps to the new `ControlFrame::WorkspaceRemoved` wire frame.
6. ✅ **Bonus fix (not in the original plan):** added `check_can_mutate` to `handle_spawn` so SpawnSession rejects a target that's currently being deleted. Without this, D5 step 1 was a promise the code didn't keep — a new session could land after `Deleting` was acquired but before the bound-session snapshot was taken.

**Test plan — all 8 tests pass (exceeds the 6 required):**

- ✅ Happy path (`remove_workspace_happy_path`)
- ✅ Live-session SHA-55 repro — uses `test_inject_live_session` + a fake supervisor that responds to `Resume+StopGraceful` with a `Status{code:0}` frame, proving the stop-and-wait drain works without a real supervisor binary (`remove_workspace_with_live_session`).
- ✅ Watcher-held (`remove_workspace_while_watcher_live`) — touches a file to force a debouncer event before `RemoveWorkspace` fires.
- ✅ Concurrent-mutation rejection (`concurrent_mutation_blocked_during_delete`) — uses the lifecycle API directly to assert the state machine. Plus `spawn_session_blocked_during_active_delete` fires an actual `SpawnSession` RPC during a `Deleting` state and asserts the typed error (covers the D5 gate end-to-end).
- ✅ Idempotency (`two_parallel_removes_both_ok`) — parallel `RemoveWorkspace`, both return Ack, only one does work.
- ✅ Partial failure + retry (`partial_failure_marks_broken_then_retry_completes`) — `FlakyGitOps` fails the first `worktree_remove`, state becomes `Broken`, DB row preserved, retry via second `RemoveWorkspace` completes cleanup.
- ✅ **is_base=true (D11) (`remove_base_workspace_preserves_checkout_dir`)** — RemoveWorkspace on a base workspace deletes the DB row but leaves the checkout directory (and its contents) on disk.

**Codex review round (Phase 1 landing):** all five findings addressed before commit:

| # | Severity | Finding | Resolution |
|---|---|---|---|
| 1 | High | `SpawnSession` bypassed the lifecycle gate | Added `state.lifecycle.check_can_mutate` at the top of `handle_spawn` |
| 2 | Medium | `BeginDelete::NotFound → register + retry` TOCTOU | Dropped the `NotFound` variant — `begin_delete` now atomically inserts `Deleting` when the entry is absent |
| 3 | Medium | Joined deletes returned `Ack` even when the first caller rolled back | Handler loops on `AlreadyDeleting` — after the notifier fires, re-enter `begin_delete` which either Starts the retry or finds the entry absent and Starts as a verify-only owner |
| 4 | Medium | Live sessions dropped from the registry even when stop failed | `sessions.remove` now only fires on successful `stop_session_and_wait`; on failure, the handler commits `Broken` and returns the error so the retry path can see the still-bound session |
| 5 | Low | Live-session test's `creation_time=0` could terminate the test process if the force-kill path ever regressed | Changed to `creation_time=1` (obviously non-matching) so `force_kill_pid_checked` refuses on mismatch |

**Deferred (cost/benefit didn't warrant a fix now):**

- Coverage for `state.monitor.get() == None` — the handler's `if let Some(monitor) = state.monitor.get()` is a defensive no-op. Exercising it would require restructuring the `OnceLock` into a `RwLock` just for tests; the production control loop always initializes the monitor before accepting the first client. Noted as a known gap in the handler doc comment.
- Subscribe-client backpressure (Risk 2) — adding `WorkspaceRemoved` didn't materially change the risk profile; `run_subscribe_loop` already handles `broadcast::RecvError::Lagged` with a resync. Defer to Phase 6 per the original plan.

### Phase 2 — Rest of Batch B ✅ LANDED

`CreateWorkspace`, `ListWorkspaces`, `ListBranchInfo`. Follows the Phase 1 template plus a new per-repo mutation mutex from the Codex round-2 finding.

1. ✅ `ControlFrame::CreateWorkspace { repo, name, mode, branch }` / `CreateWorkspaceAck { workspace }`, `ListWorkspaces { repo }` / `WorkspaceList { items }`, `ListBranchInfo { repo }` / `BranchInfoList { branches }` (type bytes `0x94–0x99`). `PROTOCOL_VERSION` bumped 3 → 4. Wire encoders added: `write_opt_str`, `write_workspace`, `mode_to_byte`; plus 14 new roundtrip tests.
2. ✅ `WorkspaceWithStatus` moved from `shard-app/src/commands/workspace.rs` to `shard-core/src/workspaces.rs` (with `PartialEq/Eq` derives on `Workspace` / `BranchInfo`) so the enriched shape can cross the wire without a duplicate type.
3. ✅ `daemon.rs::handle_create_workspace` runs the D5 workflow: resolve effective name via the new `WorkspaceStore::resolve_workspace_name` helper, `check_can_mutate` against the resolved name, `ws_store.create`, `register_active`, `poke_topology` (reuses `ChangeKind::State(repo)` per D10). `handle_list_workspaces` joins DB + monitor-cached `RepoState`. `handle_list_branch_info` delegates to `WorkspaceStore::list_branch_info` (on-demand git, deliberately not monitor-cached; the wizard wants fresh data).
4. ✅ `DaemonState::repo_mutation_locks` + `acquire_repo_mutation_lock` — per-repo `Arc<tokio::sync::Mutex<()>>` held across both `handle_create_workspace` and `handle_remove_workspace` critical sections. Closes the Codex round-2 race where a concurrent `RemoveWorkspace` on an as-yet-absent name could ack during a `CreateWorkspace` committing the same row.
5. ✅ Tauri commands (`shard-app/src/commands/workspace.rs`) now 4-line translators over `daemon_ipc::create_workspace` / `list_workspaces` / `list_branch_info`. The local `WorkspaceWithStatus` struct was removed in favor of the shard-core one (JSON shape preserved via `#[serde(flatten)]`).
6. ✅ CLI (`shard-cli/src/cmd/workspace.rs`) routes create/list through the daemon via a shared `run_daemon_rpc` helper — collapses the connect/handshake boilerplate across three call sites. `workspace info` stays direct (D4: immutable lookup).

**Test plan — 12 Phase 2 integration tests (exceeds the 5 originally planned):**

- ✅ `create_workspace_happy_path` — creates, returns populated `Workspace`, registers Active.
- ✅ `create_workspace_blocked_during_delete` + `create_workspace_blocked_on_broken_name` — gate rejects Deleting / Broken targets.
- ✅ `create_workspace_blocked_with_implicit_name` — regression guard for the Codex round-1 race where implicit-name callers bypassed the gate.
- ✅ `create_workspace_duplicate_name_errors` — DB unique-constraint surfaces via `Error { message }`.
- ✅ `create_after_delete_succeeds` — lifecycle entry cleared on `commit_gone`; proves workspace names recycle (via `ExistingBranch` on the preserved git branch, since Remove intentionally leaves the branch object — that's pre-existing behavior, not Phase 2 scope).
- ✅ `list_workspaces_returns_created_entries` / `list_workspaces_empty_repo_returns_empty` / `list_workspaces_unknown_repo_errors` — list surface.
- ✅ `list_branch_info_reflects_head_and_new_branches` — branch enumeration with `checked_out_by` occupancy.
- ✅ `concurrent_creates_on_same_repo_both_succeed` — per-repo mutex deadlock guard.
- ✅ `concurrent_create_and_remove_reach_consistent_state` — asserts DB and filesystem agree post-race (no orphan rows or dirs).

**Codex review rounds (Phase 2 landing):**

| Round | Severity | Finding | Resolution |
|-------|---|---|---|
| 1 | Medium | `handle_create_workspace` skipped `check_can_mutate` when `name: None`; the effective name is resolved by `WorkspaceStore::create` from HEAD/branch so implicit-name callers bypassed the gate | Extracted pure `WorkspaceStore::resolve_workspace_name` (side-effect-free except for a `git symbolic-ref` lookup); handler now resolves the name first and gate-checks against the resolved form unconditionally |
| 1 | Low | `handle_list_branch_info` comment claimed "monitor-aware" but the code does on-demand git | Rewrote the comment — the RPC is on-demand git routed through the daemon for serialization; the wizard wants fresh data so caching against the monitor's tick would be wrong |
| 2 | Medium | `CreateWorkspace` vs `RemoveWorkspace` on the same repo could interleave: Create's gate check sees absent, then Remove's `begin_delete` also sees absent, commits Gone, acks; Create continues and commits the row. Remove's ack is misleading | Added per-repo mutation mutex (`DaemonState::repo_mutation_locks` + `acquire_repo_mutation_lock`); both handlers hold the guard across their critical section. Per-repo (not per-workspace) for simplicity; single-user app means the coarser grain costs nothing |
| 3 | — | None | Converged |

**Phase 2 discoveries the plan didn't anticipate:**

- `WorkspaceWithStatus` needed to move to `shard-core` so both the daemon and the Tauri backend could share the type. The frontend JSON shape is preserved via `#[serde(flatten)]` on the embedded `Workspace` field — no TypeScript changes required.
- The plan's "`CreateWorkspace` should consult `check_can_mutate`" wording hid a subtle gap: the resolved workspace name is only visible after `WorkspaceStore::create` computes it, so the handler needed its own name-resolution step to gate-check uniformly. Factored `resolve_workspace_name` to avoid logic duplication with `create`.
- The per-repo mutation mutex fills a gap the lifecycle gate alone couldn't close: `begin_delete` + `check_can_mutate` are independently atomic, but the DB-plus-git workflow between them isn't, so concurrent operations on an absent name could both see "nothing to block" and both proceed. The mutex linearizes Create and Remove on the same repo.
- The `create_after_delete_succeeds` scenario exposed a pre-existing quirk: `WorkspaceStore::remove` (now routed through `RemoveWorkspace`) intentionally leaves the git branch object behind when the worktree is removed. Re-creating under the same name therefore requires `WorkspaceMode::ExistingBranch` to re-use the dangling branch. This is a separate UX concern worth a Linear issue if it surfaces in user testing; not in Phase 2 scope.

**Phase 3 starter kit — things a next session should know:**

- **Template is now stable:** `daemon_ipc.rs::create_workspace` (shard-app), `cmd/workspace.rs::create_via_daemon` (shard-cli), and the paired `handle_create_workspace` in `daemon.rs` are the reference shapes. Copy verbatim for Batch A.
- **Repo-scoped mutations share `acquire_repo_mutation_lock`** — `RemoveRepo` will need it held across workspace-by-workspace teardown. For `AddRepo`, the "repo" doesn't exist yet so the lock just serializes duplicate-alias races.
- **`FindSessionById` walks all DBs** and should hold a global (not per-repo) lock; defer that detail until the handler is written.
- **`poke_topology(None)` still exists** for full-reload needs that `AddRepo`/`RemoveRepo` might use — check if a scoped alternative is cleaner before reusing it.
- **`spawn_topology_poke` is gone** — Phase 3 deleted it once `commands/repo.rs` stopped calling it. The daemon owns all topology pokes now. Don't reintroduce a client-side poke helper; use an RPC.
- **Protocol version v5 is live** (bumped in Phase 3) — older clients hard-fail the handshake (per D7). If you bump to v6, coordinate the shard-app rebuild and any running CLI tooling.

### Phase 3 — Batch A ✅ LANDED

Repo CRUD: `AddRepo`, `RemoveRepo`, `SyncRepo`, `ListRepos`. `FindSessionById` deferred to Phase 4 per D8 — its natural callers are all session-facing, and migrating the read without its corresponding mutation would split the source of truth mid-batch.

1. ✅ `ControlFrame::AddRepo { url, alias }` / `AddRepoAck { repo }`, `RemoveRepo { alias }` / `RemoveRepoAck`, `SyncRepo { alias }` / `SyncRepoAck`, `ListRepos` / `RepoList { repos }` (type bytes `0x9A–0xA1`). `PROTOCOL_VERSION` bumped 4 → 5. Added `write_repository` / `read_repository` wire encoders plus 8 roundtrip tests. `Repository` gained `PartialEq + Eq` so the frame enum keeps its existing derives.
2. ✅ `daemon.rs::handle_add_repo` — auto-creates the default-branch base workspace just like the old direct path. Holds the per-repo mutation lock: eagerly when the caller supplies an alias, after `RepositoryStore::add` derives one otherwise. Auto-workspace failures are logged but don't abort the add.
3. ✅ `daemon.rs::handle_remove_repo` — the full cascade: per-repo mutation lock → lifecycle-gate every workspace → stop + drain every bound session → `MonitorCommand::DropRepo` (new; drops the whole debouncer at once rather than per-workspace) → `RepositoryStore::remove` → `lifecycle.clear_repo(alias)` → topology poke. Partial-failure policy preserves the original guard semantics: rollback to Active if nothing on disk was touched, commit Broken once store.remove has executed. Idempotent on unknown aliases.
4. ✅ `daemon.rs::handle_sync_repo` takes the per-repo mutation lock so a concurrent RemoveRepo can't yank the repo out. `handle_list_repos` is lockless (read-only path).
5. ✅ `MonitorCommand::DropRepo { alias, ack }` added to `workspace_monitor.rs` with ack-after-drop semantics matching `DropRepoWorkspace`. Rebuild step omitted — the entire repo is going away, so there's nothing to re-watch.
6. ✅ `LifecycleRegistry::clear_repo(alias)` drops every entry belonging to a repo and fires any `Deleting` completion notifiers so joiners unblock. Used by `handle_remove_repo` as belt-and-suspenders against phantom state from a partially-failed prior attempt.
7. ✅ Tauri `commands/repo.rs` collapsed to thin translators over `daemon_ipc::{add_repo, remove_repo, sync_repo, list_repos}`. `spawn_topology_poke` deleted — it had exactly one consumer left (the old `add_repo`/`remove_repo` path) and the daemon now emits its own topology pokes internally. `daemon_ipc.rs` module doc trimmed accordingly.
8. ✅ CLI `shard-cli/src/cmd/repo.rs` rewritten to route every subcommand through `run_daemon_rpc` (shape lifted from `cmd/workspace.rs`). Duplicate `ShardPaths::new` + `RepositoryStore::new` wiring removed; CLI no longer touches the stores directly for repo ops.
9. ✅ Test harness gained `TestHarness::create_bare_checkout` — the existing `setup_local_repo` was too eager (it registered the repo via `RepositoryStore::add`, which prevents tests from exercising `AddRepo` RPC end-to-end). `setup_local_repo` now delegates to `create_bare_checkout` to avoid drift.

**Test plan — 11 Phase 3 integration tests + 8 wire roundtrips (19 new):**

- ✅ `add_repo_happy_path_local` — RPC ack, DB row present, lifecycle entry for base workspace Active.
- ✅ `add_repo_without_explicit_alias_derives_from_path` — alias derivation via `git::default_alias` survives the daemon hop.
- ✅ `add_repo_duplicate_alias_errors` — DB unique-constraint surfaces as `Error` frame.
- ✅ `list_repos_empty_initially` / `list_repos_returns_added_entries` — list round-trip.
- ✅ `sync_repo_unknown_alias_errors` — unknown alias returns `Error`, not a vacuous Ack.
- ✅ `remove_repo_happy_path` — row gone, `.shard/` cleaned, **local checkout preserved** (D11).
- ✅ `remove_repo_cascades_workspaces` — two extra workspaces created via CreateWorkspace RPC are both cleaned on RemoveRepo.
- ✅ `remove_repo_unknown_alias_is_idempotent` — second RemoveRepo returns Ack.
- ✅ `remove_repo_stops_live_session` — uses the Phase 1 fake-supervisor pattern (Resume+Stop → Status frame) to prove stop-and-drain fires correctly in the cascade.
- ✅ `remove_repo_blocks_concurrent_create_workspace` — parallel Create + Remove on the same repo reach a consistent state: either Create wins (workspace exists in cascaded remove) or Remove wins (Create returns Error). The illegal outcome (Create Ack'd + Remove Ack'd + workspace leaked) is precisely what the per-repo mutation lock prevents.

**Phase 3 discoveries the plan didn't anticipate:**

- Per-workspace `DropRepoWorkspace` would have required N round-trips + N watcher rebuilds for an N-workspace repo. Adding `DropRepo` is one command + one rebuild skipped; simpler than iterating through `handle_remove_workspace` per workspace. Same ack-after-drop contract.
- `AddRepo` can't take the per-repo mutation lock until it knows the alias — when the caller omits `alias`, `git::default_alias` runs inside `RepositoryStore::add`. The handler acquires the lock eagerly when the caller supplies an alias, then re-acquires post-add for the auto-workspace step. A concurrent AddRepo racing the same derived alias is caught by the DB UNIQUE constraint rather than the mutex, but that's an acceptable floor (single-user app, two-concurrent-AddRepo on the same alias is essentially theoretical).
- `LifecycleRegistry::clear_repo` was not in the plan. The original sketch assumed per-workspace `commit_gone` during the cascade would handle every entry, but there's a subtle gap: a prior failed `RemoveWorkspace` could leave a `Broken` entry for a workspace whose DB row was already gone. That entry would outlive `RemoveRepo` without an explicit sweep. `clear_repo` fires every stale completion notifier (joiners unblock) and returns the cleared keys for telemetry.
- `spawn_topology_poke` had exactly one live consumer (`commands/repo.rs`). Phase 3 removing that call site let us delete the helper entirely — one fewer fire-and-forget path to reason about. The daemon owns all topology pokes now.
- The plan's starter-kit note "`FindSessionById` walks all DBs and should hold a global (not per-repo) lock" was the crux of moving it to Phase 4: the Tauri session commands that call `find_by_id` (6 call sites) have to migrate together or the handler winds up re-opening DBs the session-lifecycle path is about to mutate. Cleaner to ship it with Batch C.
- Fake supervisor frame byte: the integration test's StopGraceful tag is `0x02` (not `0x04`); Status reply is `0x05`. Mirrored the Phase 1 `remove_workspace` test's encoder rather than re-deriving — worth noting for the next session that rolls a new fake-supervisor test.

**Codex review round 1 (Phase 3 landing):** 3 findings, all applied:

| # | Severity | Finding | Resolution |
|---|---|---|---|
| 1 | High | `handle_remove_repo` snapshots `state.sessions` once, but `handle_spawn` inserts into the live registry only *after* a 10s ready-file wait. An in-flight spawn during RemoveRepo is invisible to the snapshot, so the supervisor keeps its PTY CWD open while `RepositoryStore::remove` tries to tear down the tree — reopening the SHA-55 class via a different vector | Restructured `handle_spawn` into two phases: the per-repo mutation lock is held across gate re-check + DB row + supervisor spawn + `state.sessions.insert`; the 10s ready wait runs *outside* the lock. On ready-wait failure the handler removes the registry entry and marks the DB row `failed` to avoid ghost sessions |
| 2 | Medium | Alias-less `handle_add_repo` committed the DB row via `RepositoryStore::add` *before* acquiring the per-repo mutex. A concurrent `RemoveRepo`/`SyncRepo`/`CreateWorkspace` for that derived alias could slip into the gap; the handler then re-acquired and returned `AddRepoAck` for a repo that no longer existed | Extracted pure `RepositoryStore::resolve_alias(url, alias)` so the handler resolves the effective alias first (no I/O beyond URL parsing), acquires the lock against the resolved name, then calls `add`. Single unified path, no split acquisition |
| 3 | Low | On `repo_store.remove` failure, `handle_remove_repo` committed Broken guards and returned Error without poking the monitor. `DropRepo` had already eagerly cleared `RepoState` + watcher, so subscribers saw the repo as silently missing until the 30s reconcile | Added `monitor.poke_topology(Some(alias))` to the failure path so `handle_topology_change` reloads from the DB (row still present) and emits a fresh `ChangeKind::State(alias)` to subscribers |

Two regression tests added for Fixes 2 and 3 in `repo_crud.rs`:
- `add_repo_concurrent_same_alias_serializes` — two parallel AddRepos for the same alias yield exactly one Ack + one Error.
- `add_repo_then_remove_repo_is_atomic_against_concurrent` — DB and filesystem always agree on repo presence post-race.

Fix 1 is not directly testable via the in-process harness (SpawnSession spawns a real supervisor binary), but is covered by the invariant that `handle_spawn` now holds the per-repo mutation lock — which `remove_repo_blocks_concurrent_create_workspace` and `concurrent_create_and_remove_reach_consistent_state` already exercise from the RemoveRepo / CreateWorkspace side.

**Codex review round 2 (Phase 3 polish):** 2 findings, both applied:

| # | Severity | Finding | Resolution |
|---|---|---|---|
| 1 | High | Round-1 Fix 1's ready-timeout path removed the live-registry entry and marked the DB row failed, but did **not** kill the supervisor. A slow-to-start supervisor could still bind its pipe and flip the DB row back to `running` after we marked it `failed`, leaving a zombie session that `StopSession`/`RemoveRepo` couldn't find (both key off the live map) and that daemon shutdown would leak because graceful shutdown strips `KILL_ON_JOB_CLOSE` | Introduced a three-variant `ReadyOutcome` enum. `Timeout` now calls `shard_supervisor::process_windows::force_kill_pid_checked(pid, creation_time)` before unregistering. The creation_time guard refuses to kill a recycled PID. `Died` skips the kill (no process left). Both paths still unregister and mark the DB row failed |
| 2 | Low | `add_repo_then_remove_repo_is_atomic_against_concurrent` used `Some("atomic")` so it never exercised the alias-derivation path that Fix 2 was supposed to address | Renamed `add_repo_alias_less_then_remove_is_atomic` and switched the call to `alias: None`. The derived alias is computed from the checkout's trailing path component (matching `git::default_alias` for local paths) |

**Codex review round 3:** converged — no new findings.

### Phase 4 — Batch C ✅ LANDED

Session lifecycle tail (`RemoveSession`, `RenameSession`, `DetachSession`) plus `FindSessionById` (carried over from Batch A — its callers are all session-facing, so it migrates with the tail, consistent with D8). `SpawnSession` / `StopSession` / `ListSessions` already existed; left unchanged per the plan table.

1. ✅ `ControlFrame::RemoveSession { repo, id }` / `RemoveSessionAck`, `RenameSession { repo, id, label }` / `RenameSessionAck`, `DetachSession { id }` / `DetachSessionAck`, `FindSessionById { prefix }` / `FoundSession { repo, session }` (type bytes `0xA2–0xA9`). `PROTOCOL_VERSION` bumped 5 → 6. Added `write_session` / `read_session` wire codec with `write_opt_u32` / `write_opt_i32` / `write_opt_u64` helpers; `Harness` goes over the wire via its `Display` impl and decodes back through `FromStr` (unknown strings degrade to `None`, matching the DB tolerance in `row_to_session`). `Session` gained `PartialEq + Eq`. 7 new roundtrip tests including an unknown-harness-drops regression guard.
2. ✅ `daemon.rs::handle_remove_session` — two-stage guard: the in-memory live-session registry rejects ids whose supervisor is still bound (covers the gap where the DB row is `stopped` but the supervisor hasn't released its CWD yet); `SessionStore::remove` then enforces the DB-status guard (`running`/`starting` rejected). Broadcasts `ChangeKind::Sessions(repo)` on success so subscribers invalidate their cached list.
3. ✅ `daemon.rs::handle_rename_session` — pure DB label update under the per-repo mutation lock. Broadcasts `ChangeKind::Sessions(repo)`.
4. ✅ `daemon.rs::handle_detach_session` — validates the id resolves via `find_by_id` and returns Ack. Terminal I/O stays on the direct Tauri ↔ supervisor pipe (migration non-goal); this RPC is the daemon-visible hook that gives CLI and GUI detach flows an identical round-trip, useful for telemetry and as the seam for future multi-window subscription work.
5. ✅ `daemon.rs::handle_find_session_by_id` — walks the global index lockless. DB reads under WAL + busy_timeout are safe against concurrent writes, so no global mutex is needed; the plan's "global lock" note reduced to a documented no-op.
6. ✅ `RemoveSession` / `RenameSession` take the per-repo mutation lock. Serializes against `RemoveRepo` on the same repo so the DB isn't yanked out mid-update. `DetachSession` / `FindSessionById` are lockless reads.
7. ✅ Tauri `commands/session.rs` rewired — `remove_session`, `rename_session`, `detach_session` now 3-line translators over `daemon_ipc::{find_session_by_id, remove_session, rename_session, detach_session}`. `stop_session` / `attach_session` gained the `daemon_ipc::find_session_by_id` lookup; their local `SessionStore::find_by_id` calls disappeared. `handle_supervisor_frame`'s supervisor-initiated DB update (Status frame reaction) stays direct per D3 — it's part of the supervisor ↔ client pipe, not a client-initiated mutation.
8. ✅ CLI `shard-cli/src/cmd/session.rs` — `attach`, `stop`, `remove` route their lookups through a shared `find_session_via_daemon` helper (shape lifted from `cmd/repo.rs::run_daemon_rpc`). `remove` additionally sends `RemoveSession` via the daemon; the old `SessionStore::remove` call site is gone.
9. ✅ Test harness gained `TestHarness::setup_terminal_session(repo, ws, status)` — inserts a DB row via `SessionStore::create` then flips it to a terminal status via `update_status`. Lets Phase 4 tests target rows without spawning a real supervisor.

**Test plan — 14 Phase 4 integration tests + 7 wire roundtrips (21 new):**

- ✅ `remove_session_happy_path` — DB row and session dir both gone after Ack.
- ✅ `remove_session_refuses_live_registry_entry` — injects an `exited` DB row into the live registry; the handler's in-memory guard blocks the remove with a `still live` error, proving the two-stage guard catches the gap a DB-only check would miss.
- ✅ `remove_session_refuses_running_status` — DB-status guard rejects `running` directly, no live registry entry required.
- ✅ `remove_session_unknown_id_errors` — absent id surfaces as `Error`.
- ✅ `rename_session_sets_and_clears_label` — both set and clear paths round-trip through the DB.
- ✅ `rename_session_unknown_repo_errors` — missing repo_db → `Error`.
- ✅ `detach_session_happy_path` — Ack with unchanged DB state (probe semantics).
- ✅ `detach_session_unknown_id_errors` — absent id → `Error`.
- ✅ `find_session_by_id_exact_match` / `find_session_by_id_prefix_match` / `find_session_by_id_walks_all_repos` / `find_session_by_id_unknown_errors` — full-id, 8-char prefix, cross-repo walk, and absent-id paths.
- ✅ `find_then_remove_by_prefix` — end-to-end CLI-equivalent flow: `FindSessionById` returns `(repo, session)`, caller uses the resolved repo + full id for `RemoveSession`.
- ✅ `remove_session_serializes_against_remove_repo` — parallel `RemoveRepo` + `RemoveSession` on the same repo; both legal outcomes (RemoveRepo wins / RemoveSession wins) are accepted, the illegal "both Ack with leaked DB row" is what the per-repo mutation lock prevents.

**Phase 4 discoveries the plan didn't anticipate:**

- The plan's starter-kit note said `FindSessionById walks all DBs and should hold a global (not per-repo) lock`. In practice, the read is safe lockless under WAL mode — a global lock would serialize the index walk against every other read, which is an anti-pattern for a pure-read path. Documented the "no global lock needed" reasoning in the handler doc comment. Writes that could move a row between repos are already serialized by their per-repo mutation lock, so an index walk can never see an inconsistent half-state.
- `handle_remove_session` needs **two** guards: the DB-status guard inside `SessionStore::remove` (rejects `running` / `starting`) *and* an in-memory live-registry guard. A supervisor whose fast-tick detection lagged could leave the DB `stopped` while the supervisor's actual CWD is still held — letting `remove_dir_all` succeed in that state would race the supervisor's log writer. The in-memory pre-check closes that window.
- `DetachSession` is the least load-bearing RPC in the plan. The actual attach/detach connection management is intrinsically Tauri-backend-local (which process abandoned which pipe?), so the daemon RPC is effectively a probe. Kept it per the plan for symmetry with CLI-initiated flows and to have a seam for future multi-window subscription work, but scoped the handler to `find_by_id` + Ack — no daemon state to manage.
- `Harness` Display/FromStr round-trip means we don't need a `mode_to_byte`-style wire tag for harness. An unknown-harness string on the wire decodes as `None`, matching the DB's `row_to_session` tolerance. Added a regression test (`roundtrip_found_session_unknown_harness_drops`) so a future harness addition that forgets to update `FromStr` doesn't silently corrupt decoded rows.
- `handle_supervisor_frame` in the Tauri backend still does a direct `SessionStore::find_by_id` + `update_status` when it sees a `Status` frame on the session pipe. This is **not** a migration miss — it's a reaction to a supervisor-initiated signal on the direct pipe (D3: activity + terminal I/O stay direct) and the supervisor writes its own final status from `cmd/session.rs::serve` anyway, so losing daemon availability mid-exit doesn't break status updates.

**Codex review round 1 (Phase 4 landing):** 3 findings, all applied:

| # | Severity | Finding | Resolution |
|---|---|---|---|
| 1 | Medium | `handle_spawn` ready-timeout cleanup unconditionally unregisters the live-session entry and marks the DB row `failed`, even when `force_kill_pid_checked` returns Err. A still-alive supervisor invisible to `state.sessions` would let a follow-up `RemoveSession` / `RemoveRepo` race `remove_dir_all` against the supervisor's open handles — reopening the SHA-55 class via the spawn-timeout vector | After force-kill returns Err, the handler re-checks `is_alive(pid)`. If the process is still alive, the live-registry entry is **kept** and the DB row stays `starting` — the handler returns `Error` so the operator can issue an explicit `StopSession`. Only a confirmed-dead supervisor proceeds to unregister + mark `failed` |
| 2 | Medium | `SessionStore::remove` deletes the DB row first, then swallows `remove_dir_all` errors — a leaked file handle leaves the directory orphaned with no DB row pointing at it for retry, and the RPC acks success | Order swapped: filesystem cleanup runs first and propagates errors. The DB row is only deleted after the directory is gone, so a failed cleanup leaves the row intact for a retry. `RemoveSession` now returns `Error` on cleanup failure rather than a misleading Ack |
| 3 | Low | `SessionStore::rename` issued a bare `UPDATE ... WHERE id = ?` and ignored the affected rowcount, so renaming an unknown id under a known repo silently returned `RenameSessionAck` — broke contract symmetry with `RemoveSession` | Check `rows_affected() == 1`; return `SessionNotFound` on `0`. New regression test `rename_session_unknown_id_errors` covers the path |

**Phase 4 follow-up cleanup applied alongside the Codex fixes:**

- Tauri `detach_session` no longer calls `daemon_ipc::detach_session` — its result was discarded with `let _ = ...` and the very next line did the same `find_session_by_id` lookup. Collapsed to a single `find_session_by_id` round-trip. The daemon `DetachSession` RPC + handler stay registered (covered by the wire roundtrip test) so the protocol seam for future multi-window subscription work is preserved, but no caller invokes it today. **Open question for Codex round 2:** keep the unused RPC as a forward-compat seam, or elide it and re-add when the multi-window work needs it?

**Deferred:**

- DB-backed session listing (`session.list` per D4) stays direct. The plan's Batch C table explicitly says `ListSessions: existing, no change` — the existing `LiveSessionInfo` frame covers the live-registry view, and the Tauri / CLI DB-backed `list_sessions` functions don't materially change the SHA-55 class risk model. If the UI grows consistency issues between the DB list and the event stream, revisit in Phase 6.

### Phase 5 — Batch D

Hooks install centralization. Small.

### Phase 6 — Subscription surface hardening

Add bounded mpsc per subscribe client in the daemon (agent A flagged the current subscribe write path has no per-client backpressure). Also add `WorkspaceRemoved`, `SessionCreated`, etc. if Q3 says finer events.

## Risks

1. **Windows handle ordering is subtle.** Phase 1's whole value depends on the monitor actually dropping the `ReadDirectoryChangesW` handle before the `RemoveDirectoryW` call. Tests must assert post-delete existence on Windows specifically. Adding the integration test harness is a pre-req.

2. **Daemon becomes single point of failure for mutations.** If the daemon crashes, nothing can be created or deleted until restart. Today the Tauri backend could limp along for reads. Mitigation: the existing `connect_with_retry` already covers daemon restart; daemon startup is <1s.

3. **Lock contention in daemon mutation handlers.** If every mutation serializes on the same repo lock, concurrent users (CLI + GUI) could stall each other. Real-world concurrency is low (one user), but worth per-repo locking rather than global.

4. **Monitor command channel backlog.** If the monitor is mid-reconcile when a `DropRepoWorkspace` arrives, the mutation RPC blocks until the reconcile finishes. A priority queue does **not** help — it can't preempt an in-flight iteration. Real mitigations, in order of preference: (a) make the reconcile chunked and cooperatively cancellable so it yields between workspaces and can abort early when a command arrives; (b) move the expensive git walk off the monitor's hot actor path onto a worker task, so the actor loop stays responsive to commands; (c) hold monitor state in an `Arc` and let commands run against a snapshot. (a) is the cheapest and most aligned with the existing structure.

## Codex review checkpoints

1. **After this doc stabilizes (done).** Review of D1–D11 revealed: D5 "atomic" was under-specified (concurrent handlers + `StopSession` not waiting for exit), actor pattern in D9 confirmed correct (ack semantics added), missing per-object lifecycle state machine (now D12), manual-only Phase 1 test plan was insufficient. All incorporated above.
2. **After Phase 0 lands — done as part of the Phase 1 landing review (combined).** `MonitorCommand` ordering confirmed correct; `stop_session_and_wait` with `force_kill_pid_checked` fallback validated; `LifecycleRegistry` simplified (no `NotFound` variant — atomic insert-on-absent).
3. **After Phase 1 lands and SHA-55 is fixed — done.** Five findings, all addressed: see the Phase 1 section above.
4. **After Phase 2 lands — done.** Three findings, all addressed (see Phase 2 table above): implicit-name gate bypass (round 1), misleading `list_branch_info` comment (round 1), Create-vs-Remove serialization race (round 2). Round 3 converged.

5. **After Phase 3 lands — round 1 done.** Three findings, all applied (see Phase 3 table above):
   - HIGH: `handle_spawn` now holds the per-repo mutation lock across the gate-to-register window so `RemoveRepo` can't miss an in-flight spawn.
   - MEDIUM: `handle_add_repo` resolves the alias up front via `RepositoryStore::resolve_alias` and acquires the lock BEFORE `add`, closing the alias-less race.
   - LOW: `handle_remove_repo` now pokes the monitor on `store.remove` failure so subscribers don't see a silently-missing repo.

   `DropRepo` ack semantics and `LifecycleRegistry::clear_repo` confirmed correct — no changes required. `FindSessionById`'s global lock semantics deferred to Phase 4 review along with the rest of Batch C.

6. **After Phase 4 lands — round 1 done.** Three findings, all applied (see Phase 4 table above):
   - MEDIUM: `handle_spawn` ready-timeout no longer unregisters when force-kill fails — keeps the live-registry entry so RemoveSession/RemoveRepo can still see and stop the supervisor.
   - MEDIUM: `SessionStore::remove` cleans the directory before deleting the DB row and propagates fs errors — no more silent half-removes.
   - LOW: `SessionStore::rename` checks rows_affected and returns `SessionNotFound` on a no-op UPDATE.

   `handle_find_session_by_id`'s lockless walk confirmed correct (WAL + busy_timeout + per-repo write lock). Open for round 2: the dead-weight `DetachSession` RPC — keep as a forward-compat seam for multi-window or elide and re-add later? Also out-of-scope but real: Tauri's `stop_session` still bypasses the daemon's `StopSession` RPC; track as a separate follow-up.
