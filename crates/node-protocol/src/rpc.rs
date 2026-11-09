//! The RPC surface of this build, as data: method names, payload types and
//! the honest refusal of everything we do not implement.
//!
//! Subset policy (documented deviations from the OpenBot table in §5):
//!
//! * a node serves `node.hello`, `fs.read/write/edit/stat/list/glob/grep/
//!   mkdir/move/remove` and the streaming `shell.exec`;
//! * a brain serves `node.requestApproval`, `node.event`, `node.heartbeat`;
//! * everything else in the reference table (`pty.*`, `tunnel.*`,
//!   `browser.*`, `desktop.*`, `computer.*`, `screen.*`, `fs.download`,
//!   `fs.upload`, `service.*`, `forward.listen`, `lease.set`,
//!   `node.openStream`) answers with a typed
//!   `PolicyRefused { retry: false, reason: "not implemented by this node" }`
//!   via [`unimplemented`]. Honesty over silence: a caller must be able to
//!   tell "no" from "broken".
//!
//! Payload field names follow the wire (`camelCase`); every bot-originated,
//! side-effecting call carries `inv: Invocation` (§5) for node policy,
//! idempotency and the audit log. Note the deviation: the reference protocol
//! uses `{invocationId, botId, runId, toolCallId?}` — we send the full
//! `swarmbot_contracts::Invocation` (our ids are typed and richer), which is
//! what our node policy engine and audit log consume.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use swarmbot_contracts::{
    BotId, Capabilities, Decision, Invocation, InvocationId, NodeId, NodeRoot, PolicyCeiling,
    ProtocolRange,
};

use crate::wire::TypedError;

// --- node-served methods (§5.1 subset) -------------------------------------

pub const NODE_HELLO: &str = "node.hello";
pub const FS_READ: &str = "fs.read";
pub const FS_WRITE: &str = "fs.write";
pub const FS_EDIT: &str = "fs.edit";
pub const FS_STAT: &str = "fs.stat";
pub const FS_LIST: &str = "fs.list";
pub const FS_GLOB: &str = "fs.glob";
pub const FS_GREP: &str = "fs.grep";
pub const FS_MKDIR: &str = "fs.mkdir";
pub const FS_MOVE: &str = "fs.move";
pub const FS_REMOVE: &str = "fs.remove";
/// The only streaming RPC in our subset (§4.6/§5.1).
pub const SHELL_EXEC: &str = "shell.exec";

// --- brain-served methods (§5.2 subset) ------------------------------------

pub const NODE_REQUEST_APPROVAL: &str = "node.requestApproval";
pub const NODE_EVENT: &str = "node.event";
pub const NODE_HEARTBEAT: &str = "node.heartbeat";

/// Header the brain stamps on every request it relays for a node (§4.10 —
/// ours, not `x-openbot-node-id`). Brain handlers never trust a node id from
/// a payload, only from this stamp.
pub const HEADER_NODE_ID: &str = "x-swarmbot-node-id";

/// Every method this build serves, on both sides.
pub const IMPLEMENTED_METHODS: &[&str] = &[
    NODE_HELLO,
    FS_READ,
    FS_WRITE,
    FS_EDIT,
    FS_STAT,
    FS_LIST,
    FS_GLOB,
    FS_GREP,
    FS_MKDIR,
    FS_MOVE,
    FS_REMOVE,
    SHELL_EXEC,
    NODE_REQUEST_APPROVAL,
    NODE_EVENT,
    NODE_HEARTBEAT,
];

/// Every method in the reference protocol's tables that this build knows by
/// name but does **not** implement. They are refused with a typed
/// `PolicyRefused { retry: false }` instead of an "unknown method" shrug, so
/// a bot's model learns "this team cannot do that" rather than "the node is
/// broken".
pub const UNIMPLEMENTED_METHODS: &[&str] = &[
    "fs.download",
    "fs.upload",
    "pty.open",
    "tunnel.open",
    "service.start",
    "service.stop",
    "forward.listen",
    "browser.ensure",
    "browser.action",
    "browser.screencast",
    "browser.input",
    "lease.set",
    "desktop.ensure",
    "desktop.stop",
    "computer.act",
    "computer.stream",
    "screen.ensure",
    "screen.stream",
    "screen.input",
    "node.openStream",
];

