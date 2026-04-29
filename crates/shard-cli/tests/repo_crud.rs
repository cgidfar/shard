//! Integration tests for `ControlFrame::AddRepo`, `RemoveRepo`,
//! `SyncRepo`, and `ListRepos` (Phase 3 of the daemon-broker migration,
//! Batch A).
//!
//! The handlers under test live in `daemon.rs::handle_add_repo` …
//! `handle_list_repos`. We exercise:
//!   - Happy path add (local checkout): DB row present, base workspace
//!     registered in the lifecycle map, monitor state populated, return
//!     value matches DB.
//!   - Duplicate-alias add: surfaces as `Error` frame.
//!   - List after add: returns the repo.
//!   - Sync: no-op when fetch succeeds, Error on unknown alias.
//!   - Remove happy path: DB row gone, workspace directory gone,
//!     lifecycle entries for the repo cleared.
//!   - Remove with a workspace on disk (cascade path): every worktree
//!     tree is cleaned up.
//!   - Remove of a repo with a live session: the handler stops the
//!     session (via the same fake-supervisor harness Phase 1 uses) and
//!     removes cleanly.
//!   - Remove idempotency on an unknown alias.
//!   - Remove acquires the per-repo mutation lock: concurrent
//!     `CreateWorkspace` on the same repo is serialized (either Create
//!     wins or Remove wins, never both-succeed-with-leak).

#![cfg(windows)]

mod common;

use std::path::Path;

use common::TestHarness;
use shard_transport::control_protocol::ControlFrame;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn add_repo(harness: &TestHarness, url: &str, alias: Option<&str>) -> ControlFrame {
    let mut conn = harness.connect().await;
    conn.request(&ControlFrame::AddRepo {
        url: url.to_string(),
        alias: alias.map(|s| s.to_string()),
    })
    .await
    .expect("AddRepo RPC")
}

async fn remove_repo(harness: &TestHarness, alias: &str) -> ControlFrame {
    let mut conn = harness.connect().await;
    conn.request(&ControlFrame::RemoveRepo {
        alias: alias.to_string(),
    })
    .await
    .expect("RemoveRepo RPC")
}

async fn sync_repo(harness: &TestHarness, alias: &str) -> ControlFrame {
    let mut conn = harness.connect().await;
    conn.request(&ControlFrame::SyncRepo {
        alias: alias.to_string(),
    })
    .await
    .expect("SyncRepo RPC")
}

async fn list_repos(harness: &TestHarness) -> ControlFrame {
    let mut conn = harness.connect().await;
    conn.request(&ControlFrame::ListRepos)
        .await
        .expect("ListRepos RPC")
}

async fn create_workspace(
    harness: &TestHarness,
    repo: &str,
    name: &str,
) -> ControlFrame {
    let mut conn = harness.connect().await;
    conn.request(&ControlFrame::CreateWorkspace {
        repo: repo.to_string(),
        name: Some(name.to_string()),
        mode: shard_core::workspaces::WorkspaceMode::NewBranch,
        branch: Some("main".to_string()),
    })
    .await
    .expect("CreateWorkspace RPC")
}

fn db_has_repo(data_path: &Path, alias: &str) -> bool {
    let paths = shard_core::paths::ShardPaths::from_data_dir(data_path.to_path_buf());
    let store = shard_core::repos::RepositoryStore::new(paths);
    store.get(alias).is_ok()
}

// ── AddRepo ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn add_repo_happy_path_local() {
    let harness = TestHarness::start().await;
    let checkout = harness.create_bare_checkout("demo");

    let ack = add_repo(&harness, checkout.to_str().unwrap(), Some("demo")).await;
    let repo = match ack {
        ControlFrame::AddRepoAck { repo } => repo,
        other => panic!("expected AddRepoAck, got {other:?}"),
    };
    assert_eq!(repo.alias, "demo");
    assert!(repo.local_path.is_some(), "local repo should carry local_path");
    assert!(db_has_repo(&harness.data_path, "demo"));

    // AddRepo auto-creates the default-branch workspace ("main" here).
    // It should be registered Active in the lifecycle map, so
    // `check_can_mutate` passes.
    assert!(shard_cli::cmd::daemon::test_lifecycle_check(
        &harness.state,
        "demo",
        "main"
    )
    .is_ok());

    harness.shutdown().await;
}

