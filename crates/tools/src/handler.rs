//! The ports the orchestration (and later the node daemon) rely on.
//!
//! Three traits split the responsibilities so the tool layer never grows a
//! dependency on the brain:
//!
//! * [`ToolHandler`] — one tool. Built-ins live in [`crate::handlers`]; the
//!   orchestration adds its own (messaging, tasks) to the same registry.
//! * [`ApprovalGate`] — the way back to a human or a rule. `tools` only asks;
//!   it never decides *who* answers. The gate must not block a tool call
//!   indefinitely: answering from a rule at once is the implementation's job.
//! * [`NodeExecutor`] — the way out to a machine. Local calls run in-process
//!   ([`crate::LocalExecutor`]); a brain forwards remote nodes over the node
//!   protocol. [`NodeCall`] is the RPC as *data*, so the same type crosses a
//!   socket later without a second translation layer.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use swarmbot_contracts::{
    DomainError, Invocation, NodeId, Surface, ToolError, ToolResult, ToolSpec, TurnId,
};
use tokio_util::sync::CancellationToken;

/// One tool: a definition models see, and the code that runs it.
#[async_trait::async_trait]
pub trait ToolHandler: Send + Sync {
    /// The definition models see.
    fn spec(&self) -> ToolSpec;

    /// Execute. `ctx.cancel` is tripped when the owner stops the turn.
    async fn call(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError>;
}

/// What a handler needs to know about the caller, and the two services it may
/// reach back into: approvals and remote nodes.
pub struct ToolContext {
    pub invocation: Invocation,
    /// Where the bot's files live; also the default `cwd` for `shell`.
    pub workspace: PathBuf,
    /// The node this call is aimed at (host node when the model omitted it).
    pub node_id: NodeId,
    pub approval: Arc<dyn ApprovalGate>,
    pub nodes: Arc<dyn NodeExecutor>,
    pub cancel: CancellationToken,
    /// Where truncated output is spilled.
    pub spool_dir: PathBuf,
}

impl ToolContext {
    /// A context aimed at the local node (the brain's own machine), with a
    /// fresh cancellation token.
    pub fn local(
        invocation: Invocation,
        workspace: impl Into<PathBuf>,
        approval: Arc<dyn ApprovalGate>,
        nodes: Arc<dyn NodeExecutor>,
        spool_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            invocation,
            workspace: workspace.into(),
            node_id: NodeId::local(),
            approval,
            nodes,
            cancel: CancellationToken::new(),
            spool_dir: spool_dir.into(),
        }
    }

    /// Aim the same context at another node (the bot's host is remote).
    pub fn with_node_id(mut self, node_id: NodeId) -> Self {
        self.node_id = node_id;
        self
    }

    /// Share one cancellation token, e.g. the token of the whole turn.
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// True when the target node executes in this process. Handlers run those
    /// calls inline; everything else is forwarded through [`NodeExecutor`].
    pub fn targets_local(&self) -> bool {
        self.node_id.is_local()
    }
}

impl Clone for ToolContext {
    fn clone(&self) -> Self {
        Self {
            invocation: self.invocation.clone(),
            workspace: self.workspace.clone(),
            node_id: self.node_id.clone(),
            approval: Arc::clone(&self.approval),
            nodes: Arc::clone(&self.nodes),
            cancel: self.cancel.clone(),
            spool_dir: self.spool_dir.clone(),
        }
    }
}

impl fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The trait objects are services, not data: name them instead of
        // requiring `Debug` on every implementation.
        f.debug_struct("ToolContext")
            .field("invocation", &self.invocation)
            .field("workspace", &self.workspace)
            .field("node_id", &self.node_id)
            .field("approval", &format_args!("dyn ApprovalGate"))
            .field("nodes", &format_args!("dyn NodeExecutor"))
            .field("cancel", &self.cancel)
            .field("spool_dir", &self.spool_dir)
            .finish()
    }
}

