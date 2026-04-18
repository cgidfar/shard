# Daemon-as-Broker Migration Plan

**Status:** Phase 0 + Phase 1 landed on branch `daemon-broker-migration`. Codex-reviewed; five findings applied. 71 tests pass, zero warnings. Ready for Phase 2.
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

### Phase 2 — Rest of Batch B

`CreateWorkspace`, `ListWorkspaces`, `ListBranchInfo`. Follows the Phase 1 template.

**Phase 2 starter kit — things a next session should know:**

- **Template to clone per RPC:** `daemon_ipc.rs::remove_workspace` + `commands/workspace.rs::remove_workspace` (shard-app) and `cmd/workspace.rs::remove_via_daemon` (shard-cli). Both are ~4-line translators over `DaemonConnection::request_typed`. This shape should be copied verbatim.
- **`CreateWorkspace` needs to register with the lifecycle gate** after successful creation (`state.lifecycle.register_active(repo, name)`). Otherwise subsequent `RemoveWorkspace` on the newly-created workspace falls through the "absent" branch of `begin_delete` — works, but cedes the pre-mutation guarantees from `check_can_mutate`.
- **`CreateWorkspace` should also consult `check_can_mutate`** — refuse creation if the target name is in `Deleting` or `Broken`. Mirror the pattern added to `handle_spawn` in Phase 1.
- **Reads (ListWorkspaces, ListBranchInfo):** per D4 these migrate alongside the mutation. No lifecycle gate needed for reads, just straightforward `request_typed` + new wire frames. `ListWorkspaces` should return `Vec<WorkspaceWithStatus>` enriched from the monitor's `RepoState` snapshot — the Tauri backend currently does this enrichment locally (`crates/shard-app/src/commands/workspace.rs::list_workspaces`); move that into the daemon handler.
- **Test-harness helpers to add:** `setup_base_workspace`, plus a `fake_supervisor` utility if more tests need live sessions (the current `spawn_fake_supervisor` in `remove_workspace.rs` is a good starting point to extract).
- **Unused-but-waiting:** `MonitorHandle::broadcast` is currently only consumed by `RemoveWorkspace`. `CreateWorkspace` will need a parallel `WorkspaceCreated` or `TopologyChanged` path; simplest is reusing the existing coarse `ChangeKind::State(repo)` emit post-create (fires a fresh `StateSnapshot` to subscribers which already contains the new workspace).
- **`WorkspaceStore::remove` legacy path is still compiled and unused by the daemon now** — kept because it may be called from older code. Safe to delete once all batches migrate, but not in scope for Phase 2.
- **Shared mutation-handler helper (deferred Phase 0 item):** after Phase 2 lands two or three mutation handlers, the shape of the shared plumbing (lock-store-broadcast) will be obvious. Extract then, not before.

### Phase 3 — Batch A

Repo CRUD. Independent of workspaces in wire shape but `RemoveRepo` internally calls the workspace remove path.

### Phase 4 — Batch C

Session lifecycle tail (`RemoveSession`, `RenameSession`, `DetachSession`). `SpawnSession` / `StopSession` / `ListSessions` already exist; `ListSessions` may need a small tweak if Q3 lands on finer events.

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
4. **After Phase 2 lands** — review the `CreateWorkspace` gate interaction and the list-read consistency with `StateSnapshot`.
