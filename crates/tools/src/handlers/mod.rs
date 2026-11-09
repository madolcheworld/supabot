//! The seven built-in node tools, and the plumbing they share.
//!
//! Every handler follows the same five steps, and the helpers here keep those
//! steps identical across tools so a fix lands everywhere at once:
//!
//! 1. parse arguments (the registry already schema-validated them; parsing
//!    still fails loudly rather than defaulting, because handlers are also
//!    called directly by [`crate::LocalExecutor`] and by tests);
//! 2. resolve the target node — the optional `node` argument, else
//!    `ctx.node_id`;
//! 3. remote target: ask the gate, then forward a [`NodeCall`] through
//!    [`crate::NodeExecutor`] without touching the local disk;
//! 4. local target: resolve every path through [`PathPolicy`], ask the gate
//!    when [`PolicyEngine`] says `Ask`, refuse when it says `Never`;
//! 5. run the shared implementation function and stamp decision/surface/
//!    target/node into `ToolResult::meta` — the registry turns that into the
//!    audit line.
//!
//! The implementation functions (`exec_shell`, `fs_read_file`, …) are free
//! functions, not methods, so [`crate::LocalExecutor`] runs *the same code*
//! as the handlers: a node daemon and the brain's local node cannot drift
//! apart in behaviour.

pub mod edit_file;
pub mod glob;
pub mod grep;
pub mod list_dir;
pub mod read_file;
pub mod shell;
pub mod write_file;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use swarmbot_contracts::{DomainError, NodeId, Surface, ToolError, ToolResult, ToolSpec};

use crate::handler::{ApprovalAsk, NodeCall, ToolContext, ToolHandler};
use crate::policy::{Decision, PathPolicy, PolicyEngine, destructive_label};

pub use edit_file::EditFileHandler;
pub use glob::GlobHandler;
pub use grep::GrepHandler;
pub use list_dir::ListDirHandler;
pub use read_file::ReadFileHandler;
pub use shell::ShellHandler;
pub use write_file::WriteFileHandler;

/// The tool names, as one source of truth for the registry, the MCP gateway
/// and tests.
pub const TOOL_SHELL: &str = "shell";
pub const TOOL_READ_FILE: &str = "read_file";
pub const TOOL_WRITE_FILE: &str = "write_file";
pub const TOOL_EDIT_FILE: &str = "edit_file";
pub const TOOL_LIST_DIR: &str = "list_dir";
pub const TOOL_GLOB: &str = "glob";
pub const TOOL_GREP: &str = "grep";

/// Default line window of `read_file`; matches the paging advice its result
/// text gives.
pub const DEFAULT_READ_LINE_LIMIT: u64 = 2000;
/// Default match caps. A model that needs more narrows the pattern instead of
/// paging, which keeps results inside the context budget.
pub const DEFAULT_GLOB_LIMIT: u32 = 200;
pub const DEFAULT_GREP_LIMIT: u32 = 200;
pub const DEFAULT_LIST_LIMIT: u32 = 1000;

/// The services every built-in handler needs. Cloning is cheap (two `Arc`s),
/// so handlers share one instance built by the orchestration or the daemon.
#[derive(Clone, Debug)]
pub struct ToolEnv {
    /// The filesystem boundary of the node these handlers act on.
    pub policy: Arc<PathPolicy>,
    /// Ceilings: brain ∩ node, plus the destructive-command floor.
    pub engine: Arc<PolicyEngine>,
}

impl ToolEnv {
    pub fn new(policy: PathPolicy, engine: PolicyEngine) -> Self {
        Self {
            policy: Arc::new(policy),
            engine: Arc::new(engine),
        }
    }

    /// Wrap already-shared services.
    pub fn shared(policy: Arc<PathPolicy>, engine: Arc<PolicyEngine>) -> Self {
        Self { policy, engine }
    }
}

/// All seven built-in handlers, ready for [`crate::ToolRegistry::register`].
pub fn builtin_handlers(
    policy: Arc<PathPolicy>,
    engine: Arc<PolicyEngine>,
) -> Vec<Arc<dyn ToolHandler>> {
    let env = Arc::new(ToolEnv::shared(policy, engine));
    vec![
        Arc::new(ShellHandler::new(Arc::clone(&env))),
        Arc::new(ReadFileHandler::new(Arc::clone(&env))),
        Arc::new(WriteFileHandler::new(Arc::clone(&env))),
        Arc::new(EditFileHandler::new(Arc::clone(&env))),
        Arc::new(ListDirHandler::new(Arc::clone(&env))),
        Arc::new(GlobHandler::new(Arc::clone(&env))),
        Arc::new(GrepHandler::new(Arc::clone(&env))),
    ]
}

