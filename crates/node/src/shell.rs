//! The `shell.exec` streaming adapter: this crate's events onto the wire.
//!
//! # One implementation, in `swarmbot_tools`
//!
//! The spawn/pump/killpg/spill machinery lives in exactly one place:
//! [`swarmbot_tools::handlers::shell::exec_shell_streaming`]. This module is the
//! adapter that turns its [`ShellEvent`]s into the wire's [`ShellChunk`] JSON
//! and hands them to a [`ChunkSink`], whose `send` awaits the brain's Ack — that await is the back-pressure keeping a chatty command
//! from outrunning the socket.
//!
//! The split used to be a *copy*: this file mirrored the tools implementation
//! because `exec_shell` was `pub(crate)` and its buffered `ToolResult` cannot
//! produce a pid, incremental chunks or a verifiable `killpg`. Keeping two
//! spawn/kill/spill implementations in agreement by hand is a promise that rots
//! silently, so the streaming implementation moved into `tools` and the
//! invariants (own process group, group-kill on timeout/cancel/transport
//! failure/shutdown, scrubbed environment, per-stream cap with a 0600 spill
//! file) are now one body of code. What stays here is genuinely wire-specific:
//! the chunk shapes, the `[swarmbot-node]` diagnostic prefix, and the ordering
//! guarantee that `Started` reaches the brain before any output (the shared
//! runner sends that event first; this adapter must not reorder it).
//!
//! # Limits, stated plainly
//!
//! `seq` orders chunks but nothing reassembles a multi-byte character split
//! across two reads by the *kernel*: the shared implementation carries the
//! incomplete tail over to the next read, so a UTF-8 character is never cut in
//! half, and an invalid byte becomes U+FFFD rather than stalling the stream.
//! `data` is text, never base64, so a binary-producing command yields
//! replacement characters — the protocol document pins this, and a model
//! consumes text.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use swarmbot_node_protocol::rpc::{OutputStream, ShellChunk};
use swarmbot_node_protocol::{ChunkSink, ProtocolError};

pub use swarmbot_tools::handlers::shell::{
    DEFAULT_SHELL_TIMEOUT_MS, DRAIN_TIMEOUT, ENV_ALLOWLIST, EXIT_GRACE, ExitInfo, FALLBACK_PATH,
    MAX_SHELL_TIMEOUT_MS, ProcGuard, ProcRegistry, SETTLE, ShellEvent, ShellEventSink, ShellExit,
    ShellFailure, ShellOptions, ShellRun, ShellStream, build_shell_command, exec_shell_streaming,
    kill_process_group, process_group_gone,
};

/// Run `command` and stream it to `sink` as wire chunks.
///
/// `cwd` must already be resolved through this node's
/// [`PathPolicy`](swarmbot_tools::PathPolicy), and this node's ceiling must
/// already be settled — this function executes, it does not decide.
pub async fn run_streaming(
    command: &str,
    cwd: &Path,
    timeout_ms: u64,
    env: &BTreeMap<String, String>,
    options: &ShellOptions,
    sink: &ChunkSink,
) -> Result<ShellRun, ShellFailure> {
    let adapter: Arc<dyn ShellEventSink> = Arc::new(ChunkSinkAdapter { sink: sink.clone() });
    exec_shell_streaming(command, cwd, timeout_ms, env, options, adapter).await
}

/// [`ShellEventSink`] over a [`ChunkSink`]: one `send` per chunk, awaited until
/// the brain Acks it. Nothing is buffered here — the shared runner's emitter
/// already serializes the two pumps, and a second queue would only hide a slow
/// reader.
struct ChunkSinkAdapter {
    sink: ChunkSink,
}

impl ShellEventSink for ChunkSinkAdapter {
    fn send<'a>(
        &'a self,
        event: ShellEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ShellFailure>> + Send + 'a>>
    {
        let sink = self.sink.clone();
        Box::pin(async move {
            for chunk in chunks_of(event) {
                sink.send(chunk)
                    .await
                    .map_err(|error: ProtocolError| ShellFailure::Transport {
                        reason: error.to_string(),
                    })?;
            }
            Ok(())
        })
    }

    fn cancelled(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let token: swarmbot_node_protocol::CancellationToken = self.sink.cancellation().clone();
        Box::pin(async move { token.cancelled().await })
    }
}

/// One [`ShellEvent`] as the wire's chunks, in the order they must be sent.
///
/// A [`ShellEvent::Note`] (the reason a run was killed) goes out as a stderr
/// chunk rather than as part of the `Exit`, because the `Exit` shape has no room
/// for a reason and a silent kill would read like a crash. The
/// `[swarmbot-node]` prefix says which side is talking: this text is a
/// diagnostic from the node, not output the command produced.
fn chunks_of(event: ShellEvent) -> Vec<Value> {
    match event {
        ShellEvent::Started { pid, pgid } => vec![json!(ShellChunk::Started { pid, pgid })],
        ShellEvent::Output { stream, data, seq } => vec![json!(ShellChunk::Output {
            stream: wire_stream(stream),
            data,
            seq,
        })],
        ShellEvent::Note { data, seq } => vec![json!(ShellChunk::Output {
            stream: OutputStream::Stderr,
            data: format!("[swarmbot-node] {data}\n"),
            seq,
        })],
        ShellEvent::Exit(exit) => vec![json!(ShellChunk::Exit {
            code: exit.code,
            signal: exit.signal,
            timed_out: exit.timed_out,
            duration_ms: exit.duration_ms,
            output_truncated: Some(exit.output_truncated),
            spill_path: exit.spill_path.map(|path| path.display().to_string()),
        })],
    }
}

