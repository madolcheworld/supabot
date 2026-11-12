//! Wire-neutral conversation types.
//!
//! These are the shapes the brain stores and the runner thinks in; every
//! adapter maps them to and from its own wire format. Two invariants drive
//! the design:
//!
//! * **Stability**: a bot's history is persisted opaquely as an array of
//!   [`ChatMessage`] (see `swarmbot_contracts::repo::BotSessionRecord`), so
//!   the serde representation is part of the storage schema. Optional fields
//!   serialize with `skip_serializing_if` and deserialize with `default`,
//!   which lets us *add* fields later without breaking stored rows.
//! * **No secrets**: [`ChatRequest::extra_headers`] is caller-supplied and
//!   may legitimately carry an auth header, so the `Debug` impl is manual and
//!   masks values of secret-looking header names. A transcript or log line
//!   that renders a request must never leak a key.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use swarmbot_contracts::{ToolSpec, Usage};

/// Who said a message. Serialized `snake_case`; part of the stored schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One message in a bot's history.
///
/// A single struct covers all roles (instead of an enum) because the store
/// keeps history as a plain JSON array and adapters need cheap field access;
/// role-specific fields are `Option`/empty for the roles that do not use them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    /// Text content. For a tool result this is the result text.
    pub content: String,
    /// Assistant tool calls, in the order the model produced them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Set on a `Tool` message: which call it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Optional display name (a bot's handle in a group).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::plain(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::plain(Role::Assistant, content)
    }

    /// The result of one tool call, addressed by its call id.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            name: None,
        }
    }

    /// An assistant message that carries tool calls (and possibly preamble
    /// text). The runner appends this to history before executing the calls,
    /// so the next request round-trips them exactly as the model sent them.
    pub fn assistant_tool_calls(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
            name: None,
        }
    }

    /// Attach a display name (a bot's handle in a group).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    fn plain(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }
}

/// A tool call as one backend reported it. `arguments` is parsed JSON when
/// the model produced valid JSON; the OpenAI adapter falls back to
/// `{"_raw": "<string>"}` for broken arguments so one malformed call cannot
/// kill a turn (the registry's schema validation rejects it and the loop
/// retries).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

/// An MCP (Model Context Protocol) server attached to a single model turn.
///
/// Consumed by backends that run their own tool loop (such as
/// [`crate::acp::AcpAgent`]), giving the agent per-turn access to SwarmBot's
/// tools with the caller's exact bot, conversation, and turn identity.
///
/// The `Debug` impl is manual: `Http::headers` and `Stdio::env` mask values of
/// secret-bearing keys so tokens or credentials never reach logs or transcripts.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpServerSpec {
    Http {
        name: String,
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
    },
    Stdio {
        name: String,
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
}

impl McpServerSpec {
    /// Construct an HTTP MCP server spec with no extra headers.
    pub fn http(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self::Http {
            name: name.into(),
            url: url.into(),
            headers: BTreeMap::new(),
        }
    }

    /// Construct a stdio MCP server spec with no args or extra env.
    pub fn stdio(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self::Stdio {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    /// Human-readable server identifier.
    pub fn name(&self) -> &str {
        match self {
            Self::Http { name, .. } | Self::Stdio { name, .. } => name,
        }
    }
}

impl fmt::Debug for McpServerSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http { name, url, headers } => f
                .debug_struct("Http")
                .field("name", name)
                .field("url", url)
                .field("headers", &RedactedHeaders(headers))
                .finish(),
            Self::Stdio {
                name,
                command,
                args,
                env,
            } => f
                .debug_struct("Stdio")
                .field("name", name)
                .field("command", command)
                .field("args", args)
                .field("env", &RedactedEnv(env))
                .finish(),
        }
    }
}

