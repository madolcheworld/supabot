//! SwarmBot's model layer: **one port, four adapters**.
//!
//! The runner and the brain depend on [`LlmBackend`] and nothing else; how a
//! turn reaches a model is an adapter detail:
//!
//! * [`openai::OpenAiCompat`] — any OpenAI-compatible `/chat/completions`
//!   endpoint: SSE streaming, native tool calls, retries with backoff and
//!   `Retry-After`, whole-call timeout, cancellation. All HTTP goes through
//!   the injectable [`openai::HttpTransport`] port, so tests script gateway
//!   behaviour ([`test_util::ScriptedTransport`]) without a socket.
//! * [`fake::FakeBackend`] — deterministic scripted turns for tests and the
//!   offline demo; records the requests it received so orchestration can be
//!   asserted on.
//! * [`acp::AcpAgent`] — any agent that speaks the **Agent Client Protocol**
//!   over stdio: `opencode acp`, Claude Code (`claude-code-acp`), Codex
//!   (`codex-acp`), … One adapter for all of them: it keeps the agent process
//!   alive, renders each turn's history into a fresh ACP session, streams the
//!   agent's output as [`LlmEvent`]s and answers its permission asks with the
//!   configured policy (default: reject). Like the CLI harness it owns its
//!   tool loop — the agent's tools run under the agent's own sandbox.
//! * [`harness::ClaudeCli`] — `claude -p` skeleton: honest `probe` today,
//!   stream-json adapter in a later milestone. It reports
//!   `capabilities().tool_calls == false` because the harness runs its own
//!   tool loop through our MCP server.
//!
//! [`BackendRegistry`] turns [`swarmbot_contracts::BackendSpec`]s plus a
//! secrets store into live adapters (a `fake` backend always exists), and
//! [`types`] holds the wire-neutral conversation shapes the store persists as
//! a bot's history.
//!
//! Secret invariant across the whole crate: an API key exists only inside an
//! adapter's private field and the `Authorization` header it builds — or, for
//! the ACP adapter, inside the child agent's environment. Every `Debug` impl,
//! log line, error message and serialized value this crate produces is
//! key-free — the types that could hold one implement `Debug` manually, and
//! the tests assert it.

pub mod acp;
pub mod backend;
pub mod fake;
pub mod harness;
pub mod openai;
pub mod registry;
pub mod test_util;
pub mod types;

pub use acp::{
    AcpAgent, AcpPermissionAsk, AcpPermissionDecision, AcpPermissions, AcpTransport, AgentHandle,
    AgentLaunch, AgentProcess, FixedPermissionPolicy, PermissionMode, ProcessTransport,
};
pub use backend::{LlmBackend, LlmError, is_rate_limit};
pub use fake::{EMPTY_QUEUE_REPLY, FakeBackend, ScriptedToolCall, ScriptedTurn};
pub use harness::ClaudeCli;
pub use openai::{BodyStream, HttpError, HttpTransport, OpenAiCompat, ReqwestTransport};
pub use registry::{BackendInfo, BackendRegistry, FAKE_BACKEND_ID};
pub use types::{
    AgentToolCall, AgentToolState, BackendStatus, Capabilities, ChatMessage, ChatRequest,
    ChatResponse, FinishReason, LlmEvent, McpServerSpec, ModelList, ModelOption, Role, ToolCall,
};
