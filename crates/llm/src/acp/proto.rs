//! The ACP (Agent Client Protocol) wire shapes — the subset SwarmBot speaks.
//!
//! ACP is JSON-RPC 2.0 over stdio, one newline-delimited JSON object per
//! message, in both directions simultaneously: we (the *client*) drive turns
//! with `initialize`, `session/new` and `session/prompt`; the agent streams
//! `session/update` notifications and may call *us* (`session/request_permission`,
//! `fs/*`, `terminal/*`). The full schema lives in the `agent-client-protocol`
//! package; this module types only what the adapter reads or writes and keeps
//! the rest as loose [`Value`]s — an agent that sends a field this build has
//! never seen must not fail a turn.
//!
//! Two invariants:
//!
//! * **Tolerant parsing.** Everything optional in the schema stays optional
//!   here, and unknown `sessionUpdate` kinds classify as [`SessionUpdate::Ignored`]
//!   rather than as errors. The protocol is version 1 but moving fast; a hard
//!   parse failure on an additive field would break working agents.
//! * **No secrets.** Nothing on this wire is ours to leak: prompts are the
//!   bot's own history, and authentication uses the agent's own login (or an
//!   environment variable set at spawn) — never a value this module renders.

use serde::Deserialize;
use serde_json::Value;

/// The ACP protocol version this build speaks. `initialize` sends it and the
/// agent answers with the version *it* will use; a mismatch is a hard error
/// (the spec tells the client to disconnect).
pub const PROTOCOL_VERSION: u64 = 1;

/// One parsed line from the agent's stdout.
#[derive(Clone, Debug)]
pub(crate) enum Incoming {
    /// A response to one of our requests.
    Response {
        id: u64,
        result: Result<Value, RpcError>,
    },
    /// The agent calls us; it expects a response with the same id.
    Request {
        id: u64,
        method: String,
        params: Value,
    },
    /// Fire-and-forget from the agent (`session/update` and extensions).
    Notification { method: String, params: Value },
}

/// A JSON-RPC error object, kept verbatim: the agent's own message is the most
/// honest description of why it refused, so it travels into operator-facing
/// errors untouched (truncated).
#[derive(Clone, Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    /// JSON-RPC's "the method does not exist / is not available". Agents use
    /// it for optional ACP methods (`session/set_model`, `session/load`), so
    /// callers treat it as "unsupported", not as a failure.
    pub const METHOD_NOT_FOUND: i64 = -32601;

    pub fn is_method_not_found(&self) -> bool {
        self.code == Self::METHOD_NOT_FOUND
    }
}

/// Classify one stdout line. The `Err` string says why the line is not
/// readable JSON-RPC; the connection logs it and keeps going, because one
/// stray debug print on an agent's stdout must not kill a turn.
pub(crate) fn classify(line: &str) -> Result<Incoming, String> {
    let value: Value = serde_json::from_str(line).map_err(|err| format!("not JSON: {err}"))?;
    let object = value.as_object().ok_or("not a JSON object")?;

    let method = object.get("method").and_then(Value::as_str);
    // We only ever send integer ids, and an agent must echo the id it got.
    // A string id we cannot map back is the agent's bug, not a fatal one.
    let id = match object.get("id") {
        None => None,
        Some(Value::Number(number)) => Some(
            number
                .as_u64()
                .ok_or_else(|| format!("id {number} is not a u64"))?,
        ),
        Some(other) => return Err(format!("id is not a number: {other}")),
    };

    match (method, id) {
        (Some(method), Some(id)) => Ok(Incoming::Request {
            id,
            method: method.to_string(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        }),
        (Some(method), None) => Ok(Incoming::Notification {
            method: method.to_string(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        }),
        (None, Some(id)) => {
            let result = match (object.get("result"), object.get("error")) {
                (Some(result), _) => Ok(result.clone()),
                (None, Some(error)) => Err(RpcError {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(-1),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("(no message)")
                        .to_string(),
                    data: error.get("data").cloned(),
                }),
                (None, None) => return Err("response has neither result nor error".to_string()),
            };
            Ok(Incoming::Response { id, result })
        }
        (None, None) => Err("neither method nor id".to_string()),
    }
}

/// The `initialize` result. Only the fields this build acts on are typed —
/// serde ignores the rest (`agentCapabilities`, `_meta`, future additions),
/// which is exactly the tolerant-parsing invariant.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u64,
    #[serde(rename = "authMethods", default)]
    pub auth_methods: Vec<AuthMethod>,
}

/// One authentication method an agent advertises (`claude-login`,
/// `ANTHROPIC_API_KEY`, …). We never *perform* an interactive login: the
/// agent's own CLI login or its environment is the owner's business. The list
/// exists so `probe` and errors can name what the agent expects.
#[derive(Clone, Debug, Deserialize)]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// The `session/new` result. `models` drives the best-effort
/// `session/set_model`; session modes are not used in this milestone and stay
/// unparsed (ignored, like every other field this build does not act on).
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct NewSessionResult {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(default)]
    pub models: Option<SessionModels>,
    #[serde(rename = "configOptions", default)]
    pub config_options: Vec<ConfigOption>,
}

