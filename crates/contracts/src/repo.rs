//! Repository ports: what persistence must offer, and nothing more.
//!
//! These traits are the only way the orchestration touches storage. Two
//! implementations exist and must behave identically — SQLite (durable) and an
//! in-memory map (tests) — and one shared contract suite is run against both.
//!
//! They are deliberately **synchronous**: SQLite is a local file and every
//! operation is microseconds, so making them `async` would only add colouring.
//! Implementations must be `Send + Sync` (a `Mutex<Connection>` is enough) so
//! async callers can share one store.
//!
//! Invariants every implementation keeps:
//!
//! * **whole-entry upserts**: an [`Entry`] is written with its `revision`, and
//!   a write whose revision is not greater than the stored one is a
//!   [`DomainError::Conflict`];
//! * **writes commit before publishing**: [`EventLog::append_event`] is called
//!   after the row exists, never before;
//! * **sequences are per conversation** and handed out by the conversation row
//!   ([`Conversation::next_seq`]), so a transcript never has a hole.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Seq;
use crate::bot::Bot;
use crate::config::Settings;
use crate::conversation::Conversation;
use crate::delivery::Delivery;
use crate::entry::Entry;
use crate::error::DomainError;
use crate::id::{
    BotId, ConversationId, DeliveryId, EntryId, NodeId, RequestId, RoutineId, RuleId, RunId,
    TaskId, ToolCallId, TurnId,
};
use crate::live::{LiveCursor, LiveEvent};
use crate::node::{NodeInfo, PairingRecord};
use crate::request::{ApprovalRule, RuntimeRequest};
use crate::routine::{Routine, RoutineRun};
use crate::task::{Task, TaskStep};
use crate::turn::Turn;

pub trait BotRepo: Send + Sync {
    fn insert_bot(&self, bot: &Bot) -> Result<(), DomainError>;
    fn update_bot(&self, bot: &Bot) -> Result<(), DomainError>;
    fn get_bot(&self, id: &BotId) -> Result<Option<Bot>, DomainError>;
    /// Lookup by the normalized `@handle`.
    fn bot_by_name(&self, name: &str) -> Result<Option<Bot>, DomainError>;
    fn list_bots(&self) -> Result<Vec<Bot>, DomainError>;
    fn delete_bot(&self, id: &BotId) -> Result<(), DomainError>;
}

pub trait ConversationRepo: Send + Sync {
    fn insert_conversation(&self, conversation: &Conversation) -> Result<(), DomainError>;
    fn update_conversation(&self, conversation: &Conversation) -> Result<(), DomainError>;
    fn get_conversation(&self, id: &ConversationId) -> Result<Option<Conversation>, DomainError>;
    /// The bot's 1:1 conversation with the owner.
    fn direct_conversation(&self, bot: &BotId) -> Result<Option<Conversation>, DomainError>;
    fn list_conversations(&self) -> Result<Vec<Conversation>, DomainError>;
    /// Groups only: a DM is never deleted, its bot is.
    fn delete_conversation(&self, id: &ConversationId) -> Result<(), DomainError>;
}

pub trait EntryRepo: Send + Sync {
    /// Insert or replace by id. A `revision` that is not newer is a conflict.
    fn upsert_entry(&self, entry: &Entry) -> Result<(), DomainError>;
    fn get_entry(&self, id: &EntryId) -> Result<Option<Entry>, DomainError>;
    /// Ascending by `seq`, after a cursor, up to `limit`.
    fn entries_after(
        &self,
        conversation: &ConversationId,
        after_seq: Seq,
        limit: u32,
    ) -> Result<Vec<Entry>, DomainError>;
    /// The newest `limit` entries, returned in ascending order.
    fn entry_tail(
        &self,
        conversation: &ConversationId,
        limit: u32,
    ) -> Result<Vec<Entry>, DomainError>;
    /// Every entry of one conversation, ascending (used by the JSONL mirror and
    /// by tests). Returns the count deleted.
    fn delete_entries(&self, conversation: &ConversationId) -> Result<u64, DomainError>;
}

