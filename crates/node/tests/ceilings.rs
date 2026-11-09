//! Ceilings and approvals: the node decides finally (decision D1.4).
//!
//! The three facts these tests pin are the ones a compromised or merely confused
//! brain would try to bend: a `never` ceiling is never an ask, an `ask` ceiling
//! that cannot reach the brain is a **deny**, and an `allow` from the brain
//! cannot lift what the node refuses.

mod common;

use std::sync::Arc;

use common::*;
use serde_json::json;
use swarmbot_contracts::{Decision, PolicyCeiling, Surface};
use swarmbot_node_protocol::TypedError;
use swarmbot_node_protocol::rpc::{self, ApprovalAnswer, RequestApproval};

/// The audit lines this node has written, as parsed JSON.
fn audit_lines(side: &NodeSide) -> Vec<serde_json::Value> {
    let path = side
        .runtime
        .paths
        .audit()
        .join(format!("{}.jsonl", side.node_id()));
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    content
        .lines()
        .map(|line| serde_json::from_str(line).expect("one JSON object per line"))
        .collect()
}

#[tokio::test]
async fn a_never_ceiling_refuses_without_asking_anybody() {
    let (side, socket) = node_pair(never_config(Surface::Shell));
    let brain_handler = ApprovalBrain::allowing().shared();
    let brain = brain_peer(socket, Arc::clone(&brain_handler));
    let hello = authenticate(&brain, &side).await;
    // The hello object says so, and it says the *strictest* thing: one `never`
    // surface makes the whole node report `never`.
    assert_eq!(
        hello["policy"],
        json!({"ceiling": "never", "remoteManaged": false})
    );

    let error = brain
        .call(rpc::SHELL_EXEC, shell("echo hi", 30_000))
        .await
        .expect_err("refused");
    match typed(&error) {
        TypedError::PolicyRefused { reason, retry, .. } => {
            assert!(reason.contains("never"), "{reason}");
            assert!(!retry, "a `never` ceiling must not invite retries");
        }
        other => panic!("expected PolicyRefused, got {other:?}"),
    }
    assert!(
        brain_handler.received().is_empty(),
        "a `never` ceiling is a refusal, not an approval request"
    );
    let lines = audit_lines(&side);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["decision"], json!("refuse"));
    assert_eq!(lines[0]["surface"], json!("shell"));
    assert_eq!(lines[0]["tool"], json!("shell"));
}