/// A session configuration option advertised by agents (e.g. OpenCode).
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ConfigOption {
    pub id: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub name: Option<String>,
    #[serde(rename = "type", default)]
    #[allow(dead_code)]
    pub option_type: Option<String>,
    #[serde(rename = "currentValue", default)]
    pub current_value: Option<Value>,
    #[serde(default)]
    pub options: Vec<ConfigOptionChoice>,
}

impl ConfigOption {
    pub fn current_value_str(&self) -> Option<&str> {
        self.current_value.as_ref().and_then(Value::as_str)
    }
}

/// One choice within a select-type `ConfigOption`.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ConfigOptionChoice {
    pub value: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// The **unstable** model-selection state some agents advertise.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct SessionModels {
    #[serde(rename = "availableModels", default)]
    pub available: Vec<ModelInfo>,
    #[serde(rename = "currentModelId")]
    pub current: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ModelInfo {
    #[serde(rename = "modelId")]
    pub model_id: String,
}

/// The `session/prompt` result: why the agent stopped.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct PromptResult {
    #[serde(rename = "stopReason")]
    pub stop_reason: StopReason,
}

/// Why the agent stopped a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    /// A value this build has never seen. Treated as a failure to map, not as
    /// a success — silently reading "unknown" as "end_turn" would hide bugs.
    #[serde(other)]
    Unknown,
}

/// The `session/request_permission` params, as far as we act on them.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct PermissionRequest {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// Loose on purpose: only `toolCallId`, `title`, `kind` and `rawInput`
    /// are lifted into [`super::AcpPermissionAsk`]; the rest is display noise.
    #[serde(rename = "toolCall", default)]
    pub tool_call: Value,
    #[serde(default)]
    pub options: Vec<PermissionOption>,
}

/// One selectable answer to a permission request.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PermissionOption {
    #[serde(rename = "optionId")]
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

/// The nature of a permission option. The adapter only ever *chooses*
/// `*_once` kinds: an `*_always` grant outlives the turn and this build keeps
/// no record of it, which is exactly the silent blanket approval the safety
/// rules forbid.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    #[serde(other)]
    Unknown,
}

/// What one `session/update` notification means for a turn.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SessionUpdate {
    /// Assistant output — becomes a `TextDelta` and part of the response.
    AgentMessage(String),
    /// Chain-of-thought — becomes a `ReasoningDelta`.
    AgentThought(String),
    /// The agent's own tool activity. Informational in this milestone: it is
    /// logged, not transcribed (the transcript renders text and reasoning).
    ToolCall {
        tool_call_id: String,
        title: String,
        kind: Option<String>,
        status: Option<String>,
    },
    /// Anything else (`user_message_chunk` echoes, plans, mode changes,
    /// unknown future kinds): carries the tag for an honest debug log.
    Ignored(String),
}

