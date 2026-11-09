//! The `node.hello` gate: nothing runs until the brain proves it holds the key
//! this node pinned at pairing (decision D1.3).
//!
//! All of this runs over an in-memory socket pair — no ports, no network.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use serde_json::Value;
use serde_json::json;
use swarmbot_contracts::{NodeId, ProtocolRange};
use swarmbot_node::handlers::NodeHandler;
use swarmbot_node_protocol::rpc::{self, UNIMPLEMENTED_METHODS};
use swarmbot_node_protocol::{CLOSE_HELLO_FAILED, CLOSE_NO_PROTOCOL, TypedError};

/// Every method a brain could send, taken from the protocol's own tables rather
/// than a hand-picked sample: if the table grows, this test covers the new row
/// without being edited.
fn all_methods() -> Vec<&'static str> {
    let mut methods = rpc::IMPLEMENTED_METHODS
        .iter()
        .copied()
        .filter(|method| *method != rpc::NODE_HELLO)
        .collect::<Vec<_>>();
    methods.extend(UNIMPLEMENTED_METHODS.iter().copied());
    methods.push("totally.made.up");
    methods
}

#[tokio::test]
async fn every_method_is_not_authenticated_before_a_verified_hello() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());

    for method in all_methods() {
        let params: Value = json!({"inv": inv_value()});
        match brain.call(method, params).await {
            Ok(value) => panic!("{method} answered {value} before node.hello"),
            Err(error) => match typed(&error) {
                TypedError::NotAuthenticated { reason } => {
                    assert!(reason.contains("node.hello"), "{method}: {reason}");
                }
                other => panic!("{method}: expected NotAuthenticated, got {other:?}"),
            },
        }
    }
    assert!(
        !side.handler.is_authenticated(),
        "nothing may have flipped the gate"
    );
}

#[tokio::test]
async fn a_verified_hello_flips_the_gate_and_reports_the_truth() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());

    let hello = authenticate(&brain, &side).await;
    assert!(side.handler.is_authenticated());

    // Identity facts.
    assert_eq!(hello["nodeId"], json!(side.node_id().to_string()));
    assert_eq!(
        hello["protocol"],
        json!(swarmbot_node_protocol::PROTOCOL_VERSION)
    );
    assert_eq!(hello["platform"], json!(std::env::consts::OS));
    assert_eq!(hello["arch"], json!(std::env::consts::ARCH));
    assert!(!hello["hostname"].as_str().unwrap_or_default().is_empty());
    assert!(!hello["version"].as_str().unwrap_or_default().is_empty());

    // Capabilities are honest: fs and shell, and nothing this build lacks.
    assert_eq!(hello["capabilities"], json!(["fs@1", "shell@1"]));
    let caps = hello["capabilities"].as_array().expect("capabilities");
    for absent in ["desktop", "browser", "pty", "screen", "computer", "tunnel"] {
        let declared = caps
            .iter()
            .any(|cap| cap.as_str().is_some_and(|name| name.starts_with(absent)));
        assert!(!declared, "{absent} must never be declared: {hello}");
    }
    assert!(
        hello.get("tunnels").is_none(),
        "an empty tunnel list stays off the wire: {hello}"
    );

    // Roots are the ones this node was configured with, resolved.
    let roots = hello["roots"].as_array().expect("roots");
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0]["id"], json!("root"));
    assert_eq!(roots[0]["mode"], json!("read_write"));
    assert!(
        roots[0]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("root")),
        "{roots:?}"
    );

    // The policy object reports the node's own ceiling and admits that the
    // brain does not manage it.
    assert_eq!(
        hello["policy"],
        json!({"ceiling": "always", "remoteManaged": false})
    );
    assert_eq!(root_mode(&hello, "root").as_deref(), Some("read_write"));

    // ...and the gate is genuinely open: the call that was refused above runs.
    side.write("hello.txt", "hi\n");
    let value = brain
        .call(rpc::FS_READ, fs_read("hello.txt"))
        .await
        .expect("the gate is open");
    assert!(
        value["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("hi")),
        "{value}"
    );
}

#[tokio::test]
async fn shell_withdrawal_shows_up_in_the_hello_capabilities() {
    let (side, socket) = node_pair(|dir, root| {
        let mut config = permissive_config(dir, root);
        config.shell_enabled = false;
        config
    });
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    let hello = authenticate(&brain, &side).await;
    assert_eq!(
        hello["capabilities"],
        json!(["fs@1"]),
        "--no-shell must withdraw shell@1 from the declaration"
    );
    // A call that arrives anyway is refused rather than executed.
    let error = brain
        .call(rpc::SHELL_EXEC, shell("echo hi", 5_000))
        .await
        .expect_err("refused");
    match typed(&error) {
        TypedError::PolicyRefused { reason, retry, .. } => {
            assert!(reason.contains("--no-shell"), "{reason}");
            assert!(!retry);
        }
        other => panic!("expected PolicyRefused, got {other:?}"),
    }
}

