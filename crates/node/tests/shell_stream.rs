//! `shell.exec` as a stream: real chunks, a real pid, a real process group, and
//! the kill that an interrupt, a timeout or a dead socket must cause.
//!
//! Every command here is inert (`echo`, `env`, `sleep`, `exit N`) and runs in
//! its own process group inside a `TempDir`; each test asserts its group is gone
//! afterwards, because a shell test that leaves a process behind is the failure
//! mode this crate exists to prevent.

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::Value;
use serde_json::json;
use swarmbot_node::shell::process_group_gone;
use swarmbot_node_protocol::rpc;
use swarmbot_node_protocol::{Id, ProtocolError, TypedError};
use tokio::sync::mpsc;

/// Run a `shell.exec` stream to completion and return (outcome, chunks).
async fn run_stream(brain: &BrainPeer, params: Value) -> (Result<(), ProtocolError>, Vec<Value>) {
    let (tx, mut rx) = mpsc::channel(256);
    let peer = brain.clone();
    let call = tokio::spawn(async move { peer.call_stream(rpc::SHELL_EXEC, params, tx).await });
    let mut chunks = Vec::new();
    while let Some(value) = rx.recv().await {
        chunks.push(value);
    }
    (call.await.expect("the call task does not panic"), chunks)
}

fn tag(chunk: &Value) -> &str {
    chunk["_tag"].as_str().unwrap_or_default()
}

fn started(chunks: &[Value]) -> (u32, u32) {
    let chunk = chunks
        .iter()
        .find(|chunk| tag(chunk) == "Started")
        .unwrap_or_else(|| panic!("no Started chunk in {chunks:?}"));
    (
        chunk["pid"].as_u64().expect("pid") as u32,
        chunk["pgid"].as_u64().expect("pgid") as u32,
    )
}

fn outputs(chunks: &[Value]) -> Vec<(String, String, u64)> {
    chunks
        .iter()
        .filter(|chunk| tag(chunk) == "Output")
        .map(|chunk| {
            (
                chunk["stream"].as_str().unwrap_or_default().to_string(),
                chunk["data"].as_str().unwrap_or_default().to_string(),
                chunk["seq"].as_u64().unwrap_or_default(),
            )
        })
        .collect()
}

fn exit(chunks: &[Value]) -> Value {
    chunks
        .iter()
        .find(|chunk| tag(chunk) == "Exit")
        .cloned()
        .unwrap_or_else(|| panic!("no Exit chunk in {chunks:?}"))
}

fn stdout_text(chunks: &[Value]) -> String {
    outputs(chunks)
        .into_iter()
        .filter(|(stream, _, _)| stream == "stdout")
        .map(|(_, data, _)| data)
        .collect()
}

/// Wait for a process group to disappear. Reaping is asynchronous, so this polls
/// rather than sleeping once and hoping.
async fn assert_group_gone(pgid: u32) {
    for _ in 0..100 {
        if process_group_gone(pgid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("process group {pgid} is still alive after the call returned");
}

#[tokio::test]
async fn echo_streams_started_then_output_then_exit_zero() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    let (outcome, chunks) = run_stream(&brain, shell("echo hi", 30_000)).await;
    outcome.expect("the stream ends with a result, not an error");

    // The documented order: Started, Output…, Exit.
    assert_eq!(tag(&chunks[0]), "Started");
    assert_eq!(tag(chunks.last().expect("at least one chunk")), "Exit");
    let (pid, pgid) = started(&chunks);
    assert!(pid > 0, "{chunks:?}");
    assert_eq!(
        pid, pgid,
        "`process_group(0)` makes the child its own leader"
    );
    assert_eq!(stdout_text(&chunks), "hi\n");

    let exit = exit(&chunks);
    assert_eq!(exit["code"], json!(0));
    assert_eq!(exit["signal"], json!(null));
    assert_eq!(exit["timedOut"], json!(false));
    assert!(exit["durationMs"].as_u64().is_some(), "{exit}");
    assert_eq!(exit["outputTruncated"], json!(false));
    assert!(exit.get("spillPath").is_none(), "{exit}");

    assert_group_gone(pgid).await;
    assert!(
        side.runtime.registry.is_empty(),
        "the registry is empty again"
    );
}

#[tokio::test]
async fn a_non_zero_exit_is_reported_honestly_and_is_not_an_rpc_error() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    let (outcome, chunks) =
        run_stream(&brain, shell("echo out; echo err >&2; exit 3", 30_000)).await;
    // The command ran and reported its status: that is data, so the RPC
    // succeeded and the exit code lives in the chunk.
    outcome.expect("a non-zero exit is not a protocol error");
    let exit = exit(&chunks);
    assert_eq!(exit["code"], json!(3));
    assert_eq!(exit["timedOut"], json!(false));

    let outputs = outputs(&chunks);
    assert!(
        outputs
            .iter()
            .any(|(stream, data, _)| stream == "stdout" && data.contains("out")),
        "{outputs:?}"
    );
    assert!(
        outputs
            .iter()
            .any(|(stream, data, _)| stream == "stderr" && data.contains("err")),
        "{outputs:?}"
    );
    // `seq` is one counter across both streams, so interleaving can be undone.
    let seqs: Vec<u64> = outputs.iter().map(|(_, _, seq)| *seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "seqs arrive in order: {seqs:?}");
    assert_eq!(
        sorted
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        sorted.len(),
        "no two chunks share a seq: {seqs:?}"
    );
    assert_group_gone(started(&chunks).1).await;
}