/// Everything one model call needs. Assembled by the runner (instructions and
/// history come from the bot's session), consumed by a backend adapter.
///
/// The `Debug` impl is manual: `extra_headers` values are masked when the
/// header name looks secret-bearing, because requests get logged and
/// transcribed.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// Model id for this call; when empty the backend's configured default
    /// model is used.
    pub model: String,
    /// System prompt, assembled by the caller (persona + routing rules).
    pub instructions: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Merged verbatim into the HTTP body: a gateway's own parameters (for
    /// example inferhub's bid) without this crate knowing them. Wins over
    /// every built-in key and over the backend spec's `extra_body`.
    pub extra_body: Map<String, Value>,
    pub extra_headers: BTreeMap<String, String>,
    /// Optional per-turn working directory (used by ACP backends to isolate
    /// each bot's session inside its own workspace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Optional per-turn MCP servers exposed to backends that run their own
    /// tool loop (such as ACP agents).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerSpec>,
}

impl fmt::Debug for ChatRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatRequest")
            .field("model", &self.model)
            .field("instructions", &self.instructions)
            .field("messages", &self.messages)
            .field("tools", &self.tools)
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("extra_body", &self.extra_body)
            .field("extra_headers", &RedactedHeaders(&self.extra_headers))
            .field("cwd", &self.cwd)
            .field("mcp_servers", &self.mcp_servers)
            .finish()
    }
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, instructions: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            instructions: instructions.into(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            extra_body: Map::new(),
            extra_headers: BTreeMap::new(),
            cwd: None,
            mcp_servers: Vec::new(),
        }
    }

    /// Rough token count, used by the brain to decide when to compact a
    /// bot's history.
    ///
    /// Heuristic (deliberately cheap, never exact): **chars/4** over the
    /// instructions and all message text (plus tool-call arguments), **plus 4
    /// tokens per message** for role/framing overhead, **plus the serialized
    /// size of the tool specs / 4** (tool schemas are re-sent every call and
    /// dominate small conversations). Only used for a threshold decision, so
    /// being off by tens of tokens is fine; tokenizers are model-specific and
    /// not worth a dependency here.
    pub fn token_estimate(&self) -> u64 {
        let mut chars: usize = self.instructions.chars().count() + self.model.chars().count();
        for message in &self.messages {
            chars += message.content.chars().count();
            chars += message.name.as_ref().map_or(0, |n| n.chars().count());
            for call in &message.tool_calls {
                chars += call.id.len() + call.name.len() + call.arguments.to_string().len();
            }
        }
        let tools_chars = serde_json::to_string(&self.tools).map_or(0, |s| s.len());
        (chars / 4 + 4 * self.messages.len() + tools_chars / 4) as u64
    }
}

/// One completed assistant turn, assembled by the backend.
///
/// Invariant: this value is **authoritative**. `LlmEvent` deltas streamed
/// during the call are best-effort (a retry after a mid-stream failure
/// restarts them from scratch); anything persisted or acted on comes from
/// here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub text: String,
    /// Chain-of-thought text, when the backend exposes it.
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
    /// `llm_calls` is 1 for one backend call; the runner accumulates tool
    /// executions itself.
    pub usage: Usage,
}

impl ChatResponse {
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// An empty, successful response — a useful default for adapters that
    /// assemble incrementally.
    pub fn empty() -> Self {
        Self {
            text: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                llm_calls: 1,
                ..Usage::default()
            },
        }
    }
}

/// Streaming progress of one assistant turn, for live UI and transcripts.
///
/// Serialized with serde's *external* tagging (`{"text_delta": "…"}`)
/// because the delta variants are newtype/struct shapes that internal
/// tagging cannot represent; this is the one place the crate deviates from
/// the workspace's `{"kind": …}` convention, and LlmEvents are transport
/// events, not stored rows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmEvent {
    TextDelta(String),
    ReasoningDelta(String),
    /// Emitted once per tool call, on the first fragment seen for its index.
    /// `id`/`name` are whatever arrived in that first fragment (usually both);
    /// later fragments may still fill a missing id/name, which updates the
    /// assembled call but is not re-announced.
    ToolCallStart {
        index: u32,
        id: String,
        name: String,
    },
    ToolCallArgsDelta {
        index: u32,
        fragment: String,
    },
    /// Emitted exactly once per successful call, immediately before `chat`
    /// returns; `tool_calls` are already assembled.
    Finished {
        finish_reason: FinishReason,
        usage: Usage,
        text: String,
        reasoning: String,
        tool_calls: Vec<ToolCall>,
    },
}

