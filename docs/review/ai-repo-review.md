# AI Repo Review Notes

Date started: 2026-04-26

Scope: full review of the Shard repo, with two outcomes:

1. Correctness findings: bugs, races, lifecycle leaks, data-loss risks, missing tests.
2. Simplification findings: code to delete, ownership boundaries to collapse, duplicated paths, abstractions that make future changes harder.

## Review Posture

- Lead reviewer owns integration and final prioritization.
- Parallel reviewers inspect independent slices and report findings; broad code edits wait until findings are triaged.
- Findings should include exact file/line references where possible.
- Simplification candidates should name the invariant preserved by the deletion/refactor.
- Prefer deletion and consolidation over new abstraction unless the abstraction removes concrete complexity.

## Segments

| Segment | Area | Status | Output |
| --- | --- | --- | --- |
| 1 | Baseline checks and architecture invariants | Done | Current health, build/test status, invariants |
| 2 | `shard-core` state, paths, git, SQLite | Done | Data/filesystem findings, simplification candidates |
| 3 | `shard-transport` protocols | Done | Wire robustness and drift findings |
| 4 | Daemon, lifecycle registry, workspace monitor | Done | Concurrency/process/watch findings |
| 5 | Supervisor and PTY runtime | Light pass | Terminal/process lifecycle findings |
| 6 | CLI command surface | Light pass | UX/API consistency findings |
| 7 | Tauri backend | Done | IPC/task/event lifecycle findings |
| 8 | Frontend | Done | UI state/terminal integration findings |
| 9 | Vendored `portable-pty` and platform boundary | Light pass | Patch/dependency risk |
| 10 | Test strategy and remediation roadmap | Done | Fix order and coverage plan |

## Architecture Invariants To Verify

- Daemon owns all state mutations that can affect SQLite, git worktrees, session process lifecycle, and workspace watchers.
- Tauri frontend never speaks named pipes or touches SQLite/git directly.
- Tauri backend and CLI use the same daemon RPC surface for lifecycle operations.
- Supervisors own one PTY and one session pipe, and do not write SQLite.
- Workspace deletion stops bound sessions, waits for process quiescence, drops watchers, removes filesystem/git state, then deletes DB state.
- Session stop waits for supervisor exit or force-kills with PID-reuse protection before marking the session stopped.
- Reads that remain direct from SQLite are intentional, immutable or low-risk, and documented.
- Every destructive filesystem operation is retryable or leaves enough state for later cleanup.

## Baseline Health

Commands:

- `cargo check --workspace`: passes with no warnings from the reviewed crates.
- `cargo test --workspace --target-dir target\review`: passes when run with permissions that allow named pipes and child processes.
- `npm run build` in `crates/shard-app/frontend`: passes when run with permissions that allow spawning local `esbuild`.

Notes:

- Plain `cargo test --workspace` initially failed because `target/debug/shardctl.exe` was locked by running local `shardctl` processes.
- The sandboxed test run failed at daemon control-pipe startup with `Access is denied`; the elevated run passed.
- `bun` is not installed on this machine, despite `CLAUDE.md` documenting Bun commands. `npm run build` works with the checked-in `package.json`.

## Findings

### High

1. User-controlled repo aliases and workspace names can escape their intended roots.

   `RepositoryStore::resolve_alias` accepts explicit aliases unchanged in `crates/shard-core/src/repos.rs:43`, and `ShardPaths::repo_dir` joins aliases directly under the app data repo root in `crates/shard-core/src/paths.rs:53`. `WorkspaceStore` similarly accepts explicit workspace names unchanged near `crates/shard-core/src/workspaces.rs:603`, before `ShardPaths::workspace_dir_for_repo` joins them under `.shard` or app data in `crates/shard-core/src/paths.rs:101`. On Windows, absolute path components can replace the base and `..` can traverse upward. This affects any later filesystem cleanup using those stored paths.

   Correction from second review: this is worse than "can escape" implies. `Path::join` replaces the base when the joined component is absolute, so an alias such as `C:\Windows\System32` is not rooted under Shard's repo directory at all. Combined with cleanup code that later calls `remove_dir_all` and Finding 3's swallowed cleanup errors, this is an arbitrary-path deletion risk. Fix before any broad simplification work.

   Review target: central identifier validation for repo aliases and workspace names before any path join or DB insert.

