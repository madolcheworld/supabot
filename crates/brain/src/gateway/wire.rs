//! The wire bridge: [`NodeCall`] → an RPC request, and an RPC answer →
//! [`ToolResult`].
//!
//! This module is the only place the two shapes meet, and every difference
//! between them is written down here rather than discovered at runtime:
//!
//! * `fs.*` succeeds with a serialized [`ToolResult`], **not** with the
//!   `rpc::Fs*Result` structs the protocol document names. `fs.stat` of a path
//!   that is not there answers `null`, which is an answer and not an error.
//! * `shell.exec` streams `ShellChunk`s and ends with `null`; the result text is
//!   assembled here in the *same* `exit=<code>` shape the local executor
//!   produces, so a bot cannot tell where its command ran.
//! * a non-zero exit is **data**, not an RPC error: the command ran and reported
//!   its status.
//! * `NodeCall::FsEdit` has no `expected_sha256`, so a conditional edit cannot be
//!   expressed. Nothing is invented for it and nothing is silently dropped: the
//!   request goes out unconditional, exactly like the local executor's, and the
//!   node's own refusal of a conditional edit passes straight through.
//! * `encoding: "base64"` cannot cross at all — [`NodeCall`] carries
//!   `content: String` — so it is refused here rather than sent as text that
//!   would be written verbatim.

use serde_json::{Value, json};

use swarmbot_contracts::id::NodeId;
use swarmbot_contracts::tool::{ToolError, ToolResult};
use swarmbot_contracts::{Invocation, SHELL_OUTPUT_LIMIT_BYTES};
use swarmbot_node_protocol::rpc::{self, Encoding, GrepOutputMode};
use swarmbot_node_protocol::wire::ProtocolError;
use swarmbot_tools::NodeCall;
use swarmbot_tools::handlers::shell::DEFAULT_SHELL_TIMEOUT_MS;

/// One outgoing request: the method, its params, and whether it streams.
#[derive(Clone, Debug)]
pub struct Outgoing {
    pub method: &'static str,
    pub params: Value,
    pub streaming: bool,
}