/// Asks a human (or a rule) for a decision. Implemented by the orchestration;
/// `tools` only calls it. It must never block a tool call indefinitely: the
/// implementation decides whether to answer from a rule at once.
#[async_trait::async_trait]
pub trait ApprovalGate: Send + Sync {
    /// `Ok(true)` = allowed, `Ok(false)` = denied (render `ERROR: denied …`).
    async fn request(&self, ctx: &ToolContext, ask: ApprovalAsk) -> Result<bool, DomainError>;

    /// A turn was stopped while a card was waiting: settle it as cancelled.
    ///
    /// The default is a no-op so a test double need not care. The real bridge
    /// overrides it because a blocking ask has no other way out: without this,
    /// Stop left the tool call waiting for the full `approval_expiry_secs`
    /// (ten minutes by default) before the turn could finish.
    fn cancel_turn(&self, turn: &TurnId) -> Result<usize, DomainError> {
        let _ = turn;
        Ok(0)
    }
}

/// One approval question, as data, so a gate can log it, show it in a card,
/// or match it against a rule without knowing which tool produced it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalAsk {
    pub surface: Surface,
    pub tool: String,
    /// The command line or path the decision is about.
    pub target: String,
    pub reason: Option<String>,
    /// True when the action only reads, so a rule can auto-allow it.
    pub read_only: bool,
}

/// Runs a call on a node. The local implementation executes in-process; the
/// brain's implementation forwards remote nodes over the node protocol.
///
/// `invocation` is the **caller's own**, and it travels to the executing machine
/// unchanged. It is a parameter rather than something an executor works out for
/// itself because an executor *cannot* work it out: every handler already has it
/// in [`ToolContext::invocation`], while the executor only sees a node id and a
/// [`NodeCall`]. The old signature forced the brain to guess — scan the running
/// turns and hope exactly one belonged to a bot hosted on that node — and a guess
/// is wrong the moment two turns run on one node in parallel. It is not cosmetic
/// either: the invocation is what the node audits the call against and what it
/// echoes back in `node.requestApproval`, so a guessed one puts an approval card
/// in the wrong conversation, which is a lie about who asked.
#[async_trait::async_trait]
pub trait NodeExecutor: Send + Sync {
    async fn exec(
        &self,
        node: &NodeId,
        invocation: &Invocation,
        call: NodeCall,
    ) -> Result<ToolResult, ToolError>;
}