/// Why the model stopped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    #[default]
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    /// The backend reported a failure or something this build cannot map.
    Error,
}

/// One model a backend can run, as its own listing advertises it: the id the
/// provider accepts, plus a display label when the provider offers one (ACP
/// agents do; an OpenAI-compatible `/models` row does not).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOption {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl ModelOption {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: None,
        }
    }

    pub fn with_label(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: Some(label.into()),
        }
    }
}

/// A backend's model list **and whether picking from it reaches the agent**.
///
/// `authoritative == true`: choosing an id makes the backend run that model
/// (an OpenAI-compatible gateway accepts the string; an ACP agent that
/// advertises the unstable model state accepts `session/set_model`).
/// `authoritative == false`: the list is informational — read from the
/// agent's own CLI via `options.models_command` — and a pick is recorded as
/// an override the agent may ignore (opencode keeps its own configured
/// model). The UI must say so instead of offering a choice that silently
/// does nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelList {
    pub models: Vec<ModelOption>,
    pub authoritative: bool,
}

impl ModelList {
    pub fn authoritative(models: Vec<ModelOption>) -> Self {
        Self {
            models,
            authoritative: true,
        }
    }

    pub fn informational(models: Vec<ModelOption>) -> Self {
        Self {
            models,
            authoritative: false,
        }
    }
}

/// Health of one backend, as the UI's Settings screen shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendStatus {
    pub backend: String,
    pub ok: bool,
    /// Human-readable one-liner: what was checked and what came back.
    pub detail: String,
    pub model: Option<String>,
    /// Backend or harness version, when it reports one.
    pub version: Option<String>,
}

/// What a backend can do, so the runner can adapt (drive a tool loop or not,
/// show reasoning, allow steering).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Emits incremental deltas while generating.
    pub streaming: bool,
    /// Reports native tool calls, so the runner must drive the tool loop.
    pub tool_calls: bool,
    /// Exposes reasoning/thinking text.
    pub reasoning: bool,
    /// Accepts new input while a turn is still running.
    pub steer: bool,
    /// Sessions survive a brain restart (native resume).
    pub survives_restart: bool,
}

/// True for header names whose values must never be rendered in `Debug`,
/// logs or errors. Matching is deliberately broad: a caller can smuggle a
/// key through any custom header, and leaking one is worse than masking a
/// harmless value.
pub(crate) fn is_secret_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "authorization"
        || n == "proxy-authorization"
        || n == "cookie"
        || n.contains("api-key")
        || n.contains("api_key")
        || n.contains("auth-token")
        || n.contains("auth_token")
        || n.contains("token")
        || n.contains("secret")
        || n.contains("password")
        || n.contains("credential")
}

/// True for environment variable names whose values may carry credentials and
/// must be masked in `Debug` output.
pub(crate) fn is_secret_env(name: &str) -> bool {
    if is_secret_header(name) {
        return true;
    }
    let n = name.to_ascii_lowercase();
    n.contains("key")
        || n.contains("auth")
        || n.contains("passwd")
        || n.contains("private")
        || n.contains("bearer")
}

/// The header value as it may be rendered: masked when secret-bearing.
pub(crate) fn redact_header_value<'a>(name: &str, value: &'a str) -> Cow<'a, str> {
    if is_secret_header(name) {
        Cow::Borrowed("[redacted]")
    } else {
        Cow::Borrowed(value)
    }
}

/// The env value as it may be rendered: masked when secret-bearing.
pub(crate) fn redact_env_value<'a>(name: &str, value: &'a str) -> Cow<'a, str> {
    if is_secret_env(name) {
        Cow::Borrowed("[redacted]")
    } else {
        Cow::Borrowed(value)
    }
}

/// `Debug` for a header map that masks secret-bearing values.
pub(crate) struct RedactedHeaders<'a, M>(pub(crate) &'a M);

impl<'a> fmt::Debug for RedactedHeaders<'a, BTreeMap<String, String>> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(k, v)| (k, redact_header_value(k, v))))
            .finish()
    }
}