/// Build the request for one call.
///
/// `inv` is what the node audits and what it echoes back in
/// `node.requestApproval`, so it is supplied by the caller rather than invented
/// here — see [`crate::gateway::executor`] for how (and how honestly) it is
/// derived.
pub fn encode(inv: Invocation, call: &NodeCall) -> Result<Outgoing, ToolError> {
    let (method, params) = match call {
        NodeCall::FsRead {
            path,
            offset,
            limit,
            encoding,
        } => (
            rpc::FS_READ,
            json!(rpc::FsRead {
                inv,
                path: path.clone(),
                offset: *offset,
                limit: *limit,
                encoding: encoding_of(rpc::FS_READ, encoding.as_deref())?,
            }),
        ),
        NodeCall::FsWrite {
            path,
            content,
            expected_sha256,
            create_parents,
        } => (
            rpc::FS_WRITE,
            json!(rpc::FsWrite {
                inv,
                path: path.clone(),
                content: content.clone(),
                encoding: Some(Encoding::Utf8),
                // Forwarded verbatim: a conditional write is a compare-and-set,
                // and dropping the hash would turn it into a clobber.
                expected_sha256: expected_sha256.clone(),
                create_parents: *create_parents,
            }),
        ),
        NodeCall::FsEdit {
            path,
            old_string,
            new_string,
            replace_all,
        } => (
            rpc::FS_EDIT,
            json!(rpc::FsEdit {
                inv,
                path: path.clone(),
                old_string: old_string.clone(),
                new_string: new_string.clone(),
                replace_all: *replace_all,
                // `NodeCall` cannot carry a condition, so there is none to drop:
                // this is an unconditional edit, which is what the local executor
                // performs too. A node that is asked for a conditional edit
                // refuses it, and that refusal reaches the model unchanged.
                expected_sha256: None,
            }),
        ),
        NodeCall::FsStat { path } => (
            rpc::FS_STAT,
            json!(rpc::FsStat {
                inv,
                path: path.clone(),
            }),
        ),
        NodeCall::FsList {
            path,
            depth,
            include_hidden,
            limit,
        } => (
            rpc::FS_LIST,
            json!(rpc::FsList {
                inv,
                path: path.clone(),
                depth: *depth,
                include_hidden: *include_hidden,
                limit: limit.map(u64::from),
            }),
        ),
        NodeCall::FsGlob {
            pattern,
            path,
            limit,
        } => (
            rpc::FS_GLOB,
            json!(rpc::FsGlob {
                inv,
                pattern: pattern.clone(),
                // An empty `cwd` means "the node's workspace", which is what the
                // local executor does with `None`.
                cwd: path.clone().unwrap_or_default(),
                limit: limit.map(u64::from),
            }),
        ),
        NodeCall::FsGrep {
            pattern,
            path,
            glob,
            ignore_case,
            literal,
            context_lines,
            output_mode,
            limit,
        } => (
            rpc::FS_GREP,
            json!(rpc::FsGrep {
                inv,
                pattern: pattern.clone(),
                path: path.clone().unwrap_or_default(),
                glob: glob.clone(),
                ignore_case: *ignore_case,
                literal: *literal,
                context_lines: *context_lines,
                output_mode: grep_mode_of(output_mode.as_deref())?,
                limit: limit.map(u64::from),
            }),
        ),
        NodeCall::FsMkdir { path, parents } => (
            rpc::FS_MKDIR,
            json!(rpc::FsMkdir {
                inv,
                path: path.clone(),
                recursive: *parents,
            }),
        ),
        NodeCall::FsMove { from, to } => (
            rpc::FS_MOVE,
            json!(rpc::FsMove {
                inv,
                from: from.clone(),
                to: to.clone(),
                // `NodeCall` cannot say "do not clobber", so the node's own
                // default applies. Asking for a guarantee this bridge cannot
                // express would be worse than not claiming one.
                overwrite: None,
            }),
        ),
        NodeCall::FsRemove { path, recursive } => (
            rpc::FS_REMOVE,
            json!(rpc::FsRemove {
                inv,
                path: path.clone(),
                recursive: *recursive,
            }),
        ),
        NodeCall::ShellExec {
            command,
            cwd,
            timeout_ms,
            env,
        } => (
            rpc::SHELL_EXEC,
            json!(rpc::ShellExec {
                inv,
                command: command.clone(),
                cwd: cwd.clone().unwrap_or_default(),
                timeout_ms: timeout_ms.unwrap_or(DEFAULT_SHELL_TIMEOUT_MS),
                env: env.clone(),
                output_limit_bytes: None,
            }),
        ),
    };
    Ok(Outgoing {
        method,
        params,
        streaming: rpc::is_stream_method(method),
    })
}

/// The encoding a call asked for, refusing the one that cannot cross.
fn encoding_of(method: &str, encoding: Option<&str>) -> Result<Option<Encoding>, ToolError> {
    let Some(value) = encoding.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.eq_ignore_ascii_case("utf8") {
        return Ok(Some(Encoding::Utf8));
    }
    // `base64` and anything else a future node might name: refusing beats sending
    // text that would be written verbatim into a binary file.
    Err(ToolError::PolicyRefused {
        reason: format!(
            "`{method}` cannot cross to a remote node with `encoding: {value}`: tool results and \
             write bodies are text on this side of the bridge"
        ),
        retry: false,
    })
}

fn grep_mode_of(mode: Option<&str>) -> Result<Option<GrepOutputMode>, ToolError> {
    match mode {
        None => Ok(None),
        Some("content") => Ok(Some(GrepOutputMode::Content)),
        Some("files_with_matches") => Ok(Some(GrepOutputMode::FilesWithMatches)),
        Some("count") => Ok(Some(GrepOutputMode::Count)),
        Some(other) => Err(ToolError::invalid(format!(
            "`{other}` is not a grep output mode; expected content, files_with_matches or count"
        ))),
    }
}

/// Decode a unary answer.
pub fn decode_result(method: &str, call: &NodeCall, value: Value) -> Result<ToolResult, ToolError> {
    if value.is_null() {
        // `fs.stat` answers `null` for a path that is not there — a question with
        // a "no" answer. The local executor reports that as a failed stat, so the
        // remote one does too: a bot must not get two different stories about the
        // same missing file depending on where it lives.
        return match call {
            NodeCall::FsStat { path } => Err(ToolError::failed(format!(
                "cannot stat {path}: No such file or directory (os error 2)"
            ))),
            _ => Err(ToolError::failed(format!(
                "the node ended `{method}` without a result"
            ))),
        };
    }
    serde_json::from_value::<ToolResult>(value).map_err(|error| {
        ToolError::failed(format!(
            "the node answered `{method}` with something this brain cannot read: {error}"
        ))
    })
}