pub trait TurnRepo: Send + Sync {
    fn insert_turn(&self, turn: &Turn) -> Result<(), DomainError>;
    fn update_turn(&self, turn: &Turn) -> Result<(), DomainError>;
    fn get_turn(&self, id: &TurnId) -> Result<Option<Turn>, DomainError>;
    fn turns_in(&self, conversation: &ConversationId, limit: u32)
    -> Result<Vec<Turn>, DomainError>;
    /// Turns still marked running — what boot recovery has to settle.
    fn running_turns(&self) -> Result<Vec<Turn>, DomainError>;
    /// Every turn that **started at or after `since`**, oldest first.
    ///
    /// This is the read behind the usage report, so its invariants are pinned
    /// by the shared contract suite rather than by either implementation:
    ///
    /// * the boundary is **inclusive** (`started_at >= since`) — a window that
    ///   asks for "since midnight" includes the turn that started at midnight;
    /// * the order is `started_at`, then `id`, so a report is deterministic and
    ///   two turns started in the same microsecond fall to the time-sortable id
    ///   instead of to a hash map's iteration order;
    /// * a turn that started before `since` is never in the result, and there is
    ///   no limit: the caller chose the window, and a silently truncated window
    ///   would under-report the spend.
    fn turns_since(&self, since: DateTime<Utc>) -> Result<Vec<Turn>, DomainError>;
}

pub trait DeliveryRepo: Send + Sync {
    fn insert_delivery(&self, delivery: &Delivery) -> Result<(), DomainError>;
    fn update_delivery(&self, delivery: &Delivery) -> Result<(), DomainError>;
    fn get_delivery(&self, id: &DeliveryId) -> Result<Option<Delivery>, DomainError>;
    /// FIFO: created_at, then id.
    fn queued_deliveries(&self, bot: &BotId) -> Result<Vec<Delivery>, DomainError>;
    fn drop_deliveries(
        &self,
        conversation: &ConversationId,
        reason: &str,
    ) -> Result<u64, DomainError>;
}

pub trait TaskRepo: Send + Sync {
    fn insert_task(&self, task: &Task) -> Result<(), DomainError>;
    fn update_task(&self, task: &Task) -> Result<(), DomainError>;
    fn get_task(&self, id: &TaskId) -> Result<Option<Task>, DomainError>;
    fn tasks_of(&self, owner: &BotId) -> Result<Vec<Task>, DomainError>;
    fn tasks_in(&self, origin: &ConversationId) -> Result<Vec<Task>, DomainError>;
    /// Queued tasks in FIFO order, across bots (the pump picks from these).
    fn queued_tasks(&self) -> Result<Vec<Task>, DomainError>;
    fn running_tasks(&self) -> Result<Vec<Task>, DomainError>;
    fn running_task_count(&self, owner: &BotId) -> Result<u32, DomainError>;
    fn append_task_step(&self, step: &TaskStep) -> Result<(), DomainError>;
    fn task_steps(&self, task: &TaskId) -> Result<Vec<TaskStep>, DomainError>;
}

pub trait RequestRepo: Send + Sync {
    fn insert_request(&self, request: &RuntimeRequest) -> Result<(), DomainError>;
    fn update_request(&self, request: &RuntimeRequest) -> Result<(), DomainError>;
    fn get_request(&self, id: &RequestId) -> Result<Option<RuntimeRequest>, DomainError>;
    fn pending_requests(&self) -> Result<Vec<RuntimeRequest>, DomainError>;
    fn requests_in(
        &self,
        conversation: &ConversationId,
    ) -> Result<Vec<RuntimeRequest>, DomainError>;
    fn cancel_requests_of_turn(&self, turn: &TurnId, reason: &str) -> Result<u64, DomainError>;
}

pub trait RuleRepo: Send + Sync {
    fn insert_rule(&self, rule: &ApprovalRule) -> Result<(), DomainError>;
    fn list_rules(&self) -> Result<Vec<ApprovalRule>, DomainError>;
    fn delete_rule(&self, id: &RuleId) -> Result<(), DomainError>;
}