2. Session/control frame readers allocate unbounded peer-controlled lengths.

   `crates/shard-transport/src/protocol.rs:128` and `crates/shard-transport/src/control_protocol.rs:575` read a `u32` frame length and allocate that buffer before applying any maximum. The control decoder also trusts embedded counts for vector/map capacity around `crates/shard-transport/src/control_protocol.rs:664`, `:710`, `:781`, `:807`, and `:848`. A local malformed client can force large allocations before validation.

   Review target: explicit max frame size, bounded collection counts, and malformed-frame tests.

3. `RepositoryStore::remove` can report success while cleanup failed.

   `crates/shard-core/src/repos.rs` ignores workspace listing failure, worktree removal failure, worktree prune failure, `.shard` deletion failure, exclude cleanup failure, and repo data directory deletion failure around `:215`, `:220`, `:225`, `:232`, and `:246`, then deletes the index row around `:241`. That can lose the retry handle while leaving worktrees or repo data behind. The daemon owns higher-level `RemoveRepo`, but this destructive store method remains callable and should not silently swallow cleanup failures.

   Review target: split destructive orchestration out of the store or make the store preserve DB state on cleanup failure.

4. CLI `session stop` still has the old direct-stop fallback and writes session state itself.

   `crates/shard-cli/src/cmd/session.rs:259` routes through daemon first, but if that fails it writes `StopGraceful`/`StopForce` directly to the session pipe around `:303`, may terminate child/supervisor PIDs directly around `:321`, then calls `SessionStore::update_status(..., "stopped")` around `:345`. This bypasses the daemon's drain, live-registry cleanup, and PID-reuse guarded force-kill path. It also sends stop as the first frame on the fallback path, matching the class of direct-pipe stop bug documented in `docs/daemon-broker-migration.md`.

   Review target: delete the direct fallback and make CLI stop a daemon-only lifecycle operation, or define an explicit offline recovery command that does not pretend the daemon registry was updated.

5. `StopSession` can race `RemoveRepo` and recreate a deleted repo DB directory.

   `crates/shard-cli/src/cmd/daemon.rs:2024` snapshots the live entry, stops the supervisor, then writes DB status around `:2095` without taking the per-repo mutation lock. A concurrent `RemoveRepo` holds that lock and can delete the repo directory through `RepositoryStore::remove`. If `handle_stop` resumes after that, `db::open_connection` creates the repo DB parent directory in `crates/shard-core/src/db.rs:11` before failing or writing. Final state can be "repo removed from index, empty repo dir/repo.db resurrected".

   Review target: make `StopSession` serialize with repo/workspace destructive mutations or make update paths refuse to create missing repo DBs for status backstops. Treat this as part of the first remediation batch because it shares the same destructive-filesystem hazard surface as root-safe identifier validation.

6. PID reuse protection is incomplete after daemon restart.

   Live session entries store a process creation time, but persisted session rows only store PIDs (`crates/shard-core/src/db.rs:55`, `crates/shard-core/src/sessions.rs:132`). On daemon restart, orphan adoption trusts any alive process with the old PID and records its current creation time (`crates/shard-cli/src/cmd/daemon.rs:3159`, `:3186`). The monitor fast tick also uses PID-only liveness around `crates/shard-cli/src/cmd/workspace_monitor.rs:696`. A reused PID can be adopted into daemon state and later killed as though it were a Shard supervisor.

   Review target: persist supervisor creation time or replace PID-only adoption with a verifiable supervisor identity.

### Medium

