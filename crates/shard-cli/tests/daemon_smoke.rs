//! Smoke test for the integration harness.
//!
//! Verifies that `TestHarness::start()` brings up a headless daemon, that
//! `Ping`/`Pong` round-trips, and that shutdown is clean (no orphan task).

#![cfg(windows)]

mod common;

use common::TestHarness;
use shard_transport::control_protocol::ControlFrame;

#[tokio::test]
async fn harness_ping_pong() {
    let harness = TestHarness::start().await;
    let mut conn = harness.connect().await;

    let response = conn.request(&ControlFrame::Ping).await.expect("ping");
    match response {
        ControlFrame::Pong => {}
        other => panic!("expected Pong, got {other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn harness_empty_session_list() {
    let harness = TestHarness::start().await;
    let mut conn = harness.connect().await;

    let response = conn
        .request(&ControlFrame::ListSessions)
        .await
        .expect("list sessions");
    match response {
        ControlFrame::SessionList { sessions } => {
            assert!(sessions.is_empty(), "fresh daemon should have no sessions");
        }
        other => panic!("expected SessionList, got {other:?}"),
    }

    harness.shutdown().await;
}