pub trait NodeRepo: Send + Sync {
    fn insert_node(&self, node: &NodeInfo) -> Result<(), DomainError>;
    /// Write a whole roster row, guarded by [`NodeInfo::revision`].
    ///
    /// The revision must be **strictly newer** than the stored one, exactly like
    /// [`Entry`]: a stale write is [`DomainError::Conflict`], never a silent
    /// overwrite. The guard exists because the row has several concurrent
    /// writers on one connection — the mux spawns a task per inbound request, so
    /// `node.hello` and `node.heartbeat` genuinely overlap — and each is a
    /// read–modify–write of the *whole* row. Without the guard the loser
    /// restores the row it read and erases the winner's fields; the observed
    /// symptom was a node's `policy` reverting to its pre-hello value while the
    /// node was online.
    ///
    /// A conflict is not an error condition to surface to a user: the caller
    /// re-reads, re-applies its own field and retries. Implementations must make
    /// the read-and-write atomic (one transaction, or one locked connection),
    /// otherwise the guard can be raced the same way the row was.
    fn update_node(&self, node: &NodeInfo) -> Result<(), DomainError>;
    fn get_node(&self, id: &NodeId) -> Result<Option<NodeInfo>, DomainError>;
    fn list_nodes(&self) -> Result<Vec<NodeInfo>, DomainError>;
    /// A pairing code waiting to be used (stored hashed).
    fn insert_pairing(&self, record: &PairingRecord) -> Result<(), DomainError>;
    fn get_pairing(&self, code_hash: &str) -> Result<Option<PairingRecord>, DomainError>;
    fn update_pairing(&self, record: &PairingRecord) -> Result<(), DomainError>;
    fn expired_pairings(&self, now: DateTime<Utc>) -> Result<Vec<PairingRecord>, DomainError>;
    /// A node's pinned public key, base64url. Kept apart from `NodeInfo` so a
    /// roster read never has to carry key material.
    fn put_node_key(&self, node: &NodeId, public_key_base64url: &str) -> Result<(), DomainError>;
    fn node_key(&self, node: &NodeId) -> Result<Option<String>, DomainError>;
    fn list_node_keys(&self) -> Result<Vec<(NodeId, String)>, DomainError>;
}

/// One mind per bot: the session state shared by all of its conversations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BotSessionRecord {
    pub bot_id: BotId,
    /// The backend's own session id, when it has one (`claude --resume`).
    pub native_session_id: Option<String>,
    /// The message history, opaque to the store: an array of
    /// `swarmbot_llm::ChatMessage`.
    pub history: Value,
    /// Per-conversation read cursors, so a group catch-up shows each message
    /// to each bot exactly once.
    pub seen: std::collections::BTreeMap<ConversationId, Seq>,
    /// Rough token count of `history`, for the compaction decision.
    pub token_estimate: u64,
    pub updated_at: DateTime<Utc>,
}

impl BotSessionRecord {
    pub fn new(bot_id: BotId) -> Self {
        Self {
            bot_id,
            native_session_id: None,
            history: Value::Array(Vec::new()),
            seen: Default::default(),
            token_estimate: 0,
            updated_at: Utc::now(),
        }
    }
}

pub trait SessionRepo: Send + Sync {
    fn get_session(&self, bot: &BotId) -> Result<Option<BotSessionRecord>, DomainError>;
    fn put_session(&self, record: &BotSessionRecord) -> Result<(), DomainError>;
    fn delete_session(&self, bot: &BotId) -> Result<(), DomainError>;
    /// Drop the native session (New session), keeping the read cursors.
    fn reset_session(&self, bot: &BotId) -> Result<(), DomainError>;
}

/// Idempotency: one row per tool call a turn already made.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub turn_id: TurnId,
    pub call_id: ToolCallId,
    pub tool: String,
    pub args: Value,
    pub result_text: String,
    pub is_error: bool,
    pub node_id: Option<NodeId>,
    pub created_at: DateTime<Utc>,
}

pub trait ToolCallRepo: Send + Sync {
    /// Returns the stored result when this call already ran (a harness may
    /// replay a call id after a resume).
    fn find_tool_call(
        &self,
        turn: &TurnId,
        call: &ToolCallId,
    ) -> Result<Option<ToolCallRecord>, DomainError>;
    fn insert_tool_call(&self, record: &ToolCallRecord) -> Result<(), DomainError>;
}