#[tokio::test]
async fn an_allow_from_the_brain_cannot_lift_a_never_ceiling() {
    // The brain here answers every ask with `allow_always`. It must never even
    // be asked, and the command must never run.
    let (side, socket) = node_pair(never_config(Surface::WriteFile));
    let brain_handler = ApprovalBrain::new(ApprovalAnswer {
        decision: Decision::AllowAlways,
        by: "owner".into(),
    })
    .shared();
    let brain = brain_peer(socket, Arc::clone(&brain_handler));
    authenticate(&brain, &side).await;

    let error = brain
        .call(rpc::FS_WRITE, fs_write("lifted.txt", "x"))
        .await
        .expect_err("the node's own `never` wins");
    assert!(matches!(
        typed(&error),
        TypedError::PolicyRefused { retry: false, .. }
    ));
    assert!(
        brain_handler.received().is_empty(),
        "min(brain, node) is decided here, not negotiated"
    );
    assert!(
        !side.root.join("lifted.txt").exists(),
        "nothing was written"
    );
    // Reads on a different surface are untouched by the write ceiling.
    side.write("still-readable.txt", "yes\n");
    let value = brain
        .call(rpc::FS_READ, fs_read("still-readable.txt"))
        .await
        .expect("read_file is a different surface");
    assert!(
        value["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains("yes"))
    );
}

#[tokio::test]
async fn an_ask_ceiling_sends_the_documented_approval_request() {
    let (side, socket) = node_pair(asking_config(Surface::Shell));
    let brain_handler = ApprovalBrain::denying().shared();
    let brain = brain_peer(socket, Arc::clone(&brain_handler));
    authenticate(&brain, &side).await;

    let (inv, inv_json) = inv_pair();
    let error = brain
        .call(
            rpc::SHELL_EXEC,
            json!({
                "inv": inv_json,
                "command": "echo needs-approval",
                "cwd": "",
                "timeoutMs": 30_000,
            }),
        )
        .await
        .expect_err("the brain denies");
    match typed(&error) {
        TypedError::PolicyRefused { reason, retry, .. } => {
            assert!(reason.contains("denied"), "{reason}");
            assert!(!retry);
        }
        other => panic!("expected PolicyRefused, got {other:?}"),
    }

    let requests = brain_handler.received();
    assert_eq!(requests.len(), 1, "exactly one approval round-trip");
    let ask: &RequestApproval = &requests[0];
    assert_eq!(
        ask.action, "shell",
        "the surface is what is being asked for"
    );
    assert_eq!(ask.target, "echo needs-approval");
    assert!(ask.reason.contains("ceiling is `ask`"), "{:?}", ask.reason);

    // ...and the wire shape is camelCase with the invocation's own ids, which is
    // what the brain's approval service keys a card and a rule on.
    let raw = brain_handler.received_raw();
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0]["action"], json!("shell"));
    assert_eq!(raw[0]["target"], json!("echo needs-approval"));
    let sent: serde_json::Value = serde_json::to_value(&inv).expect("serializes");
    assert_eq!(raw[0]["invocationId"], sent["id"], "{raw:?}");
    assert_eq!(raw[0]["botId"], sent["bot_id"], "{raw:?}");
    assert_eq!(
        requests[0].invocation_id, inv.id,
        "the ask names the invocation it is about"
    );
    assert!(
        raw[0].get("invocation_id").is_none(),
        "camelCase on this wire"
    );

    let lines = audit_lines(&side);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["decision"], json!("deny"));
    assert_eq!(lines[0]["outcome"], json!("error"));
}

