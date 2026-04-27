//! Integration tests for `ControlFrame::CreateWorkspace`,
//! `ControlFrame::ListWorkspaces`, and `ControlFrame::ListBranchInfo`
//! (Phase 2 of the daemon-broker migration).
//!
//! These assert the RPCs:
//!   - Create a workspace end-to-end: DB row written, disk tree exists,
//!     lifecycle registry entered Active, subsequent check_can_mutate OK.
//!   - Block create against a workspace name currently in `Deleting`.
//!   - Reject duplicate-name creates via the DB unique constraint.
//!   - After a committed delete, re-create with the same name succeeds.
//!   - List returns enriched rows (workspace + status) for a repo.
//!   - list_branch_info surfaces both checked-out and unclaimed branches.

#![cfg(windows)]

mod common;

use std::path::Path;

use common::TestHarness;
use shard_core::workspaces::WorkspaceMode;
use shard_transport::control_protocol::ControlFrame;

/// Issue a CreateWorkspace RPC on a fresh connection.
async fn create_workspace(
    harness: &TestHarness,
    repo: &str,
    name: Option<&str>,
    mode: WorkspaceMode,
    branch: Option<&str>,
) -> ControlFrame {
    let mut conn = harness.connect().await;
    conn.request(&ControlFrame::CreateWorkspace {
        repo: repo.to_string(),
        name: name.map(|s| s.to_string()),
        mode,
        branch: branch.map(|s| s.to_string()),
    })
    .await
    .expect("CreateWorkspace RPC")
}

async fn list_workspaces(harness: &TestHarness, repo: &str) -> ControlFrame {
    let mut conn = harness.connect().await;
    conn.request(&ControlFrame::ListWorkspaces {
        repo: repo.to_string(),
    })
    .await
    .expect("ListWorkspaces RPC")
}

async fn list_branch_info(harness: &TestHarness, repo: &str) -> ControlFrame {
    let mut conn = harness.connect().await;
    conn.request(&ControlFrame::ListBranchInfo {
        repo: repo.to_string(),
    })
    .await
    .expect("ListBranchInfo RPC")
}

async fn remove_workspace(harness: &TestHarness, repo: &str, name: &str) -> ControlFrame {
    let mut conn = harness.connect().await;
    conn.request(&ControlFrame::RemoveWorkspace {
        repo: repo.to_string(),
        name: name.to_string(),
    })
    .await
    .expect("RemoveWorkspace RPC")
}

fn db_has_workspace(data_path: &Path, repo: &str, ws_name: &str) -> bool {
    let paths = shard_core::paths::ShardPaths::from_data_dir(data_path.to_path_buf());
    let store = shard_core::workspaces::WorkspaceStore::new(paths);
    store.get(repo, ws_name).is_ok()
}

// ── Happy path ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_workspace_happy_path() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");

    let ack = create_workspace(
        &harness,
        "demo",
        Some("feature-a"),
        WorkspaceMode::NewBranch,
        Some("main"),
    )
    .await;

    let ws = match ack {
        ControlFrame::CreateWorkspaceAck { workspace } => workspace,
        other => panic!("expected Ack, got {other:?}"),
    };
    assert_eq!(ws.name, "feature-a");
    assert_eq!(ws.branch, "feature-a");
    assert!(!ws.is_base);
    assert!(Path::new(&ws.path).exists(), "worktree dir must exist");
    assert!(db_has_workspace(&harness.data_path, "demo", "feature-a"));

    // Registered Active → check_can_mutate OK and Delete gate lets us start.
    assert!(shard_cli::cmd::daemon::test_lifecycle_check(
        &harness.state,
        "demo",
        "feature-a"
    )
    .is_ok());

    harness.shutdown().await;
}

// ── Gate: create blocked during active delete ──────────────────────────────