/// The append-only change log that live clients resume from.
pub trait EventLog: Send + Sync {
    /// The epoch changes whenever the log is reset (a fresh data dir), which
    /// tells clients to take a snapshot instead of resuming.
    fn epoch(&self) -> Result<u64, DomainError>;
    fn current_cursor(&self) -> Result<LiveCursor, DomainError>;
    /// Store the event and return the cursor it got.
    fn append_event(&self, event: &LiveEvent) -> Result<LiveCursor, DomainError>;
    /// Events strictly after `cursor`, ascending, up to `limit`.
    fn events_since(
        &self,
        cursor: LiveCursor,
        limit: u32,
    ) -> Result<Vec<(LiveCursor, LiveEvent)>, DomainError>;
    /// Drop all but the newest `keep` events (the ring is bounded on disk too).
    fn truncate_events(&self, keep: u64) -> Result<u64, DomainError>;
}

pub trait SettingsStore: Send + Sync {
    fn settings(&self) -> Result<Settings, DomainError>;
    fn put_settings(&self, settings: &Settings) -> Result<(), DomainError>;
}

pub trait RoutineRepo: Send + Sync {
    fn insert_routine(&self, routine: &Routine) -> Result<(), DomainError>;
    fn update_routine(&self, routine: &Routine) -> Result<(), DomainError>;
    fn get_routine(&self, id: &RoutineId) -> Result<Option<Routine>, DomainError>;
    fn list_routines(&self) -> Result<Vec<Routine>, DomainError>;
    fn list_routines_for_bot(&self, bot: &BotId) -> Result<Vec<Routine>, DomainError>;
    fn due_routines(&self, now: DateTime<Utc>) -> Result<Vec<Routine>, DomainError>;
    fn delete_routine(&self, id: &RoutineId) -> Result<(), DomainError>;

    fn insert_run(&self, run: &RoutineRun) -> Result<(), DomainError>;
    fn update_run(&self, run: &RoutineRun) -> Result<(), DomainError>;
    fn get_run(&self, id: &RunId) -> Result<Option<RoutineRun>, DomainError>;
    fn list_runs(&self, routine_id: &RoutineId, limit: u32)
    -> Result<Vec<RoutineRun>, DomainError>;
}

/// Everything the brain needs from storage, as one object.
pub trait Persistence:
    BotRepo
    + ConversationRepo
    + EntryRepo
    + TurnRepo
    + DeliveryRepo
    + TaskRepo
    + RequestRepo
    + RuleRepo
    + NodeRepo
    + SessionRepo
    + ToolCallRepo
    + RoutineRepo
    + EventLog
    + SettingsStore
    + Send
    + Sync
{
}

impl<T> Persistence for T where
    T: BotRepo
        + ConversationRepo
        + EntryRepo
        + TurnRepo
        + DeliveryRepo
        + TaskRepo
        + RequestRepo
        + RuleRepo
        + NodeRepo
        + SessionRepo
        + ToolCallRepo
        + RoutineRepo
        + EventLog
        + SettingsStore
        + Send
        + Sync
{
}

/// What boot recovery did. Every field is reported to the owner because each
/// one is work that was cut off, not work that finished.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub interrupted_turns: Vec<TurnId>,
    pub expired_requests: Vec<RequestId>,
    pub interrupted_tasks: Vec<TaskId>,
    /// Queued deliveries were dropped: their conversation may be gone, and a
    /// stale queue must not wake a bot with last week's message.
    pub dropped_deliveries: u64,
}

impl RecoveryReport {
    pub fn is_empty(&self) -> bool {
        self.interrupted_turns.is_empty()
            && self.expired_requests.is_empty()
            && self.interrupted_tasks.is_empty()
            && self.dropped_deliveries == 0
    }

    /// One line for the boot log and for the owner.
    pub fn summary(&self) -> String {
        if self.is_empty() {
            return "nothing to recover".to_string();
        }
        format!(
            "recovered: {} turn(s) interrupted, {} request(s) expired, {} task(s) interrupted, {} delivery(ies) dropped",
            self.interrupted_turns.len(),
            self.expired_requests.len(),
            self.interrupted_tasks.len(),
            self.dropped_deliveries
        )
    }
}