/// The refusal reason for [`UNIMPLEMENTED_METHODS`].
pub const NOT_IMPLEMENTED_REASON: &str = "not implemented by this node";

/// The typed refusal for a method the reference protocol defines but this
/// build does not implement; `None` for anything else (implemented methods
/// and genuinely unknown names alike — unknown names are the dispatcher's
/// own `PolicyRefused { reason: "unknown method" }`).
pub fn unimplemented(method: &str) -> Option<TypedError> {
    UNIMPLEMENTED_METHODS
        .contains(&method)
        .then(|| TypedError::policy_refused(method.to_string(), NOT_IMPLEMENTED_REASON, false))
}

/// Whether a method streams chunks (§5.1): only `shell.exec` in our subset.
/// The mux uses this to decide whether a handler gets a `ChunkSink`.
pub fn is_stream_method(method: &str) -> bool {
    method == SHELL_EXEC
}

/// Decode request params into a payload struct, mapping a decode failure to a
/// non-retryable refusal: the *brain* built the payload, so retrying the
/// identical call cannot help, and the model should be told that plainly.
pub fn decode_params<T: serde::de::DeserializeOwned>(
    method: &str,
    params: Value,
) -> Result<T, TypedError> {
    serde_json::from_value(params).map_err(|e| {
        TypedError::policy_refused(method.to_string(), format!("malformed params: {e}"), false)
    })
}

// --- node.hello (§3.2) ------------------------------------------------------

/// `node.hello` params: the brain's challenge. `brain_sig` is verified
/// against the **pinned** brain key over
/// `swarmbot-brain-v1|<nodeId>|<upgrade nonce>|<brainOrigin>` before the node
/// answers anything else (§3.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloParams {
    pub protocol: ProtocolRange,
    pub brain_version: String,
    pub brain_sig: String,
}

/// The node's answer (§3.2), minus the `desktop`/`screen` fields we do not
/// implement. `protocol` is the negotiated version (the highest in both
/// ranges), `tunnels` stays empty in this build, and `policy` reports the
/// node's own ceiling — the brain can never raise it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeHello {
    pub node_id: NodeId,
    pub version: String,
    pub hostname: String,
    pub platform: String,
    pub arch: String,
    pub protocol: u32,
    pub roots: Vec<NodeRoot>,
    pub capabilities: Capabilities,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tunnels: Vec<String>,
    pub policy: HelloPolicy,
    pub brain_verified: bool,
}

/// The `policy` object of `NodeHello`: the node's aggregate ceiling and
/// whether the owner let the brain manage policy (it never raises ceilings;
/// the effective ceiling is always `min(brain, node)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloPolicy {
    pub ceiling: PolicyCeiling,
    pub remote_managed: bool,
}

// --- fs.* (§5.1) -------------------------------------------------------------

/// How file content is encoded on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    Utf8,
    Base64,
}