fn wire_stream(stream: ShellStream) -> OutputStream {
    match stream {
        ShellStream::Stdout => OutputStream::Stdout,
        ShellStream::Stderr => OutputStream::Stderr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_killed_run_reports_its_reason_before_the_exit_chunk() {
        // The ordering is the point: an `Exit` with no explanation reads as a
        // crash, and the brain renders the stderr chunk it arrives with.
        let note = chunks_of(ShellEvent::Note {
            data: "timed out after 300 ms; killed process group 4242".into(),
            seq: 4,
        });
        assert_eq!(note.len(), 1, "{note:?}");
        assert_eq!(note[0]["_tag"], json!("Output"));
        assert_eq!(note[0]["stream"], json!("stderr"));
        assert_eq!(note[0]["seq"], json!(4));
        assert!(
            note[0]["data"]
                .as_str()
                .unwrap_or_default()
                .starts_with("[swarmbot-node] timed out after 300 ms"),
            "{:?}",
            note[0]
        );
        let exit = chunks_of(ShellEvent::Exit(ShellExit {
            code: None,
            signal: Some(libc::SIGKILL),
            timed_out: true,
            duration_ms: 12,
            output_truncated: false,
            spill_path: None,
            killed: Some("timed out after 300 ms; killed process group 4242".into()),
        }));
        assert_eq!(exit.len(), 1, "{exit:?}");
        assert_eq!(exit[0]["_tag"], json!("Exit"));
        assert_eq!(exit[0]["timedOut"], json!(true));
        assert_eq!(exit[0]["signal"], json!(libc::SIGKILL));
        assert_eq!(exit[0]["outputTruncated"], json!(false));
        // A clean exit is one chunk: no diagnostic, nothing invented.
        let clean = chunks_of(ShellEvent::Exit(ShellExit {
            code: Some(0),
            signal: None,
            timed_out: false,
            duration_ms: 3,
            output_truncated: false,
            spill_path: None,
            killed: None,
        }));
        assert_eq!(clean.len(), 1, "{clean:?}");
        assert_eq!(clean[0]["_tag"], json!("Exit"));
        assert_eq!(clean[0]["code"], json!(0));
    }

    #[test]
    fn a_spilled_run_names_the_file_in_the_exit_chunk() {
        let chunks = chunks_of(ShellEvent::Exit(ShellExit {
            code: Some(0),
            signal: None,
            timed_out: false,
            duration_ms: 5,
            output_truncated: true,
            spill_path: Some("/tmp/spill/inv-stdout.log".into()),
            killed: None,
        }));
        assert_eq!(chunks[0]["outputTruncated"], json!(true));
        assert_eq!(chunks[0]["spillPath"], json!("/tmp/spill/inv-stdout.log"));
    }

    #[test]
    fn started_and_output_keep_their_wire_shape() {
        let started = chunks_of(ShellEvent::Started { pid: 7, pgid: 7 });
        assert_eq!(started[0], json!({"_tag": "Started", "pid": 7, "pgid": 7}));
        let output = chunks_of(ShellEvent::Output {
            stream: ShellStream::Stderr,
            data: "e".into(),
            seq: 3,
        });
        assert_eq!(
            output[0],
            json!({"_tag": "Output", "stream": "stderr", "data": "e", "seq": 3})
        );
    }

    // --- the primitives this crate re-exports from `tools` -----------------
    //
    // These stay here on purpose: they pin that `swarmbot_node::shell` still
    // points at the tools implementation instead of having quietly grown a
    // second copy of it.

    #[test]
    fn the_guard_kills_its_group_on_drop_and_releases_its_slot() {
        let registry = ProcRegistry::new();
        // A pid that cannot exist: the guard must treat ESRCH as success rather
        // than report a failure, because "already gone" is the goal.
        let bogus = i32::MAX as u32;
        {
            let guard = registry.register(bogus);
            assert_eq!(guard.pgid(), bogus);
            assert_eq!(registry.len(), 1);
        }
        assert_eq!(registry.len(), 0, "the guard releases its slot on drop");
        assert!(process_group_gone(bogus));
    }

    #[test]
    fn a_disarmed_guard_does_not_kill() {
        let registry = ProcRegistry::new();
        let mut guard = registry.register(i32::MAX as u32);
        guard.disarm();
        drop(guard);
        assert!(registry.is_empty());
    }

    #[test]
    fn kill_all_empties_the_registry() {
        let registry = ProcRegistry::new();
        let _a = registry.register(i32::MAX as u32);
        registry.kill_all();
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn a_zero_pgid_is_neither_killed_nor_alive() {
        // pid 0 means "every process in my group" to killpg; sending a signal
        // there would be a disaster, so it is refused outright.
        assert!(!kill_process_group(0));
        assert!(process_group_gone(0));
        assert!(process_group_gone(i32::MAX as u32));
    }

    #[test]
    fn the_timeout_constants_are_the_tools_ones() {
        // The point of re-exporting: if `tools` changes the budget, this crate
        // changes with it instead of drifting.
        assert_eq!(DEFAULT_SHELL_TIMEOUT_MS, 120_000);
        assert_eq!(MAX_SHELL_TIMEOUT_MS, 15 * 60 * 1000);
    }

    #[test]
    fn the_env_allowlist_is_the_documented_one() {
        assert_eq!(
            ENV_ALLOWLIST,
            &["PATH", "HOME", "LANG", "LC_ALL", "TERM", "TMPDIR"]
        );
    }
}