/// Settle everything a restart cut off: running turns become `interrupted`,
/// pending requests `expired`, running or queued tasks `interrupted`, and the
/// delivery queue is emptied.
///
/// One function over the port, so both implementations behave the same and one
/// test covers both.
pub fn recover(store: &dyn Persistence, now: DateTime<Utc>) -> Result<RecoveryReport, DomainError> {
    let mut report = RecoveryReport::default();

    for mut turn in store.running_turns()? {
        if turn.interrupt(now, "interrupted by a restart") {
            store.update_turn(&turn)?;
            report.interrupted_turns.push(turn.id.clone());
        }
    }

    for mut request in store.pending_requests()? {
        if request.expire(now) {
            store.update_request(&request)?;
            report.expired_requests.push(request.id.clone());
        }
    }

    // A task that ran, or waited to run, cannot be assumed finished.
    for mut task in store.running_tasks()? {
        if task.interrupt(now) {
            store.update_task(&task)?;
            report.interrupted_tasks.push(task.id.clone());
        }
    }
    for mut task in store.queued_tasks()? {
        if task.interrupt(now) {
            store.update_task(&task)?;
            report.interrupted_tasks.push(task.id.clone());
        }
    }

    for conversation in store.list_conversations()? {
        report.dropped_deliveries +=
            store.drop_deliveries(&conversation.id, "dropped by a restart")?;
    }

    // Requests that a turn raised but never got answered are already expired
    // above; a turn's own requests are cancelled so a card cannot outlive it.
    for turn_id in &report.interrupted_turns {
        store.cancel_requests_of_turn(turn_id, "turn interrupted by a restart")?;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::RoutineId;
    use crate::task::TaskStatus;
    use crate::turn::TurnStatus;

    /// A deliberately tiny store, only what these tests need, to prove the
    /// recovery function is written against the port and not against SQLite.
    #[derive(Default)]
    struct Stub {
        turns: std::sync::Mutex<Vec<Turn>>,
        requests: std::sync::Mutex<Vec<RuntimeRequest>>,
        tasks: std::sync::Mutex<Vec<Task>>,
        deliveries: std::sync::Mutex<Vec<Delivery>>,
        conversations: std::sync::Mutex<Vec<Conversation>>,
        cancelled: std::sync::Mutex<Vec<TurnId>>,
    }

    impl Stub {
        fn new() -> Self {
            Self::default()
        }
    }

    // The stub implements the whole port by refusing what it does not model.
    impl BotRepo for Stub {
        fn insert_bot(&self, _: &Bot) -> Result<(), DomainError> {
            Ok(())
        }
        fn update_bot(&self, _: &Bot) -> Result<(), DomainError> {
            Ok(())
        }
        fn get_bot(&self, _: &BotId) -> Result<Option<Bot>, DomainError> {
            Ok(None)
        }
        fn bot_by_name(&self, _: &str) -> Result<Option<Bot>, DomainError> {
            Ok(None)
        }
        fn list_bots(&self) -> Result<Vec<Bot>, DomainError> {
            Ok(vec![])
        }
        fn delete_bot(&self, _: &BotId) -> Result<(), DomainError> {
            Ok(())
        }
    }
    impl ConversationRepo for Stub {
        fn insert_conversation(&self, c: &Conversation) -> Result<(), DomainError> {
            self.conversations.lock().unwrap().push(c.clone());
            Ok(())
        }
        fn update_conversation(&self, _: &Conversation) -> Result<(), DomainError> {
            Ok(())
        }
        fn get_conversation(
            &self,
            _: &ConversationId,
        ) -> Result<Option<Conversation>, DomainError> {
            Ok(None)
        }
        fn direct_conversation(&self, _: &BotId) -> Result<Option<Conversation>, DomainError> {
            Ok(None)
        }
        fn list_conversations(&self) -> Result<Vec<Conversation>, DomainError> {
            Ok(self.conversations.lock().unwrap().clone())
        }
        fn delete_conversation(&self, _: &ConversationId) -> Result<(), DomainError> {
            Ok(())
        }
    }
    impl EntryRepo for Stub {
        fn upsert_entry(&self, _: &Entry) -> Result<(), DomainError> {
            Ok(())
        }
        fn get_entry(&self, _: &EntryId) -> Result<Option<Entry>, DomainError> {
            Ok(None)
        }
        fn entries_after(
            &self,
            _: &ConversationId,
            _: Seq,
            _: u32,
        ) -> Result<Vec<Entry>, DomainError> {
            Ok(vec![])
        }
        fn entry_tail(&self, _: &ConversationId, _: u32) -> Result<Vec<Entry>, DomainError> {
            Ok(vec![])
        }
        fn delete_entries(&self, _: &ConversationId) -> Result<u64, DomainError> {
            Ok(0)
        }
    }
    impl TurnRepo for Stub {
        fn insert_turn(&self, t: &Turn) -> Result<(), DomainError> {
            self.turns.lock().unwrap().push(t.clone());
            Ok(())
        }
        fn update_turn(&self, t: &Turn) -> Result<(), DomainError> {
            let mut turns = self.turns.lock().unwrap();
            if let Some(slot) = turns.iter_mut().find(|x| x.id == t.id) {
                *slot = t.clone();
            }
            Ok(())
        }
        fn get_turn(&self, _: &TurnId) -> Result<Option<Turn>, DomainError> {
            Ok(None)
        }
        fn turns_in(&self, _: &ConversationId, _: u32) -> Result<Vec<Turn>, DomainError> {
            Ok(vec![])
        }
        fn running_turns(&self) -> Result<Vec<Turn>, DomainError> {
            Ok(self
                .turns
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.is_running())
                .cloned()
                .collect())
        }
        fn turns_since(&self, since: DateTime<Utc>) -> Result<Vec<Turn>, DomainError> {
            Ok(self
                .turns
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.started_at >= since)
                .cloned()
                .collect())
        }
    }
    impl DeliveryRepo for Stub {
        fn insert_delivery(&self, d: &Delivery) -> Result<(), DomainError> {
            self.deliveries.lock().unwrap().push(d.clone());
            Ok(())
        }
        fn update_delivery(&self, _: &Delivery) -> Result<(), DomainError> {
            Ok(())
        }
        fn get_delivery(&self, _: &DeliveryId) -> Result<Option<Delivery>, DomainError> {
            Ok(None)
        }
        fn queued_deliveries(&self, _: &BotId) -> Result<Vec<Delivery>, DomainError> {
            Ok(vec![])
        }
        fn drop_deliveries(
            &self,
            conversation: &ConversationId,
            _: &str,
        ) -> Result<u64, DomainError> {
            let mut deliveries = self.deliveries.lock().unwrap();
            let before = deliveries.len();
            deliveries.retain(|d| d.conversation_id != *conversation);
            Ok((before - deliveries.len()) as u64)
        }
    }
    impl TaskRepo for Stub {
        fn insert_task(&self, t: &Task) -> Result<(), DomainError> {
            self.tasks.lock().unwrap().push(t.clone());
            Ok(())
        }
        fn update_task(&self, t: &Task) -> Result<(), DomainError> {
            let mut tasks = self.tasks.lock().unwrap();
            if let Some(slot) = tasks.iter_mut().find(|x| x.id == t.id) {
                *slot = t.clone();
            }
            Ok(())
        }
        fn get_task(&self, _: &TaskId) -> Result<Option<Task>, DomainError> {
            Ok(None)
        }
        fn tasks_of(&self, _: &BotId) -> Result<Vec<Task>, DomainError> {
            Ok(vec![])
        }
        fn tasks_in(&self, _: &ConversationId) -> Result<Vec<Task>, DomainError> {
            Ok(vec![])
        }
        fn queued_tasks(&self) -> Result<Vec<Task>, DomainError> {
            Ok(self
                .tasks
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.status == TaskStatus::Queued)
                .cloned()
                .collect())
        }
        fn running_tasks(&self) -> Result<Vec<Task>, DomainError> {
            Ok(self
                .tasks
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.status == TaskStatus::Running)
                .cloned()
                .collect())
        }
        fn running_task_count(&self, _: &BotId) -> Result<u32, DomainError> {
            Ok(0)
        }
        fn append_task_step(&self, _: &TaskStep) -> Result<(), DomainError> {
            Ok(())
        }
        fn task_steps(&self, _: &TaskId) -> Result<Vec<TaskStep>, DomainError> {
            Ok(vec![])
        }
    }
    impl RequestRepo for Stub {
        fn insert_request(&self, r: &RuntimeRequest) -> Result<(), DomainError> {
            self.requests.lock().unwrap().push(r.clone());
            Ok(())
        }
        fn update_request(&self, r: &RuntimeRequest) -> Result<(), DomainError> {
            let mut requests = self.requests.lock().unwrap();
            if let Some(slot) = requests.iter_mut().find(|x| x.id == r.id) {
                *slot = r.clone();
            }
            Ok(())
        }
        fn get_request(&self, _: &RequestId) -> Result<Option<RuntimeRequest>, DomainError> {
            Ok(None)
        }
        fn pending_requests(&self) -> Result<Vec<RuntimeRequest>, DomainError> {
            Ok(self
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.is_pending())
                .cloned()
                .collect())
        }
        fn requests_in(&self, _: &ConversationId) -> Result<Vec<RuntimeRequest>, DomainError> {
            Ok(vec![])
        }
        fn cancel_requests_of_turn(&self, turn: &TurnId, _: &str) -> Result<u64, DomainError> {
            self.cancelled.lock().unwrap().push(turn.clone());
            Ok(0)
        }
    }
    impl RuleRepo for Stub {
        fn insert_rule(&self, _: &ApprovalRule) -> Result<(), DomainError> {
            Ok(())
        }
        fn list_rules(&self) -> Result<Vec<ApprovalRule>, DomainError> {
            Ok(vec![])
        }
        fn delete_rule(&self, _: &RuleId) -> Result<(), DomainError> {
            Ok(())
        }
    }
    impl NodeRepo for Stub {
        fn insert_node(&self, _: &NodeInfo) -> Result<(), DomainError> {
            Ok(())
        }
        fn update_node(&self, _: &NodeInfo) -> Result<(), DomainError> {
            Ok(())
        }
        fn get_node(&self, _: &NodeId) -> Result<Option<NodeInfo>, DomainError> {
            Ok(None)
        }
        fn list_nodes(&self) -> Result<Vec<NodeInfo>, DomainError> {
            Ok(vec![])
        }
        fn insert_pairing(&self, _: &PairingRecord) -> Result<(), DomainError> {
            Ok(())
        }
        fn get_pairing(&self, _: &str) -> Result<Option<PairingRecord>, DomainError> {
            Ok(None)
        }
        fn update_pairing(&self, _: &PairingRecord) -> Result<(), DomainError> {
            Ok(())
        }
        fn expired_pairings(&self, _: DateTime<Utc>) -> Result<Vec<PairingRecord>, DomainError> {
            Ok(vec![])
        }
        fn put_node_key(&self, _: &NodeId, _: &str) -> Result<(), DomainError> {
            Ok(())
        }
        fn node_key(&self, _: &NodeId) -> Result<Option<String>, DomainError> {
            Ok(None)
        }
        fn list_node_keys(&self) -> Result<Vec<(NodeId, String)>, DomainError> {
            Ok(vec![])
        }
    }
    impl SessionRepo for Stub {
        fn get_session(&self, _: &BotId) -> Result<Option<BotSessionRecord>, DomainError> {
            Ok(None)
        }
        fn put_session(&self, _: &BotSessionRecord) -> Result<(), DomainError> {
            Ok(())
        }
        fn delete_session(&self, _: &BotId) -> Result<(), DomainError> {
            Ok(())
        }
        fn reset_session(&self, _: &BotId) -> Result<(), DomainError> {
            Ok(())
        }
    }
    impl ToolCallRepo for Stub {
        fn find_tool_call(
            &self,
            _: &TurnId,
            _: &ToolCallId,
        ) -> Result<Option<ToolCallRecord>, DomainError> {
            Ok(None)
        }
        fn insert_tool_call(&self, _: &ToolCallRecord) -> Result<(), DomainError> {
            Ok(())
        }
    }
    impl EventLog for Stub {
        fn epoch(&self) -> Result<u64, DomainError> {
            Ok(1)
        }
        fn current_cursor(&self) -> Result<LiveCursor, DomainError> {
            Ok(LiveCursor::default())
        }
        fn append_event(&self, _: &LiveEvent) -> Result<LiveCursor, DomainError> {
            Ok(LiveCursor::default())
        }
        fn events_since(
            &self,
            _: LiveCursor,
            _: u32,
        ) -> Result<Vec<(LiveCursor, LiveEvent)>, DomainError> {
            Ok(vec![])
        }
        fn truncate_events(&self, _: u64) -> Result<u64, DomainError> {
            Ok(0)
        }
    }
    impl SettingsStore for Stub {
        fn settings(&self) -> Result<Settings, DomainError> {
            Ok(Settings::default())
        }
        fn put_settings(&self, _: &Settings) -> Result<(), DomainError> {
            Ok(())
        }
    }
    impl RoutineRepo for Stub {
        fn insert_routine(&self, _: &Routine) -> Result<(), DomainError> {
            Ok(())
        }
        fn update_routine(&self, _: &Routine) -> Result<(), DomainError> {
            Ok(())
        }
        fn get_routine(&self, _: &RoutineId) -> Result<Option<Routine>, DomainError> {
            Ok(None)
        }
        fn list_routines(&self) -> Result<Vec<Routine>, DomainError> {
            Ok(vec![])
        }
        fn list_routines_for_bot(&self, _: &BotId) -> Result<Vec<Routine>, DomainError> {
            Ok(vec![])
        }
        fn due_routines(&self, _: DateTime<Utc>) -> Result<Vec<Routine>, DomainError> {
            Ok(vec![])
        }
        fn delete_routine(&self, _: &RoutineId) -> Result<(), DomainError> {
            Ok(())
        }
        fn insert_run(&self, _: &RoutineRun) -> Result<(), DomainError> {
            Ok(())
        }
        fn update_run(&self, _: &RoutineRun) -> Result<(), DomainError> {
            Ok(())
        }
        fn get_run(&self, _: &RunId) -> Result<Option<RoutineRun>, DomainError> {
            Ok(None)
        }
        fn list_runs(&self, _: &RoutineId, _: u32) -> Result<Vec<RoutineRun>, DomainError> {
            Ok(vec![])
        }
    }

    #[test]
    fn recovery_settles_everything_a_restart_cut_off() {
        let store = Stub::new();
        let conversation = Conversation::direct(BotId::new());
        store.insert_conversation(&conversation).unwrap();

        let turn = Turn::start(
            BotId::new(),
            conversation.id.clone(),
            crate::turn::TurnInitiator::Owner,
            None,
            None,
        );
        store.insert_turn(&turn).unwrap();
        let mut done = Turn::start(
            BotId::new(),
            conversation.id.clone(),
            crate::turn::TurnInitiator::Owner,
            None,
            None,
        );
        done.complete(Utc::now());
        store.insert_turn(&done).unwrap();

        let request =
            RuntimeRequest::question(BotId::new(), conversation.id.clone(), "which one?", vec![]);
        store.insert_request(&request).unwrap();

        let mut running = Task::own(BotId::new(), conversation.id.clone(), "build", "x");
        running.start(Utc::now(), Some(TurnId::new()));
        store.insert_task(&running).unwrap();
        let queued = Task::own(BotId::new(), conversation.id.clone(), "later", "y");
        store.insert_task(&queued).unwrap();
        let finished = {
            let mut t = Task::own(BotId::new(), conversation.id.clone(), "earlier", "z");
            t.succeed(Utc::now(), "done");
            t
        };
        store.insert_task(&finished).unwrap();

        store
            .insert_delivery(&Delivery::owner_message(
                conversation.id.clone(),
                BotId::new(),
                "stale",
            ))
            .unwrap();
        // A routine delivery is dropped like any other: nothing resumes it.
        store
            .insert_delivery(&Delivery::routine(
                conversation.id.clone(),
                BotId::new(),
                RoutineId::new(),
                "daily",
            ))
            .unwrap();

        let report = recover(&store, Utc::now()).unwrap();
        assert_eq!(report.interrupted_turns, vec![turn.id.clone()]);
        assert_eq!(report.expired_requests, vec![request.id.clone()]);
        assert_eq!(report.interrupted_tasks.len(), 2, "{report:?}");
        assert!(report.interrupted_tasks.contains(&running.id));
        assert!(report.interrupted_tasks.contains(&queued.id));
        assert!(!report.interrupted_tasks.contains(&finished.id));
        assert_eq!(report.dropped_deliveries, 2);
        assert!(!report.is_empty());
        assert!(store.cancelled.lock().unwrap().contains(&turn.id));

        // Terminal work is left alone.
        let turns = store.turns.lock().unwrap();
        let finished_turn = turns.iter().find(|t| t.id == done.id).unwrap();
        assert_eq!(finished_turn.status, TurnStatus::Completed);
    }

    #[test]
    fn recovery_on_a_clean_store_reports_nothing() {
        let store = Stub::new();
        let report = recover(&store, Utc::now()).unwrap();
        assert!(report.is_empty());
        assert_eq!(report.summary(), "nothing to recover");
        assert_eq!(report, RecoveryReport::default());
    }
}