/// The specs of the seven built-ins — what a model sees before any gating.
pub fn builtin_specs() -> Vec<ToolSpec> {
    vec![
        shell::spec(),
        read_file::spec(),
        write_file::spec(),
        edit_file::spec(),
        list_dir::spec(),
        glob::spec(),
        grep::spec(),
    ]
}

// ---------------------------------------------------------------------------
// Shared handler plumbing
// ---------------------------------------------------------------------------

/// The outcome of the approval step: proceed (with the audit label for the
/// decision), or a block the caller renders as an error *result* — so the
/// audit line keeps its decision, surface and target even when nothing ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    /// Ran without asking; audit label `run`.
    Auto,
    /// Asked and the answer was yes; audit label `allow`.
    Allowed,
    /// Asked and the answer was no; audit label `deny`.
    Denied,
    /// A `Never` ceiling refused the call before anyone was asked; audit
    /// label `refuse`. The string is the reason the model sees.
    Refused(String),
}

impl Gate {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Auto => "run",
            Self::Allowed => "allow",
            Self::Denied => "deny",
            Self::Refused(_) => "refuse",
        }
    }

    /// The error result for a blocked call, or `None` when it may proceed.
    /// Denials and refusals are `Ok(ToolResult::error(…))` at the handler
    /// boundary, not `Err`: they are legitimate answers the model should
    /// read and react to, not infrastructure failures. (`Err` remains the
    /// shape for wire-level refusals, e.g. a remote node's backstop.)
    pub(crate) fn blocked_result(&self, tool: &str, target: &str) -> Option<ToolResult> {
        match self {
            Self::Denied => Some(ToolResult::error(format!(
                "denied by the owner: {tool} `{target}`"
            ))),
            Self::Refused(reason) => {
                Some(ToolResult::error(format!("refused by policy: {reason}")))
            }
            _ => None,
        }
    }
}

/// Consult the engine and, when it says `Ask`, the approval gate.
///
/// A *failing* gate (not a denying one) becomes [`ToolError::Failed`] — an
/// unreachable owner is an infrastructure error, not a decision.
pub(crate) async fn gate_call(
    ctx: &ToolContext,
    env: &ToolEnv,
    surface: Surface,
    tool: &str,
    target: &str,
    read_only: bool,
    command: Option<&str>,
) -> Result<Gate, ToolError> {
    match env.engine.decide(surface, read_only, command) {
        Decision::Run => Ok(Gate::Auto),
        Decision::Refuse => Ok(Gate::Refused(format!(
            "`{tool}` on {target} is refused by this node's policy ({surface} ceiling: never)"
        ))),
        Decision::Ask => {
            let reason = command.and_then(destructive_label).map(|label| {
                format!("command matches a destructive pattern ({label}); destructive commands always ask")
            });
            let ask = ApprovalAsk {
                surface,
                tool: tool.to_string(),
                target: target.to_string(),
                reason,
                read_only,
            };
            match ctx.approval.request(ctx, ask).await {
                Ok(true) => Ok(Gate::Allowed),
                Ok(false) => Ok(Gate::Denied),
                Err(error) => Err(gate_failure(error)),
            }
        }
    }
}

fn gate_failure(error: DomainError) -> ToolError {
    ToolError::failed(format!("the approval gate could not be reached: {error}"))
}

/// Record the policy facts the registry needs for the audit line and the UI
/// needs for the tool-call card. Kept in `meta`, never in `content`: the
/// model does not need to see its own approval metadata.
pub(crate) fn stamp_meta(
    result: &mut ToolResult,
    gate: &Gate,
    surface: Surface,
    target: &str,
    node: &NodeId,
) {
    result
        .meta
        .insert("decision".into(), Value::from(gate.label()));
    result
        .meta
        .insert("surface".into(), Value::from(surface.as_str()));
    result.meta.insert("target".into(), Value::from(target));
    result
        .meta
        .insert("node".into(), Value::from(node.as_str()));
}

/// The node a call is aimed at: the explicit `node` argument (`"local"` and
/// `nod_local` both mean this machine; anything else must parse as a
/// [`NodeId`]), else the context's host node.
pub(crate) fn target_node(ctx: &ToolContext, args: &Value) -> Result<NodeId, ToolError> {
    match opt_str(args, "node") {
        None => Ok(ctx.node_id.clone()),
        Some(name) if name == "local" || name == NodeId::LOCAL => Ok(NodeId::local()),
        Some(name) => NodeId::parse(&name)
            .map_err(|error| ToolError::invalid(format!("`node` is not a valid node id: {error}"))),
    }
}

