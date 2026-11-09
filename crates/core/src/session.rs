//! One mind per bot.
//!
//! A bot has a single LLM session shared by every conversation it takes part
//! in: its DM with the owner, each group, bot DMs, routine runs and tasks. The
//! price of one mind is that a turn has to say where its input came from, so
//! [`Session::origin_tag`] and the instruction renderer exist. The benefit is
//! that what a bot did in a group is remembered in its DM.
//!
//! What is stored:
//!
//! * `history` — the model's own message list, opaque to the store;
//! * `seen` — per-conversation read cursors, so a group catch-up shows each
//!   message to each bot exactly once;
//! * `native_session_id` — a harness session to resume, when the backend has one;
//! * `token_estimate` — the number that decides when to compact.

use chrono::Utc;
use serde_json::Value;
use tracing::debug;

use swarmbot_contracts::Seq;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{BotId, ConversationId};
use swarmbot_contracts::repo::{BotSessionRecord, Persistence};
use swarmbot_llm::{ChatMessage, Role};

/// How many of the newest messages a compaction keeps verbatim.
pub const COMPACT_KEEP_LAST: usize = 8;

/// The cap on the memory index injected into instructions.
pub const MEMORY_INDEX_LIMIT_BYTES: usize = 4 * 1024;

pub struct Session {
    record: BotSessionRecord,
    messages: Vec<ChatMessage>,
    /// Where the bot's last turn ran, so a DM turn after group work can be
    /// tagged. Not persisted as its own column: it falls out of the history.
    last_conversation: Option<ConversationId>,
    dirty: bool,
}

impl Session {
    /// Load a bot's session, or start one.
    pub fn load(store: &dyn Persistence, bot_id: &BotId) -> Result<Self, DomainError> {
        let record = store
            .get_session(bot_id)?
            .unwrap_or_else(|| BotSessionRecord::new(bot_id.clone()));
        let messages = decode_history(&record.history);
        Ok(Self {
            record,
            messages,
            last_conversation: None,
            dirty: false,
        })
    }

    pub fn bot_id(&self) -> &BotId {
        &self.record.bot_id
    }

    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    pub fn push(&mut self, message: ChatMessage) {
        self.dirty = true;
        self.messages.push(message);
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.push(ChatMessage {
            role: Role::User,
            content: text.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        });
    }

    pub fn native_session_id(&self) -> Option<&str> {
        self.record.native_session_id.as_deref()
    }

    pub fn set_native_session_id(&mut self, id: impl Into<String>) {
        self.dirty = true;
        self.record.native_session_id = Some(id.into());
    }

    /// Drop the native session ("New session") but keep the read cursors: what
    /// the bot has already seen in each group is a fact about the transcript,
    /// not about the model's memory.
    pub fn reset_native(&mut self) {
        self.dirty = true;
        self.record.native_session_id = None;
        self.messages.clear();
    }

    pub fn seen(&self, conversation: &ConversationId) -> Seq {
        self.record.seen.get(conversation).copied().unwrap_or(0)
    }

    pub fn mark_seen(&mut self, conversation: &ConversationId, seq: Seq) {
        let entry = self.record.seen.entry(conversation.clone()).or_insert(0);
        if seq > *entry {
            *entry = seq;
            self.dirty = true;
        }
    }

    pub fn last_conversation(&self) -> Option<&ConversationId> {
        self.last_conversation.as_ref()
    }

    pub fn note_conversation(&mut self, conversation: &ConversationId) {
        if self.last_conversation.as_ref() != Some(conversation) {
            self.dirty = true;
        }
        self.last_conversation = Some(conversation.clone());
    }

    /// A rough token count: the model's own tokenizer is not available here,
    /// and the decision it feeds ("compact or not") only needs to be honest to
    /// within a few percent.
    pub fn token_estimate(&self) -> u64 {
        estimate_messages(&self.messages)
    }