#[tokio::test]
async fn a_timeout_kills_the_group_and_says_so_before_the_exit_chunk() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    let started_at = Instant::now();
    let (outcome, chunks) = run_stream(&brain, shell("echo $$; sleep 30 & sleep 30", 300)).await;
    let elapsed = started_at.elapsed();
    outcome.expect("a timeout is reported in the Exit chunk, not as an RPC error");
    assert!(
        elapsed < Duration::from_secs(10),
        "the call returned in {elapsed:?}, not after the command's own 30 s"
    );

    let exit = exit(&chunks);
    assert_eq!(exit["timedOut"], json!(true));
    assert_eq!(exit["signal"], json!(9), "killed with SIGKILL");
    assert_eq!(
        exit["code"],
        json!(null),
        "a signalled process has no exit code"
    );

    // The kill is announced: an Exit with `signal: 9` and no explanation would
    // read like a crash of the command itself.
    let note = outputs(&chunks)
        .iter()
        .find(|(_, data, _)| data.contains("swarmbot-node"))
        .map(|(_, data, _)| data.clone())
        .unwrap_or_default();
    assert!(note.contains("timed out after 300 ms"), "{note}");
    assert!(note.contains("killed process group"), "{note}");

    let (pid, pgid) = started(&chunks);
    assert!(
        pid > 1,
        "the pid came from the Started chunk, not the output"
    );
    assert_group_gone(pgid).await;
}

#[tokio::test]
async fn an_interrupt_kills_the_whole_process_group_and_returns_promptly() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    let (tx, mut rx) = mpsc::channel(256);
    let id = Id::Number(4242);
    let call = {
        let peer = brain.clone();
        let id = id.clone();
        tokio::spawn(async move {
            peer.call_stream_with_id(
                id,
                rpc::SHELL_EXEC,
                shell("sleep 30 & sleep 30", 60_000),
                Vec::new(),
                tx,
            )
            .await
        })
    };

    // Wait for Started so we know the group id before cancelling anything.
    let first = rx.recv().await.expect("a Started chunk");
    assert_eq!(tag(&first), "Started");
    let pid = first["pid"].as_u64().expect("pid") as u32;
    let pgid = first["pgid"].as_u64().expect("pgid") as u32;
    assert_eq!(
        pid, pgid,
        "`process_group(0)` makes the child its own leader"
    );
    assert!(!process_group_gone(pgid), "the command is running");

    let interrupted_at = Instant::now();
    brain.interrupt(&id).await.expect("the interrupt is queued");
    let outcome = tokio::time::timeout(Duration::from_secs(20), call)
        .await
        .expect("the call returns")
        .expect("the task does not panic");
    let elapsed = interrupted_at.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "the interrupted call returned in {elapsed:?} instead of waiting for `sleep 30`"
    );

    // §4.7: the server answers an interrupt with an error frame whose data holds
    // an Interrupt reason — not with a late success.
    match outcome {
        Err(ProtocolError::Remote { error: cause }) => {
            assert!(cause.is_interrupted(), "{cause:?}");
        }
        other => panic!("expected an interrupted remote error, got {other:?}"),
    }

    // The group — including the backgrounded `sleep 30` — is gone.
    assert_group_gone(pgid).await;
    while rx.recv().await.is_some() {}
    assert!(
        side.runtime.registry.is_empty(),
        "an interrupted run releases its registry slot"
    );
}

#[tokio::test]
async fn a_dead_socket_kills_the_running_group() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    let (tx, mut rx) = mpsc::channel(256);
    let call = {
        let peer = brain.clone();
        tokio::spawn(async move {
            peer.call_stream(rpc::SHELL_EXEC, shell("sleep 30 & sleep 30", 60_000), tx)
                .await
        })
    };
    let first = rx.recv().await.expect("a Started chunk");
    let pgid = first["pgid"].as_u64().expect("pgid") as u32;
    assert!(!process_group_gone(pgid), "the command is running");

    // Kill the transport. Dropping the brain's `Peer` handles would *not* do it
    // — a `node_protocol` driver keeps its own command-sender clone, so it never
    // notices that its handles are gone — and in production the socket dies on
    // its own anyway. So this is what a dropped connection looks like from the
    // node's side: `recv_text` reports EOF and every send fails. §3.4 says
    // in-flight work does not survive that.
    side.kill.kill();
    drop(brain);
    // `call.abort()` also drops the `tx` it owns, so the receiver below ends
    // with the connection.
    call.abort();

    let died_at = Instant::now();
    assert_group_gone(pgid).await;
    let elapsed = died_at.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "the group died in {elapsed:?}, not when the command's own 60 s ended"
    );
    while rx.recv().await.is_some() {}
    assert!(
        side.runtime.registry.is_empty(),
        "a disconnected run releases its registry slot"
    );
}