1. Control/session frame writers silently truncate oversized lengths.

   The protocol code casts payload lengths/string lengths/command counts to smaller integer widths without checked conversion. Oversized local values can encode corrupt frames instead of returning an error.

2. Partial frame prefixes are treated as clean EOF.

   A short read of the 4-byte length prefix maps to `Ok(None)` in both protocols, making truncated input indistinguishable from clean close.

3. Several `SessionStore` update methods silently succeed for nonexistent rows.

   `rename` checks `rows_affected`, but status/PID/transport update helpers do not. Lifecycle code can therefore believe a DB backstop succeeded when no row was updated.

4. Tauri backend still mutates session status from supervisor frames.

   `crates/shard-app/src/commands/session.rs::handle_supervisor_frame` finds the session and calls `SessionStore::update_status` on `Frame::Status`. That violates the review invariant that daemon owns lifecycle state mutation. It may be harmless as a backstop today, but it should be made explicit or removed after daemon lifecycle ownership is complete.

5. Session list views still read SQLite directly.

   Both CLI and Tauri list sessions by opening stores directly, even though the daemon-broker migration says `session.list` should pair with daemon lifecycle events. This is read-only, but it is a drift point for CLI/GUI equivalence and stale live-state handling.

6. Control-pipe shutdown does not quiesce mutating handlers.

   `state.quitting` gates mutating frames, but it is set by tray quit rather than the control-protocol shutdown path. `Shutdown` aborts accept/monitor tasks without tracking already spawned client handlers, so in-flight or already accepted mutating handlers can continue while the monitor is gone.

   Verification note: plausible but less deeply verified than the other medium findings. Re-check daemon event-loop handler tracking before making this actionable.

7. Tauri terminal attach enables input and resize before backend attach completes.

   `frontend/src/lib/terminal.ts` fires `attachSession()` without awaiting it, then immediately enables `writeToSession()` and sends initial `resizeSession()`. A fast keystroke or initial resize can race before `AppState.connections` contains the attached writer, causing "session not attached" behavior.

8. Tauri `attach_session` can drop monitoring if attach setup fails.

   `crates/shard-app/src/commands/session.rs` aborts and removes the monitor before opening the attach connection and sending `Resume`. If connect or resume fails, no monitor is restored and activity/status updates stop for that running session.

9. Natural session exit is not surfaced cleanly to attached terminal UI.

   The attach reader handles `Frame::Status` and exits, but it does not send an end marker/event to the terminal and does not remove the stale `Attached` entry from `AppState`. The frontend only marks disconnected after a later failed write.

10. Tauri state subscriber ignores `WorkspaceRemoved`.

   The daemon sends `ControlFrame::WorkspaceRemoved`, but `run_state_subscriber` treats it as an unexpected frame. Workspace removals from CLI or another client can leave sidebar/cache state stale until another refresh-triggering event arrives.

11. Sidebar refresh can drop invalidations during in-flight refresh.

   `Sidebar.refresh()` returns immediately when `refreshing` is true. Because a refresh performs sequential repo/workspace/session RPCs, an event that lands mid-refresh can be lost and leave the rendered tree stale.

### Low

1. SQLite migration errors can be swallowed when the message contains duplicate-column handling.

2. `NamedPipeTransport::accept` is a public trait method whose Windows implementation reaches an `unreachable!` placeholder. The event loop uses direct named-pipe handling instead, so the trait shape no longer matches reality.

   Status: resolved in Batch 5 by removing `SessionTransport::accept` and the Windows placeholder.

3. Frontend Tauri `listen()` unlisten handles are ignored.

   Mostly a dev/HMR risk, but duplicate listeners can cause duplicate refresh or activity handling after reloads.

## Simplification And Deletion Candidates

### High Confidence

1. Delete `DetachSession` protocol/RPC/app helper unless a concrete multi-window behavior needs it now.

   The Tauri detach command already uses `FindSessionById` to both validate the id and recover the transport address for monitor restart. The unused `daemon_ipc::detach_session` warning is a direct signal that this protocol branch is dead weight.

   Status: resolved in Batch 5. The remaining `detach_session` symbol is the Tauri UI command, not a daemon control frame.

