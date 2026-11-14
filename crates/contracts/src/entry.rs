//! Transcript entries: the whole-entity upserts a conversation is made of.
//!
//! Six kinds (message, reasoning, tool-call, card, event, notice) plus an
//! `unknown` placeholder, so a client built against an older schema still
//! renders something instead of failing.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Seq;
use crate::id::{BotId, ConversationId, EntryId, NodeId, RequestId, TaskId, ToolCallId, TurnId};
use crate::request::{RequestKind, RequestStatus};

/// Who wrote an entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Author {
    /// The single owner of this brain.
    Owner,
    /// A bot, in its own words.
    Bot { id: BotId },
    /// The system: notices, recovery lines, routine starts.
    System,
}

impl Author {
    pub fn bot(id: BotId) -> Self {
        Self::Bot { id }
    }

    pub fn bot_id(&self) -> Option<&BotId> {
        match self {
            Self::Bot { id } => Some(id),
            _ => None,
        }
    }

    pub fn is_owner(&self) -> bool {
        matches!(self, Self::Owner)
    }
}

/// Whether a message reached its conversation immediately or waited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostState {
    #[default]
    Posted,
    /// The bot was busy in another conversation: this one waits its turn.
    Queued,
    /// Pushed into the bot's running turn of the same conversation.
    Steered,
}

/// A file a bot sent or received.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
    /// Where the bytes live on the brain (a bot's `inbox/`).
    pub path: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    #[default]
    Pending,
    Running,
    Completed,
    Failed,
    /// Refused by policy, a rule or the owner.
    Denied,
    /// The turn was stopped while the call ran.
    Cancelled,
}

impl ToolCallStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Denied | Self::Cancelled
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageEntry {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    pub reply_to: Option<EntryId>,
    #[serde(default)]
    pub post: PostState,
    /// In a DM, a reply to another bot's message is labelled "to @sender".
    pub addressed_to: Option<BotId>,
    /// The turn that produced it, when a bot wrote it.
    pub turn_id: Option<TurnId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReasoningEntry {
    pub text: String,
    pub turn_id: Option<TurnId>,
}

/// **Who ran this tool call.** The distinction is not cosmetic: SwarmBot's
/// policy engine, approval cards, audit log and node dispatch only ever see the
/// `Swarm` kind.
///
/// An ACP backend is a full agent with its own sandbox and its own tool loop
/// (`BackendKind::owns_tool_loop()`), so the calls it makes reach this
/// transcript through a different door entirely: nothing in `tools` decided
/// them, no `PolicyEngine` saw them, no `AuditLog` line was written, and
/// `node_id` is `None` because no SwarmBot node was involved. Recording that as
/// a plain `Swarm` call would be the small version of the lie this repository's
/// honesty rules are about — a reader (and a future audit query) would credit
/// our executor for work it never gated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallOrigin {
    /// Ran on a SwarmBot node under our policy: the calls `run_tool_call`
    /// makes, and the ones the MCP gateway writes for harness-run tools.
    #[default]
    Swarm,
    /// Announced by an ACP agent's own `session/update`, in the agent's own
    /// process and sandbox. Informational: we render it, we do not control it.
    Agent,
}

/// One tool call, from the model's request to its result. Upserted in place:
/// the same `id` first appears `running`, then `completed` with the result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallEntry {
    pub call_id: ToolCallId,
    pub tool: String,
    pub args: Value,
    pub status: ToolCallStatus,
    /// Truncated tool output (`ERROR: …` when it failed).
    pub result_text: Option<String>,
    /// Where it ran: the local node or a remote one.
    pub node_id: Option<NodeId>,
    pub task_id: Option<TaskId>,
    pub turn_id: Option<TurnId>,
    /// Set when output was spilled to a file.
    pub spill_path: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<u64>,
    /// Defaults to [`ToolCallOrigin::Swarm`], which is what every row written
    /// before this field existed actually was (D9.31); absent means Swarm, so
    /// an old transcript still loads unchanged.
    #[serde(default)]
    pub origin: ToolCallOrigin,
}

