//! The transcript: one place that allocates a sequence, writes an entry,
//! publishes it and mirrors it to JSONL.
//!
//! Everything that appends to a conversation goes through here, because three
//! things have to stay in order: the per-conversation `seq` (no holes, no
//! duplicates), the commit-before-publish rule, and the JSONL mirror that lets
//! a lost database be rebuilt.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use tracing::warn;

use swarmbot_contracts::Seq;
use swarmbot_contracts::entry::{
    Author, CardEntry, Entry, EntryKind, EventEntry, MessageEntry, NoticeEntry, NoticeLevel,
    PostState, ReasoningEntry, ToolCallEntry,
};
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{BotId, ConversationId, EntryId, NodeId, TaskId, ToolCallId, TurnId};
use swarmbot_contracts::repo::Persistence;
use swarmbot_contracts::request::RuntimeRequest;
use swarmbot_store::JsonlMirror;

use crate::bus::{Change, LiveHub};

/// A transcript writer bound to one store, hub and (optional) mirror.
pub struct Transcript {
    store: Arc<dyn Persistence>,
    hub: Arc<LiveHub>,
    mirror: Option<Arc<JsonlMirror>>,
    /// Guards sequence allocation. One lock for the whole brain is enough: a
    /// write is a few SQLite statements, and correctness beats a little
    /// contention between two bots answering in the same group.
    seq_lock: Mutex<HashMap<ConversationId, Seq>>,
}