/// A node RPC, as data, so the same type can cross a socket later.
///
/// Paths are the caller's *unresolved* strings: a remote node has its own
/// filesystem and its own [`crate::PathPolicy`], so resolution must happen
/// there, never on the sending side.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub enum NodeCall {
    FsRead {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encoding: Option<String>,
    },
    FsWrite {
        path: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_sha256: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        create_parents: Option<bool>,
    },
    FsEdit {
        path: String,
        old_string: String,
        new_string: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replace_all: Option<bool>,
    },
    FsStat {
        path: String,
    },
    FsList {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        depth: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        include_hidden: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    FsGlob {
        pattern: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    FsGrep {
        pattern: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        glob: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ignore_case: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        literal: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_lines: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_mode: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    FsMkdir {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parents: Option<bool>,
    },
    FsMove {
        from: String,
        to: String,
    },
    FsRemove {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recursive: Option<bool>,
    },
    ShellExec {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<BTreeMap<String, String>>,
    },
}

impl NodeCall {
    /// The policy surface this call acts on. Nodes use it to apply their own
    /// ceilings as a backstop, and the audit log to label the line.
    pub fn surface(&self) -> Surface {
        match self {
            Self::FsRead { .. }
            | Self::FsStat { .. }
            | Self::FsList { .. }
            | Self::FsGlob { .. }
            | Self::FsGrep { .. } => Surface::ReadFile,
            // `FsEdit` is its own surface: the local `edit_file` handler and
            // `LocalExecutor` both ask on `Surface::EditFile`, so a node whose
            // owner tightened only that surface must still be backstopped here.
            Self::FsEdit { .. } => Surface::EditFile,
            Self::FsWrite { .. }
            | Self::FsMkdir { .. }
            | Self::FsMove { .. }
            | Self::FsRemove { .. } => Surface::WriteFile,
            Self::ShellExec { .. } => Surface::Shell,
        }
    }

    /// True when the call cannot modify anything on the node.
    pub fn read_only(&self) -> bool {
        matches!(
            self,
            Self::FsRead { .. }
                | Self::FsStat { .. }
                | Self::FsList { .. }
                | Self::FsGlob { .. }
                | Self::FsGrep { .. }
        )
    }

    /// A short, human-readable description of the target, for cards and logs.
    /// Never includes file contents.
    pub fn describe(&self) -> String {
        match self {
            Self::FsRead { path, .. } => format!("read {path}"),
            Self::FsWrite { path, content, .. } => {
                format!("write {path} ({} bytes)", content.len())
            }
            Self::FsEdit { path, .. } => format!("edit {path}"),
            Self::FsStat { path } => format!("stat {path}"),
            Self::FsList { path, .. } => format!("list {path}"),
            Self::FsGlob { pattern, .. } => format!("glob {pattern}"),
            Self::FsGrep { pattern, .. } => format!("grep {pattern}"),
            Self::FsMkdir { path, .. } => format!("mkdir {path}"),
            Self::FsMove { from, to } => format!("move {from} -> {to}"),
            Self::FsRemove { path, .. } => format!("remove {path}"),
            Self::ShellExec { command, .. } => command.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_call_serde_is_tagged_snake_case() {
        let call = NodeCall::FsRead {
            path: "notes/todo.md".into(),
            offset: Some(10),
            limit: None,
            encoding: None,
        };
        let json = serde_json::to_value(&call).expect("serializes");
        assert_eq!(json["call"], "fs_read");
        assert_eq!(json["path"], "notes/todo.md");
        assert_eq!(json["offset"], 10);
        // Absent optionals stay off the wire: an old node must be able to
        // decode a new call.
        assert!(!json.as_object().expect("object").contains_key("limit"));
        let back: NodeCall = serde_json::from_value(json).expect("round-trips");
        assert_eq!(back, call);
    }

    #[test]
    fn shell_call_carries_env_as_a_map() {
        let mut env = BTreeMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        let call = NodeCall::ShellExec {
            command: "echo $FOO".into(),
            cwd: Some("/srv/bot".into()),
            timeout_ms: Some(5_000),
            env: Some(env),
        };
        let json = serde_json::to_value(&call).expect("serializes");
        assert_eq!(json["call"], "shell_exec");
        assert_eq!(json["env"]["FOO"], "bar");
        let back: NodeCall =
            serde_json::from_value(serde_json::to_value(&call).expect("serializes"))
                .expect("round-trips");
        assert_eq!(back, call);
    }

    #[test]
    fn surfaces_and_read_only_flags_are_consistent() {
        assert!(
            NodeCall::FsRead {
                path: "x".into(),
                offset: None,
                limit: None,
                encoding: None
            }
            .read_only()
        );
        assert_eq!(
            NodeCall::FsWrite {
                path: "x".into(),
                content: String::new(),
                expected_sha256: None,
                create_parents: None
            }
            .surface(),
            Surface::WriteFile
        );
        assert_eq!(
            NodeCall::ShellExec {
                command: "true".into(),
                cwd: None,
                timeout_ms: None,
                env: None
            }
            .surface(),
            Surface::Shell
        );
        assert_eq!(
            NodeCall::FsEdit {
                path: "x".into(),
                old_string: "a".into(),
                new_string: "b".into(),
                replace_all: None
            }
            .surface(),
            Surface::EditFile,
            "a remote edit is governed by the same surface as a local one"
        );
        assert!(
            !NodeCall::FsRemove {
                path: "x".into(),
                recursive: None
            }
            .read_only()
        );
    }

    #[test]
    fn describe_never_leaks_content() {
        let call = NodeCall::FsWrite {
            path: "secrets.env".into(),
            content: "API_KEY=hunter2".into(),
            expected_sha256: None,
            create_parents: None,
        };
        let text = call.describe();
        assert!(text.contains("secrets.env"));
        assert!(!text.contains("hunter2"), "{text}");
    }
}
