//! Refusals: what this build answers for what it does not do.
//!
//! An unimplemented capability is a typed `PolicyRefused { retry: false }`, never
//! a fake success and never a silent drop (decision D0.4, protocol doc §9). The
//! list is the protocol's own `UNIMPLEMENTED_METHODS` table, looped over rather
//! than transcribed, so a new row there is covered here without an edit.

mod common;

use common::*;
use serde_json::json;
use swarmbot_node_protocol::TypedError;
use swarmbot_node_protocol::rpc::{self, NOT_IMPLEMENTED_REASON};

#[tokio::test]
async fn every_method_this_build_lacks_is_a_typed_refusal() {
    // Looped over the protocol's own table, not a hand-written list: a new row
    // in `UNIMPLEMENTED_METHODS` is covered without editing this test.
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    for method in rpc::UNIMPLEMENTED_METHODS {
        let error = brain
            .call(method, json!({"inv": inv_value()}))
            .await
            .expect_err("a missing capability is a refusal");
        match typed(&error) {
            TypedError::PolicyRefused {
                action,
                reason,
                retry,
            } => {
                assert_eq!(action, *method);
                assert_eq!(reason, NOT_IMPLEMENTED_REASON);
                assert!(!retry, "a missing capability must never invite retries");
            }
            other => panic!("{method}: expected PolicyRefused, got {other:?}"),
        }
    }
    // A name nobody ever defined is an unknown method, which is a different
    // answer from "we know this one and do not do it".
    let error = brain
        .call("totally.made.up", json!({"inv": inv_value()}))
        .await
        .expect_err("unknown");
    match typed(&error) {
        TypedError::PolicyRefused { reason, retry, .. } => {
            assert_eq!(reason, "unknown method");
            assert!(!retry);
        }
        other => panic!("expected PolicyRefused, got {other:?}"),
    }
    // And the brain's own methods, sent to a node, say whose they are.
    for method in [
        rpc::NODE_REQUEST_APPROVAL,
        rpc::NODE_EVENT,
        rpc::NODE_HEARTBEAT,
    ] {
        let error = brain
            .call(method, json!({"inv": inv_value()}))
            .await
            .expect_err("a node does not serve the brain's methods");
        match typed(&error) {
            TypedError::PolicyRefused { reason, retry, .. } => {
                assert!(reason.contains("served by the brain"), "{reason}");
                assert!(!retry);
            }
            other => panic!("{method}: expected PolicyRefused, got {other:?}"),
        }
    }
}