/// Translate a protocol or typed failure into the error a tool caller expects.
///
/// A denial stays a denial (data the model can react to) and a transport failure
/// stays an error — the two are never merged, because "the node said no" and "the
/// node could not be reached" call for different next moves.
///
/// `node` is named in the message because the caller of a tool cannot otherwise
/// tell *which* machine went quiet: a bot hosted on one node never talks to
/// another, but its operator reads the error out of context.
pub fn translate(node: &NodeId, method: &str, error: &ProtocolError) -> ToolError {
    use swarmbot_node_protocol::wire::TypedError;

    if matches!(error, ProtocolError::Interrupted) {
        return ToolError::Cancelled;
    }
    if let ProtocolError::CallTimedOut { after, .. } = error {
        // A deadline is a failure, never a denial: nothing was refused, the
        // node simply did not finish. The message says what was done about it
        // (an interrupt was *asked for*) and what that does not promise (the
        // node may still be running the call), because a model that reads
        // "timed out" as "it did not happen" would be wrong.
        return ToolError::failed(format!(
            "node {node} did not answer `{method}` ({}) within {}s: the brain stopped waiting and \
             sent `@effect/rpc/Interrupt`. The node may still be running the call; its answer, if it \
             ever comes, is discarded.",
            kind_of(method),
            after.as_secs(),
        ));
    }
    match error.typed() {
        Some(TypedError::PathDenied { path, reason }) => ToolError::PathDenied {
            path: path.clone(),
            reason: reason.clone(),
        },
        Some(TypedError::PolicyRefused { reason, retry, .. }) => ToolError::PolicyRefused {
            reason: reason.clone(),
            retry: *retry,
        },
        Some(TypedError::NotAuthenticated { reason }) => ToolError::PolicyRefused {
            // The node does not trust *this brain*. Retrying cannot help until
            // somebody re-pairs, so `retry: false`.
            reason: format!("the node does not trust this brain: {reason}"),
            retry: false,
        },
        Some(TypedError::EditFailed { path, reason }) => {
            ToolError::failed(format!("the node could not edit {path}: {reason}"))
        }
        Some(TypedError::FsError {
            op,
            path,
            code,
            reason,
        }) => ToolError::failed(format!(
            "filesystem error {code} during {op} on {path}: {reason}"
        )),
        Some(other) => ToolError::failed(format!("the node refused `{method}`: {other}")),
        None => ToolError::failed(format!("the node could not be reached: {error}")),
    }
}

/// A plain-language name for the kind of call a method is.
///
/// The method name (`fs.read`) is the precise answer; this is the sentence a
/// model reads in an error, where "a filesystem call" says what `fs.read` is.
fn kind_of(method: &str) -> &'static str {
    if method == rpc::SHELL_EXEC {
        "a shell command"
    } else {
        "a filesystem call"
    }
}

/// What one streamed `shell.exec` produced.
#[derive(Clone, Debug, Default)]
pub struct ShellRun {
    pub pid: Option<u32>,
    pub pgid: Option<u32>,
    pub stdout: String,
    pub stderr: String,
    /// Bytes dropped locally after the cap: a node that streams more than the
    /// contract allows must not be able to exhaust the brain's memory.
    pub dropped_here: usize,
    pub exit: Option<ShellExit>,
    /// Undecodable chunks, kept so the result can say a line was skipped rather
    /// than pretending the output was complete.
    pub skipped: usize,
}

/// The `Exit` chunk's facts.
#[derive(Clone, Debug, PartialEq)]
pub struct ShellExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub output_truncated: Option<bool>,
    pub spill_path: Option<String>,
}