impl<'a> fmt::Debug for RedactedHeaders<'a, Vec<(String, String)>> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(
                self.0
                    .iter()
                    .map(|(k, v)| (k.as_str(), redact_header_value(k, v))),
            )
            .finish()
    }
}

/// `Debug` for a process env map that masks secret-bearing values.
pub(crate) struct RedactedEnv<'a>(pub(crate) &'a BTreeMap<String, String>);

impl<'a> fmt::Debug for RedactedEnv<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(k, v)| (k, redact_env_value(k, v))))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::Toolset;

    fn tool_spec(name: &str, params: Value) -> ToolSpec {
        ToolSpec::new(name, format!("{name} description"), params, Toolset::Bot)
    }

    #[test]
    fn message_constructors_fill_only_their_role_fields() {
        let user = ChatMessage::user("hi").with_name("lead");
        assert_eq!(user.role, Role::User);
        assert_eq!(user.name.as_deref(), Some("lead"));
        assert!(user.tool_calls.is_empty());

        let tool = ChatMessage::tool("call_1", "ERROR: boom");
        assert_eq!(tool.role, Role::Tool);
        assert_eq!(tool.tool_call_id.as_deref(), Some("call_1"));

        let assistant = ChatMessage::assistant_tool_calls(
            "",
            vec![ToolCall::new("call_1", "shell", serde_json::json!({}))],
        );
        assert_eq!(assistant.tool_calls.len(), 1);
        assert_eq!(assistant.tool_calls[0].name, "shell");
    }

    #[test]
    fn history_json_stays_compact_and_tolerates_missing_fields() {
        // The stored schema: plain messages carry no empty option fields...
        let json = serde_json::to_value(ChatMessage::user("hello")).expect("serializable");
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hello");
        assert!(json.get("tool_calls").is_none());
        assert!(json.get("name").is_none());

        // ...and a row written by an older build (no new fields) still loads.
        let old = serde_json::json!({"role": "assistant", "content": "ok"});
        let parsed: ChatMessage = serde_json::from_value(old).expect("forward compatible");
        assert_eq!(parsed.role, Role::Assistant);
        assert!(parsed.tool_calls.is_empty());

        // A whole history round-trips as an opaque array (BotSessionRecord).
        let history = vec![ChatMessage::user("a"), ChatMessage::assistant("b")];
        let value = serde_json::to_value(&history).expect("serializable");
        let back: Vec<ChatMessage> = serde_json::from_value(value).expect("round trip");
        assert_eq!(back, history);
    }

    #[test]
    fn token_estimate_grows_with_history_and_tools() {
        let mut request = ChatRequest::new("m", "short instructions");
        let base = request.token_estimate();

        request.messages.push(ChatMessage::user("x".repeat(400)));
        let with_history = request.token_estimate();
        assert!(
            with_history >= base + 100,
            "400 chars must add ~100 tokens: {base} -> {with_history}"
        );

        request.tools.push(tool_spec(
            "message_bot",
            serde_json::json!({"type": "object", "properties": {"to": {"type": "string"}, "text": {"type": "string"}}, "required": ["to", "text"]}),
        ));
        let with_tools = request.token_estimate();
        assert!(
            with_tools > with_history,
            "tool specs must add to the estimate: {with_history} -> {with_tools}"
        );
    }

    #[test]
    fn chat_request_debug_masks_secret_headers() {
        let mut request = ChatRequest::new("m", "i");
        request
            .extra_headers
            .insert("Authorization".into(), "Bearer sk-super-secret".into());
        request
            .extra_headers
            .insert("X-Gateway".into(), "inferhub".into());
        let debug = format!("{request:?}");
        assert!(!debug.contains("sk-super-secret"), "{debug}");
        assert!(debug.contains("[redacted]"), "{debug}");
        // Non-secret headers stay readable: debugging must not go blind.
        assert!(debug.contains("inferhub"), "{debug}");
    }

    #[test]
    fn chat_response_reports_tool_calls() {
        assert!(!ChatResponse::empty().has_tool_calls());
        let mut response = ChatResponse::empty();
        response
            .tool_calls
            .push(ToolCall::new("c", "shell", serde_json::json!({})));
        assert!(response.has_tool_calls());
    }

    #[test]
    fn chat_request_mcp_servers_serde_is_backward_compatible() {
        let legacy = serde_json::json!({
            "model": "m",
            "instructions": "be helpful",
            "messages": [],
            "tools": [],
            "max_tokens": null,
            "temperature": null,
            "extra_body": {},
            "extra_headers": {}
        });
        let parsed: ChatRequest = serde_json::from_value(legacy).expect("legacy ChatRequest loads");
        assert!(parsed.mcp_servers.is_empty());
        assert!(parsed.cwd.is_none());

        // Empty mcp_servers is omitted when serializing.
        let compact = serde_json::to_value(&parsed).expect("serializable");
        assert!(compact.get("mcp_servers").is_none());

        // Populated Http & Stdio specs round-trip cleanly.
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".into(), "Bearer tok-1".into());
        let mut env = BTreeMap::new();
        env.insert("API_KEY".into(), "secret-1".into());
        let mut req = ChatRequest::new("m", "i");
        req.mcp_servers = vec![
            McpServerSpec::Http {
                name: "swarmbot".into(),
                url: "http://127.0.0.1:9000/mcp/t/1".into(),
                headers,
            },
            McpServerSpec::Stdio {
                name: "local".into(),
                command: "/usr/bin/mcp".into(),
                args: vec!["--stdio".into()],
                env,
            },
        ];
        let value = serde_json::to_value(&req).expect("serializable");
        let back: ChatRequest = serde_json::from_value(value).expect("round trips");
        assert_eq!(back, req);

        // Minimal Http & Stdio JSON without optional fields deserializes via defaults.
        let minimal_http: McpServerSpec = serde_json::from_value(serde_json::json!({
            "kind": "http",
            "name": "s1",
            "url": "http://127.0.0.1/mcp"
        }))
        .expect("minimal http");
        assert_eq!(
            minimal_http,
            McpServerSpec::http("s1", "http://127.0.0.1/mcp")
        );

        let minimal_stdio: McpServerSpec = serde_json::from_value(serde_json::json!({
            "kind": "stdio",
            "name": "s2",
            "command": "node"
        }))
        .expect("minimal stdio");
        assert_eq!(minimal_stdio, McpServerSpec::stdio("s2", "node"));
    }

    #[test]
    fn mcp_server_spec_debug_masks_secret_headers_and_env() {
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".into(), "Bearer sk-mcp-secret".into());
        headers.insert("X-Turn-Token".into(), "turn-secret-99".into());
        headers.insert("X-Client".into(), "swarmbot".into());
        let http = McpServerSpec::Http {
            name: "swarmbot".into(),
            url: "http://127.0.0.1:8788/mcp".into(),
            headers,
        };

        let mut env = BTreeMap::new();
        env.insert("OPENAI_API_KEY".into(), "sk-env-secret".into());
        env.insert("AUTH_TOKEN".into(), "tok-env-secret".into());
        env.insert("RUST_LOG".into(), "info".into());
        let stdio = McpServerSpec::Stdio {
            name: "helper".into(),
            command: "helper-mcp".into(),
            args: vec!["--flag".into()],
            env,
        };

        let mut req = ChatRequest::new("m", "i");
        req.mcp_servers = vec![http, stdio];
        let debug = format!("{req:?}");

        assert!(!debug.contains("sk-mcp-secret"), "{debug}");
        assert!(!debug.contains("turn-secret-99"), "{debug}");
        assert!(!debug.contains("sk-env-secret"), "{debug}");
        assert!(!debug.contains("tok-env-secret"), "{debug}");
        assert!(debug.contains("[redacted]"), "{debug}");
        assert!(debug.contains("swarmbot"), "{debug}");
        assert!(debug.contains("info"), "{debug}");
        assert!(debug.contains("http://127.0.0.1:8788/mcp"), "{debug}");
        assert!(debug.contains("helper-mcp"), "{debug}");
    }
}