2. Delete CLI `session stop` direct fallback.

   The invariant preserved is daemon ownership of session lifecycle and live registry. If offline cleanup is needed, it should be a separate recovery command with different guarantees.

3. Consolidate repeated daemon RPC helper boilerplate.

   `crates/shard-app/src/daemon_ipc.rs` repeats connect/handshake/request extraction for every RPC, and CLI repo/workspace commands have their own versions. A small shared typed-request helper would remove duplicated error plumbing without changing behavior.

   Status: resolved in Batch 5 for app daemon RPC helpers and CLI repo/workspace/session daemon RPC calls.

4. Collapse duplicate daemon spawn/connect logic.

   CLI and Tauri backend both implement `connect_or_spawn_daemon` with different executable-location assumptions. A shared helper with injected executable resolution would make daemon startup behavior easier to reason about.

   Status: resolved in Batch 5 by moving the transport decision tree to `daemon_client::connect_or_spawn`; CLI and Tauri provide only their executable-resolution/spawn closures.

5. Collapse duplicate workspace removal logic.

   `WorkspaceStore::remove` and `remove_worktree_fs` mirror filesystem cleanup behavior. Since the daemon needs split FS/DB phases, the older all-in-one store method should either delegate to the helper or be narrowed to DB-only primitives.

6. Narrow or delete `SessionTransport::accept`.

   The current trait method does not model Windows named-pipe accept semantics and is not used by the real multi-client event loop.

   Status: resolved in Batch 5.

7. Handle `WorkspaceRemoved` in the app subscriber and delete any redundant sidebar pokes this makes unnecessary.

8. Align Tauri frontend build commands with the package manager actually present.

   `tauri.conf.json` hardcodes `bun`, while this environment has working `npm run build` and no `bun`.

### Medium Confidence

1. Narrow `RepositoryStore::remove` into explicit primitives used by daemon workflows.

   Today it mixes git worktree cleanup, `.shard` cleanup, DB index deletion, and repo data deletion. That is too much authority for a store method now that the daemon owns mutation sequencing.

2. Add a central identifier type or validator for repo aliases, workspace names, and session IDs.

   This would remove ad hoc `safe_workspace_name` behavior and make every path join root-safe by construction.

3. Delete or use `AppState::repo_states`.

   The cache is still populated by snapshots, but app commands now get workspace status through daemon RPC. The comment says it is read by `list_workspaces`, which is no longer true.

4. Isolate the vendored `portable-pty` patch behind local wrapper tests.

   The vendored crate appears to carry Windows ConPTY passthrough behavior. Keep it if needed, but document the local delta and test Shard's expected PTY semantics at the wrapper boundary rather than relying on broad vendored code review.

## Missing Tests

- Path traversal and absolute path rejection for repo aliases and workspace names.
- Malformed protocol tests: maximum frame size, huge count fields, oversized strings, partial length-prefix EOF, invalid option/bool tags, and trailing bytes.
- `RemoveRepo` failure tests proving cleanup failures preserve DB rows for retry.
- `SessionStore` rowcount tests for nonexistent IDs on update helpers.
- CLI `session stop` lifecycle tests, especially daemon failure behavior.
- Tauri backend session lifecycle tests for attach/status/stop/remove flows.
- Workspace monitor unit tests for watcher-drop ack ordering, `classify_workspace`, path normalization, `DropRepoWorkspace`, and `DropRepo`.
- Frontend unit tests or at least focused integration/manual checks for terminal attach, resize debounce, activity state, and stale-session handling.
- `StopSession` racing `RemoveRepo` and `RemoveWorkspace`, asserting no repo DB/session dir resurrection and no stale live-registry entry.
- PID reuse/adoption tests: stale running DB row with a reused PID must not be adopted or killed.
- Shutdown tests: after `ShutdownAck`, mutating RPCs are rejected and in-flight mutation behavior is deterministic.
- Tauri subscriber test/manual scenario: remove workspace from CLI while app is open; sidebar should refresh.
- Sidebar refresh coalescing test: an event during refresh should schedule a second refresh.