/// Ask the gate, then forward a remote call. Shared by every handler so the
/// remote path asks exactly like the local one does.
pub(crate) async fn forward_remote(
    ctx: &ToolContext,
    env: &ToolEnv,
    tool: &str,
    node: NodeId,
    call: NodeCall,
) -> Result<ToolResult, ToolError> {
    let surface = call.surface();
    let read_only = call.read_only();
    let target = call.describe();
    let command = match &call {
        NodeCall::ShellExec { command, .. } => Some(command.as_str()),
        _ => None,
    };
    let gate = gate_call(ctx, env, surface, tool, &target, read_only, command).await?;
    let mut result = match gate.blocked_result(tool, &target) {
        Some(blocked) => blocked,
        None => ctx.nodes.exec(&node, call).await?,
    };
    stamp_meta(&mut result, &gate, surface, &target, &node);
    Ok(result)
}

// ---------------------------------------------------------------------------
// Argument extraction
// ---------------------------------------------------------------------------

pub(crate) fn req_str(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError::invalid(format!("`{key}` is required and must be a string")))
}

pub(crate) fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

pub(crate) fn opt_u64(args: &Value, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(Some)
            .ok_or_else(|| ToolError::invalid(format!("`{key}` must be a non-negative integer"))),
        Some(_) => Err(ToolError::invalid(format!("`{key}` must be a number"))),
    }
}

pub(crate) fn opt_u32(args: &Value, key: &str) -> Result<Option<u32>, ToolError> {
    opt_u64(args, key)?
        .map(|value| {
            u32::try_from(value).map_err(|_| ToolError::invalid(format!("`{key}` is too large")))
        })
        .transpose()
}

pub(crate) fn opt_bool(args: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(ToolError::invalid(format!("`{key}` must be a boolean"))),
    }
}

pub(crate) fn opt_env_map(args: &Value) -> Result<Option<BTreeMap<String, String>>, ToolError> {
    let Some(value) = args.get("env") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let obj = value
        .as_object()
        .ok_or_else(|| ToolError::invalid("`env` must be an object of string values"))?;
    let mut map = BTreeMap::new();
    for (key, item) in obj {
        let text = item
            .as_str()
            .ok_or_else(|| ToolError::invalid(format!("`env.{key}` must be a string")))?;
        map.insert(key.clone(), text.to_string());
    }
    Ok(Some(map))
}

/// Run a blocking filesystem implementation off the async worker. The
/// walkers (`glob`, `grep`, `list_dir`) and streaming reads can take seconds
/// on big trees; blocking a runtime worker that long would stall other turns.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        // A JoinError here means the blocking task panicked. Nothing in the
        // fs layer panics by construction (no unwrap/expect), so treat it as
        // an internal failure instead of propagating.
        Err(error) => Err(ToolError::failed(format!(
            "internal error: the file operation task failed: {error}"
        ))),
    }
}