/// An interactive card bound to a runtime request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CardEntry {
    pub request_id: RequestId,
    pub card: RequestKind,
    pub status: RequestStatus,
    pub title: String,
    pub body: String,
    /// Request-specific payload (question fields, the command to approve, …).
    #[serde(default)]
    pub payload: Value,
    pub expires_at: Option<DateTime<Utc>>,
}

/// A non-message line in the transcript: "Messaged @Scout", "Routine started".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventEntry {
    /// Stable machine key (`bot-messaged`, `handoff`, `hop-limit`, …).
    pub event: String,
    /// The human line the UI shows.
    pub text: String,
    #[serde(default)]
    pub data: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoticeEntry {
    pub text: String,
    pub level: NoticeLevel,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryKind {
    Message(Box<MessageEntry>),
    Reasoning(Box<ReasoningEntry>),
    ToolCall(Box<ToolCallEntry>),
    Card(Box<CardEntry>),
    Event(Box<EventEntry>),
    Notice(Box<NoticeEntry>),
    /// A kind this build does not know; rendered as a placeholder.
    #[serde(other)]
    Unknown,
}

impl EntryKind {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Message(_) => "message",
            Self::Reasoning(_) => "reasoning",
            Self::ToolCall(_) => "tool_call",
            Self::Card(_) => "card",
            Self::Event(_) => "event",
            Self::Notice(_) => "notice",
            Self::Unknown => "unknown",
        }
    }

    /// The text a search or a summary sees, if this kind has any.
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Message(m) => Some(&m.text),
            Self::Reasoning(r) => Some(&r.text),
            Self::ToolCall(t) => t.result_text.as_deref(),
            Self::Card(c) => Some(&c.body),
            Self::Event(e) => Some(&e.text),
            Self::Notice(n) => Some(&n.text),
            Self::Unknown => None,
        }
    }
}