#[tokio::test]
async fn a_hello_signed_by_the_wrong_key_is_refused_and_closes_4403() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());

    let hijack = hello_params(&wrong_brain_key(), side.node_id(), NONCE);
    let error = brain
        .call(rpc::NODE_HELLO, hijack)
        .await
        .expect_err("a signature from another key must not verify");
    match typed(&error) {
        TypedError::NotAuthenticated { reason } => {
            assert!(reason.contains("pinned at pairing"), "{reason}");
        }
        other => panic!("expected NotAuthenticated, got {other:?}"),
    }

    // The socket must not stay open: leaving it up would let the peer keep
    // probing for a signature that works.
    tokio::time::timeout(Duration::from_secs(5), brain.closed())
        .await
        .expect("the node closed the control socket");
    let closes = recorded_closes(&side.closes);
    assert_eq!(
        closes.first().map(|(code, _)| *code),
        Some(CLOSE_HELLO_FAILED),
        "expected a 4403 close, got {closes:?}"
    );
    assert!(!side.handler.is_authenticated(), "the gate stayed shut");
}

#[tokio::test]
async fn a_signature_over_another_nonce_or_node_does_not_verify() {
    // The signature is bound to *this connection's* nonce and to *this* node's
    // id, so one captured hello cannot be replayed on another connection.
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());

    let other_nonce = hello_params(&brain_key(), side.node_id(), "BBBBBBBBBBBBBBBBBBBBBg");
    let error = brain
        .call(rpc::NODE_HELLO, other_nonce)
        .await
        .expect_err("the nonce is part of the signed message");
    assert!(
        matches!(typed(&error), TypedError::NotAuthenticated { .. }),
        "{error:?}"
    );
    assert!(!side.handler.is_authenticated());
}

#[tokio::test]
async fn a_hello_signed_for_another_node_is_refused() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    let somebody_else = NodeId::new();
    assert_ne!(&somebody_else, side.node_id());

    let error = brain
        .call(
            rpc::NODE_HELLO,
            hello_params(&brain_key(), &somebody_else, NONCE),
        )
        .await
        .expect_err("the node id is part of the signed message");
    assert!(
        matches!(typed(&error), TypedError::NotAuthenticated { .. }),
        "{error:?}"
    );
    assert!(!side.handler.is_authenticated());
}

#[tokio::test]
async fn no_common_protocol_version_closes_4426() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());

    let future = hello_params_ranged(
        &brain_key(),
        side.node_id(),
        NONCE,
        ProtocolRange { min: 5, max: 6 },
    );
    let error = brain
        .call(rpc::NODE_HELLO, future)
        .await
        .expect_err("there is nothing to negotiate");
    match typed(&error) {
        TypedError::PolicyRefused { reason, retry, .. } => {
            assert!(reason.contains("no common protocol version"), "{reason}");
            assert!(!retry);
        }
        other => panic!("expected PolicyRefused, got {other:?}"),
    }
    tokio::time::timeout(Duration::from_secs(5), brain.closed())
        .await
        .expect("the node closed the control socket");
    assert_eq!(
        recorded_closes(&side.closes).first().map(|(code, _)| *code),
        Some(CLOSE_NO_PROTOCOL)
    );
    assert!(!side.handler.is_authenticated());
}

#[tokio::test]
async fn a_malformed_hello_is_a_typed_refusal_not_a_panic() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());

    // Note what is *not* here: `params: {}` is rejected by the frame parser
    // (§4.8) before it ever reaches a handler, so the caller gets no answer at
    // all and waits for its liveness timeout. That is `node-protocol`'s
    // behaviour, not this crate's, and its own tests pin it.
    for params in [
        json!(null),
        json!({"protocol": {"min": 1, "max": 1}}),
        json!({"protocol": {"min": 1, "max": 1}, "brainVersion": "0.1.0", "brainSig": 3}),
        json!({"protocol": {"min": 1}, "brainVersion": "0.1.0", "brainSig": "c2ln"}),
    ] {
        let error = brain
            .call(rpc::NODE_HELLO, params.clone())
            .await
            .expect_err("a malformed challenge is a refusal, not an answer");
        match typed(&error) {
            TypedError::PolicyRefused { reason, retry, .. } => {
                assert!(reason.contains("malformed params"), "{params}: {reason}");
                assert!(!retry, "the brain built the payload; retrying cannot help");
            }
            other => panic!("{params}: expected PolicyRefused, got {other:?}"),
        }
    }
    assert!(!side.handler.is_authenticated());
}

#[tokio::test]
async fn the_gate_is_per_connection_not_per_daemon() {
    // A verified hello proves the peer on *that* socket holds the key, not that
    // every future peer will: a second connection over the same runtime starts
    // behind the gate again.
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;
    assert!(side.handler.is_authenticated());

    let second = NodeHandler::new(Arc::clone(&side.runtime), NONCE);
    assert!(
        !second.is_authenticated(),
        "a fresh handler must start behind the gate"
    );
    // It shares the runtime — the same roots, the same audit log — so this is
    // the same daemon with a new connection, not a new node.
    assert_eq!(
        second.runtime().identity.node_id().as_str(),
        side.node_id().as_str()
    );
}