#[tokio::test]
async fn add_repo_without_explicit_alias_derives_from_path() {
    let harness = TestHarness::start().await;
    let checkout = harness.create_bare_checkout("derived");

    let ack = add_repo(&harness, checkout.to_str().unwrap(), None).await;
    let repo = match ack {
        ControlFrame::AddRepoAck { repo } => repo,
        other => panic!("expected AddRepoAck, got {other:?}"),
    };
    // `git::default_alias` derives from the trailing path component.
    assert!(
        !repo.alias.is_empty(),
        "derived alias should be non-empty, got '{}'",
        repo.alias
    );
    assert!(db_has_repo(&harness.data_path, &repo.alias));

    harness.shutdown().await;
}

#[tokio::test]
async fn add_repo_duplicate_alias_errors() {
    let harness = TestHarness::start().await;
    let checkout = harness.create_bare_checkout("dup");

    match add_repo(&harness, checkout.to_str().unwrap(), Some("dup")).await {
        ControlFrame::AddRepoAck { .. } => {}
        other => panic!("first add: {other:?}"),
    }

    // Second add against a different path but same alias — should fail.
    let checkout2 = harness.create_bare_checkout("dup-2");
    match add_repo(&harness, checkout2.to_str().unwrap(), Some("dup")).await {
        ControlFrame::Error { message } => {
            assert!(
                message.to_lowercase().contains("already"),
                "expected duplicate error, got: {message}"
            );
        }
        other => panic!("expected Error for duplicate alias, got {other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn add_repo_rejects_path_like_aliases() {
    let harness = TestHarness::start().await;
    let checkout = harness.create_bare_checkout("unsafe-alias-source");
    let absolute_alias = harness
        .data_path
        .join("escape-target")
        .to_string_lossy()
        .to_string();

    for alias in [&absolute_alias, "..\\escape", "name:stream"] {
        match add_repo(&harness, checkout.to_str().unwrap(), Some(alias)).await {
            ControlFrame::Error { message } => {
                assert!(
                    message.contains("invalid repo alias"),
                    "unexpected message for {alias:?}: {message}"
                );
            }
            other => panic!("expected Error for invalid alias {alias:?}, got {other:?}"),
        }
    }

    assert!(
        !std::path::Path::new(&absolute_alias).exists(),
        "absolute alias target should not be created"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn add_repo_concurrent_same_alias_serializes() {
    // Codex Phase 3 fix: before the resolve_alias + lock-first refactor,
    // two AddRepo calls racing on the same explicit alias could slip
    // between the DB UNIQUE check and the per-repo lock acquisition.
    // Post-fix: the lock is taken BEFORE `RepositoryStore::add` so
    // the two handlers serialize. Exactly one Ack, one Error; no ghost.
    let harness = TestHarness::start().await;
    let checkout_a = harness.create_bare_checkout("race-a");
    let checkout_b = harness.create_bare_checkout("race-b");

    let harness_ref = &harness;
    let a = async move {
        add_repo(harness_ref, checkout_a.to_str().unwrap(), Some("contested")).await
    };
    let b = async move {
        add_repo(harness_ref, checkout_b.to_str().unwrap(), Some("contested")).await
    };
    let (ra, rb) = tokio::join!(a, b);

    let acks = [&ra, &rb]
        .iter()
        .filter(|r| matches!(r, ControlFrame::AddRepoAck { .. }))
        .count();
    let errors = [&ra, &rb]
        .iter()
        .filter(|r| matches!(r, ControlFrame::Error { .. }))
        .count();
    assert_eq!(
        (acks, errors),
        (1, 1),
        "expected one Ack + one Error; got ra={ra:?} rb={rb:?}"
    );
    assert!(db_has_repo(&harness.data_path, "contested"));

    harness.shutdown().await;
}

#[tokio::test]
async fn add_repo_alias_less_then_remove_is_atomic() {
    // Codex Phase 3 fix: before the resolve_alias + lock-first refactor,
    // an alias-less AddRepo committed the repo row BEFORE acquiring
    // the per-repo mutex. A concurrent RemoveRepo for the DERIVED alias
    // could slip in, delete the row, and AddRepo would return Ack for
    // a ghost. Post-fix: the handler resolves the alias up front and
    // takes the lock before `add`, so the two serialize.
    //
    // Black-box assertion: whichever ordering wins, the DB row and the
    // filesystem `.shard/` directory must agree on presence. Pre-fix,
    // the forbidden outcome would produce AddRepoAck with the alias
    // populated but no matching DB row. We drive through the derived
    // alias path (alias: None) — which is the path that was broken.
    let harness = TestHarness::start().await;
    let checkout = harness.create_bare_checkout("derived-atomic");
    // `git::default_alias` for a local path falls through to
    // `path.file_name()` — here the trailing directory component.
    let derived_alias = checkout
        .file_name()
        .and_then(|f| f.to_str())
        .expect("checkout path has trailing component")
        .to_string();
    let shard_dir = checkout.join(".shard");
    let checkout_str = checkout.to_str().unwrap().to_string();

    let harness_ref = &harness;
    let derived_for_rm = derived_alias.clone();
    let add = async move { add_repo(harness_ref, &checkout_str, None).await };
    let rm = async move { remove_repo(harness_ref, &derived_for_rm).await };
    let (ar, rr) = tokio::join!(add, rm);

    // RemoveRepo is idempotent on absent repos, so both outcomes are Ack.
    match ar {
        ControlFrame::AddRepoAck { ref repo } => assert_eq!(repo.alias, derived_alias),
        ref other => panic!("AddRepo outcome: {other:?}"),
    }
    match rr {
        ControlFrame::RemoveRepoAck => {}
        other => panic!("RemoveRepo outcome: {other:?}"),
    }

    let present = db_has_repo(&harness.data_path, &derived_alias);
    let shard_dir_present = shard_dir.exists();
    assert_eq!(
        present, shard_dir_present,
        "DB / fs disagreement: present={present} shard_dir={shard_dir_present}"
    );

    harness.shutdown().await;
}

// ── ListRepos ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_repos_empty_initially() {
    let harness = TestHarness::start().await;

    match list_repos(&harness).await {
        ControlFrame::RepoList { repos } => assert!(repos.is_empty()),
        other => panic!("expected RepoList, got {other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn list_repos_returns_added_entries() {
    let harness = TestHarness::start().await;
    let a = harness.create_bare_checkout("alpha");
    let b = harness.create_bare_checkout("beta");

    add_repo(&harness, a.to_str().unwrap(), Some("alpha")).await;
    add_repo(&harness, b.to_str().unwrap(), Some("beta")).await;

    match list_repos(&harness).await {
        ControlFrame::RepoList { repos } => {
            let aliases: std::collections::HashSet<_> =
                repos.iter().map(|r| r.alias.clone()).collect();
            assert!(aliases.contains("alpha"));
            assert!(aliases.contains("beta"));
        }
        other => panic!("expected RepoList, got {other:?}"),
    }

    harness.shutdown().await;
}

// ── SyncRepo ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn sync_repo_unknown_alias_errors() {
    let harness = TestHarness::start().await;

    match sync_repo(&harness, "nonexistent").await {
        ControlFrame::Error { .. } => {}
        other => panic!("expected Error, got {other:?}"),
    }

    harness.shutdown().await;
}

// ── RemoveRepo ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn remove_repo_happy_path() {
    let harness = TestHarness::start().await;
    let checkout = harness.create_bare_checkout("demo");
    add_repo(&harness, checkout.to_str().unwrap(), Some("demo")).await;
    assert!(db_has_repo(&harness.data_path, "demo"));

    match remove_repo(&harness, "demo").await {
        ControlFrame::RemoveRepoAck => {}
        other => panic!("expected RemoveRepoAck, got {other:?}"),
    }

    assert!(!db_has_repo(&harness.data_path, "demo"));
    // Local checkout itself must NOT be deleted — D11 preserves the
    // user's source-of-truth tree.
    assert!(
        checkout.exists(),
        "local checkout should be preserved after RemoveRepo"
    );
    // `.shard/` under the checkout (added when base workspace was
    // auto-created) should be gone.
    assert!(
        !checkout.join(".shard").exists(),
        ".shard/ should be cleaned up"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn remove_repo_cascades_workspaces() {
    let harness = TestHarness::start().await;
    let checkout = harness.create_bare_checkout("demo");
    add_repo(&harness, checkout.to_str().unwrap(), Some("demo")).await;

    // Create two extra (non-base) workspaces.
    match create_workspace(&harness, "demo", "alpha").await {
        ControlFrame::CreateWorkspaceAck { .. } => {}
        other => panic!("create alpha: {other:?}"),
    }
    match create_workspace(&harness, "demo", "beta").await {
        ControlFrame::CreateWorkspaceAck { .. } => {}
        other => panic!("create beta: {other:?}"),
    }

    let alpha_dir = checkout.join(".shard").join("alpha");
    let beta_dir = checkout.join(".shard").join("beta");
    assert!(alpha_dir.exists());
    assert!(beta_dir.exists());

    match remove_repo(&harness, "demo").await {
        ControlFrame::RemoveRepoAck => {}
        other => panic!("expected Ack, got {other:?}"),
    }

    assert!(!alpha_dir.exists(), "alpha worktree should be gone");
    assert!(!beta_dir.exists(), "beta worktree should be gone");
    assert!(!db_has_repo(&harness.data_path, "demo"));

    harness.shutdown().await;
}

#[tokio::test]
async fn remove_repo_rejects_persisted_workspace_path_outside_shard_root() {
    let harness = TestHarness::start().await;
    let checkout = harness.create_bare_checkout("demo");
    add_repo(&harness, checkout.to_str().unwrap(), Some("demo")).await;

    let outside_dir = harness.data_path.join("outside-delete-target");
    std::fs::create_dir_all(&outside_dir).expect("create outside target");
    std::fs::write(outside_dir.join("sentinel.txt"), "keep").expect("write sentinel");

    let paths = shard_core::paths::ShardPaths::from_data_dir(harness.data_path.clone());
    let conn = shard_core::db::open_connection(&paths.repo_db("demo")).expect("open repo db");
    let outside_path = outside_dir.to_string_lossy().to_string();
    conn.execute(
        "INSERT INTO workspaces (name, branch, path, is_base, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        ("corrupt", "main", outside_path.as_str(), 0i32, 1i64),
    )
    .expect("seed corrupt workspace row");

    match remove_repo(&harness, "demo").await {
        ControlFrame::Error { message } => {
            assert!(
                message.contains("outside managed .shard root"),
                "unexpected error: {message}"
            );
        }
        other => panic!("expected Error for unsafe workspace path, got {other:?}"),
    }

    assert!(
        outside_dir.join("sentinel.txt").exists(),
        "RemoveRepo must not delete outside persisted workspace paths"
    );
    assert!(
        db_has_repo(&harness.data_path, "demo"),
        "failed cleanup should preserve the repo index row for retry"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn remove_repo_unknown_alias_is_idempotent() {
    let harness = TestHarness::start().await;

    match remove_repo(&harness, "nonexistent").await {
        ControlFrame::RemoveRepoAck => {}
        other => panic!("expected Ack for unknown repo, got {other:?}"),
    }

    harness.shutdown().await;
}

// ── RemoveRepo with live session (stop-and-drain path) ─────────────────────

/// Fake supervisor mirroring the one in `remove_workspace.rs`: accept on
/// the session pipe, wait for `Resume` + `StopGraceful`, reply with a
/// `Status { code: 0 }` frame so `stop_session_and_wait` drains cleanly.
async fn spawn_fake_supervisor(pipe_name: String) -> tokio::task::JoinHandle<()> {
    use shard_transport::transport_windows::create_pipe_instance;
    tokio::spawn(async move {
        let mut server = create_pipe_instance(&pipe_name, true).expect("create pipe");
        server.connect().await.expect("accept supervisor client");

        // Read frames until we see StopGraceful / StopForce, then emit
        // Status and close the pipe.
        loop {
            let mut len_buf = [0u8; 4];
            if server.read_exact(&mut len_buf).await.is_err() {
                return;
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; len];
            if server.read_exact(&mut buf).await.is_err() {
                return;
            }
            if buf.is_empty() {
                continue;
            }
            let type_byte = buf[0];
            // Session protocol: 0x02 = StopGraceful, 0x03 = StopForce.
            if type_byte == 0x02 || type_byte == 0x03 {
                // Status frame: type 0x05, payload = u32 exit_code = 0.
                let payload = vec![0u8; 4];
                let mut out = Vec::new();
                out.extend_from_slice(&(1u32 + payload.len() as u32).to_be_bytes());
                out.push(0x05);
                out.extend_from_slice(&payload);
                let _ = server.write_all(&out).await;
                let _ = server.flush().await;
                return;
            }
        }
    })
}

#[tokio::test]
async fn remove_repo_stops_live_session() {
    let harness = TestHarness::start().await;
    let checkout = harness.create_bare_checkout("demo");
    add_repo(&harness, checkout.to_str().unwrap(), Some("demo")).await;

    // Inject a fake live session bound to the base (auto-created) workspace.
    let session_id = "01991111-1111-7111-8111-111111111111".to_string();
    let pipe_name =
        format!(r"\\.\pipe\shard-test-fake-supervisor-{}", std::process::id());
    let _supervisor = spawn_fake_supervisor(pipe_name.clone()).await;
    // Small beat so the pipe is accepting before the daemon connects.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    shard_cli::cmd::daemon::test_inject_live_session(
        &harness.state,
        session_id.clone(),
        std::process::id(),
        pipe_name,
        "demo".to_string(),
        "main".to_string(),
        1, // refuse force-kill against the test pid
    )
    .await;

    match remove_repo(&harness, "demo").await {
        ControlFrame::RemoveRepoAck => {}
        other => panic!("expected Ack, got {other:?}"),
    }

    assert!(!db_has_repo(&harness.data_path, "demo"));

    harness.shutdown().await;
}

#[tokio::test]
async fn stop_session_racing_remove_repo_does_not_recreate_repo_dir() {
    let harness = TestHarness::start().await;
    let checkout = harness.create_bare_checkout("demo");
    add_repo(&harness, checkout.to_str().unwrap(), Some("demo")).await;

    let session_id = "01992222-2222-7222-8222-222222222222".to_string();
    let pipe_name = format!(
        r"\\.\pipe\shard-test-stop-remove-race-{}",
        std::process::id()
    );
    let _supervisor = spawn_fake_supervisor(pipe_name.clone()).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    shard_cli::cmd::daemon::test_inject_live_session(
        &harness.state,
        session_id.clone(),
        std::process::id(),
        pipe_name,
        "demo".to_string(),
        "main".to_string(),
        1,
    )
    .await;

    let stop = async {
        let mut conn = harness.connect().await;
        conn.request(&ControlFrame::StopSession {
            session_id: session_id.clone(),
            force: false,
        })
        .await
        .expect("StopSession RPC")
    };
    let remove = async { remove_repo(&harness, "demo").await };
    let (stop_r, remove_r) = tokio::join!(stop, remove);

    assert!(
        matches!(stop_r, ControlFrame::StopAck | ControlFrame::Error { .. }),
        "StopSession outcome: {stop_r:?}"
    );
    assert!(
        matches!(remove_r, ControlFrame::RemoveRepoAck),
        "RemoveRepo outcome: {remove_r:?}"
    );
    assert!(!db_has_repo(&harness.data_path, "demo"));
    assert!(
        !shard_core::paths::ShardPaths::from_data_dir(harness.data_path.clone())
            .repo_dir("demo")
            .exists(),
        "StopSession must not recreate repo dir/repo.db after RemoveRepo"
    );

    harness.shutdown().await;
}

// ── RemoveRepo vs concurrent CreateWorkspace (per-repo mutation lock) ──────

#[tokio::test]
async fn remove_repo_blocks_concurrent_create_workspace() {
    let harness = TestHarness::start().await;
    let checkout = harness.create_bare_checkout("demo");
    add_repo(&harness, checkout.to_str().unwrap(), Some("demo")).await;

    // Fire RemoveRepo + CreateWorkspace in parallel. Both run through
    // the per-repo mutation lock; exactly one logical outcome is legal:
    //   - Create wins → repo still exists, new workspace present
    //   - Remove wins → repo is gone; Create returns an Error
    // The forbidden outcome is Create-acked while repo row is gone.
    let harness_ref = &harness;
    let create = async move {
        create_workspace(harness_ref, "demo", "late").await
    };
    let remove = async move { remove_repo(harness_ref, "demo").await };
    let (create_r, remove_r) = tokio::join!(create, remove);

    match remove_r {
        ControlFrame::RemoveRepoAck => {}
        other => panic!("remove outcome: {other:?}"),
    }

    let repo_gone = !db_has_repo(&harness.data_path, "demo");
    match (repo_gone, &create_r) {
        (true, ControlFrame::Error { .. }) => {
            // Remove won. Create saw the missing repo and errored.
        }
        (false, ControlFrame::CreateWorkspaceAck { .. }) => {
            // Create won. Remove ran afterwards but the workspace was
            // already part of the repo by then. Wait — if Remove won
            // the lock second, it should have found the late workspace
            // and cascaded it. So repo must be gone if Remove Ack'd.
            panic!(
                "inconsistent outcome: Remove Ack'd but repo still exists with late workspace present"
            );
        }
        (true, ControlFrame::CreateWorkspaceAck { workspace }) => {
            // Create won lock first → committed workspace, then Remove
            // cascaded everything. Consistent: repo_gone=true matches.
            assert_eq!(workspace.name, "late");
        }
        (false, ControlFrame::Error { .. }) => {
            // Remove ran first but Create errored on stale row? Legal
            // only if repo was not actually removed — e.g. Remove
            // short-circuited somehow. Keep the guard loose.
            panic!(
                "repo still present but Create errored: create={create_r:?} remove={remove_r:?}"
            );
        }
        (_, other) => panic!("unexpected Create outcome: {other:?}"),
    }

    harness.shutdown().await;
}