## Proposed First Remediation Batches

### Batch 1: Root-Safe Identifiers And Destructive Store Semantics

Goal: prevent path traversal, prevent repo DB resurrection during destructive workflows, and preserve retry handles on cleanup failure.

- Add shared validation for repo aliases and workspace names.
- Reject absolute paths, `..`, path separators where not intentionally sanitized, reserved names, and empty/whitespace identifiers.
- Apply validation in `RepositoryStore::resolve_alias`, workspace name resolution, CLI/Tauri command surfaces as needed.
- Add path traversal tests for local and remote repo paths.
- Serialize `StopSession` with `RemoveRepo` / `RemoveWorkspace`, or make status-update backstops unable to create missing repo DBs.
- Add a regression test: `StopSession` after/concurrent with `RemoveRepo` must not recreate the repo directory or `repo.db`.
- Change `RepositoryStore::remove` to propagate cleanup failures before index deletion, or split it into daemon-owned primitives.

Implementation note:

- Started in this branch by adding `shard_core::identifiers`, validating repo aliases and workspace names at store boundaries, preserving branch-slash behavior through sanitized implicit workspace names, making `RepositoryStore::remove` propagate cleanup errors before index deletion, and serializing daemon `StopSession` behind the per-repo mutation lock.
- Compatibility caveat: legacy DB rows that already contain path-like aliases or workspace names will now be rejected by store APIs. If such rows exist in a real profile, recovery should be explicit rather than letting normal destructive commands operate on unsafe path segments.
- Review follow-up: Batch 1 now also rejects persisted non-base local workspace paths outside the repo's managed `.shard` root during `RemoveRepo`, preserves implicit slash-branch names for both `ExistingBranch` and `NewBranch`, and wraps the final index delete/repo-dir cleanup in a transaction so filesystem cleanup failure rolls the index mutation back.

### Batch 2: Daemon-Only Session Stop

Goal: remove lifecycle paths that bypass daemon registry and PID-reuse checks.

- Delete CLI `session stop` direct-pipe/PID fallback.
- Make CLI stop fail clearly when daemon stop fails.
- Define the daemon-dead behavior before deleting the fallback: either "daemon must be running; start it and retry" or a separate explicit recovery command with different guarantees.
- Add fallback-removal tests where daemon stop returns an error and CLI does not unchecked-terminate DB PIDs or write DB status itself.

Implementation note:

- Implemented by routing CLI `session stop` through `FindSessionById` + `StopSession` on the daemon control pipe only. If the daemon is absent or rejects the stop, the CLI now returns an explicit daemon error and does not write session status, connect to the DB transport pipe, or terminate DB PIDs. Daemon-dead recovery is intentionally a separate future command/policy, not an implicit direct cleanup path.

### Batch 3: Bounded Protocol Decode

Goal: make local IPC robust against malformed clients.

- Add `MAX_SESSION_FRAME_LEN` and `MAX_CONTROL_FRAME_LEN`.
- Replace unchecked length/count casts with checked conversions.
- Treat partial length-prefix reads as invalid data after any byte is received.
- Add malformed-frame tests for frame length, string length, counts, invalid tags, and trailing bytes.

Implementation note:

- Implemented by adding bounded session/control frame length reads, converting clean EOF versus partial length-prefix EOF distinctly, capping control collection counts before allocation, rejecting invalid option/bool tags, and checking exact payload consumption for control frames and fixed-size session frames. Added malformed-frame tests covering oversized frames, partial prefixes, unknown tags/types, string length overruns, excessive counts, and trailing bytes.
- Review follow-up: session `Status` decoding accepts the current one-byte payload and the legacy four-byte fake-supervisor payload used by existing stop-and-drain tests, while still rejecting other unexpected trailing-byte shapes.
- Second review follow-up: the four-byte compatibility path now parses the value as a legacy big-endian `u32` and rejects values outside the current `u8` status-code range, rather than ignoring trailing bytes.