#[tokio::test]
async fn create_workspace_blocked_during_delete() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    harness.setup_workspace("demo", "in-flight");

    // Enter Deleting via the lifecycle API and hold the guard open so the
    // CreateWorkspace RPC sees the gate.
    let guard = match harness.state.lifecycle.begin_delete("demo", "in-flight") {
        shard_cli::cmd::lifecycle::BeginDelete::Started(g) => g,
        other => panic!("expected Started, got {other:?}"),
    };

    let response = create_workspace(
        &harness,
        "demo",
        Some("in-flight"),
        WorkspaceMode::NewBranch,
        Some("main"),
    )
    .await;

    match response {
        ControlFrame::Error { message } => {
            assert!(
                message.contains("being deleted"),
                "expected 'being deleted' error, got: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }

    guard.rollback();
    harness.shutdown().await;
}

// ── Gate: implicit-name create also respects the gate (Codex round 1 fix) ──

#[tokio::test]
async fn create_workspace_blocked_with_implicit_name() {
    // `name: None` + NewBranch + `branch: Some("side")` resolves to the
    // name "side". Drive the same-named workspace into Deleting and verify
    // that the implicit-name create is still rejected — regression guard
    // for the Codex-round-1 race where the handler skipped
    // `check_can_mutate` when `name` was None.
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    let ws_path = harness.setup_workspace("demo", "side");

    assert!(ws_path.exists());

    let guard = match harness.state.lifecycle.begin_delete("demo", "side") {
        shard_cli::cmd::lifecycle::BeginDelete::Started(g) => g,
        other => panic!("expected Started, got {other:?}"),
    };

    let response = create_workspace(
        &harness,
        "demo",
        None, // implicit — resolves to "side" via branch_for_db
        WorkspaceMode::NewBranch,
        Some("side"),
    )
    .await;

    match response {
        ControlFrame::Error { message } => {
            assert!(
                message.contains("being deleted"),
                "expected 'being deleted' error, got: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }

    guard.rollback();
    harness.shutdown().await;
}

// ── Gate: create blocked when a Broken workspace exists with that name ─────

#[tokio::test]
async fn create_workspace_blocked_on_broken_name() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    harness.setup_workspace("demo", "half-gone");

    // Drive the workspace into Broken via a begin_delete + commit_broken.
    match harness.state.lifecycle.begin_delete("demo", "half-gone") {
        shard_cli::cmd::lifecycle::BeginDelete::Started(g) => g.commit_broken(),
        other => panic!("expected Started, got {other:?}"),
    }

    let response = create_workspace(
        &harness,
        "demo",
        Some("half-gone"),
        WorkspaceMode::NewBranch,
        Some("main"),
    )
    .await;

    match response {
        ControlFrame::Error { message } => {
            assert!(
                message.contains("broken"),
                "expected 'broken' error, got: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }

    harness.shutdown().await;
}

// ── Duplicate name ─────────────────────────────────────────────────────────

#[tokio::test]
async fn create_workspace_duplicate_name_errors() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");

    // First create succeeds.
    match create_workspace(
        &harness,
        "demo",
        Some("dup"),
        WorkspaceMode::NewBranch,
        Some("main"),
    )
    .await
    {
        ControlFrame::CreateWorkspaceAck { .. } => {}
        other => panic!("first create should succeed, got {other:?}"),
    }

    // Second create collides — daemon surfaces the WorkspaceAlreadyExists
    // error via the generic Error frame.
    match create_workspace(
        &harness,
        "demo",
        Some("dup"),
        WorkspaceMode::NewBranch,
        Some("main"),
    )
    .await
    {
        ControlFrame::Error { message } => {
            assert!(
                message.to_lowercase().contains("already"),
                "expected duplicate error, got: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn create_workspace_rejects_path_like_names() {
    let harness = TestHarness::start().await;
    let (_alias, _repo_path) = harness.setup_local_repo("demo");
    let absolute_name = harness
        .data_path
        .join("workspace-escape")
        .to_string_lossy()
        .to_string();

    for name in [&absolute_name, "..\\escape", "name:stream"] {
        match create_workspace(
            &harness,
            "demo",
            Some(name),
            WorkspaceMode::NewBranch,
            Some("main"),
        )
        .await
        {
            ControlFrame::Error { message } => {
                assert!(
                    message.contains("invalid workspace name"),
                    "unexpected message for {name:?}: {message}"
                );
            }
            other => panic!("expected Error for invalid workspace name {name:?}, got {other:?}"),
        }
    }

    assert!(
        !Path::new(&absolute_name).exists(),
        "absolute workspace name target should not be created"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn implicit_existing_branch_name_is_sanitized_before_validation() {
    let harness = TestHarness::start().await;
    let (_alias, repo_path) = harness.setup_local_repo("demo");
    let branch = "feature/path-like-name";
    let output = std::process::Command::new("git")
        .args(["branch", branch])
        .current_dir(&repo_path)
        .output()
        .expect("spawn git branch");
    assert!(
        output.status.success(),
        "git branch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    match create_workspace(
        &harness,
        "demo",
        Some(branch),
        WorkspaceMode::ExistingBranch,
        Some(branch),
    )
    .await
    {
        ControlFrame::CreateWorkspaceAck { workspace } => {
            assert_eq!(workspace.name, "feature-path-like-name");
            assert_eq!(workspace.branch, branch);
        }
        other => panic!("expected Ack, got {other:?}"),
    }

    harness.shutdown().await;
}

// ── Create after committed delete succeeds (lifecycle entry removed) ───────

#[tokio::test]
async fn create_after_delete_succeeds() {
    // Asserts the lifecycle registry properly removes the `recycle` entry on
    // `commit_gone` so a subsequent create isn't blocked. (The re-create
    // itself uses `ExistingBranch` on the branch that `WorkspaceStore::remove`
    // intentionally leaves behind — that's the pre-existing reclaim path,
    // not something Phase 2 changes.)
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");

    match create_workspace(
        &harness,
        "demo",
        Some("recycle"),
        WorkspaceMode::NewBranch,
        Some("main"),
    )
    .await
    {
        ControlFrame::CreateWorkspaceAck { .. } => {}
        other => panic!("first create: {other:?}"),
    }
    match remove_workspace(&harness, "demo", "recycle").await {
        ControlFrame::RemoveWorkspaceAck => {}
        other => panic!("remove: {other:?}"),
    }

    // Confirm the lifecycle entry is gone (would be Deleting/Broken otherwise
    // and the following create would be gated).
    assert!(shard_cli::cmd::daemon::test_lifecycle_check(
        &harness.state,
        "demo",
        "recycle"
    )
    .is_ok());

    // Recycle the dangling `recycle` branch via ExistingBranch mode.
    match create_workspace(
        &harness,
        "demo",
        Some("recycle"),
        WorkspaceMode::ExistingBranch,
        Some("recycle"),
    )
    .await
    {
        ControlFrame::CreateWorkspaceAck { workspace } => {
            assert_eq!(workspace.name, "recycle");
            assert_eq!(workspace.branch, "recycle");
        }
        other => panic!("second create: {other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn create_existing_branch_with_slash_uses_single_workspace_dir() {
    let harness = TestHarness::start().await;
    let (_alias, repo_path) = harness.setup_local_repo("demo");
    let branch = "feature/FD-6557-correct-projection-model";
    let output = std::process::Command::new("git")
        .args(["branch", branch])
        .current_dir(&repo_path)
        .output()
        .expect("spawn git branch");
    assert!(
        output.status.success(),
        "git branch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    match create_workspace(
        &harness,
        "demo",
        Some(branch),
        WorkspaceMode::ExistingBranch,
        Some(branch),
    )
    .await
    {
        ControlFrame::CreateWorkspaceAck { workspace } => {
            assert_eq!(workspace.name, "feature-FD-6557-correct-projection-model");
            assert_eq!(workspace.branch, branch);
            let path = Path::new(&workspace.path);
            assert!(path.exists(), "worktree dir must exist");
            assert_eq!(
                path.file_name().and_then(|s| s.to_str()),
                Some("feature-FD-6557-correct-projection-model")
            );
            assert_eq!(
                path.parent()
                    .and_then(|p| p.file_name())
                    .and_then(|s| s.to_str()),
                Some(".shard")
            );
        }
        other => panic!("expected Ack, got {other:?}"),
    }

    harness.shutdown().await;
}

// ── ListWorkspaces ─────────────────────────────────────────────────────────

#[tokio::test]
async fn list_workspaces_returns_created_entries() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");

    // Seed two workspaces via the RPC (exercises the full create path too).
    for name in &["alpha", "beta"] {
        match create_workspace(
            &harness,
            "demo",
            Some(name),
            WorkspaceMode::NewBranch,
            Some("main"),
        )
        .await
        {
            ControlFrame::CreateWorkspaceAck { .. } => {}
            other => panic!("create {name}: {other:?}"),
        }
    }

    let response = list_workspaces(&harness, "demo").await;
    match response {
        ControlFrame::WorkspaceList { items } => {
            let names: std::collections::HashSet<_> =
                items.iter().map(|i| i.workspace.name.clone()).collect();
            assert!(names.contains("alpha"), "missing alpha: {names:?}");
            assert!(names.contains("beta"), "missing beta: {names:?}");
            // `status` may be None if the monitor hasn't ticked yet; both
            // shapes are valid. Just assert the field is serialized.
            for item in &items {
                assert!(!item.workspace.path.is_empty());
            }
        }
        other => panic!("expected WorkspaceList, got {other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn list_workspaces_empty_repo_returns_empty() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");

    match list_workspaces(&harness, "demo").await {
        ControlFrame::WorkspaceList { items } => {
            assert!(items.is_empty(), "expected empty list, got {items:?}");
        }
        other => panic!("expected WorkspaceList, got {other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn list_workspaces_unknown_repo_errors() {
    let harness = TestHarness::start().await;

    match list_workspaces(&harness, "nonexistent").await {
        ControlFrame::Error { .. } => {}
        other => panic!("expected Error for unknown repo, got {other:?}"),
    }

    harness.shutdown().await;
}

// ── ListBranchInfo ─────────────────────────────────────────────────────────

// ── Per-repo mutation lock (Codex round 2 finding) ─────────────────────────

#[tokio::test]
async fn concurrent_creates_on_same_repo_both_succeed() {
    // The per-repo mutation mutex serializes CreateWorkspace /
    // RemoveWorkspace. Two concurrent creates of distinct names on the
    // same repo must both complete — this would deadlock if the mutex
    // were accidentally scoped too broadly or held across an await that
    // re-enters the same lock.
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");

    let h1 = {
        let harness = &harness;
        async move {
            create_workspace(
                harness,
                "demo",
                Some("alpha"),
                WorkspaceMode::NewBranch,
                Some("main"),
            )
            .await
        }
    };
    let h2 = {
        let harness = &harness;
        async move {
            create_workspace(
                harness,
                "demo",
                Some("beta"),
                WorkspaceMode::NewBranch,
                Some("main"),
            )
            .await
        }
    };
    let (r1, r2) = tokio::join!(h1, h2);

    assert!(
        matches!(r1, ControlFrame::CreateWorkspaceAck { .. }),
        "first: {r1:?}"
    );
    assert!(
        matches!(r2, ControlFrame::CreateWorkspaceAck { .. }),
        "second: {r2:?}"
    );
    assert!(db_has_workspace(&harness.data_path, "demo", "alpha"));
    assert!(db_has_workspace(&harness.data_path, "demo", "beta"));

    harness.shutdown().await;
}

#[tokio::test]
async fn concurrent_create_and_remove_reach_consistent_state() {
    // Fire Create + Remove for the same name concurrently. Both complete
    // (no deadlock) and the end state is either "exists" or "absent" —
    // never "exists-but-Remove-said-gone" (the race this mutex closes).
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");

    let create_fut = {
        let harness = &harness;
        async move {
            create_workspace(
                harness,
                "demo",
                Some("contested"),
                WorkspaceMode::NewBranch,
                Some("main"),
            )
            .await
        }
    };
    let remove_fut = {
        let harness = &harness;
        async move { remove_workspace(harness, "demo", "contested").await }
    };
    let (create_r, remove_r) = tokio::join!(create_fut, remove_fut);

    // Both must produce an Ack variant (Remove is idempotent on absence).
    let created_ok = matches!(create_r, ControlFrame::CreateWorkspaceAck { .. });
    assert!(created_ok, "create outcome: {create_r:?}");
    assert!(
        matches!(remove_r, ControlFrame::RemoveWorkspaceAck),
        "remove outcome: {remove_r:?}"
    );

    // The end state is well-defined under the mutex: either Remove won
    // the lock first (row absent at Remove time, then Create commits,
    // row ends present) OR Create won (Remove observes the row it
    // created and deletes it, row ends absent). The pathological
    // outcome — Remove acked but the row still exists and then Create
    // also acked — is precisely what the per-repo mutex was added to
    // prevent. Both legal outcomes are valid; asserting which won isn't
    // the invariant.
    let present = db_has_workspace(&harness.data_path, "demo", "contested");
    let ws_path = harness.data_path.join("repo-source-demo/.shard/contested");
    // On-disk and DB state must agree (no orphan rows, no orphan dirs).
    assert_eq!(
        present,
        ws_path.exists(),
        "DB and filesystem must agree on workspace presence"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn list_branch_info_reflects_head_and_new_branches() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");

    // Create a workspace to introduce a second branch, then list.
    match create_workspace(
        &harness,
        "demo",
        Some("feat"),
        WorkspaceMode::NewBranch,
        Some("main"),
    )
    .await
    {
        ControlFrame::CreateWorkspaceAck { .. } => {}
        other => panic!("create: {other:?}"),
    }

    let response = list_branch_info(&harness, "demo").await;
    match response {
        ControlFrame::BranchInfoList { branches } => {
            let main = branches
                .iter()
                .find(|b| b.name == "main")
                .expect("main branch must be in the list");
            assert!(main.is_head, "main should be HEAD");

            let feat = branches
                .iter()
                .find(|b| b.name == "feat")
                .expect("feat branch must exist after create");
            assert!(!feat.is_head, "feat is not HEAD of the source repo");
            assert_eq!(
                feat.checked_out_by.as_deref(),
                Some("feat"),
                "feat should be claimed by the 'feat' workspace"
            );
        }
        other => panic!("expected BranchInfoList, got {other:?}"),
    }

    harness.shutdown().await;
}
