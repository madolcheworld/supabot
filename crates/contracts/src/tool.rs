//! Tools: what a bot can do, and the shape of every result.
//!
//! Rules (identical for local handlers, remote node handlers and the MCP
//! gateway, so a model sees one behaviour everywhere):
//!
//! * results are `content` (text plus optional images), never a free-form
//!   `structuredContent` blob;
//! * an error's text starts with `ERROR: ` and reads as an error on its own;
//! * results stay under [`TOOL_RESULT_LIMIT_BYTES`]; larger output is spilled
//!   to a file and the tool returns the path plus paging hints.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::id::{BotId, ConversationId, InvocationId, NodeId, TaskId, ToolCallId, TurnId};

/// Results are capped so one `cat` of a huge file cannot blow up a context.
pub const TOOL_RESULT_LIMIT_BYTES: usize = 10 * 1024;

/// Shell output is capped before spilling, per invocation.
pub const SHELL_OUTPUT_LIMIT_BYTES: usize = 1024 * 1024;

/// Which set a tool belongs to. Gating decides what a bot actually sees:
/// a bot's placement, its node's capabilities and whether it runs a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Toolset {
    /// Bot-to-bot and owner-facing tools (`message_bot`, `ask_user`, …).
    Bot,
    /// Tools that act on a machine (`shell`, `read_file`, …).
    Node,
    /// Tools only the owner may trigger.
    Owner,
    /// Task-management tools, hidden from a task's own session.
    Task,
}

/// A tool definition as the model sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema (draft 2020-12) for the arguments. No tuples, no
    /// `prefixItems`: some harnesses reject them.
    pub parameters: Value,
    pub toolset: Toolset,
    /// True when the tool only reads, so it may run without approval and in
    /// parallel with other read-only tools.
    pub read_only: bool,
    /// True when the tool takes an optional `node` parameter and therefore
    /// needs a resolvable host node.
    pub needs_node: bool,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        toolset: Toolset,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            toolset,
            read_only: false,
            needs_node: false,
        }
    }

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    pub fn needs_node(mut self) -> Self {
        self.needs_node = true;
        self
    }
}

/// A tool call as the model produced it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: ToolCallId,
    pub name: String,
    pub args: Value,
}

/// One piece of tool output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text { text: String },
    Image { mime: String, data_base64: String },
}

