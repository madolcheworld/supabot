//! SwarmBot tools: the registry, the local handlers, and the safety rails
//! around them.
//!
//! Layout, following the call path of one tool invocation:
//!
//! * [`ToolRegistry`] — the funnel. Validates arguments against each spec's
//!   JSON Schema, dispatches to a [`ToolHandler`], enforces the result rules
//!   from `contracts/tool.rs` (capped text, `ERROR: ` prefixes, oversize
//!   output spilled to a page-able file) and writes the [`AuditLog`] line.
//!   `visible_specs` gates what a bot sees by toolset, node capabilities and
//!   task context.
//! * [`handler`] — the ports: [`ToolHandler`], [`ToolContext`],
//!   [`ApprovalGate`] (the way back to a human or rule) and [`NodeExecutor`]
//!   with [`NodeCall`] (the way out to a machine, as wire-ready data).
//! * [`handlers`] — the seven built-in node tools: `shell`, `read_file`,
//!   `write_file`, `edit_file`, `list_dir`, `glob`, `grep`. [`builtin_specs`]
//!   and [`builtin_handlers`] hand them to the orchestration in one call.
//!   `shell` has two shapes over one implementation: [`exec_shell_streaming`]
//!   (events + a real pid + a killable process group) is what a node daemon
//!   adapts onto its socket, and the buffered `exec_shell` behind
//!   [`LocalExecutor`] is what a model reads.
//! * [`policy`] — [`PathPolicy`] (the filesystem boundary: roots, deny list,
//!   symlink-safe resolution) and [`PolicyEngine`] (ceilings: the brain can
//!   ask, never raise; destructive shell commands always ask).
//! * [`audit`] — [`AuditLog`]: JSONL, one line per invocation, and it never
//!   fails a tool call.
//! * [`local`] — [`LocalExecutor`]: the [`NodeExecutor`] for this machine,
//!   which is what makes the brain's host a node and lets a node daemon reuse
//!   the exact same execution code.
//!
//! Nothing here depends on the brain: the orchestration calls into this crate
//! through the ports, and a node daemon embeds the same handlers behind a
//! [`LocalExecutor`].

pub mod audit;
pub mod handler;
pub mod handlers;
pub mod local;
pub mod policy;
pub mod registry;

pub use audit::{AuditDecision, AuditEntry, AuditEntryBuilder, AuditLog, AuditOutcome};
pub use handler::{ApprovalAsk, ApprovalGate, NodeCall, NodeExecutor, ToolContext, ToolHandler};
pub use handlers::{
    EditFileHandler, GlobHandler, GrepHandler, ListDirHandler, ReadFileHandler, ShellHandler,
    ToolEnv, WriteFileHandler, builtin_handlers, builtin_specs,
};
// The streaming shell contract, at the crate root because `swarmbot-node`
// implements `ShellEventSink` for its socket: it is a port, not an internal.
pub use handlers::shell::{
    ExitInfo, ProcGuard, ProcRegistry, ShellEvent, ShellEventSink, ShellExit, ShellFailure,
    ShellOptions, ShellRun, ShellStream, exec_shell_streaming,
};
pub use local::LocalExecutor;
pub use policy::{
    DESTRUCTIVE_PATTERNS, Decision, PathPolicy, PolicyEngine, destructive_label, is_destructive,
};
pub use registry::{ASK_TOOLS, TASK_TOOLS, ToolRegistry};