    /// True once the history passes `fraction` of `budget`.
    pub fn needs_compaction(&self, budget: u64, fraction: f32) -> bool {
        budget > 0 && self.token_estimate() as f32 >= budget as f32 * fraction
    }

    /// Replace everything but the newest [`COMPACT_KEEP_LAST`] messages with
    /// one summary, so a long-lived bot keeps working instead of overflowing.
    /// Returns how many messages were folded away.
    pub fn compact(&mut self, summary: impl Into<String>) -> usize {
        if self.messages.len() <= COMPACT_KEEP_LAST {
            return 0;
        }
        let split = self.messages.len() - COMPACT_KEEP_LAST;
        let folded: Vec<ChatMessage> = self.messages.drain(0..split).collect();
        let summary = summary.into();
        self.messages.insert(
            0,
            ChatMessage {
                role: Role::User,
                content: format!("[Earlier in this session, summarized]\n{summary}"),
                tool_calls: Vec::new(),
                tool_call_id: None,
                name: None,
            },
        );
        self.dirty = true;
        debug!(
            bot = %self.record.bot_id,
            folded = folded.len(),
            tokens = self.token_estimate(),
            "compacted a bot session"
        );
        folded.len()
    }

    /// The text a compaction summary is made from, when one is needed.
    pub fn compaction_source(&self) -> String {
        let limit = self.messages.len().saturating_sub(COMPACT_KEEP_LAST);
        self.messages
            .iter()
            .take(limit)
            .map(|m| match m.role {
                Role::Assistant => format!("you: {}", truncate(&m.content, 400)),
                Role::User => format!("they: {}", truncate(&m.content, 400)),
                Role::Tool => format!("tool: {}", truncate(&m.content, 200)),
                Role::System => String::new(),
            })
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Persist, if anything changed.
    pub fn save(&mut self, store: &dyn Persistence) -> Result<(), DomainError> {
        if !self.dirty {
            return Ok(());
        }
        self.record.history = encode_history(&self.messages);
        self.record.token_estimate = self.token_estimate();
        self.record.updated_at = Utc::now();
        store.put_session(&self.record)?;
        self.dirty = false;
        Ok(())
    }

    pub fn into_record(mut self) -> BotSessionRecord {
        self.record.history = encode_history(&self.messages);
        self.record.token_estimate = self.token_estimate();
        self.record
    }
}

/// The line that tells one shared mind where this input came from.
pub fn origin_tag(
    conversation_title: Option<&str>,
    members: &[String],
    lead: Option<&str>,
    from_bot: Option<&str>,
    is_owner_dm: bool,
    previous_was_elsewhere: bool,
) -> String {
    if let Some(title) = conversation_title {
        let mut tag = format!("[Group chat \"{title}\"");
        if !members.is_empty() {
            tag.push_str(&format!(" · members: {}", members.join(", ")));
        }
        if let Some(lead) = lead {
            tag.push_str(&format!(" · lead: {lead}"));
        }
        tag.push_str(" — your reply is posted here]");
        return tag;
    }
    if let Some(from) = from_bot {
        return format!("[Direct message from @{from}]");
    }
    if is_owner_dm && previous_was_elsewhere {
        return "[DM with the owner]".to_string();
    }
    String::new()
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Rough tokens: 4 characters each, plus 4 per message of framing, which is
/// what the OpenAI tokenizers land on for English prose.
pub fn estimate_messages(messages: &[ChatMessage]) -> u64 {
    messages
        .iter()
        .map(|m| {
            let calls: usize = m
                .tool_calls
                .iter()
                .map(|c| (c.name.len() + c.arguments.to_string().len()) / 4 + 8)
                .sum();
            (m.content.len() / 4 + 4 + calls) as u64
        })
        .sum()
}

fn decode_history(history: &Value) -> Vec<ChatMessage> {
    match history {
        Value::Array(items) => items
            .iter()
            .filter_map(
                |item| match serde_json::from_value::<ChatMessage>(item.clone()) {
                    Ok(message) => Some(message),
                    Err(err) => {
                        // A history this build cannot read is dropped rather than
                        // sent to a model as garbage; the transcript still has it.
                        debug!(error = %err, "skipping an unreadable history message");
                        None
                    }
                },
            )
            .collect(),
        Value::Null => Vec::new(),
        other => {
            debug!(?other, "session history is not an array; starting fresh");
            Vec::new()
        }
    }
}

fn encode_history(messages: &[ChatMessage]) -> Value {
    Value::Array(
        messages
            .iter()
            .filter_map(|m| serde_json::to_value(m).ok())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    #[test]
    fn a_new_session_starts_empty() {
        let store = testing::memory_store();
        let bot = testing::seed_bot(&store, "lead");
        let session = Session::load(store.as_ref(), &bot.id).unwrap();
        assert!(session.messages().is_empty());
        assert_eq!(session.token_estimate(), 0);
        assert_eq!(session.seen(&bot.direct_conversation_id), 0);
    }

    #[test]
    fn history_round_trips_through_the_store() {
        let store = testing::memory_store();
        let bot = testing::seed_bot(&store, "lead");
        {
            let mut session = Session::load(store.as_ref(), &bot.id).unwrap();
            session.push_user("hello");
            session.push(ChatMessage {
                role: Role::Assistant,
                content: "hi".into(),
                tool_calls: vec![],
                tool_call_id: None,
                name: None,
            });
            session.set_native_session_id("native-1");
            session.mark_seen(&bot.direct_conversation_id, 7);
            session.save(store.as_ref()).unwrap();
        }
        let reopened = Session::load(store.as_ref(), &bot.id).unwrap();
        assert_eq!(reopened.messages().len(), 2);
        assert_eq!(reopened.messages()[0].role, Role::User);
        assert_eq!(reopened.native_session_id(), Some("native-1"));
        assert_eq!(reopened.seen(&bot.direct_conversation_id), 7);
        assert!(reopened.token_estimate() > 0);
    }

    #[test]
    fn saving_an_unchanged_session_writes_nothing() {
        let store = testing::memory_store();
        let bot = testing::seed_bot(&store, "lead");
        let mut session = Session::load(store.as_ref(), &bot.id).unwrap();
        session.save(store.as_ref()).unwrap();
        assert!(store.get_session(&bot.id).unwrap().is_none());
        session.push_user("now it changes");
        session.save(store.as_ref()).unwrap();
        assert!(store.get_session(&bot.id).unwrap().is_some());
    }

    #[test]
    fn seen_cursors_never_go_backwards() {
        let store = testing::memory_store();
        let bot = testing::seed_bot(&store, "lead");
        let conversation = bot.direct_conversation_id.clone();
        let mut session = Session::load(store.as_ref(), &bot.id).unwrap();
        session.mark_seen(&conversation, 5);
        session.mark_seen(&conversation, 3);
        assert_eq!(session.seen(&conversation), 5);
        session.mark_seen(&conversation, 9);
        assert_eq!(session.seen(&conversation), 9);
    }

    #[test]
    fn compaction_folds_the_old_and_keeps_the_recent() {
        let store = testing::memory_store();
        let bot = testing::seed_bot(&store, "lead");
        let mut session = Session::load(store.as_ref(), &bot.id).unwrap();
        for i in 0..20 {
            session.push_user(format!("message {i}"));
        }
        let before = session.token_estimate();
        let folded = session.compact("we discussed 12 messages");
        assert_eq!(folded, 20 - COMPACT_KEEP_LAST);
        assert_eq!(session.messages().len(), COMPACT_KEEP_LAST + 1);
        assert!(session.messages()[0].content.contains("summarized"));
        assert!(
            session.messages()[0]
                .content
                .contains("we discussed 12 messages")
        );
        // The newest messages are still verbatim.
        assert!(
            session
                .messages()
                .last()
                .unwrap()
                .content
                .contains("message 19")
        );
        assert!(session.token_estimate() < before);
    }

    #[test]
    fn compaction_is_only_offered_when_it_is_needed() {
        let store = testing::memory_store();
        let bot = testing::seed_bot(&store, "lead");
        let mut session = Session::load(store.as_ref(), &bot.id).unwrap();
        assert!(!session.needs_compaction(10_000, 0.6));
        for i in 0..200 {
            session.push_user(format!(
                "a fairly long message number {i} with some words in it"
            ));
        }
        assert!(
            session.needs_compaction(4_000, 0.6),
            "{} tokens",
            session.token_estimate()
        );
        // A summary of the folded part exists before compacting, and the
        // production summarizer trims it (the raw source would be as big as
        // what it replaces, which buys no room back).
        let source = session.compaction_source();
        assert!(source.contains("message number 0"));
        assert!(
            !source.contains("message number 199"),
            "the kept tail is not summarized"
        );
        let summary = crate::runners::compact_summary(&source);
        assert!(summary.len() < source.len(), "the summary is smaller");
        assert!(session.compact(summary) > 0);
        assert!(
            !session.needs_compaction(4_000, 0.6),
            "compaction bought room back"
        );
    }

    #[test]
    fn resetting_a_session_keeps_the_read_cursors() {
        let store = testing::memory_store();
        let bot = testing::seed_bot(&store, "lead");
        let conversation = bot.direct_conversation_id.clone();
        let mut session = Session::load(store.as_ref(), &bot.id).unwrap();
        session.push_user("old work");
        session.set_native_session_id("native-1");
        session.mark_seen(&conversation, 4);
        session.save(store.as_ref()).unwrap();

        let mut reopened = Session::load(store.as_ref(), &bot.id).unwrap();
        reopened.reset_native();
        reopened.save(store.as_ref()).unwrap();

        let after = Session::load(store.as_ref(), &bot.id).unwrap();
        assert!(after.messages().is_empty());
        assert_eq!(after.native_session_id(), None);
        assert_eq!(after.seen(&conversation), 4, "what it saw is not forgotten");
    }

    #[test]
    fn origin_tags_say_where_the_input_came_from() {
        let group = origin_tag(
            Some("Team"),
            &["lead".into(), "scout".into()],
            Some("lead"),
            None,
            false,
            false,
        );
        assert!(group.contains("Group chat \"Team\""));
        assert!(group.contains("members: lead, scout"));
        assert!(group.contains("lead: lead"));
        assert!(group.contains("your reply is posted here"));

        assert_eq!(
            origin_tag(None, &[], None, Some("ada"), false, false),
            "[Direct message from @ada]"
        );
        assert_eq!(
            origin_tag(None, &[], None, None, true, true),
            "[DM with the owner]"
        );
        assert_eq!(origin_tag(None, &[], None, None, true, false), "");
    }

    #[test]
    fn token_estimates_grow_with_content_and_tool_calls() {
        let small = vec![ChatMessage {
            role: Role::User,
            content: "hi".into(),
            tool_calls: vec![],
            tool_call_id: None,
            name: None,
        }];
        let big = vec![ChatMessage {
            role: Role::Assistant,
            content: "x".repeat(4000),
            tool_calls: vec![swarmbot_llm::ToolCall {
                id: "call_1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "cargo test"}),
            }],
            tool_call_id: None,
            name: None,
        }];
        assert!(estimate_messages(&big) > estimate_messages(&small) * 10);
    }

    #[test]
    fn an_unreadable_history_starts_fresh_instead_of_failing() {
        let store = testing::memory_store();
        let bot = testing::seed_bot(&store, "lead");
        let mut record = BotSessionRecord::new(bot.id.clone());
        record.history = serde_json::json!([{"role": "user"}, "nonsense", 42]);
        store.put_session(&record).unwrap();
        let session = Session::load(store.as_ref(), &bot.id).unwrap();
        assert!(session.messages().is_empty());
    }
}