### Batch 4: App Event And Terminal State Coalescing

Goal: remove stale UI states and races without large UI rewrites.

- Await or gate terminal input/resize until `attachSession` succeeds.
- Restore monitor if attach fails after aborting the old monitor.
- Send a terminal-ended event/message on `Frame::Status` and remove stale attached state.
- Handle `WorkspaceRemoved` in `run_state_subscriber`.
- Add pending-refresh coalescing while `Sidebar.refresh()` is already running.

Implementation note:

- Implemented by restoring the background monitor if app attach fails after aborting the old monitor, gating terminal input/resize behind attach completion, emitting `terminal-ended` on supervisor `Status`, clearing stale attached state when the attach reader exits, handling `WorkspaceRemoved` in the daemon subscriber, and coalescing `Sidebar.refresh()` calls that arrive while a refresh is already in flight.
- Review follow-up: attached connections now carry a generation token so stale reader tasks only remove their own connection, late attach completion refuses to overwrite a monitor/attachment installed by a concurrent detach/reopen, and `terminal-ended` is emitted after the defensive trailing-output drain.
- Second review follow-up: `detach_session` now also restores a monitor only when no newer connection exists, preventing a slow detach from overwriting a reopened attachment.

### Batch 5: Deletion/Consolidation Sweep

Goal: shrink surface area after correctness fixes.

- Remove `DetachSession` protocol/helper/handler/tests.
- Consolidate daemon RPC connect/handshake/request boilerplate.
- Consolidate daemon connect-or-spawn logic between CLI and Tauri.
- Remove or update stale `repo_states` cache/comment.
- Narrow `SessionTransport` to methods that match real Windows usage.

Implementation note:

- Removed the dead `DetachSession` control frames, daemon handler, app helper, and integration/protocol tests. `FindSessionById` remains the single daemon read path used by detach/attach flows that need session resolution.
- Bumped the control protocol to v8 and left the old `0xA6`/`0xA7` type bytes reserved rather than renumbering later frames.
- Added a shared app-side `request_daemon` wrapper for connect/handshake/request boilerplate, plus a CLI `cmd::daemon_rpc` helper used by repo/workspace/session read/delete RPCs.
- Moved the shared connect-or-spawn transport decision tree into `shard_transport::daemon_client::connect_or_spawn`; CLI and Tauri still own their different executable-resolution closures.
- Removed the unusable `SessionTransport::accept` method and the Windows placeholder that only existed to satisfy that trait shape.

Follow-up simplification pass:

- Centralized Tauri's `shardctl.exe` spawn resolution in `daemon_ipc::connect_or_spawn`, removing the app startup/create-session duplicate closure.
- Kept hook installation on its own best-effort daemon connection while deleting the app-side single-use `install_harness_hooks` helper and sharing the app daemon spawn helper.
- Aligned `request_typed` with its documented `Option<T>` extractor shape, removing repeated large-`Err(ControlFrame)` call-site patterns.
- Removed the unused `run_headless_daemon` wrapper, stale `dead_code` allowances, and public visibility from daemon/monitor helpers that are only used inside their modules/crate.
- Applied mechanical Clippy simplifications where they reduced code without changing behavior.

## Open Questions

- Which current behaviors are intentionally Windows-only versus accidental coupling?
- Which direct SQLite read paths are still meant to remain after the daemon-broker migration?
- Should the vendored `portable-pty` remain a long-term patch or be isolated behind a narrower local wrapper?
- For the planned Mac port, which Windows lifecycle assumptions need a portable identity model: PID creation-time checks, Job Object containment, named-pipe accept semantics, and daemon adoption of orphaned supervisors?
- What is the intended user-facing recovery path when the daemon is genuinely dead but a supervisor/session is still running?