/// What kind of thing a path points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsRead {
    pub inv: Invocation,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<Encoding>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsReadResult {
    pub content: String,
    pub encoding: Encoding,
    pub sha256: String,
    pub size: u64,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWrite {
    pub inv: Invocation,
    pub path: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<Encoding>,
    /// Compare-and-set by hash; the empty string means "must not exist".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_parents: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsWriteResult {
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsEdit {
    pub inv: Invocation,
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_all: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsEditResult {
    pub sha256: String,
    pub replacements: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsStat {
    pub inv: Invocation,
    pub path: String,
}

/// The `fs.stat` success value (`null` when the path does not exist).
/// The reference doc names the shape `FileStat` without pinning fields; ours
/// carries what tools and the UI need: kind, size, mtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileStat {
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsList {
    pub inv: Invocation,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_hidden: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListEntry {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsListResult {
    pub entries: Vec<ListEntry>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsGlob {
    pub inv: Invocation,
    pub pattern: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsGlobResult {
    pub paths: Vec<String>,
    pub truncated: bool,
}

/// How `fs.grep` reports matches (§5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrepOutputMode {
    #[serde(rename = "content")]
    Content,
    #[serde(rename = "files_with_matches")]
    FilesWithMatches,
    #[serde(rename = "count")]
    Count,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsGrep {
    pub inv: Invocation,
    pub pattern: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_case: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub literal: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_lines: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_mode: Option<GrepOutputMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepMatch {
    pub path: String,
    pub line: u64,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathCount {
    pub path: String,
    pub count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsGrepResult {
    pub matches: Vec<GrepMatch>,
    pub files: u64,
    pub counts: Vec<PathCount>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsMkdir {
    pub inv: Invocation,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recursive: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsMove {
    pub inv: Invocation,
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsRemove {
    pub inv: Invocation,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recursive: Option<bool>,
}

// --- shell.exec (§5.1, streaming) -------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellExec {
    pub inv: Invocation,
    pub command: String,
    pub cwd: String,
    pub timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_limit_bytes: Option<u64>,
}

/// Which pipe an output chunk came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// The chunks of a `shell.exec` stream, in the order a run produces them:
/// `Started`, zero or more `Output`, then exactly one `Exit`. `data` is
/// UTF-8 (lossily decoded by the node); the reference doc does not pin an
/// encoding and models consume text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename_all_fields = "camelCase")]
pub enum ShellChunk {
    Started {
        pid: u32,
        pgid: u32,
    },
    Output {
        stream: OutputStream,
        data: String,
        /// Per-invocation sequence number, so the brain can order stdout and
        /// stderr chunks that arrived on one socket.
        seq: u64,
    },
    Exit {
        /// Process exit code; `null` when killed by a signal.
        code: Option<i32>,
        signal: Option<i32>,
        timed_out: bool,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_truncated: Option<bool>,
        /// Where output beyond the cap was spilled, if anywhere.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        spill_path: Option<String>,
    },
}

// --- brain-served (§5.2) ------------------------------------------------------

/// `node.requestApproval`: the node asks the owner (through the brain) before
/// doing something above its auto-allow ceiling (§5.2, §8.5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestApproval {
    pub invocation_id: InvocationId,
    pub bot_id: BotId,
    /// What is being asked for, e.g. `shell`, `write_file`.
    pub action: String,
    /// The concrete target, e.g. the command line or the path.
    pub target: String,
    pub reason: String,
}

/// The answer to [`RequestApproval`]. `decision` uses the contracts enum
/// (wire: `allow_once` / `allow_always` / `deny` — the reference protocol
/// spells these with dashes; ours with underscores, matching every other
/// enum in `swarmbot-contracts`). `by` names who decided (`owner`, or a rule).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalAnswer {
    pub decision: Decision,
    pub by: String,
}

/// `node.event`: fire-and-forget facts the brain may care about. Unknown
/// types must be ignored, not rejected (§5.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub data: Value,
}

/// Well-known `node.event` types in our subset (§5.2 lists more; the
/// browser ones do not exist here).
pub const EVENT_AUDIT: &str = "audit";
pub const EVENT_NODE_CONNECTED: &str = "node.connected";
pub const EVENT_ROOTS_CHANGED: &str = "node.roots-changed";

/// `node.heartbeat`: liveness plus a load snapshot (§5.2). `at` is Unix
/// milliseconds — cheaper to compare than RFC3339 strings on a hot path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Heartbeat {
    pub at: i64,
    pub active_invocations: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_mem_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use swarmbot_contracts::{BotId, ConversationId, Invocation, PROTOCOL_VERSION, PolicyMap};

    fn inv() -> Invocation {
        Invocation::new(
            BotId::from_suffix(&"0".repeat(32)),
            ConversationId::from_suffix(&"0".repeat(32)),
        )
    }

    /// Every method name in the reference protocol's §5 tables, transcribed.
    /// If the doc grows a row, this list must grow with it — that is the
    /// point of the test.
    const DOC_TABLE: &[&str] = &[
        // §5.1 NodeRpcs
        "node.hello",
        "fs.read",
        "fs.download",
        "fs.upload",
        "fs.write",
        "fs.edit",
        "fs.stat",
        "fs.list",
        "fs.glob",
        "fs.grep",
        "fs.mkdir",
        "fs.move",
        "fs.remove",
        "shell.exec",
        "pty.open",
        "tunnel.open",
        "service.start",
        "service.stop",
        "forward.listen",
        "browser.ensure",
        "browser.action",
        "browser.screencast",
        "browser.input",
        "lease.set",
        "desktop.ensure",
        "desktop.stop",
        "computer.act",
        "computer.stream",
        "screen.ensure",
        "screen.stream",
        "screen.input",
        // §5.2 BrainRpcs
        "node.requestApproval",
        "node.openStream",
        "node.event",
        "node.heartbeat",
    ];

    #[test]
    fn every_documented_method_is_implemented_or_explicitly_refused() {
        for method in DOC_TABLE {
            let implemented = IMPLEMENTED_METHODS.contains(method);
            let refused = unimplemented(method).is_some();
            assert!(
                implemented ^ refused,
                "{method} must be exactly one of implemented/refused (implemented={implemented}, refused={refused})"
            );
        }
        // No drift in either direction: our lists contain nothing the doc
        // does not name.
        for method in IMPLEMENTED_METHODS.iter().chain(UNIMPLEMENTED_METHODS) {
            assert!(
                DOC_TABLE.contains(method),
                "{method} is not in the doc table"
            );
        }
    }

    #[test]
    fn unimplemented_methods_refuse_with_a_typed_non_retryable_error() {
        for method in UNIMPLEMENTED_METHODS {
            let error = unimplemented(method).unwrap();
            match error {
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
        // Implemented methods are not refused...
        assert!(unimplemented(NODE_HELLO).is_none());
        assert!(unimplemented(FS_READ).is_none());
        assert!(unimplemented(SHELL_EXEC).is_none());
        // ...and a name nobody ever defined is not "unimplemented" either —
        // the dispatcher answers that as an unknown method.
        assert!(unimplemented("totally.made.up").is_none());
    }

    #[test]
    fn only_shell_exec_streams() {
        assert!(is_stream_method(SHELL_EXEC));
        for method in IMPLEMENTED_METHODS {
            if *method != SHELL_EXEC {
                assert!(!is_stream_method(method), "{method} must not stream");
            }
        }
    }

    #[test]
    fn node_hello_matches_the_documented_shape() {
        let hello = NodeHello {
            node_id: NodeId::new(),
            version: "0.1.0".into(),
            hostname: "box".into(),
            platform: "linux".into(),
            arch: "x86_64".into(),
            protocol: PROTOCOL_VERSION,
            roots: vec![NodeRoot::read_write("work", "/home/u/swarmbot")],
            capabilities: Capabilities::new(["fs@1", "shell@1"]),
            tunnels: vec![],
            policy: HelloPolicy {
                ceiling: PolicyCeiling::Ask,
                remote_managed: false,
            },
            brain_verified: true,
        };
        let value = serde_json::to_value(&hello).unwrap();
        // §3.2 field names, camelCase; tunnels omitted while empty.
        assert_eq!(value["nodeId"], json!(hello.node_id.to_string()));
        assert_eq!(value["brainVerified"], json!(true));
        assert_eq!(value["protocol"], json!(1));
        assert_eq!(
            value["policy"],
            json!({"ceiling": "ask", "remoteManaged": false})
        );
        assert!(value.get("tunnels").is_none());
        assert!(
            value.get("desktop").is_none(),
            "we do not implement desktops"
        );
        assert_eq!(value["capabilities"], json!(["fs@1", "shell@1"]));
        assert_eq!(value["roots"][0]["mode"], json!("read_write"));
        // Round-trips.
        let back: NodeHello = serde_json::from_value(value).unwrap();
        assert_eq!(back, hello);
    }

    #[test]
    fn hello_params_match_the_challenge_shape() {
        let params = HelloParams {
            protocol: ProtocolRange::current(),
            brain_version: "0.1.0".into(),
            brain_sig: "c2ln".into(),
        };
        assert_eq!(
            serde_json::to_value(&params).unwrap(),
            json!({
                "protocol": {"min": 1, "max": 1},
                "brainVersion": "0.1.0",
                "brainSig": "c2ln",
            })
        );
    }

    #[test]
    fn shell_chunks_use_tags_and_camel_case() {
        let started = json!({"_tag": "Started", "pid": 4242, "pgid": 4242});
        let output = json!({"_tag": "Output", "stream": "stdout", "data": "hi\n", "seq": 0});
        let exit = json!({
            "_tag": "Exit",
            "code": 0,
            "signal": null,
            "timedOut": false,
            "durationMs": 12,
            "outputTruncated": false,
        });
        assert_eq!(
            serde_json::to_value(ShellChunk::Started {
                pid: 4242,
                pgid: 4242
            })
            .unwrap(),
            started
        );
        assert_eq!(
            serde_json::from_value::<ShellChunk>(output.clone()).unwrap(),
            ShellChunk::Output {
                stream: OutputStream::Stdout,
                data: "hi\n".into(),
                seq: 0,
            }
        );
        let exit_chunk: ShellChunk = serde_json::from_value(exit.clone()).unwrap();
        assert!(matches!(
            exit_chunk,
            ShellChunk::Exit {
                code: Some(0),
                signal: None,
                timed_out: false,
                duration_ms: 12,
                output_truncated: Some(false),
                spill_path: None
            }
        ));
        // The optional fields stay optional both ways.
        assert_eq!(
            serde_json::to_value(&exit_chunk).unwrap(),
            json!({"_tag": "Exit", "code": 0, "signal": null, "timedOut": false, "durationMs": 12, "outputTruncated": false})
        );
        // Signal death: code null, signal set.
        let killed = ShellChunk::Exit {
            code: None,
            signal: Some(9),
            timed_out: true,
            duration_ms: 5000,
            output_truncated: None,
            spill_path: Some("/var/lib/swarmbot/spill/inv_x".into()),
        };
        let value = serde_json::to_value(&killed).unwrap();
        assert_eq!(value["code"], json!(null));
        assert_eq!(value["spillPath"], json!("/var/lib/swarmbot/spill/inv_x"));
        assert_eq!(serde_json::from_value::<ShellChunk>(value).unwrap(), killed);
    }

    #[test]
    fn payloads_carry_the_invocation_and_camel_case_fields() {
        let read = FsRead {
            inv: inv(),
            path: "/tmp/x".into(),
            offset: Some(10),
            limit: None,
            encoding: Some(Encoding::Base64),
        };
        let value = serde_json::to_value(&read).unwrap();
        assert_eq!(value["path"], json!("/tmp/x"));
        assert_eq!(value["offset"], json!(10));
        assert_eq!(value["encoding"], json!("base64"));
        assert!(
            value["inv"]["bot_id"].is_string(),
            "inv is the contracts Invocation"
        );
        assert!(value.get("limit").is_none(), "None fields are omitted");
        let back: FsRead = serde_json::from_value(value).unwrap();
        assert_eq!(back, read);

        let exec = ShellExec {
            inv: inv(),
            command: "cargo test".into(),
            cwd: "/work".into(),
            timeout_ms: 30_000,
            env: None,
            output_limit_bytes: None,
        };
        let value = serde_json::to_value(&exec).unwrap();
        assert_eq!(value["timeoutMs"], json!(30_000));

        let edit = FsEdit {
            inv: inv(),
            path: "/work/a.rs".into(),
            old_string: "a".into(),
            new_string: "b".into(),
            replace_all: Some(true),
            expected_sha256: None,
        };
        let value = serde_json::to_value(&edit).unwrap();
        assert_eq!(value["oldString"], json!("a"));
        assert_eq!(value["replaceAll"], json!(true));

        let grep = FsGrep {
            inv: inv(),
            pattern: "TODO".into(),
            path: "/work".into(),
            glob: None,
            ignore_case: Some(true),
            literal: None,
            context_lines: None,
            output_mode: Some(GrepOutputMode::FilesWithMatches),
            limit: None,
        };
        let value = serde_json::to_value(&grep).unwrap();
        assert_eq!(value["ignoreCase"], json!(true));
        assert_eq!(value["outputMode"], json!("files_with_matches"));
    }

    #[test]
    fn brain_side_payloads_match_their_wire_names() {
        let approval = RequestApproval {
            invocation_id: InvocationId::new(),
            bot_id: BotId::new(),
            action: "shell".into(),
            target: "rm -rf build".into(),
            reason: "destructive".into(),
        };
        let value = serde_json::to_value(&approval).unwrap();
        assert!(value["invocationId"].is_string());
        assert!(value["botId"].is_string());

        let answer = ApprovalAnswer {
            decision: Decision::AllowOnce,
            by: "owner".into(),
        };
        assert_eq!(
            serde_json::to_value(&answer).unwrap(),
            json!({"decision": "allow_once", "by": "owner"})
        );

        let event = NodeEvent {
            event_type: EVENT_AUDIT.into(),
            data: json!({"tool": "shell"}),
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({"type": "audit", "data": {"tool": "shell"}})
        );
        // `data` may be absent on decode (§5.2 sends null for some events).
        let decoded: NodeEvent = serde_json::from_value(json!({"type": "node.connected"})).unwrap();
        assert_eq!(decoded.data, Value::Null);

        let beat = Heartbeat {
            at: 1_700_000_000_000,
            active_invocations: 2,
            load: Some(0.5),
            free_mem_bytes: None,
        };
        let value = serde_json::to_value(&beat).unwrap();
        assert_eq!(value["activeInvocations"], json!(2));
        assert!(value.get("freeMemBytes").is_none());
    }

    #[test]
    fn decode_params_maps_bad_payloads_to_a_non_retryable_refusal() {
        let good = decode_params::<FsStat>(
            FS_STAT,
            json!({"inv": serde_json::to_value(inv()).unwrap(), "path": "/x"}),
        );
        assert!(good.is_ok());
        let bad = decode_params::<FsStat>(FS_STAT, json!({"path": 3}));
        match bad {
            Err(TypedError::PolicyRefused {
                action,
                reason,
                retry,
            }) => {
                assert_eq!(action, FS_STAT);
                assert!(reason.contains("malformed params"));
                assert!(!retry);
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn file_stat_and_list_shapes_round_trip() {
        let stat = FileStat {
            kind: FileKind::Dir,
            size: 4096,
            mtime_ms: 123,
        };
        assert_eq!(
            serde_json::to_value(&stat).unwrap(),
            json!({"kind": "dir", "size": 4096, "mtimeMs": 123})
        );
        let list = FsListResult {
            entries: vec![ListEntry {
                path: "a.txt".into(),
                kind: FileKind::File,
                size: 2,
                mtime_ms: 7,
            }],
            truncated: false,
        };
        let value = serde_json::to_value(&list).unwrap();
        assert_eq!(value["entries"][0]["type"], json!("file"));
        assert_eq!(value["entries"][0]["mtimeMs"], json!(7));
        assert_eq!(serde_json::from_value::<FsListResult>(value).unwrap(), list);
    }

    #[test]
    fn policy_map_is_not_the_hello_policy() {
        // The hello `policy` object is the doc's {ceiling, remoteManaged};
        // the node's own per-surface PolicyMap stays local to the node.
        let map = PolicyMap::default();
        let hello_policy = HelloPolicy {
            ceiling: map.default,
            remote_managed: false,
        };
        assert_eq!(hello_policy.ceiling, PolicyCeiling::Ask);
    }
}