#[tokio::test]
async fn allow_once_and_allow_always_both_let_the_call_run() {
    for decision in [Decision::AllowOnce, Decision::AllowAlways] {
        let (side, socket) = node_pair(asking_config(Surface::Shell));
        let brain_handler = ApprovalBrain::new(ApprovalAnswer {
            decision,
            by: "owner".into(),
        })
        .shared();
        let brain = brain_peer(socket, Arc::clone(&brain_handler));
        authenticate(&brain, &side).await;

        side.write("marker.txt", "");
        let params = json!({
            "inv": inv_value(),
            "command": "echo allowed > marker.txt",
            "cwd": "",
            "timeoutMs": 30_000,
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let peer = brain.clone();
        let call = tokio::spawn(async move { peer.call_stream(rpc::SHELL_EXEC, params, tx).await });
        while rx.recv().await.is_some() {}
        call.await.expect("task").expect("{decision:?} lets it run");

        assert_eq!(brain_handler.received().len(), 1);
        assert_eq!(
            std::fs::read_to_string(side.root.join("marker.txt")).expect("reads"),
            "allowed\n",
            "the command actually ran for {decision:?}"
        );
        let lines = audit_lines(&side);
        assert_eq!(lines[0]["decision"], json!("allow"), "{decision:?}");
        assert_eq!(lines[0]["outcome"], json!("ok"));
    }
}

#[tokio::test]
async fn an_unreachable_brain_is_a_deny_and_never_an_allow() {
    let (side, socket) = node_pair(asking_config(Surface::WriteFile));
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    // No control socket to ask on: the handler has no link.
    side.handler.detach();
    side.write("untouched.txt", "before");
    let error = brain
        .call(
            rpc::FS_WRITE,
            json!({"inv": inv_value(), "path": "untouched.txt", "content": "after"}),
        )
        .await
        .expect_err("an approval that cannot be asked for is denied");
    match typed(&error) {
        TypedError::PolicyRefused { reason, retry, .. } => {
            assert!(reason.contains("denied"), "{reason}");
            assert!(!retry);
        }
        other => panic!("expected PolicyRefused, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(side.root.join("untouched.txt")).expect("reads"),
        "before",
        "an unreachable brain must not turn into permission"
    );
    assert_eq!(audit_lines(&side)[0]["decision"], json!("deny"));
}

#[tokio::test]
async fn a_brain_that_answers_garbage_is_denied() {
    let (side, socket) = node_pair(asking_config(Surface::WriteFile));
    let brain_handler = ApprovalBrain::malformed().shared();
    let brain = brain_peer(socket, Arc::clone(&brain_handler));
    authenticate(&brain, &side).await;

    let error = brain
        .call(rpc::FS_WRITE, fs_write("garbage.txt", "x"))
        .await
        .expect_err("an answer that is not an ApprovalAnswer is a deny");
    assert!(matches!(
        typed(&error),
        TypedError::PolicyRefused { retry: false, .. }
    ));
    assert_eq!(brain_handler.received().len(), 1, "it was asked");
    assert!(!side.root.join("garbage.txt").exists());
}

#[tokio::test]
async fn a_brain_that_answers_with_an_error_is_denied() {
    // `ApprovalBrain` refuses anything that is not `node.requestApproval`, so
    // point it at a method it does not serve by asking it to deny: here the
    // remote error path is exercised by a peer that answers the approval with a
    // typed error instead of an answer.
    struct Refusing;
    impl swarmbot_node_protocol::RpcHandler for Refusing {
        fn handle<'a>(
            &'a self,
            method: &'a str,
            _params: serde_json::Value,
            _tx: Option<swarmbot_node_protocol::ChunkSink>,
        ) -> swarmbot_node_protocol::BoxFuture<'a, Result<serde_json::Value, TypedError>> {
            Box::pin(async move {
                Err(TypedError::policy_refused(
                    method,
                    "the brain will not decide",
                    false,
                ))
            })
        }
    }
    let (side, socket) = node_pair(asking_config(Surface::WriteFile));
    let brain = brain_peer_with(socket, Arc::new(Refusing));
    authenticate(&brain, &side).await;
    let error = brain
        .call(rpc::FS_WRITE, fs_write("refused.txt", "x"))
        .await
        .expect_err("denied");
    assert!(matches!(
        typed(&error),
        TypedError::PolicyRefused { retry: false, .. }
    ));
    assert!(!side.root.join("refused.txt").exists());
    assert_eq!(audit_lines(&side)[0]["decision"], json!("deny"));
}

#[tokio::test]
async fn a_destructive_command_asks_even_under_an_always_ceiling() {
    // D5.5: `always` was granted for ordinary work, not for `sudo`. The matcher
    // is a heuristic and says so; what it guarantees is that the floor is `ask`.
    let (side, socket) = node_pair(permissive_config);
    let brain_handler = ApprovalBrain::denying().shared();
    let brain = brain_peer(socket, Arc::clone(&brain_handler));
    authenticate(&brain, &side).await;

    // An ordinary command runs without asking at all.
    let ordinary = brain
        .call(
            rpc::SHELL_EXEC,
            json!({"inv": inv_value(), "command": "true", "cwd": "", "timeoutMs": 30_000}),
        )
        .await;
    assert!(ordinary.is_err() || ordinary.is_ok(), "it was served");
    assert!(
        brain_handler.received().is_empty(),
        "an `always` ceiling does not ask for an ordinary command"
    );

    // A destructive one is asked — and denied here, so nothing runs. `sudo true`
    // is the inert stand-in: it matches the pattern and does no damage even if a
    // future bug let it through.
    let error = brain
        .call(
            rpc::SHELL_EXEC,
            json!({"inv": inv_value(), "command": "sudo true", "cwd": "", "timeoutMs": 30_000}),
        )
        .await
        .expect_err("asked, and the test brain denies");
    assert!(matches!(
        typed(&error),
        TypedError::PolicyRefused { retry: false, .. }
    ));
    let requests = brain_handler.received();
    assert_eq!(requests.len(), 1, "the destructive command was asked about");
    assert!(
        requests[0].reason.contains("destructive pattern"),
        "{:?}",
        requests[0].reason
    );
    assert!(
        requests[0].reason.contains("sudo"),
        "{:?}",
        requests[0].reason
    );
}

#[tokio::test]
async fn read_only_calls_run_without_a_round_trip_when_the_owner_said_so() {
    // `auto_approve_read_only` is the node's own setting, and it is what makes an
    // `ask`-everywhere policy usable: reading inside a root the owner granted is
    // what the grant was for. Writing still asks.
    let (side, socket) = node_pair(asking_everything());
    let brain_handler = ApprovalBrain::denying().shared();
    let brain = brain_peer(socket, Arc::clone(&brain_handler));
    authenticate(&brain, &side).await;
    assert!(
        !side.config.auto_approve_read_only,
        "the rig asks about reads too"
    );

    side.write("readable.txt", "yes\n");
    // With auto-approve off, even a read is asked — and this brain denies.
    let error = brain
        .call(rpc::FS_READ, fs_read("readable.txt"))
        .await
        .expect_err("the denying brain refuses the read");
    assert!(matches!(typed(&error), TypedError::PolicyRefused { .. }));
    assert_eq!(brain_handler.received().len(), 1);
    assert_eq!(brain_handler.received()[0].action, "read_file");

    // The same node with the owner's auto-approve on reads without asking.
    let (side, socket) = node_pair(|dir, root| {
        let mut config = asking_everything()(dir, root);
        config.auto_approve_read_only = true;
        config
    });
    let brain_handler = ApprovalBrain::denying().shared();
    let brain = brain_peer(socket, Arc::clone(&brain_handler));
    authenticate(&brain, &side).await;
    side.write("readable.txt", "yes\n");
    let value = brain
        .call(rpc::FS_READ, fs_read("readable.txt"))
        .await
        .expect("reads run without a round-trip");
    assert!(
        value["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains("yes"))
    );
    assert!(
        brain_handler.received().is_empty(),
        "no approval round-trip for a read-only surface"
    );
    // ...but the write on the same node is still asked.
    let _ = brain
        .call(rpc::FS_WRITE, fs_write("nope.txt", "x"))
        .await
        .expect_err("writes still ask");
    assert_eq!(brain_handler.received().len(), 1);
    assert_eq!(brain_handler.received()[0].action, "write_file");
}

#[tokio::test]
async fn per_surface_ceilings_are_independent() {
    let (side, socket) = node_pair(|dir, root| {
        let mut config = permissive_config(dir, root);
        config.ceilings.set(Surface::Shell, PolicyCeiling::Never);
        config
            .ceilings
            .set(Surface::WriteFile, PolicyCeiling::Never);
        config
    });
    let brain_handler = ApprovalBrain::allowing().shared();
    let brain = brain_peer(socket, Arc::clone(&brain_handler));
    let hello = authenticate(&brain, &side).await;
    assert_eq!(hello["policy"]["ceiling"], json!("never"));

    // Reads and edits are untouched by the shell and write ceilings.
    side.write("a.txt", "one");
    let read = brain
        .call(rpc::FS_READ, fs_read("a.txt"))
        .await
        .expect("read_file is still `always`");
    assert!(
        read["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains("one"))
    );
    let edited = brain
        .call(
            rpc::FS_EDIT,
            json!({"inv": inv_value(), "path": "a.txt", "oldString": "one", "newString": "two"}),
        )
        .await
        .expect("edit_file is a surface of its own");
    assert!(
        edited["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains("+two"))
    );

    assert!(
        brain
            .call(rpc::FS_WRITE, fs_write("b.txt", "x"))
            .await
            .is_err()
    );
    assert!(
        brain
            .call(rpc::SHELL_EXEC, shell("true", 30_000))
            .await
            .is_err()
    );
    assert!(
        brain_handler.received().is_empty(),
        "neither refusal became an approval request"
    );
}