/// The result of a tool call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: Vec<Content>,
    pub is_error: bool,
    /// Free-form facts the UI shows but the model does not need (node id,
    /// spill path, duration). Kept out of `content`.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub meta: Map<String, Value>,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text { text: text.into() }],
            is_error: false,
            meta: Map::new(),
        }
    }

    /// An error result. The text always starts with `ERROR: `.
    pub fn error(message: impl AsRef<str>) -> Self {
        let message = message.as_ref();
        let text = if message.starts_with("ERROR:") {
            message.to_string()
        } else {
            format!("ERROR: {message}")
        };
        Self {
            content: vec![Content::Text { text }],
            is_error: true,
            meta: Map::new(),
        }
    }

    pub fn with_meta(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.meta.insert(key.into(), value.into());
        self
    }

    /// The whole result as one string, for transcripts and for models that
    /// take plain text tool results.
    pub fn text_only(&self) -> String {
        self.content
            .iter()
            .map(|c| match c {
                Content::Text { text } => text.as_str(),
                Content::Image { .. } => "[image]",
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Cap the text at [`TOOL_RESULT_LIMIT_BYTES`], reporting what was cut.
    /// Returns `true` when something was truncated.
    pub fn truncate_to_limit(&mut self) -> bool {
        let mut truncated = false;
        for content in &mut self.content {
            if let Content::Text { text } = content
                && text.len() > TOOL_RESULT_LIMIT_BYTES
            {
                let cut = floor_char_boundary(text, TOOL_RESULT_LIMIT_BYTES);
                let hidden = text.len() - cut;
                text.truncate(cut);
                text.push_str(&format!(
                    "\n… [output truncated: {hidden} more bytes; re-run with a narrower filter]"
                ));
                truncated = true;
            }
        }
        truncated
    }
}

/// Shrink `s` to at most `limit` bytes without splitting a UTF-8 sequence.
fn floor_char_boundary(s: &str, limit: usize) -> usize {
    if s.len() <= limit {
        return s.len();
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Everything a tool handler needs to know about who is calling and from
/// where. Also the identity carried into node RPCs for policy and audit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invocation {
    pub id: InvocationId,
    pub bot_id: BotId,
    pub conversation_id: ConversationId,
    pub turn_id: Option<TurnId>,
    pub task_id: Option<TaskId>,
    pub tool_call_id: Option<ToolCallId>,
    /// The node the call is aimed at; `None` means the bot's host node.
    pub node_id: Option<NodeId>,
}

impl Invocation {
    pub fn new(bot_id: BotId, conversation_id: ConversationId) -> Self {
        Self {
            id: InvocationId::new(),
            bot_id,
            conversation_id,
            turn_id: None,
            task_id: None,
            tool_call_id: None,
            node_id: None,
        }
    }
}

/// A tool failure, before it becomes an `ERROR: …` result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolError {
    #[error("invalid arguments: {message}")]
    InvalidArgs { message: String },

    #[error("unknown tool: {tool}")]
    UnknownTool { tool: String },

    #[error("path denied: {path} ({reason})")]
    PathDenied { path: String, reason: String },

    #[error("refused by policy: {reason}")]
    PolicyRefused {
        reason: String,
        /// False tells the model not to try again.
        retry: bool,
    },

    #[error("denied: {reason}")]
    Denied { reason: String },

    #[error("cancelled")]
    Cancelled,

    #[error("{message}")]
    Failed { message: String },
}

impl ToolError {
    /// Render as the tool result a model sees.
    pub fn to_result(&self) -> ToolResult {
        ToolResult::error(self.to_string())
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidArgs {
            message: message.into(),
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_results_are_prefixed_once() {
        assert_eq!(ToolResult::error("boom").text_only(), "ERROR: boom");
        assert_eq!(
            ToolResult::error("ERROR: already").text_only(),
            "ERROR: already"
        );
        assert!(ToolResult::error("boom").is_error);
        assert!(!ToolResult::text("ok").is_error);
    }

    #[test]
    fn truncation_keeps_utf8_intact() {
        // 2-byte characters must not be split by a byte-wise cut.
        let text = "é".repeat(TOOL_RESULT_LIMIT_BYTES);
        let original_len = text.len();
        let mut result = ToolResult::text(text);
        assert!(result.truncate_to_limit());
        let out = result.text_only();
        assert!(out.contains("output truncated"));
        // The kept part is still valid UTF-8 and starts with the original run.
        assert!(out.starts_with("é"));
        assert!(out.len() < original_len * 2 + 128);
    }

    #[test]
    fn short_results_are_left_alone() {
        let mut result = ToolResult::text("short");
        assert!(!result.truncate_to_limit());
        assert_eq!(result.text_only(), "short");
    }

    #[test]
    fn tool_errors_render_as_error_text() {
        let err = ToolError::PathDenied {
            path: "/etc/shadow".into(),
            reason: "deny-list".into(),
        };
        assert_eq!(
            err.to_result().text_only(),
            "ERROR: path denied: /etc/shadow (deny-list)"
        );
        assert!(err.to_result().is_error);
    }

    #[test]
    fn specs_carry_gating_facts() {
        let spec = ToolSpec::new(
            "shell",
            "run a command",
            serde_json::json!({}),
            Toolset::Node,
        )
        .needs_node();
        assert!(spec.needs_node);
        assert!(!spec.read_only);
        assert_eq!(spec.toolset, Toolset::Node);
        assert_eq!(
            serde_json::to_value(&spec).unwrap()["toolset"],
            serde_json::json!("node")
        );
    }
}