/// A readable display form of a resolved path, used for card targets and
/// audit lines.
pub(crate) fn display(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
pub(crate) mod testkit {
    //! Shared test fixtures: a temp-rooted rig with an allow/deny gate and a
    //! recording node executor. Every handler test builds one of these, so
    //! the "never touch anything outside a TempDir" rule is enforced in one
    //! place.

    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use swarmbot_contracts::{BotId, ConversationId, Invocation, PolicyMap};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use crate::handler::{ApprovalGate, NodeExecutor};
    use crate::policy::PolicyEngine;

    /// An approval gate with a scripted answer that records every ask.
    #[derive(Debug, Default)]
    pub(crate) struct TestGate {
        answer: AtomicBool,
        pub asks: Mutex<Vec<ApprovalAsk>>,
    }

    impl TestGate {
        pub fn allowing() -> Arc<Self> {
            let gate = Arc::new(Self::default());
            gate.answer.store(true, Ordering::SeqCst);
            gate
        }

        pub fn denying() -> Arc<Self> {
            let gate = Arc::new(Self::default());
            gate.answer.store(false, Ordering::SeqCst);
            gate
        }

        pub fn ask_count(&self) -> usize {
            self.asks.lock().expect("test mutex").len()
        }

        pub fn last_ask(&self) -> Option<ApprovalAsk> {
            self.asks.lock().expect("test mutex").last().cloned()
        }
    }

    #[async_trait::async_trait]
    impl ApprovalGate for TestGate {
        async fn request(&self, _ctx: &ToolContext, ask: ApprovalAsk) -> Result<bool, DomainError> {
            self.asks.lock().expect("test mutex").push(ask);
            Ok(self.answer.load(Ordering::SeqCst))
        }
    }

    /// A node executor that records forwarded calls instead of moving them.
    #[derive(Debug, Default)]
    pub(crate) struct TestNodes {
        pub calls: Mutex<Vec<(NodeId, NodeCall)>>,
        response: Mutex<Option<ToolResult>>,
    }

    impl TestNodes {
        pub fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        pub fn responding(result: ToolResult) -> Arc<Self> {
            let nodes = Arc::new(Self::default());
            *nodes.response.lock().expect("test mutex") = Some(result);
            nodes
        }

        pub fn recorded(&self) -> Vec<(NodeId, NodeCall)> {
            self.calls.lock().expect("test mutex").clone()
        }
    }

    #[async_trait::async_trait]
    impl NodeExecutor for TestNodes {
        async fn exec(&self, node: &NodeId, call: NodeCall) -> Result<ToolResult, ToolError> {
            self.calls
                .lock()
                .expect("test mutex")
                .push((node.clone(), call));
            match self.response.lock().expect("test mutex").clone() {
                Some(result) => Ok(result),
                None => Err(ToolError::failed(format!(
                    "test executor: node {node} is not wired"
                ))),
            }
        }
    }

    /// One temp-rooted world: workspace, spool, allow-everything policy and
    /// (by default) always-run ceilings.
    pub(crate) struct Rig {
        pub tmp: TempDir,
        pub workspace: std::path::PathBuf,
        pub spool: std::path::PathBuf,
        pub gate: Arc<TestGate>,
        pub nodes: Arc<TestNodes>,
        pub env: Arc<ToolEnv>,
        pub ctx: ToolContext,
    }

    impl Rig {
        /// A rig where every ceiling is `Always`: tests exercise the tool
        /// logic, not the gate. Use [`Rig::with_engine`] for policy tests.
        pub fn new() -> Self {
            Self::with_engine(
                PolicyMap::uniform(swarmbot_contracts::PolicyCeiling::Always),
                PolicyMap::uniform(swarmbot_contracts::PolicyCeiling::Always),
                true,
            )
        }

        pub fn with_engine(
            brain: PolicyMap,
            node: PolicyMap,
            auto_approve_read_only: bool,
        ) -> Self {
            Self::with_engine_and_nodes(brain, node, auto_approve_read_only, TestNodes::new())
        }

        /// A rig whose remote forwards are answered by `nodes`.
        pub fn with_nodes(nodes: Arc<TestNodes>) -> Self {
            Self::with_engine_and_nodes(
                PolicyMap::uniform(swarmbot_contracts::PolicyCeiling::Always),
                PolicyMap::uniform(swarmbot_contracts::PolicyCeiling::Always),
                true,
                nodes,
            )
        }

        fn with_engine_and_nodes(
            brain: PolicyMap,
            node: PolicyMap,
            auto_approve_read_only: bool,
            nodes: Arc<TestNodes>,
        ) -> Self {
            let tmp = TempDir::new().expect("tempdir");
            let workspace = tmp.path().join("workspace");
            let spool = tmp.path().join("spool");
            std::fs::create_dir_all(&workspace).expect("workspace dir");
            std::fs::create_dir_all(&spool).expect("spool dir");
            let policy = Arc::new(PathPolicy::new(
                vec![swarmbot_contracts::NodeRoot::read_write(
                    "tmp",
                    tmp.path().display().to_string(),
                )],
                Vec::new(),
            ));
            let engine = Arc::new(PolicyEngine::new(brain, node, auto_approve_read_only));
            let gate = TestGate::allowing();
            let ctx = ToolContext {
                invocation: test_invocation(),
                workspace: workspace.clone(),
                node_id: NodeId::local(),
                approval: Arc::clone(&gate) as Arc<dyn ApprovalGate>,
                nodes: Arc::clone(&nodes) as Arc<dyn NodeExecutor>,
                cancel: CancellationToken::new(),
                spool_dir: spool.clone(),
            };
            Self {
                tmp,
                workspace,
                spool,
                gate,
                nodes,
                env: Arc::new(ToolEnv::shared(policy, engine)),
                ctx,
            }
        }

        /// Run one handler call and flatten `Err` into the error result a
        /// model would see, so tests assert on final text.
        pub async fn call(&self, handler: &dyn ToolHandler, args: Value) -> ToolResult {
            match handler.call(&self.ctx, args).await {
                Ok(result) => result,
                Err(error) => error.to_result(),
            }
        }

        pub fn write(&self, relative: &str, content: &str) -> std::path::PathBuf {
            let path = self.workspace.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("parent dir");
            }
            std::fs::write(&path, content).expect("test file write");
            path
        }
    }

    pub(crate) fn test_invocation() -> Invocation {
        Invocation::new(
            BotId::from_suffix(&"b".repeat(32)),
            ConversationId::from_suffix(&"c".repeat(32)),
        )
    }
}