impl Transcript {
    pub fn new(
        store: Arc<dyn Persistence>,
        hub: Arc<LiveHub>,
        mirror: Option<Arc<JsonlMirror>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            hub,
            mirror,
            seq_lock: Mutex::new(HashMap::new()),
        })
    }

    pub fn store(&self) -> Arc<dyn Persistence> {
        Arc::clone(&self.store)
    }

    pub fn hub(&self) -> Arc<LiveHub> {
        Arc::clone(&self.hub)
    }

    /// The next sequence for a conversation, from the cache or the row.
    fn next_seq(&self, conversation: &ConversationId) -> Result<Seq, DomainError> {
        let mut cache = self.seq_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(seq) = cache.get(conversation) {
            let next = seq + 1;
            cache.insert(conversation.clone(), next);
            // The conversation row carries the counter too, so a restart
            // continues where the transcript stopped.
            if let Ok(mut row) = self
                .store
                .get_conversation(conversation)?
                .ok_or_else(|| swarmbot_contracts::not_found::conversation(conversation))
            {
                row.last_seq = next;
                row.updated_at = Utc::now();
                self.store.update_conversation(&row)?;
            }
            return Ok(next);
        }
        let mut row = self
            .store
            .get_conversation(conversation)?
            .ok_or_else(|| swarmbot_contracts::not_found::conversation(conversation))?;
        row.last_seq += 1;
        let seq = row.last_seq;
        row.updated_at = Utc::now();
        self.store.update_conversation(&row)?;
        cache.insert(conversation.clone(), seq);
        Ok(seq)
    }

    /// Append a new entry and publish it.
    pub fn append(
        &self,
        conversation: &ConversationId,
        author: Author,
        kind: EntryKind,
    ) -> Result<Entry, DomainError> {
        let seq = self.next_seq(conversation)?;
        let entry = Entry::new(conversation.clone(), seq, author, kind);
        self.write(&entry)?;
        Ok(entry)
    }

    /// Replace an entry in place (a streaming message growing, a tool call
    /// finishing). Bumps the revision; a stale write is a conflict.
    pub fn update(&self, entry: &Entry) -> Result<(), DomainError> {
        self.write(entry)
    }

    fn write(&self, entry: &Entry) -> Result<(), DomainError> {
        self.store.upsert_entry(entry)?;
        if let Some(mirror) = &self.mirror {
            // The header goes in before the first entry of a conversation (and
            // again if the row changed: a renamed group, an added member). It
            // is what makes `JsonlMirror::rebuild` able to restore a store that
            // can still be appended to — entries alone have nowhere to get the
            // next sequence from. Idempotent, so this costs a stat per write.
            if let Ok(Some(conversation)) = self.store.get_conversation(&entry.conversation_id)
                && let Err(err) = mirror.ensure_header(&conversation)
            {
                warn!(error = %err, "cannot write the transcript header to JSONL");
            }
            if let Err(err) = mirror.append(&entry.conversation_id, entry) {
                // The database is authoritative; a mirror that cannot be written
                // is a warning, not a failed turn.
                warn!(error = %err, "cannot mirror the transcript to JSONL");
            }
        }
        self.hub.publish(Change::Entry(entry.clone()));
        Ok(())
    }

    // --- convenience constructors -----------------------------------------

    pub fn owner_message(
        &self,
        conversation: &ConversationId,
        text: impl Into<String>,
    ) -> Result<Entry, DomainError> {
        self.append(
            conversation,
            Author::Owner,
            EntryKind::Message(Box::new(MessageEntry {
                text: text.into(),
                attachments: Vec::new(),
                reply_to: None,
                // The orchestrator flips this to `Queued` when the bot is busy
                // in another conversation, so the owner can see it waiting.
                post: PostState::Posted,
                addressed_to: None,
                turn_id: None,
            })),
        )
    }

    pub fn bot_message(
        &self,
        conversation: &ConversationId,
        bot: &BotId,
        text: impl Into<String>,
        turn_id: Option<TurnId>,
        addressed_to: Option<BotId>,
    ) -> Result<Entry, DomainError> {
        self.append(
            conversation,
            Author::bot(bot.clone()),
            EntryKind::Message(Box::new(MessageEntry {
                text: text.into(),
                attachments: Vec::new(),
                reply_to: None,
                post: PostState::Posted,
                addressed_to,
                turn_id,
            })),
        )
    }

    /// A streaming reply: created empty, then grown with [`Self::update`].
    pub fn begin_bot_message(
        &self,
        conversation: &ConversationId,
        bot: &BotId,
        turn_id: TurnId,
    ) -> Result<Entry, DomainError> {
        self.append(
            conversation,
            Author::bot(bot.clone()),
            EntryKind::Message(Box::new(MessageEntry {
                text: String::new(),
                attachments: Vec::new(),
                reply_to: None,
                post: PostState::Posted,
                addressed_to: None,
                turn_id: Some(turn_id),
            })),
        )
    }

    pub fn reasoning(
        &self,
        conversation: &ConversationId,
        bot: &BotId,
        text: impl Into<String>,
        turn_id: Option<TurnId>,
    ) -> Result<Entry, DomainError> {
        self.append(
            conversation,
            Author::bot(bot.clone()),
            EntryKind::Reasoning(Box::new(ReasoningEntry {
                text: text.into(),
                turn_id,
            })),
        )
    }

    /// A tool-call entry. Eight arguments because that is what a call is made
    /// of; bundling them would only move the noise to the call site.
    #[allow(clippy::too_many_arguments)]
    pub fn tool_call(
        &self,
        conversation: &ConversationId,
        turn_id: Option<TurnId>,
        call_id: ToolCallId,
        tool: impl Into<String>,
        args: serde_json::Value,
        node_id: Option<NodeId>,
        task_id: Option<TaskId>,
    ) -> Result<Entry, DomainError> {
        let entry = Entry::tool_call(
            conversation.clone(),
            self.next_seq(conversation)?,
            turn_id,
            call_id,
            tool,
            args,
            node_id,
            task_id,
        );
        self.write(&entry)?;
        Ok(entry)
    }

    pub fn card(
        &self,
        request: &RuntimeRequest,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<Entry, DomainError> {
        self.append(
            &request.conversation_id,
            Author::System,
            EntryKind::Card(Box::new(CardEntry {
                request_id: request.id.clone(),
                card: request.kind,
                status: request.status,
                title: title.into(),
                body: body.into(),
                payload: serde_json::json!({
                    "tool": request.tool,
                    "surface": request.surface,
                    "target": request.target,
                    "prompt": request.prompt,
                    "fields": request.fields,
                }),
                expires_at: request.expires_at,
            })),
        )
    }

    /// A non-message line: "Messaged @scout", "Stopped: too many hand-offs".
    pub fn event(
        &self,
        conversation: &ConversationId,
        event: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<Entry, DomainError> {
        self.append(
            conversation,
            Author::System,
            EntryKind::Event(Box::new(EventEntry {
                event: event.into(),
                text: text.into(),
                data: serde_json::Value::Null,
            })),
        )
    }

    pub fn notice(
        &self,
        conversation: &ConversationId,
        text: impl Into<String>,
        level: NoticeLevel,
    ) -> Result<Entry, DomainError> {
        self.append(
            conversation,
            Author::System,
            EntryKind::Notice(Box::new(NoticeEntry {
                text: text.into(),
                level,
            })),
        )
    }

    // --- reads --------------------------------------------------------------

    pub fn tail(
        &self,
        conversation: &ConversationId,
        limit: u32,
    ) -> Result<Vec<Entry>, DomainError> {
        self.store.entry_tail(conversation, limit)
    }

    pub fn since(
        &self,
        conversation: &ConversationId,
        after: Seq,
        limit: u32,
    ) -> Result<Vec<Entry>, DomainError> {
        self.store.entries_after(conversation, after, limit)
    }

    pub fn get(&self, id: &EntryId) -> Result<Option<Entry>, DomainError> {
        self.store.get_entry(id)
    }

    /// A tool-call entry that a later update finishes.
    pub fn finish_tool_call(
        &self,
        entry: &mut Entry,
        status: swarmbot_contracts::entry::ToolCallStatus,
        result_text: Option<String>,
        spill_path: Option<String>,
        duration_ms: Option<u64>,
    ) -> Result<(), DomainError> {
        let now = Utc::now();
        if let Some(call) = entry.as_tool_call_mut() {
            call.status = status;
            call.result_text = result_text;
            call.spill_path = spill_path;
            call.ended_at = Some(now);
            call.duration_ms = duration_ms;
        }
        entry.revision += 1;
        entry.updated_at = now;
        self.update(entry)
    }

    /// The tool-call payload of an entry, for callers that just upserted one.
    pub fn tool_call_of(entry: &Entry) -> Option<&ToolCallEntry> {
        entry.as_tool_call()
    }

    /// Forget a conversation's cache entry (a deleted group).
    pub fn forget(&self, conversation: &ConversationId) {
        self.seq_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(conversation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::entry::ToolCallStatus;

    fn setup() -> (
        Arc<dyn Persistence>,
        Arc<LiveHub>,
        Arc<Transcript>,
        ConversationId,
    ) {
        let store = crate::testing::memory_store();
        let hub = LiveHub::new(Arc::clone(&store));
        let bot = crate::testing::seed_bot(&store, "lead");
        let transcript = Transcript::new(store.clone(), hub, None);
        (
            store,
            transcript.hub(),
            transcript,
            bot.direct_conversation_id,
        )
    }

    #[test]
    fn sequences_count_up_without_holes() {
        let (store, _, transcript, conversation) = setup();
        for i in 0..3 {
            let entry = transcript
                .owner_message(&conversation, format!("line {i}"))
                .unwrap();
            assert_eq!(entry.seq, (i + 1) as Seq);
        }
        let row = store.get_conversation(&conversation).unwrap().unwrap();
        assert_eq!(row.last_seq, 3, "the conversation carries the counter");
        assert_eq!(transcript.tail(&conversation, 10).unwrap().len(), 3);
    }

    #[test]
    fn a_restart_continues_from_the_stored_counter() {
        let (store, hub, transcript, conversation) = setup();
        transcript.owner_message(&conversation, "before").unwrap();
        drop(transcript);

        // A fresh writer must not hand out seq 1 again.
        let again = Transcript::new(store.clone(), hub, None);
        let entry = again.owner_message(&conversation, "after").unwrap();
        assert_eq!(entry.seq, 2);
    }

    #[tokio::test]
    async fn updates_bump_the_revision_and_republish() {
        let (_, hub, transcript, conversation) = setup();
        let bot = BotId::new();
        let mut entry = transcript
            .begin_bot_message(&conversation, &bot, TurnId::new())
            .unwrap();
        assert_eq!(entry.revision, 1);

        let mut rx = hub.subscribe(None, Some(conversation.clone()));

        match &mut entry.kind {
            EntryKind::Message(message) => message.text = "hello".into(),
            other => panic!("expected a message, got {other:?}"),
        }
        entry.revision += 1;
        transcript.update(&entry).unwrap();

        // The update may arrive as a live upsert, or already be inside the
        // snapshot when the forwarder got there first: either way a subscriber
        // must end up seeing revision 2 with the new text.
        let mut saw_update = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let Ok(Some(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
            else {
                break;
            };
            match event {
                swarmbot_contracts::live::LiveEvent::EntryUpsert { entry: got, .. } => {
                    saw_update |= got.id == entry.id && got.revision == 2;
                }
                swarmbot_contracts::live::LiveEvent::Snapshot(snapshot) => {
                    saw_update |= snapshot
                        .entries
                        .iter()
                        .any(|got| got.id == entry.id && got.revision == 2);
                }
                swarmbot_contracts::live::LiveEvent::Synchronized { .. } if saw_update => break,
                _ => {}
            }
        }
        assert!(saw_update, "the grown message reaches a subscriber");
        let stored = transcript.get(&entry.id).unwrap().unwrap();
        assert_eq!(stored.revision, 2);
        assert_eq!(stored.as_message().unwrap().text, "hello");
    }

    #[test]
    fn a_stale_revision_is_a_conflict() {
        let (_, _, transcript, conversation) = setup();
        let entry = transcript.owner_message(&conversation, "v1").unwrap();
        // Somebody else already moved it on.
        let mut newer = entry.clone();
        newer.revision = 5;
        transcript.update(&newer).unwrap();
        let stale = entry; // revision 1
        let err = transcript.update(&stale).unwrap_err();
        assert_eq!(err.code(), "conflict", "{err}");
    }

    #[test]
    fn tool_calls_finish_in_place() {
        let (_, _, transcript, conversation) = setup();
        let mut entry = transcript
            .tool_call(
                &conversation,
                Some(TurnId::new()),
                ToolCallId::new(),
                "shell",
                serde_json::json!({"command": "echo hi"}),
                Some(NodeId::local()),
                None,
            )
            .unwrap();
        assert_eq!(
            Transcript::tool_call_of(&entry).unwrap().status,
            ToolCallStatus::Running
        );
        transcript
            .finish_tool_call(
                &mut entry,
                ToolCallStatus::Completed,
                Some("hi".into()),
                None,
                Some(12),
            )
            .unwrap();
        let call = Transcript::tool_call_of(&entry).unwrap();
        assert_eq!(call.status, ToolCallStatus::Completed);
        assert_eq!(call.result_text.as_deref(), Some("hi"));
        assert_eq!(call.duration_ms, Some(12));
        assert_eq!(entry.revision, 2);
    }

    #[test]
    fn events_and_notices_are_system_authored() {
        let (_, _, transcript, conversation) = setup();
        let event = transcript
            .event(&conversation, "bot-messaged", "Messaged @scout")
            .unwrap();
        assert_eq!(event.author, Author::System);
        assert_eq!(event.kind.text(), Some("Messaged @scout"));
        let notice = transcript
            .notice(&conversation, "backend refused", NoticeLevel::Error)
            .unwrap();
        assert_eq!(notice.kind.kind_str(), "notice");
    }

    #[test]
    fn writing_to_an_unknown_conversation_fails_loudly() {
        let store = crate::testing::memory_store();
        let hub = LiveHub::new(store.clone());
        let transcript = Transcript::new(store, hub, None);
        let err = transcript
            .owner_message(&ConversationId::new(), "nowhere")
            .unwrap_err();
        assert_eq!(err.code(), "not_found");
    }

    #[test]
    fn a_deleted_group_leaves_no_cached_counter() {
        let (store, _, transcript, conversation) = setup();
        transcript.owner_message(&conversation, "x").unwrap();
        transcript.forget(&conversation);
        store.delete_conversation(&conversation).unwrap();
        assert!(transcript.owner_message(&conversation, "y").is_err());
    }
}