/// One transcript entry. Whole-entity upserts: a writer bumps `revision` and
/// replaces the row; a stale write is a [`crate::error::DomainError::Conflict`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: EntryId,
    pub conversation_id: ConversationId,
    /// Position in the conversation, from 1.
    pub seq: Seq,
    pub author: Author,
    pub kind: EntryKind,
    pub revision: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Entry {
    pub fn new(conversation_id: ConversationId, seq: Seq, author: Author, kind: EntryKind) -> Self {
        let now = Utc::now();
        Self {
            id: EntryId::new(),
            conversation_id,
            seq,
            author,
            kind,
            revision: 1,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn owner_message(
        conversation_id: ConversationId,
        seq: Seq,
        text: impl Into<String>,
    ) -> Self {
        Self::new(
            conversation_id,
            seq,
            Author::Owner,
            EntryKind::Message(Box::new(MessageEntry {
                text: text.into(),
                attachments: Vec::new(),
                reply_to: None,
                post: PostState::Posted,
                addressed_to: None,
                turn_id: None,
            })),
        )
    }

    pub fn bot_message(
        conversation_id: ConversationId,
        seq: Seq,
        bot: BotId,
        text: impl Into<String>,
        turn_id: Option<TurnId>,
    ) -> Self {
        Self::new(
            conversation_id,
            seq,
            Author::bot(bot),
            EntryKind::Message(Box::new(MessageEntry {
                text: text.into(),
                attachments: Vec::new(),
                reply_to: None,
                post: PostState::Posted,
                addressed_to: None,
                turn_id,
            })),
        )
    }

    pub fn event(
        conversation_id: ConversationId,
        seq: Seq,
        event: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self::new(
            conversation_id,
            seq,
            Author::System,
            EntryKind::Event(Box::new(EventEntry {
                event: event.into(),
                text: text.into(),
                data: Value::Null,
            })),
        )
    }

    pub fn notice(
        conversation_id: ConversationId,
        seq: Seq,
        text: impl Into<String>,
        level: NoticeLevel,
    ) -> Self {
        Self::new(
            conversation_id,
            seq,
            Author::System,
            EntryKind::Notice(Box::new(NoticeEntry {
                text: text.into(),
                level,
            })),
        )
    }

    /// A tool-call entry. Eight arguments on purpose: they are the wire shape
    /// of a call, and bundling them into a struct would only move the noise.
    #[allow(clippy::too_many_arguments)]
    pub fn tool_call(
        conversation_id: ConversationId,
        seq: Seq,
        turn_id: Option<TurnId>,
        call_id: ToolCallId,
        tool: impl Into<String>,
        args: Value,
        node_id: Option<NodeId>,
        task_id: Option<TaskId>,
    ) -> Self {
        Self::new(
            conversation_id,
            seq,
            Author::System,
            EntryKind::ToolCall(Box::new(ToolCallEntry {
                call_id,
                tool: tool.into(),
                args,
                status: ToolCallStatus::Running,
                result_text: None,
                node_id,
                task_id,
                turn_id,
                spill_path: None,
                started_at: Utc::now(),
                ended_at: None,
                duration_ms: None,
                origin: ToolCallOrigin::Swarm,
            })),
        )
    }

    /// A tool call an **agent ran itself** (ACP `session/update`), which never
    /// passed through our policy engine, audit log or node dispatch. Same shape
    /// as [`Entry::tool_call`] with `origin: Agent` and no node, because there
    /// is no node: the agent's own sandbox ran it. D9.31.
    pub fn agent_tool_call(
        conversation_id: ConversationId,
        seq: Seq,
        turn_id: Option<TurnId>,
        call_id: ToolCallId,
        tool: impl Into<String>,
        args: Value,
    ) -> Self {
        let mut entry = Self::tool_call(
            conversation_id,
            seq,
            turn_id,
            call_id,
            tool,
            args,
            None,
            None,
        );
        if let Some(call) = entry.as_tool_call_mut() {
            call.origin = ToolCallOrigin::Agent;
        }
        entry
    }

    /// The tool-call payload, when this entry is one.
    pub fn as_tool_call(&self) -> Option<&ToolCallEntry> {
        match &self.kind {
            EntryKind::ToolCall(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_tool_call_mut(&mut self) -> Option<&mut ToolCallEntry> {
        match &mut self.kind {
            EntryKind::ToolCall(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_message(&self) -> Option<&MessageEntry> {
        match &self.kind {
            EntryKind::Message(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_message_mut(&mut self) -> Option<&mut MessageEntry> {
        match &mut self.kind {
            EntryKind::Message(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_card(&self) -> Option<&CardEntry> {
        match &self.kind {
            EntryKind::Card(c) => Some(c),
            _ => None,
        }
    }

    pub fn as_card_mut(&mut self) -> Option<&mut CardEntry> {
        match &mut self.kind {
            EntryKind::Card(c) => Some(c),
            _ => None,
        }
    }

    pub fn is_message(&self) -> bool {
        matches!(self.kind, EntryKind::Message(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv() -> ConversationId {
        ConversationId::new()
    }

    /// A tool-call row written **before** `origin` existed must still load, as
    /// `Swarm` — which is what every such row actually was: the field was added
    /// when an ACP agent's own calls started being transcribed (D9.31), and
    /// until then every row in every transcript came from `run_tool_call` or
    /// the MCP gateway.
    #[test]
    fn a_tool_call_row_without_an_origin_loads_as_ours() {
        let old = serde_json::json!({
            "kind": "tool_call",
            "call_id": "tcl_0123456789abcdef0123456789abcdef",
            "tool": "read_file",
            "args": {"path": "README.md"},
            "status": "completed",
            "result_text": "hello",
            "node_id": "nod_local",
            "task_id": null,
            "turn_id": null,
            "spill_path": null,
            "started_at": "2026-11-10T00:00:00Z",
            "ended_at": "2026-11-10T00:00:01Z",
            "duration_ms": 1000
        });
        let parsed: EntryKind = serde_json::from_value(old).expect("an old row still parses");
        let EntryKind::ToolCall(call) = parsed else {
            panic!("expected a tool call");
        };
        assert_eq!(
            call.origin,
            ToolCallOrigin::Swarm,
            "absent origin means Swarm: those rows ran through our policy"
        );
    }

    /// The two constructors must not drift: the only difference between them is
    /// the origin (and therefore the node, which an agent's call cannot have).
    #[test]
    fn agent_tool_calls_are_ours_in_every_field_but_the_origin() {
        let ours = Entry::tool_call(
            conv(),
            1,
            None,
            ToolCallId::new(),
            "read",
            serde_json::json!({}),
            None,
            None,
        );
        let theirs = Entry::agent_tool_call(
            conv(),
            2,
            None,
            ToolCallId::new(),
            "read",
            serde_json::json!({}),
        );
        let (Some(ours), Some(theirs)) = (ours.as_tool_call(), theirs.as_tool_call()) else {
            panic!("both are tool calls");
        };
        assert_eq!(ours.origin, ToolCallOrigin::Swarm);
        assert_eq!(theirs.origin, ToolCallOrigin::Agent);
        assert_eq!(theirs.node_id, None, "no node ran an agent's own call");
        assert_eq!(theirs.task_id, None);
        assert_eq!(theirs.status, ToolCallStatus::Running);
        assert_eq!(theirs.tool, ours.tool);
    }

    #[test]
    fn entries_start_at_revision_one() {
        let e = Entry::owner_message(conv(), 1, "hi");
        assert_eq!(e.revision, 1);
        assert_eq!(e.seq, 1);
        assert!(e.is_message());
        assert_eq!(e.kind.text(), Some("hi"));
        assert_eq!(e.kind.kind_str(), "message");
    }

    #[test]
    fn authors_carry_the_bot_id() {
        let bot = BotId::new();
        let e = Entry::bot_message(conv(), 2, bot.clone(), "on it", None);
        assert_eq!(e.author.bot_id(), Some(&bot));
        assert!(!e.author.is_owner());
        assert!(Entry::owner_message(conv(), 1, "x").author.is_owner());
    }

    #[test]
    fn tool_call_entries_upsert_in_place() {
        let mut e = Entry::tool_call(
            conv(),
            3,
            Some(TurnId::new()),
            ToolCallId::new(),
            "shell",
            serde_json::json!({"command": "ls"}),
            Some(NodeId::local()),
            None,
        );
        let call = e.as_tool_call().expect("tool call");
        assert_eq!(call.status, ToolCallStatus::Running);
        assert!(!call.status.is_terminal());

        let call = e.as_tool_call_mut().unwrap();
        call.status = ToolCallStatus::Completed;
        call.result_text = Some("Cargo.toml".into());
        call.ended_at = Some(Utc::now());
        e.revision += 1;
        assert_eq!(e.revision, 2);
        assert!(e.as_tool_call().unwrap().status.is_terminal());
        assert_eq!(e.kind.text(), Some("Cargo.toml"));
    }

    #[test]
    fn kinds_serialize_with_a_tag() {
        let e = Entry::event(conv(), 4, "bot-messaged", "Messaged @scout");
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["kind"]["kind"], "event");
        assert_eq!(json["author"]["kind"], "system");
        let back: Entry = serde_json::from_value(json).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn unknown_kinds_become_a_placeholder() {
        let json = serde_json::json!({
            "id": EntryId::new().to_string(),
            "conversation_id": conv().to_string(),
            "seq": 5,
            "author": {"kind": "system"},
            "kind": {"kind": "screencast", "frames": 3},
            "revision": 1,
            "created_at": Utc::now(),
            "updated_at": Utc::now(),
        });
        let entry: Entry = serde_json::from_value(json).expect("tolerates a newer kind");
        assert_eq!(entry.kind, EntryKind::Unknown);
        assert_eq!(entry.kind.kind_str(), "unknown");
        assert_eq!(entry.kind.text(), None);
    }
}
