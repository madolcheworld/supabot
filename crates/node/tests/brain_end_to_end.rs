//! What this node tells its operator when a brain refuses to pair.
//!
//! The brain's node gateway is **built** (`crates/brain/src/gateway/`, covered by
//! `crates/brain/tests/node_gateway.rs` and by a two-process run of the real
//! binaries: pair → `node.hello` → heartbeats → revoke → 4401 → the node stops
//! redialing). So this file no longer waits on a milestone; it keeps the one
//! behaviour that belongs to *this* crate and is worth pinning on its own: a
//! refusal — any non-2xx, from any brain — is reported verbatim, and nothing is
//! written to disk. A node that met a refusal with silence, a retry storm or a
//! fake success would be lying.

mod common;

use common::FakeBrain;
use swarmbot_node::NodeError;
use swarmbot_node::config::NodePaths;

/// A refusal envelope built by the brain's own helper, so this test cannot
/// drift from the shape a real brain sends. The status and message stand in for
/// any refusal: an expired code, a used code, a gateway that is not there.
fn stub_body() -> String {
    swarmbot_brain::routes::node_gateway::error_body(swarmbot_brain::routes::node_gateway::NOT_YET)
        .to_string()
}

#[tokio::test]
async fn a_brain_that_refuses_to_pair_is_reported_verbatim() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The status is whatever a refusing brain sends; the body is built by the
    // brain's own `error_body`, so neither half can drift. `NOT_YET` is used
    // here only because it is a sentence the brain really owns.
    let fake = FakeBrain::start(501, stub_body()).await;

    let error = swarmbot_node::cli::pair_command(Some(dir.path()), &fake.url(), "ABCD-EFGH")
        .await
        .expect_err("the gateway is a stub");
    match &error {
        NodeError::PairingRejected { status, reason } => {
            assert_eq!(*status, 501);
            assert_eq!(
                reason,
                swarmbot_brain::routes::node_gateway::NOT_YET,
                "the operator sees the brain's own sentence, not a guess"
            );
        }
        other => panic!("expected PairingRejected, got {other:?}"),
    }
    assert!(
        !NodePaths::new(dir.path()).identity().exists(),
        "a refused pairing writes nothing"
    );
    assert_eq!(
        swarmbot_brain::routes::node_gateway::CLOSE_CODE,
        1011,
        "and the control socket is closed with the code this crate reports"
    );
}

// The full round trip is no longer a recipe waiting for a milestone — it is
// tested where both halves live:
//
// * `crates/brain/tests/node_gateway.rs` (8 tests) drives the real protocol
//   library over a real loopback socket: mint a code through the API →
//   `PairRequest` → dial with a signed upgrade → `node.hello` verified both ways
//   → `fs.stat`/`fs.read`/`shell.exec` chunks with Acks → revoke → 4401 → a
//   redial refused before any upgrade;
// * `target/two-process/run.sh` runs the two real binaries end to end and its
//   transcript is quoted in the milestone report;
// * this crate's own halves are covered by `tests/pairing.rs` (the HTTP
//   round trip), `tests/hello_gate.rs` (nothing is served before a verified
//   `node.hello`) and `tests/redial.rs` (4401 stops redialing permanently).
//
// What none of them cover, stated plainly: a remote tool call in the
// *two-process* run. A real brain process cannot be handed a scripted model
// turn from outside, so no bot in that demo emits a tool call; the call path is
// covered against the real protocol library instead. And `wss://`/TLS has never
// been exercised — there is no TLS-terminating brain here.