impl ShellRun {
    /// Append one output chunk, honouring the per-stream cap.
    ///
    /// The node caps its own output and says so in the `Exit` chunk; this cap is
    /// the brain's, and it exists because a node is a machine this process does
    /// not control — a stream of gigabytes must not be able to exhaust it.
    pub fn push(&mut self, stderr: bool, data: &str) {
        let stream = if stderr {
            &mut self.stderr
        } else {
            &mut self.stdout
        };
        let room = SHELL_OUTPUT_LIMIT_BYTES.saturating_sub(stream.len());
        if room == 0 {
            self.dropped_here = self.dropped_here.saturating_add(data.len());
            return;
        }
        if data.len() > room {
            // Cut on a character boundary: the text is displayed as-is.
            let kept: String = data.chars().take(room).collect();
            self.dropped_here = self.dropped_here.saturating_add(data.len() - kept.len());
            stream.push_str(&kept);
            return;
        }
        stream.push_str(data);
    }
}

/// Render a streamed run in the local executor's own shape.
///
/// `exit=<code>` first, then the non-empty sections, then an honest note about
/// anything that was capped: a model that has learned to read one machine's shell
/// output must be able to read another's.
pub fn shell_result(run: &ShellRun) -> Result<ToolResult, ToolError> {
    let Some(exit) = run.exit.as_ref() else {
        return Err(ToolError::failed(
            "the node ended the shell stream without an exit chunk; the command's status is unknown",
        ));
    };
    let code_text = match exit.code {
        Some(code) => code.to_string(),
        None => format!("signal:{}", exit.signal.unwrap_or(-1)),
    };
    let mut text = format!("exit={code_text}");
    if exit.timed_out {
        text.push_str(" (timed out)");
    }
    // The section layout is the local executor's, byte for byte: a blank line
    // before the first section and none between them.
    let mut first = true;
    for (heading, body) in [
        ("--- stdout ---", &run.stdout),
        ("--- stderr ---", &run.stderr),
    ] {
        if body.is_empty() {
            continue;
        }
        if first {
            text.push('\n');
            first = false;
        }
        text.push_str(heading);
        text.push('\n');
        text.push_str(body);
        if !body.ends_with('\n') {
            text.push('\n');
        }
    }
    if run.dropped_here > 0 {
        text.push_str(&format!(
            "\n[output capped on the brain: {} more bytes were discarded after {SHELL_OUTPUT_LIMIT_BYTES} bytes per stream]",
            run.dropped_here
        ));
    } else if exit.output_truncated == Some(true) {
        text.push_str(&format!(
            "\n[output capped on the node after {SHELL_OUTPUT_LIMIT_BYTES} bytes per stream]"
        ));
    }
    if run.skipped > 0 {
        text.push_str(&format!(
            "\n[{} stream chunk(s) could not be decoded and were skipped]",
            run.skipped
        ));
    }
    if text.ends_with('\n') {
        text.pop();
    }

    let mut result = ToolResult {
        content: vec![swarmbot_contracts::Content::Text { text }],
        // A non-zero exit is an error *result*, so a model treats it as a
        // failure, while the text keeps the machine-findable `exit=<code>`.
        is_error: exit.code != Some(0),
        meta: serde_json::Map::new(),
    };
    if let Some(code) = exit.code {
        result.meta.insert("exit_code".into(), Value::from(code));
    }
    if let Some(signal) = exit.signal {
        result.meta.insert("signal".into(), Value::from(signal));
    }
    result
        .meta
        .insert("duration_ms".into(), Value::from(exit.duration_ms));
    if let Some(spill) = &exit.spill_path {
        result
            .meta
            .insert("spill_path".into(), Value::from(spill.clone()));
    }
    result
        .meta
        .insert("timed_out".into(), Value::from(exit.timed_out));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use swarmbot_contracts::id::{BotId, ConversationId};
    use swarmbot_node_protocol::wire::{CauseError, TypedError};

    fn invocation() -> Invocation {
        Invocation::new(BotId::new(), ConversationId::new())
    }

    #[test]
    fn every_call_kind_maps_to_its_method_and_payload() {
        let cases: Vec<(NodeCall, &str, bool)> = vec![
            (
                NodeCall::FsRead {
                    path: "a.txt".into(),
                    offset: None,
                    limit: Some(10),
                    encoding: Some("utf8".into()),
                },
                rpc::FS_READ,
                false,
            ),
            (
                NodeCall::FsWrite {
                    path: "a.txt".into(),
                    content: "hi".into(),
                    expected_sha256: Some("ab".into()),
                    create_parents: Some(true),
                },
                rpc::FS_WRITE,
                false,
            ),
            (
                NodeCall::FsEdit {
                    path: "a.txt".into(),
                    old_string: "x".into(),
                    new_string: "y".into(),
                    replace_all: None,
                },
                rpc::FS_EDIT,
                false,
            ),
            (NodeCall::FsStat { path: "a".into() }, rpc::FS_STAT, false),
            (
                NodeCall::FsList {
                    path: ".".into(),
                    depth: Some(2),
                    include_hidden: None,
                    limit: Some(5),
                },
                rpc::FS_LIST,
                false,
            ),
            (
                NodeCall::FsGlob {
                    pattern: "*.rs".into(),
                    path: None,
                    limit: None,
                },
                rpc::FS_GLOB,
                false,
            ),
            (
                NodeCall::FsGrep {
                    pattern: "todo".into(),
                    path: None,
                    glob: None,
                    ignore_case: Some(true),
                    literal: None,
                    context_lines: Some(2),
                    output_mode: Some("count".into()),
                    limit: None,
                },
                rpc::FS_GREP,
                false,
            ),
            (
                NodeCall::FsMkdir {
                    path: "d".into(),
                    parents: Some(true),
                },
                rpc::FS_MKDIR,
                false,
            ),
            (
                NodeCall::FsMove {
                    from: "a".into(),
                    to: "b".into(),
                },
                rpc::FS_MOVE,
                false,
            ),
            (
                NodeCall::FsRemove {
                    path: "a".into(),
                    recursive: Some(false),
                },
                rpc::FS_REMOVE,
                false,
            ),
            (
                NodeCall::ShellExec {
                    command: "true".into(),
                    cwd: None,
                    timeout_ms: None,
                    env: None,
                },
                rpc::SHELL_EXEC,
                true,
            ),
        ];
        for (call, method, streaming) in cases {
            let outgoing = encode(invocation(), &call).unwrap_or_else(|e| panic!("{method}: {e}"));
            assert_eq!(outgoing.method, method);
            assert_eq!(outgoing.streaming, streaming, "{method}");
            // `params: {}` is dropped at frame-parse time by the reference mux,
            // so a payload-less call must never produce an empty object.
            assert!(outgoing.params.is_object(), "{method}");
            assert!(
                !outgoing.params.as_object().expect("an object").is_empty(),
                "{method} sent empty params"
            );
            assert!(outgoing.params["inv"]["id"].is_string(), "{method}");
        }
    }

    #[test]
    fn a_conditional_write_keeps_its_condition() {
        let call = NodeCall::FsWrite {
            path: "a.txt".into(),
            content: "new".into(),
            expected_sha256: Some("deadbeef".into()),
            create_parents: None,
        };
        let outgoing = encode(invocation(), &call).expect("encodes");
        assert_eq!(outgoing.params["expectedSha256"], json!("deadbeef"));
        assert_eq!(outgoing.params["encoding"], json!("utf8"));
    }

    #[test]
    fn an_edit_carries_no_invented_condition() {
        let call = NodeCall::FsEdit {
            path: "a.txt".into(),
            old_string: "x".into(),
            new_string: "y".into(),
            replace_all: Some(true),
        };
        let outgoing = encode(invocation(), &call).expect("encodes");
        // Absent, not null-as-a-check: `NodeCall` cannot express a condition, and
        // inventing one would make the node refuse work the local executor does.
        assert!(outgoing.params.get("expectedSha256").is_none());
        assert_eq!(outgoing.params["replaceAll"], json!(true));
    }

    #[test]
    fn base64_is_refused_rather_than_sent_as_text() {
        for call in [
            NodeCall::FsRead {
                path: "a.bin".into(),
                offset: None,
                limit: None,
                encoding: Some("base64".into()),
            },
            NodeCall::FsWrite {
                path: "a.bin".into(),
                content: "AA==".into(),
                expected_sha256: None,
                create_parents: None,
            },
        ] {
            let outgoing = encode(invocation(), &call);
            match &call {
                NodeCall::FsRead { .. } => {
                    let err = outgoing.expect_err("base64 reads cannot cross");
                    assert!(matches!(err, ToolError::PolicyRefused { retry: false, .. }));
                }
                // A write's encoding is not part of `NodeCall`, so there is
                // nothing to refuse: the content is text by construction.
                _ => assert!(outgoing.is_ok()),
            }
        }
        // An encoding name nobody defined is refused too, not guessed at.
        let err = encode(
            invocation(),
            &NodeCall::FsRead {
                path: "a".into(),
                offset: None,
                limit: None,
                encoding: Some("utf16".into()),
            },
        )
        .expect_err("refused");
        assert!(matches!(err, ToolError::PolicyRefused { retry: false, .. }));
    }

    #[test]
    fn shell_defaults_match_the_local_executor() {
        let outgoing = encode(
            invocation(),
            &NodeCall::ShellExec {
                command: "true".into(),
                cwd: None,
                timeout_ms: None,
                env: None,
            },
        )
        .expect("encodes");
        assert_eq!(
            outgoing.params["timeoutMs"],
            json!(DEFAULT_SHELL_TIMEOUT_MS)
        );
        assert_eq!(
            outgoing.params["cwd"],
            json!(""),
            "empty means the workspace"
        );
        assert!(outgoing.streaming);
    }

    #[test]
    fn a_tool_result_round_trips_and_a_null_stat_is_a_missing_file() {
        let result = ToolResult::text("hello").with_meta("exit_code", 0);
        let value = serde_json::to_value(&result).expect("serializes");
        let call = NodeCall::FsRead {
            path: "a".into(),
            offset: None,
            limit: None,
            encoding: None,
        };
        let back = decode_result(rpc::FS_READ, &call, value).expect("decodes");
        assert_eq!(back.text_only(), "hello");
        assert_eq!(back.meta["exit_code"], json!(0));

        let err = decode_result(
            rpc::FS_STAT,
            &NodeCall::FsStat {
                path: "gone".into(),
            },
            Value::Null,
        )
        .expect_err("a missing path is an error here, as it is locally");
        assert!(err.to_string().contains("gone"), "{err}");

        let err = decode_result(rpc::FS_READ, &call, Value::Null).expect_err("no result");
        assert!(err.to_string().contains("without a result"), "{err}");

        let err = decode_result(rpc::FS_READ, &call, json!({"nope": 1})).expect_err("undecodable");
        assert!(err.to_string().contains("cannot read"), "{err}");
    }

    #[test]
    fn typed_errors_keep_their_kind_and_a_denial_stays_data() {
        let node = NodeId::new();
        let denied = ProtocolError::remote(CauseError::fail(TypedError::policy_refused(
            "shell.exec",
            "this node's `shell` ceiling is `never`",
            false,
        )));
        match translate(&node, rpc::SHELL_EXEC, &denied) {
            ToolError::PolicyRefused { reason, retry } => {
                assert!(reason.contains("ceiling"), "{reason}");
                assert!(!retry);
            }
            other => panic!("a refusal must stay a refusal: {other:?}"),
        }

        let path = ProtocolError::remote(CauseError::fail(TypedError::path_denied(
            "/etc/shadow",
            "outside roots",
        )));
        match translate(&node, rpc::FS_READ, &path) {
            ToolError::PathDenied { path, reason } => {
                assert_eq!(path, "/etc/shadow");
                assert_eq!(reason, "outside roots");
            }
            other => panic!("expected PathDenied, got {other:?}"),
        }

        let unauth = ProtocolError::remote(CauseError::fail(TypedError::not_authenticated(
            "node.hello has not been verified",
        )));
        match translate(&node, rpc::FS_READ, &unauth) {
            ToolError::PolicyRefused { retry, .. } => assert!(!retry, "re-pairing is a human job"),
            other => panic!("expected a refusal, got {other:?}"),
        }

        assert_eq!(
            translate(&node, rpc::FS_READ, &ProtocolError::Interrupted),
            ToolError::Cancelled
        );
        let gone = translate(&node, rpc::FS_READ, &ProtocolError::SocketClosed);
        assert!(matches!(gone, ToolError::Failed { .. }), "{gone:?}");
        assert!(gone.to_string().contains("socket closed"), "{gone}");
    }

    /// The deadline is the one failure the wire cannot report as a remote error:
    /// nothing came back, so the brain has to say so itself — naming the node and
    /// the kind of call, and not pretending the interrupt stopped the work.
    #[test]
    fn a_deadline_is_a_failure_that_names_the_node_and_the_call() {
        let node = NodeId::new();
        let error = ProtocolError::CallTimedOut {
            method: rpc::FS_READ.to_string(),
            after: Duration::from_secs(60),
        };
        match translate(&node, rpc::FS_READ, &error) {
            ToolError::Failed { message } => {
                assert!(message.contains(&node.to_string()), "{message}");
                assert!(message.contains(rpc::FS_READ), "{message}");
                assert!(message.contains("did not answer"), "{message}");
                assert!(message.contains("60s"), "{message}");
                assert!(message.contains("a filesystem call"), "{message}");
                // Honest about what the interrupt does not promise.
                assert!(message.contains("may still be running"), "{message}");
            }
            other => panic!("a timeout is a failure, not a denial: {other:?}"),
        }
        // A shell command is named as one: `fs.read` and `shell.exec` are not
        // the same kind of thing to wait for, and the message says which it was.
        let shell = ProtocolError::CallTimedOut {
            method: rpc::SHELL_EXEC.to_string(),
            after: Duration::from_secs(150),
        };
        let message = translate(&node, rpc::SHELL_EXEC, &shell).to_string();
        assert!(message.contains("a shell command"), "{message}");
    }

    #[test]
    fn a_streamed_run_reads_like_a_local_one() {
        let mut run = ShellRun {
            pid: Some(4242),
            ..ShellRun::default()
        };
        run.push(false, "hello\n");
        run.push(true, "warning: x");
        run.exit = Some(ShellExit {
            code: Some(0),
            signal: None,
            timed_out: false,
            duration_ms: 12,
            output_truncated: None,
            spill_path: None,
        });
        let result = shell_result(&run).expect("renders");
        assert!(!result.is_error);
        assert_eq!(
            result.text_only(),
            "exit=0\n--- stdout ---\nhello\n--- stderr ---\nwarning: x"
        );
        assert_eq!(result.meta["exit_code"], json!(0));
        assert_eq!(result.meta["duration_ms"], json!(12));

        // A non-zero exit is an error result that still reads as data.
        let mut failed = ShellRun {
            exit: Some(ShellExit {
                code: Some(3),
                signal: None,
                timed_out: false,
                duration_ms: 1,
                output_truncated: None,
                spill_path: Some("/tmp/spill".into()),
            }),
            ..ShellRun::default()
        };
        failed.push(false, "boom");
        let result = shell_result(&failed).expect("renders");
        assert!(result.is_error);
        assert!(
            result.text_only().starts_with("exit=3"),
            "{}",
            result.text_only()
        );
        assert_eq!(result.meta["spill_path"], json!("/tmp/spill"));

        // Killed by a signal, and timed out.
        let killed = ShellRun {
            exit: Some(ShellExit {
                code: None,
                signal: Some(9),
                timed_out: true,
                duration_ms: 5000,
                output_truncated: Some(true),
                spill_path: None,
            }),
            ..ShellRun::default()
        };
        let result = shell_result(&killed).expect("renders");
        assert!(
            result.text_only().starts_with("exit=signal:9 (timed out)"),
            "{}",
            result.text_only()
        );
        assert!(result.is_error);
        assert!(result.text_only().contains("capped on the node"));

        // No exit chunk at all is a failure, not a silent success.
        let err = shell_result(&ShellRun::default()).expect_err("no exit");
        assert!(err.to_string().contains("without an exit chunk"), "{err}");
    }

    #[test]
    fn output_is_capped_on_this_side_too() {
        let mut run = ShellRun::default();
        let chunk = "x".repeat(SHELL_OUTPUT_LIMIT_BYTES);
        run.push(false, &chunk);
        run.push(false, "and more");
        assert_eq!(run.stdout.len(), SHELL_OUTPUT_LIMIT_BYTES);
        assert_eq!(run.dropped_here, "and more".len());
        run.exit = Some(ShellExit {
            code: Some(0),
            signal: None,
            timed_out: false,
            duration_ms: 1,
            output_truncated: None,
            spill_path: None,
        });
        let result = shell_result(&run).expect("renders");
        assert!(result.text_only().contains("capped on the brain"));
    }
}