#[tokio::test]
async fn the_environment_is_scrubbed_to_the_allowlist() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    // The test process is started by cargo, so its environment is full of
    // CARGO_*/RUST* variables; none of them may reach a command a model typed.
    let params = json!({
        "inv": inv_value(),
        "command": "env",
        "cwd": "",
        "timeoutMs": 30_000,
        "env": {"SWARMBOT_TEST_PASSED": "yes"},
    });
    let (outcome, chunks) = run_stream(&brain, params).await;
    outcome.expect("env runs");
    let env = stdout_text(&chunks);
    assert!(env.contains("SWARMBOT_TEST_PASSED=yes"), "{env}");
    assert!(env.contains("PATH="), "PATH is allowlisted: {env}");
    for leaked in [
        "CARGO_HOME",
        "CARGO_MANIFEST_DIR",
        "RUSTUP_HOME",
        "CARGO_PKG_NAME",
    ] {
        assert!(
            !env.contains(leaked),
            "{leaked} reached the child; the environment is not scrubbed:\n{env}"
        );
    }
    assert_group_gone(started(&chunks).1).await;
}

#[tokio::test]
async fn the_working_directory_goes_through_the_path_policy() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    // Inside the root: `pwd` reports the resolved directory.
    let inside = json!({
        "inv": inv_value(),
        "command": "pwd",
        "cwd": root_str(&side),
        "timeoutMs": 30_000,
    });
    let (outcome, chunks) = run_stream(&brain, inside).await;
    outcome.expect("pwd runs");
    assert!(stdout_text(&chunks).trim().ends_with("root"), "{chunks:?}");

    // Outside the root: a path denial, and no chunk at all — the command never
    // started, so there is no pid to report.
    let outside = json!({
        "inv": inv_value(),
        "command": "pwd",
        "cwd": "/etc",
        "timeoutMs": 30_000,
    });
    let error = brain
        .call(rpc::SHELL_EXEC, outside)
        .await
        .expect_err("a cwd outside the roots is refused");
    match typed(&error) {
        TypedError::PathDenied { reason, path } => {
            assert_eq!(reason, "outside roots");
            assert!(path.contains("/etc"), "{path}");
        }
        other => panic!("expected PathDenied, got {other:?}"),
    }
    // The shell surface was audited as a refusal, not as a run.
    let audit = std::fs::read_to_string(
        side.runtime
            .paths
            .audit()
            .join(format!("{}.jsonl", side.node_id())),
    )
    .expect("the audit log exists");
    assert!(audit.contains("\"surface\":\"shell\""), "{audit}");
}

#[tokio::test]
async fn output_beyond_the_cap_is_spilled_and_the_exit_chunk_says_where() {
    let (side, socket) = node_pair(permissive_config);
    let brain = brain_peer(socket, ApprovalBrain::allowing().shared());
    authenticate(&brain, &side).await;

    // A small explicit cap keeps the test cheap; the default is 1 MiB.
    let params = json!({
        "inv": inv_value(),
        "command": "for i in $(seq 1 400); do echo line-$i; done",
        "cwd": "",
        "timeoutMs": 30_000,
        "outputLimitBytes": 64,
    });
    let (outcome, chunks) = run_stream(&brain, params).await;
    outcome.expect("the run completes");
    let exit = exit(&chunks);
    assert_eq!(exit["code"], json!(0));
    assert_eq!(exit["outputTruncated"], json!(true), "{exit}");
    let spill = exit["spillPath"]
        .as_str()
        .unwrap_or_else(|| panic!("the spill path is reported: {exit}"))
        .to_string();
    let spilled = std::fs::read_to_string(&spill)
        .unwrap_or_else(|error| panic!("{spill} is not readable: {error}"));
    assert!(!spilled.is_empty(), "the overflow was written somewhere");
    // Only what fit the cap was streamed.
    assert!(stdout_text(&chunks).len() <= 64, "{chunks:?}");
    assert!(
        spill.starts_with(&side.runtime.paths.spill().display().to_string()),
        "the spill stays inside the node's spill directory: {spill}"
    );
    assert_group_gone(started(&chunks).1).await;
}
