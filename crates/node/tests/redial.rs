//! Redial: backoff on a refused connection, and the permanent stop on close code
//! 4401 (§3.3, §3.4).
//!
//! This is the second of the two test files that bind a port, and only because a
//! WebSocket close code cannot be produced by an in-memory socket: `NodeSocket`
//! reads it out of a real `Close` frame. Both listeners are `127.0.0.1:0`.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use common::*;
use swarmbot_node::dial::{self, DialPlan, ServeOptions, StopReason};
use swarmbot_node::state::{NodeState, StateWriter};
use swarmbot_node::{NodeConfig, NodePaths};
use swarmbot_node_protocol::Socket as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// A plan compressed enough that a test does not spend the documented 250 ms…30 s
/// curve in wall time. The shape is the same; only the scale differs.
fn fast_plan() -> DialPlan {
    DialPlan {
        initial: Duration::from_millis(5),
        max: Duration::from_millis(40),
        jitter: 0.0,
        reset_after: Duration::from_secs(10),
    }
}

fn state_writer(dir: &std::path::Path, runtime: &swarmbot_node::NodeRuntime) -> StateWriter {
    let paths = NodePaths::new(dir);
    let config = &runtime.config;
    StateWriter::start(
        &paths,
        &NodeState::starting(
            std::process::id(),
            runtime.identity.node_id().as_str(),
            &config.label,
            runtime.identity.brain_origin(),
            config.brain_url.clone(),
            &paths.data_dir,
            runtime.roots(),
            config.ceilings.clone(),
            config.shell_enabled,
            config.capabilities().as_slice().to_vec(),
        ),
    )
    .expect("state file")
}

#[tokio::test]
async fn a_refused_connection_is_retried_until_the_daemon_is_stopped() {
    let (tmp, root) = temp_root();
    // Accept and immediately hang up: the upgrade never completes, so every dial
    // fails the same way a refused one does, and the test can count them.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let attempts = Arc::new(AtomicU32::new(0));
    let server = {
        let attempts = Arc::clone(&attempts);
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((mut stream, _)) => {
                        attempts.fetch_add(1, Ordering::SeqCst);
                        // Read whatever arrived, then hang up without answering:
                        // an incomplete handshake is a failed dial.
                        let mut buf = [0u8; 512];
                        let _ =
                            tokio::time::timeout(Duration::from_millis(50), stream.read(&mut buf))
                                .await;
                        let _ = stream.shutdown().await;
                    }
                    Err(_) => return,
                }
            }
        })
    };

    let runtime = runtime_in(tmp.path(), &root, &format!("ws://{addr}"));
    let state = state_writer(tmp.path(), &runtime);
    let shutdown = CancellationToken::new();
    let stopper = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            shutdown.cancel();
        })
    };

    let reason = dial::serve(
        Arc::clone(&runtime),
        shutdown,
        ServeOptions {
            plan: fast_plan(),
            state: Some(state.clone()),
        },
    )
    .await
    .expect("serve reports rather than fails");
    stopper.abort();
    server.abort();

    assert_eq!(reason, StopReason::Shutdown);
    let tried = attempts.load(Ordering::SeqCst);
    assert!(
        tried >= 3,
        "a failed dial must be retried; the node tried {tried} time(s)"
    );
    // ...and `status` can say why it was not connected, instead of guessing.
    let snapshot = swarmbot_node::state::read_file(state.path())
        .expect("reads")
        .expect("the state file is there");
    assert!(!snapshot.connected);
    let last = snapshot.last_error.unwrap_or_default();
    assert!(
        last.contains(&addr.to_string()) || last.to_ascii_lowercase().contains("handshake"),
        "the last dial failure is recorded: {last}"
    );
    let _ = NodeConfig::load(tmp.path()).expect("the config still loads");
}

#[tokio::test]
async fn a_4401_close_stops_the_node_permanently() {
    let (tmp, root) = temp_root();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        // One connection, closed with the revoked code. If the node redialed,
        // the second accept would hang — which is what the timeout below proves
        // did not happen.
        if let Ok((stream, _)) = listener.accept().await
            && let Ok(mut socket) = swarmbot_node_protocol::accept_ws(stream).await
        {
            let _ = socket
                .close(swarmbot_node_protocol::CLOSE_REVOKED, "node revoked")
                .await;
            // Give the client time to read the close frame before the socket is
            // dropped and the connection reset.
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    });

    let runtime = runtime_in(tmp.path(), &root, &format!("ws://{addr}"));
    let state = state_writer(tmp.path(), &runtime);
    let shutdown = CancellationToken::new();
    let guard = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            // If the node kept redialing, this is what would end the test — and
            // the assertion below would fail instead of hanging.
            tokio::time::sleep(Duration::from_secs(10)).await;
            shutdown.cancel();
        })
    };

    let reason = dial::serve(
        Arc::clone(&runtime),
        shutdown,
        ServeOptions {
            plan: fast_plan(),
            state: Some(state.clone()),
        },
    )
    .await
    .expect("serve reports rather than fails");
    guard.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;

    assert_eq!(
        reason,
        StopReason::Revoked,
        "close code 4401 must stop redialing for good"
    );
    assert!(
        StopReason::Revoked.to_string().contains("4401"),
        "and the operator is told which code caused it"
    );
    let snapshot = swarmbot_node::state::read_file(state.path())
        .expect("reads")
        .expect("the state file is there");
    assert!(!snapshot.connected);
    let last = snapshot.last_error.clone().unwrap_or_default();
    assert!(
        last.contains("4401"),
        "the state file records the revocation: {snapshot:?}"
    );
}

#[tokio::test]
async fn nothing_is_listening_is_a_dial_failure_not_a_panic() {
    let (tmp, root) = temp_root();
    // Bind and drop: a port nothing listens on.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);

    let runtime = runtime_in(tmp.path(), &root, &format!("ws://{addr}"));
    let end = dial::session(Arc::clone(&runtime), &format!("ws://{addr}"), None)
        .await
        .expect("a failed dial is reported, not propagated");
    match end {
        dial::SessionEnd::Failed { reason } => {
            assert!(!reason.is_empty(), "the reason is kept for the log");
        }
        other => panic!("expected a failed dial, got {other:?}"),
    }
}