/// Classify the `update` object of a `session/update` notification.
pub(crate) fn classify_update(update: &Value) -> SessionUpdate {
    let tag = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("(missing sessionUpdate)");
    match tag {
        "agent_message_chunk" => match text_of_content(update.get("content")) {
            Some(text) => SessionUpdate::AgentMessage(text),
            None => SessionUpdate::Ignored(format!("{tag} with non-text content")),
        },
        "agent_thought_chunk" => match text_of_content(update.get("content")) {
            Some(text) => SessionUpdate::AgentThought(text),
            None => SessionUpdate::Ignored(format!("{tag} with non-text content")),
        },
        "tool_call" | "tool_call_update" => SessionUpdate::ToolCall {
            tool_call_id: string_field(update, "toolCallId"),
            title: string_field(update, "title"),
            kind: update
                .get("kind")
                .and_then(Value::as_str)
                .map(str::to_string),
            status: update
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        other => SessionUpdate::Ignored(other.to_string()),
    }
}

/// The text of a content block, when it is a text block. Images, audio and
/// embedded resources are honestly ignored: this adapter renders text.
fn text_of_content(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if content.get("type").and_then(Value::as_str) == Some("text") {
        return content
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    None
}

/// A string field, or empty: agents disagree about which display fields are
/// always present, and a missing title must not fail a turn.
fn string_field(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classify_separates_responses_requests_and_notifications() {
        match classify(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
            .expect("valid")
        {
            Incoming::Response { id, result } => {
                assert_eq!(id, 3);
                assert_eq!(result.expect("ok")["stopReason"], "end_turn");
            }
            other => panic!("expected a response, got {other:?}"),
        }

        match classify(
            r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32601,"message":"no such method"}}"#,
        )
        .expect("valid")
        {
            Incoming::Response { id, result } => {
                assert_eq!(id, 4);
                let err = result.expect_err("an error");
                assert_eq!(err.code, -32601);
                assert!(err.is_method_not_found());
            }
            other => panic!("expected a response, got {other:?}"),
        }

        match classify(r#"{"jsonrpc":"2.0","id":5,"method":"fs/read_text_file","params":{}}"#)
            .expect("valid")
        {
            Incoming::Request { id, method, .. } => {
                assert_eq!(id, 5);
                assert_eq!(method, "fs/read_text_file");
            }
            other => panic!("expected a request, got {other:?}"),
        }

        match classify(r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s"}}"#)
            .expect("valid")
        {
            Incoming::Notification { method, params } => {
                assert_eq!(method, "session/update");
                assert_eq!(params["sessionId"], "s");
            }
            other => panic!("expected a notification, got {other:?}"),
        }
    }

    #[test]
    fn junk_lines_are_described_not_fatal() {
        for junk in [
            "Loading…",                    // an agent's startup chatter
            "{}",                          // neither method nor id
            r#"{"id":"abc","result":{}}"#, // an id we cannot correlate
            r#"{"id":7}"#,                 // response without result/error
            "[1,2]",                       // not an object
        ] {
            let reason = classify(junk).expect_err("must not parse");
            assert!(!reason.is_empty());
        }
    }

    #[test]
    fn updates_classify_by_kind_and_ignore_the_unknown() {
        let message = classify_update(&json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hello"}
        }));
        assert_eq!(message, SessionUpdate::AgentMessage("hello".into()));

        let thought = classify_update(&json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": "hmm"}
        }));
        assert_eq!(thought, SessionUpdate::AgentThought("hmm".into()));

        // An image chunk is ignored with a reason, not rendered as empty text.
        let image = classify_update(&json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "image", "data": "…", "mimeType": "image/png"}
        }));
        assert!(matches!(image, SessionUpdate::Ignored(reason) if reason.contains("non-text")));

        let tool = classify_update(&json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Edit src/main.rs",
            "kind": "edit",
            "status": "in_progress"
        }));
        match tool {
            SessionUpdate::ToolCall {
                tool_call_id,
                title,
                kind,
                status,
            } => {
                assert_eq!(tool_call_id, "t1");
                assert_eq!(title, "Edit src/main.rs");
                assert_eq!(kind.as_deref(), Some("edit"));
                assert_eq!(status.as_deref(), Some("in_progress"));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }

        // Tolerant parsing: fields nobody filled in become empty, not errors.
        let sparse = classify_update(&json!({"sessionUpdate": "tool_call_update"}));
        assert!(
            matches!(sparse, SessionUpdate::ToolCall { tool_call_id, title, .. }
            if tool_call_id.is_empty() && title.is_empty())
        );

        // Future protocol additions land here instead of breaking the turn.
        let future = classify_update(&json!({"sessionUpdate": "brand_new_kind"}));
        assert_eq!(future, SessionUpdate::Ignored("brand_new_kind".into()));
        let missing = classify_update(&json!({}));
        assert!(matches!(missing, SessionUpdate::Ignored(tag) if tag.contains("missing")));
    }

    #[test]
    fn permission_requests_parse_their_options() {
        let parsed: PermissionRequest = serde_json::from_value(json!({
            "sessionId": "s1",
            "toolCall": {"toolCallId": "t1", "title": "Run `rm x`"},
            "options": [
                {"optionId": "allow", "name": "Yes, allow once", "kind": "allow_once"},
                {"optionId": "always", "name": "Always allow", "kind": "allow_always"},
                {"optionId": "reject", "name": "No", "kind": "reject_once"},
                {"optionId": "weird", "name": "?", "kind": "something_new"},
            ]
        }))
        .expect("parses");
        assert_eq!(parsed.session_id, "s1");
        assert_eq!(parsed.options.len(), 4);
        assert_eq!(parsed.options[0].kind, PermissionOptionKind::AllowOnce);
        assert_eq!(parsed.options[3].kind, PermissionOptionKind::Unknown);
    }

    #[test]
    fn stop_reasons_decode_and_unknown_stays_unknown() {
        let parsed: StopReason = serde_json::from_value(json!("end_turn")).expect("decodes");
        assert_eq!(parsed, StopReason::EndTurn);
        let parsed: StopReason =
            serde_json::from_value(json!("max_turn_requests")).expect("decodes");
        assert_eq!(parsed, StopReason::MaxTurnRequests);
        let parsed: StopReason =
            serde_json::from_value(json!("some_future_reason")).expect("decodes");
        assert_eq!(parsed, StopReason::Unknown);
    }

    #[test]
    fn new_session_result_parses_config_options_and_models() {
        let parsed: NewSessionResult = serde_json::from_value(json!({
            "sessionId": "s-123",
            "configOptions": [
                {
                    "id": "model",
                    "name": "Model",
                    "type": "select",
                    "currentValue": "m1",
                    "options": [
                        {"value": "m1", "name": "Model 1"},
                        {"value": "m2", "name": "Model 2"}
                    ]
                }
            ]
        }))
        .expect("parses");
        assert_eq!(parsed.session_id, "s-123");
        assert!(parsed.models.is_none());
        assert_eq!(parsed.config_options.len(), 1);
        let opt = &parsed.config_options[0];
        assert_eq!(opt.id, "model");
        assert_eq!(opt.current_value_str(), Some("m1"));
        assert_eq!(opt.options.len(), 2);
        assert_eq!(opt.options[1].value, "m2");
    }
}
