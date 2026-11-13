//! The in-memory double of the SQLite store.
//!
//! Orchestration tests run against this at a fraction of the cost of a file
//! store, which only works if it is *indistinguishable* through the port —
//! so every rule in `sqlite.rs` is mirrored here on purpose:
//!
//! * inserts conflict on duplicate ids with the same [`duplicate_id`]
//!   message, updates of missing rows are `NotFound`, deletes are idempotent;
//! * entry upserts enforce the revision guard and one entry per
//!   `(conversation, seq)`;
//! * anything SQLite orders by a stored timestamp column is ordered here by
//!   the same canonical string ([`crate::ts`]), so sub-microsecond ties fall
//!   to the id tie-break in both stores identically;
//! * `drop_deliveries` marks dropped (never deletes) and counts what it
//!   marked.
//!
//! Correctness over speed: this is the double the contract suite pins, and
//! through it the double the whole orchestration test pyramid trusts.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use serde_json::Value;

use swarmbot_contracts::bot::{Bot, normalize_name};
use swarmbot_contracts::config::Settings;
use swarmbot_contracts::conversation::{Conversation, ConversationKind};
use swarmbot_contracts::delivery::{Delivery, DeliveryState};
use swarmbot_contracts::entry::Entry;
use swarmbot_contracts::error::{DomainError, not_found};
use swarmbot_contracts::id::{
    BotId, ConversationId, DeliveryId, EntryId, NodeId, RequestId, RoutineId, RuleId, RunId,
    TaskId, ToolCallId, TurnId,
};
use swarmbot_contracts::live::{LiveCursor, LiveEvent};
use swarmbot_contracts::node::{NodeInfo, PairingRecord};
use swarmbot_contracts::repo::{
    BotRepo, BotSessionRecord, ConversationRepo, DeliveryRepo, EntryRepo, EventLog, NodeRepo,
    RequestRepo, RoutineRepo, RuleRepo, SessionRepo, SettingsStore, TaskRepo, ToolCallRecord,
    ToolCallRepo, TurnRepo,
};
use swarmbot_contracts::request::{ApprovalRule, RuntimeRequest};
use swarmbot_contracts::routine::{Routine, RoutineRun, RoutineStatus};
use swarmbot_contracts::task::{Task, TaskStep};
use swarmbot_contracts::turn::Turn;

use crate::{duplicate_bot_name, duplicate_id, seq_taken, stale_node_revision, stale_revision, ts};

/// The epoch a fresh log starts at — the same value the SQLite migration
/// seeds, so both doubles report one cursor space.
const EPOCH: u64 = 1;

/// One stored live event with the cursor `append_event` gave it.
#[derive(Clone)]
struct StoredEvent {
    cursor: LiveCursor,
    event: LiveEvent,
}

/// The in-memory [`Persistence`](swarmbot_contracts::repo::Persistence)
/// store: one mutex per collection, behaviour identical to [`crate::SqliteStore`].
#[derive(Default)]
pub struct MemoryStore {
    bots: Mutex<HashMap<BotId, Bot>>,
    conversations: Mutex<HashMap<ConversationId, Conversation>>,
    entries: Mutex<HashMap<EntryId, Entry>>,
    turns: Mutex<HashMap<TurnId, Turn>>,
    deliveries: Mutex<HashMap<DeliveryId, Delivery>>,
    tasks: Mutex<HashMap<TaskId, Task>>,
    /// Steps keyed by seq, so re-appending one step replaces it — the same
    /// idempotency the SQLite primary key gives.
    task_steps: Mutex<HashMap<TaskId, BTreeMap<u32, TaskStep>>>,
    requests: Mutex<HashMap<RequestId, RuntimeRequest>>,
    rules: Mutex<HashMap<RuleId, ApprovalRule>>,
    nodes: Mutex<HashMap<NodeId, NodeInfo>>,
    /// Ordered by node id like `ORDER BY node_id` on the SQL side.
    node_keys: Mutex<BTreeMap<NodeId, String>>,
    pairings: Mutex<HashMap<String, PairingRecord>>,
    sessions: Mutex<HashMap<BotId, BotSessionRecord>>,
    tool_calls: Mutex<HashMap<(TurnId, ToolCallId), ToolCallRecord>>,
    routines: Mutex<HashMap<RoutineId, Routine>>,
    routine_runs: Mutex<HashMap<RunId, RoutineRun>>,
    /// Append order *is* `(epoch, sequence)` order, which the truncate and
    /// since implementations rely on.
    events: Mutex<Vec<StoredEvent>>,
    settings: Mutex<Option<Settings>>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

/// A poisoned mutex means something already panicked while holding store
/// state; like the SQL double, this store reports that instead of panicking
/// again.
fn guard<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, DomainError> {
    mutex
        .lock()
        .map_err(|_| DomainError::internal("memory store lock poisoned"))
}

impl BotRepo for MemoryStore {
    fn insert_bot(&self, bot: &Bot) -> Result<(), DomainError> {
        let mut bots = guard(&self.bots)?;
        // Same order of checks as SQLite: the name guard first (a nice
        // message beats a constraint violation), then the id.
        if bots
            .values()
            .any(|other| other.name == bot.name && other.id != bot.id)
        {
            return Err(duplicate_bot_name(&bot.name));
        }
        if bots.contains_key(&bot.id) {
            return Err(duplicate_id("bot", &bot.id));
        }
        bots.insert(bot.id.clone(), bot.clone());
        Ok(())
    }

    fn update_bot(&self, bot: &Bot) -> Result<(), DomainError> {
        let mut bots = guard(&self.bots)?;
        if bots
            .values()
            .any(|other| other.name == bot.name && other.id != bot.id)
        {
            return Err(duplicate_bot_name(&bot.name));
        }
        if !bots.contains_key(&bot.id) {
            return Err(not_found::bot(&bot.id));
        }
        bots.insert(bot.id.clone(), bot.clone());
        Ok(())
    }

    fn get_bot(&self, id: &BotId) -> Result<Option<Bot>, DomainError> {
        Ok(guard(&self.bots)?.get(id).cloned())
    }

    fn bot_by_name(&self, name: &str) -> Result<Option<Bot>, DomainError> {
        // Mentions arrive free-form; the stored handle is normalized.
        let normalized = normalize_name(name);
        Ok(guard(&self.bots)?
            .values()
            .find(|bot| bot.name == normalized)
            .cloned())
    }

    fn list_bots(&self) -> Result<Vec<Bot>, DomainError> {
        let mut bots: Vec<Bot> = guard(&self.bots)?.values().cloned().collect();
        bots.sort_by(|a, b| {
            ts(&a.created_at)
                .cmp(&ts(&b.created_at))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        Ok(bots)
    }

    fn delete_bot(&self, id: &BotId) -> Result<(), DomainError> {
        guard(&self.bots)?.remove(id);
        Ok(())
    }
}

impl ConversationRepo for MemoryStore {
    fn insert_conversation(&self, conversation: &Conversation) -> Result<(), DomainError> {
        let mut conversations = guard(&self.conversations)?;
        if conversations.contains_key(&conversation.id) {
            return Err(duplicate_id("conversation", &conversation.id));
        }
        conversations.insert(conversation.id.clone(), conversation.clone());
        Ok(())
    }

    fn update_conversation(&self, conversation: &Conversation) -> Result<(), DomainError> {
        let mut conversations = guard(&self.conversations)?;
        if !conversations.contains_key(&conversation.id) {
            return Err(not_found::conversation(&conversation.id));
        }
        conversations.insert(conversation.id.clone(), conversation.clone());
        Ok(())
    }

    fn get_conversation(&self, id: &ConversationId) -> Result<Option<Conversation>, DomainError> {
        Ok(guard(&self.conversations)?.get(id).cloned())
    }

    fn direct_conversation(&self, bot: &BotId) -> Result<Option<Conversation>, DomainError> {
        let mut matches: Vec<Conversation> = guard(&self.conversations)?
            .values()
            .filter(|conversation| {
                matches!(
                    conversation.kind,
                    ConversationKind::Direct { ref bot_id } if bot_id == bot
                )
            })
            .cloned()
            .collect();
        matches.sort_by(|a, b| {
            ts(&a.created_at)
                .cmp(&ts(&b.created_at))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        Ok(matches.into_iter().next())
    }

    fn list_conversations(&self) -> Result<Vec<Conversation>, DomainError> {
        let mut conversations: Vec<Conversation> =
            guard(&self.conversations)?.values().cloned().collect();
        conversations.sort_by(|a, b| {
            ts(&a.created_at)
                .cmp(&ts(&b.created_at))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        Ok(conversations)
    }

    fn delete_conversation(&self, id: &ConversationId) -> Result<(), DomainError> {
        guard(&self.conversations)?.remove(id);
        Ok(())
    }
}

impl EntryRepo for MemoryStore {
    fn upsert_entry(&self, entry: &Entry) -> Result<(), DomainError> {
        let mut entries = guard(&self.entries)?;
        // The whole-entry-upsert invariant: only a strictly newer revision
        // replaces a stored entry.
        if let Some(stored) = entries.get(&entry.id)
            && stored.revision >= entry.revision
        {
            return Err(stale_revision(entry, stored.revision));
        }
        if entries.values().any(|other| {
            other.id != entry.id
                && other.conversation_id == entry.conversation_id
                && other.seq == entry.seq
        }) {
            return Err(seq_taken(&entry.conversation_id, entry.seq, &entry.id));
        }
        entries.insert(entry.id.clone(), entry.clone());
        Ok(())
    }

    fn get_entry(&self, id: &EntryId) -> Result<Option<Entry>, DomainError> {
        Ok(guard(&self.entries)?.get(id).cloned())
    }

    fn entries_after(
        &self,
        conversation: &ConversationId,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<Entry>, DomainError> {
        let mut entries: Vec<Entry> = guard(&self.entries)?
            .values()
            .filter(|entry| entry.conversation_id == *conversation && entry.seq > after_seq)
            .cloned()
            .collect();
        entries.sort_by_key(|entry| entry.seq);
        entries.truncate(limit as usize);
        Ok(entries)
    }

    fn entry_tail(
        &self,
        conversation: &ConversationId,
        limit: u32,
    ) -> Result<Vec<Entry>, DomainError> {
        let mut entries: Vec<Entry> = guard(&self.entries)?
            .values()
            .filter(|entry| entry.conversation_id == *conversation)
            .cloned()
            .collect();
        entries.sort_by_key(|entry| entry.seq);
        let keep = limit as usize;
        if entries.len() > keep {
            entries = entries.split_off(entries.len() - keep);
        }
        Ok(entries)
    }

    fn delete_entries(&self, conversation: &ConversationId) -> Result<u64, DomainError> {
        let mut entries = guard(&self.entries)?;
        let before = entries.len();
        entries.retain(|_, entry| entry.conversation_id != *conversation);
        Ok((before - entries.len()) as u64)
    }
}

impl TurnRepo for MemoryStore {
    fn insert_turn(&self, turn: &Turn) -> Result<(), DomainError> {
        let mut turns = guard(&self.turns)?;
        if turns.contains_key(&turn.id) {
            return Err(duplicate_id("turn", &turn.id));
        }
        turns.insert(turn.id.clone(), turn.clone());
        Ok(())
    }

    fn update_turn(&self, turn: &Turn) -> Result<(), DomainError> {
        let mut turns = guard(&self.turns)?;
        if !turns.contains_key(&turn.id) {
            return Err(not_found::turn(&turn.id));
        }
        turns.insert(turn.id.clone(), turn.clone());
        Ok(())
    }

    fn get_turn(&self, id: &TurnId) -> Result<Option<Turn>, DomainError> {
        Ok(guard(&self.turns)?.get(id).cloned())
    }

    fn turns_in(
        &self,
        conversation: &ConversationId,
        limit: u32,
    ) -> Result<Vec<Turn>, DomainError> {
        let mut turns: Vec<Turn> = guard(&self.turns)?
            .values()
            .filter(|turn| turn.conversation_id == *conversation)
            .cloned()
            .collect();
        turns.sort_by(|a, b| {
            ts(&a.started_at)
                .cmp(&ts(&b.started_at))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        // The newest window, ascending — matching the SQL subquery.
        let keep = limit as usize;
        if turns.len() > keep {
            turns = turns.split_off(turns.len() - keep);
        }
        Ok(turns)
    }

    fn running_turns(&self) -> Result<Vec<Turn>, DomainError> {
        let mut turns: Vec<Turn> = guard(&self.turns)?
            .values()
            .filter(|turn| turn.is_running())
            .cloned()
            .collect();
        turns.sort_by(|a, b| {
            ts(&a.started_at)
                .cmp(&ts(&b.started_at))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        Ok(turns)
    }

    fn turns_since(&self, since: DateTime<Utc>) -> Result<Vec<Turn>, DomainError> {
        // Compared as the canonical stored string, not as a `DateTime`: SQLite
        // compares the TEXT column, so this is how sub-microsecond differences
        // collapse the *same way* in both stores at the window boundary.
        let since = ts(&since);
        let mut turns: Vec<Turn> = guard(&self.turns)?
            .values()
            .filter(|turn| ts(&turn.started_at).as_str() >= since.as_str())
            .cloned()
            .collect();
        turns.sort_by(|a, b| {
            ts(&a.started_at)
                .cmp(&ts(&b.started_at))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        Ok(turns)
    }
}

impl DeliveryRepo for MemoryStore {
    fn insert_delivery(&self, delivery: &Delivery) -> Result<(), DomainError> {
        let mut deliveries = guard(&self.deliveries)?;
        if deliveries.contains_key(&delivery.id) {
            return Err(duplicate_id("delivery", &delivery.id));
        }
        deliveries.insert(delivery.id.clone(), delivery.clone());
        Ok(())
    }

    fn update_delivery(&self, delivery: &Delivery) -> Result<(), DomainError> {
        let mut deliveries = guard(&self.deliveries)?;
        if !deliveries.contains_key(&delivery.id) {
            return Err(DomainError::not_found("delivery", &delivery.id));
        }
        deliveries.insert(delivery.id.clone(), delivery.clone());
        Ok(())
    }

    fn get_delivery(&self, id: &DeliveryId) -> Result<Option<Delivery>, DomainError> {
        Ok(guard(&self.deliveries)?.get(id).cloned())
    }

    fn queued_deliveries(&self, bot: &BotId) -> Result<Vec<Delivery>, DomainError> {
        let mut queued: Vec<Delivery> = guard(&self.deliveries)?
            .values()
            .filter(|delivery| delivery.bot_id == *bot && delivery.state == DeliveryState::Queued)
            .cloned()
            .collect();
        // FIFO: created_at then id, exactly like the SQL ORDER BY.
        queued.sort_by(|a, b| {
            ts(&a.created_at)
                .cmp(&ts(&b.created_at))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        Ok(queued)
    }

    fn drop_deliveries(
        &self,
        conversation: &ConversationId,
        reason: &str,
    ) -> Result<u64, DomainError> {
        let now = Utc::now();
        let mut count = 0u64;
        // Marked dropped, not deleted — same observable behaviour as SQLite.
        for delivery in guard(&self.deliveries)?.values_mut() {
            if delivery.conversation_id == *conversation && delivery.state == DeliveryState::Queued
            {
                delivery.mark_dropped(now);
                count += 1;
            }
        }
        tracing::debug!(
            conversation = %conversation,
            count,
            reason,
            "dropped queued deliveries"
        );
        Ok(count)
    }
}

impl TaskRepo for MemoryStore {
    fn insert_task(&self, task: &Task) -> Result<(), DomainError> {
        let mut tasks = guard(&self.tasks)?;
        if tasks.contains_key(&task.id) {
            return Err(duplicate_id("task", &task.id));
        }
        tasks.insert(task.id.clone(), task.clone());
        Ok(())
    }

    fn update_task(&self, task: &Task) -> Result<(), DomainError> {
        let mut tasks = guard(&self.tasks)?;
        if !tasks.contains_key(&task.id) {
            return Err(not_found::task(&task.id));
        }
        tasks.insert(task.id.clone(), task.clone());
        Ok(())
    }

    fn get_task(&self, id: &TaskId) -> Result<Option<Task>, DomainError> {
        Ok(guard(&self.tasks)?.get(id).cloned())
    }

    fn tasks_of(&self, owner: &BotId) -> Result<Vec<Task>, DomainError> {
        let mut tasks: Vec<Task> = guard(&self.tasks)?
            .values()
            .filter(|task| task.owner == *owner)
            .cloned()
            .collect();
        sort_tasks(&mut tasks);
        Ok(tasks)
    }

    fn tasks_in(&self, origin: &ConversationId) -> Result<Vec<Task>, DomainError> {
        let mut tasks: Vec<Task> = guard(&self.tasks)?
            .values()
            .filter(|task| task.origin == *origin)
            .cloned()
            .collect();
        sort_tasks(&mut tasks);
        Ok(tasks)
    }

    fn queued_tasks(&self) -> Result<Vec<Task>, DomainError> {
        let mut tasks: Vec<Task> = guard(&self.tasks)?
            .values()
            .filter(|task| task.status == swarmbot_contracts::task::TaskStatus::Queued)
            .cloned()
            .collect();
        sort_tasks(&mut tasks);
        Ok(tasks)
    }

    fn running_tasks(&self) -> Result<Vec<Task>, DomainError> {
        let mut tasks: Vec<Task> = guard(&self.tasks)?
            .values()
            .filter(|task| task.status == swarmbot_contracts::task::TaskStatus::Running)
            .cloned()
            .collect();
        sort_tasks(&mut tasks);
        Ok(tasks)
    }

    fn running_task_count(&self, owner: &BotId) -> Result<u32, DomainError> {
        let count = guard(&self.tasks)?
            .values()
            .filter(|task| {
                task.owner == *owner && task.status == swarmbot_contracts::task::TaskStatus::Running
            })
            .count();
        Ok(count as u32)
    }

    fn append_task_step(&self, step: &TaskStep) -> Result<(), DomainError> {
        // A step for a task that does not exist is a bug upstream; the SQL
        // side reports NotFound (and its FK would reject the row anyway).
        if !guard(&self.tasks)?.contains_key(&step.task_id) {
            return Err(not_found::task(&step.task_id));
        }
        guard(&self.task_steps)?
            .entry(step.task_id.clone())
            .or_default()
            .insert(step.seq, step.clone());
        Ok(())
    }

    fn task_steps(&self, task: &TaskId) -> Result<Vec<TaskStep>, DomainError> {
        // BTreeMap keeps seq order, matching `ORDER BY seq`.
        Ok(guard(&self.task_steps)?
            .get(task)
            .map(|steps| steps.values().cloned().collect())
            .unwrap_or_default())
    }
}

/// The task ordering every list uses: creation order, id as the tie-break.
fn sort_tasks(tasks: &mut [Task]) {
    tasks.sort_by(|a, b| {
        ts(&a.created_at)
            .cmp(&ts(&b.created_at))
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
}

impl RequestRepo for MemoryStore {
    fn insert_request(&self, request: &RuntimeRequest) -> Result<(), DomainError> {
        let mut requests = guard(&self.requests)?;
        if requests.contains_key(&request.id) {
            return Err(duplicate_id("request", &request.id));
        }
        requests.insert(request.id.clone(), request.clone());
        Ok(())
    }

    fn update_request(&self, request: &RuntimeRequest) -> Result<(), DomainError> {
        let mut requests = guard(&self.requests)?;
        if !requests.contains_key(&request.id) {
            return Err(not_found::request(&request.id));
        }
        requests.insert(request.id.clone(), request.clone());
        Ok(())
    }

    fn get_request(&self, id: &RequestId) -> Result<Option<RuntimeRequest>, DomainError> {
        Ok(guard(&self.requests)?.get(id).cloned())
    }

    fn pending_requests(&self) -> Result<Vec<RuntimeRequest>, DomainError> {
        let mut requests: Vec<RuntimeRequest> = guard(&self.requests)?
            .values()
            .filter(|request| request.is_pending())
            .cloned()
            .collect();
        sort_requests(&mut requests);
        Ok(requests)
    }

    fn requests_in(
        &self,
        conversation: &ConversationId,
    ) -> Result<Vec<RuntimeRequest>, DomainError> {
        let mut requests: Vec<RuntimeRequest> = guard(&self.requests)?
            .values()
            .filter(|request| request.conversation_id == *conversation)
            .cloned()
            .collect();
        sort_requests(&mut requests);
        Ok(requests)
    }

    fn cancel_requests_of_turn(&self, turn: &TurnId, reason: &str) -> Result<u64, DomainError> {
        let now = Utc::now();
        let mut count = 0u64;
        for request in guard(&self.requests)?.values_mut() {
            // Only pending requests of *that* turn; a settled request is
            // history and stays exactly as it was.
            if request.turn_id.as_ref() == Some(turn) && request.cancel(now, reason) {
                count += 1;
            }
        }
        Ok(count)
    }
}

fn sort_requests(requests: &mut [RuntimeRequest]) {
    requests.sort_by(|a, b| {
        ts(&a.created_at)
            .cmp(&ts(&b.created_at))
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
}

impl RuleRepo for MemoryStore {
    fn insert_rule(&self, rule: &ApprovalRule) -> Result<(), DomainError> {
        let mut rules = guard(&self.rules)?;
        if rules.contains_key(&rule.id) {
            return Err(duplicate_id("rule", &rule.id));
        }
        rules.insert(rule.id.clone(), rule.clone());
        Ok(())
    }

    fn list_rules(&self) -> Result<Vec<ApprovalRule>, DomainError> {
        let mut rules: Vec<ApprovalRule> = guard(&self.rules)?.values().cloned().collect();
        rules.sort_by(|a, b| {
            ts(&a.created_at)
                .cmp(&ts(&b.created_at))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        Ok(rules)
    }

    fn delete_rule(&self, id: &RuleId) -> Result<(), DomainError> {
        guard(&self.rules)?.remove(id);
        Ok(())
    }
}

impl NodeRepo for MemoryStore {
    fn insert_node(&self, node: &NodeInfo) -> Result<(), DomainError> {
        let mut nodes = guard(&self.nodes)?;
        if nodes.contains_key(&node.id) {
            return Err(duplicate_id("node", &node.id));
        }
        nodes.insert(node.id.clone(), node.clone());
        Ok(())
    }

    fn update_node(&self, node: &NodeInfo) -> Result<(), DomainError> {
        let mut nodes = guard(&self.nodes)?;
        let Some(existing) = nodes.get(&node.id) else {
            return Err(not_found::node(&node.id));
        };
        // The same guard `SqliteStore` applies, with the same message: the two
        // stores must stay observably identical, and a roster writer that races
        // is a real interleaving in both of them — the in-memory store is what
        // every orchestration test runs on, so if it accepted a stale write the
        // tests would pass while production lost updates.
        if existing.revision >= node.revision {
            return Err(stale_node_revision(node, existing.revision));
        }
        nodes.insert(node.id.clone(), node.clone());
        Ok(())
    }

    fn get_node(&self, id: &NodeId) -> Result<Option<NodeInfo>, DomainError> {
        Ok(guard(&self.nodes)?.get(id).cloned())
    }

    fn list_nodes(&self) -> Result<Vec<NodeInfo>, DomainError> {
        let mut nodes: Vec<NodeInfo> = guard(&self.nodes)?.values().cloned().collect();
        nodes.sort_by(|a, b| {
            ts(&a.paired_at)
                .cmp(&ts(&b.paired_at))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        Ok(nodes)
    }

    fn insert_pairing(&self, record: &PairingRecord) -> Result<(), DomainError> {
        let mut pairings = guard(&self.pairings)?;
        if pairings.contains_key(&record.code_hash) {
            return Err(duplicate_id("pairing", &record.code_hash));
        }
        pairings.insert(record.code_hash.clone(), record.clone());
        Ok(())
    }

    fn get_pairing(&self, code_hash: &str) -> Result<Option<PairingRecord>, DomainError> {
        Ok(guard(&self.pairings)?.get(code_hash).cloned())
    }

    fn update_pairing(&self, record: &PairingRecord) -> Result<(), DomainError> {
        let mut pairings = guard(&self.pairings)?;
        if !pairings.contains_key(&record.code_hash) {
            return Err(DomainError::not_found("pairing", &record.code_hash));
        }
        pairings.insert(record.code_hash.clone(), record.clone());
        Ok(())
    }

    fn expired_pairings(
        &self,
        now: chrono::DateTime<Utc>,
    ) -> Result<Vec<PairingRecord>, DomainError> {
        let now = ts(&now);
        let mut expired: Vec<PairingRecord> = guard(&self.pairings)?
            .values()
            // Only unused codes expire; comparing canonical strings is what
            // the SQL side does on its column.
            .filter(|record| !record.used && ts(&record.expires_at) <= now)
            .cloned()
            .collect();
        expired.sort_by(|a, b| {
            ts(&a.created_at)
                .cmp(&ts(&b.created_at))
                .then_with(|| a.code_hash.cmp(&b.code_hash))
        });
        Ok(expired)
    }

    fn put_node_key(&self, node: &NodeId, public_key_base64url: &str) -> Result<(), DomainError> {
        guard(&self.node_keys)?.insert(node.clone(), public_key_base64url.to_string());
        Ok(())
    }

    fn node_key(&self, node: &NodeId) -> Result<Option<String>, DomainError> {
        Ok(guard(&self.node_keys)?.get(node).cloned())
    }

    fn list_node_keys(&self) -> Result<Vec<(NodeId, String)>, DomainError> {
        // BTreeMap iterates by node id — the `ORDER BY node_id` of the SQL
        // side, byte-wise in both.
        Ok(guard(&self.node_keys)?
            .iter()
            .map(|(node, key)| (node.clone(), key.clone()))
            .collect())
    }
}

impl SessionRepo for MemoryStore {
    fn get_session(&self, bot: &BotId) -> Result<Option<BotSessionRecord>, DomainError> {
        Ok(guard(&self.sessions)?.get(bot).cloned())
    }

    fn put_session(&self, record: &BotSessionRecord) -> Result<(), DomainError> {
        guard(&self.sessions)?.insert(record.bot_id.clone(), record.clone());
        Ok(())
    }

    fn delete_session(&self, bot: &BotId) -> Result<(), DomainError> {
        guard(&self.sessions)?.remove(bot);
        Ok(())
    }

    fn reset_session(&self, bot: &BotId) -> Result<(), DomainError> {
        // "New session": the backend forgets, but the `seen` cursors stay —
        // a reset must never make a group re-show old messages.
        if let Some(record) = guard(&self.sessions)?.get_mut(bot) {
            record.native_session_id = None;
            record.history = Value::Array(Vec::new());
            record.token_estimate = 0;
            record.updated_at = Utc::now();
        }
        Ok(())
    }
}

impl ToolCallRepo for MemoryStore {
    fn find_tool_call(
        &self,
        turn: &TurnId,
        call: &ToolCallId,
    ) -> Result<Option<ToolCallRecord>, DomainError> {
        Ok(guard(&self.tool_calls)?
            .get(&(turn.clone(), call.clone()))
            .cloned())
    }

    fn insert_tool_call(&self, record: &ToolCallRecord) -> Result<(), DomainError> {
        guard(&self.tool_calls)?.insert(
            (record.turn_id.clone(), record.call_id.clone()),
            record.clone(),
        );
        Ok(())
    }

    fn prune_tool_calls(&self, before: DateTime<Utc>) -> Result<u64, DomainError> {
        let mut calls = guard(&self.tool_calls)?;
        let stale: Vec<(TurnId, ToolCallId)> = calls
            .iter()
            .filter(|(_, record)| record.created_at < before)
            .map(|(key, _)| key.clone())
            .collect();
        let removed = stale.len() as u64;
        for key in stale {
            calls.remove(&key);
        }
        Ok(removed)
    }
}

impl EventLog for MemoryStore {
    fn epoch(&self) -> Result<u64, DomainError> {
        Ok(EPOCH)
    }

    fn current_cursor(&self) -> Result<LiveCursor, DomainError> {
        // Rows are in cursor order, so the tip is the last row.
        let tip = guard(&self.events)?
            .last()
            .map(|stored| stored.cursor.sequence)
            .unwrap_or(0);
        Ok(LiveCursor::new(EPOCH, tip))
    }

    fn append_event(&self, event: &LiveEvent) -> Result<LiveCursor, DomainError> {
        let mut events = guard(&self.events)?;
        // MAX(sequence)+1 within the epoch — the same arithmetic the SQL
        // side runs, including "an emptied log restarts at 1".
        let next = events
            .iter()
            .filter(|stored| stored.cursor.epoch == EPOCH)
            .map(|stored| stored.cursor.sequence)
            .max()
            .unwrap_or(0)
            + 1;
        let cursor = LiveCursor::new(EPOCH, next);
        events.push(StoredEvent {
            cursor,
            event: event.clone(),
        });
        Ok(cursor)
    }

    fn events_since(
        &self,
        cursor: LiveCursor,
        limit: u32,
    ) -> Result<Vec<(LiveCursor, LiveEvent)>, DomainError> {
        Ok(guard(&self.events)?
            .iter()
            // Strictly after the cursor, ascending — rows are already in
            // cursor order, so filtering preserves it.
            .filter(|stored| stored.cursor > cursor)
            .take(limit as usize)
            .map(|stored| (stored.cursor, stored.event.clone()))
            .collect())
    }

    fn truncate_events(&self, keep: u64) -> Result<u64, DomainError> {
        let mut events = guard(&self.events)?;
        // "Keep the newest" on an ascending list is "drop from the front".
        let remove = (events.len() as u64).saturating_sub(keep);
        events.drain(..remove as usize);
        Ok(remove)
    }
}

impl SettingsStore for MemoryStore {
    fn settings(&self) -> Result<Settings, DomainError> {
        // Nothing stored yet means the defaults — the same answer the SQL
        // side gives on an empty table.
        Ok(guard(&self.settings)?.clone().unwrap_or_default())
    }

    fn put_settings(&self, settings: &Settings) -> Result<(), DomainError> {
        *guard(&self.settings)? = Some(settings.clone());
        Ok(())
    }
}

fn sort_routines(routines: &mut [Routine]) {
    routines.sort_by(|a, b| {
        ts(&a.created_at)
            .cmp(&ts(&b.created_at))
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
}

fn sort_due_routines(routines: &mut [Routine]) {
    routines.sort_by(|a, b| {
        let a_next = a.next_run_at.as_ref().map(ts);
        let b_next = b.next_run_at.as_ref().map(ts);
        a_next
            .cmp(&b_next)
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
}

fn sort_runs(runs: &mut [RoutineRun]) {
    runs.sort_by(|a, b| {
        ts(&b.started_at)
            .cmp(&ts(&a.started_at))
            .then_with(|| b.id.as_str().cmp(a.id.as_str()))
    });
}

impl RoutineRepo for MemoryStore {
    fn insert_routine(&self, routine: &Routine) -> Result<(), DomainError> {
        let mut routines = guard(&self.routines)?;
        if routines.contains_key(&routine.id) {
            return Err(duplicate_id("routine", &routine.id));
        }
        routines.insert(routine.id.clone(), routine.clone());
        Ok(())
    }

    fn update_routine(&self, routine: &Routine) -> Result<(), DomainError> {
        let mut routines = guard(&self.routines)?;
        if !routines.contains_key(&routine.id) {
            return Err(not_found::routine(&routine.id));
        }
        routines.insert(routine.id.clone(), routine.clone());
        Ok(())
    }

    fn get_routine(&self, id: &RoutineId) -> Result<Option<Routine>, DomainError> {
        Ok(guard(&self.routines)?.get(id).cloned())
    }

    fn list_routines(&self) -> Result<Vec<Routine>, DomainError> {
        let mut routines: Vec<Routine> = guard(&self.routines)?.values().cloned().collect();
        sort_routines(&mut routines);
        Ok(routines)
    }

    fn list_routines_for_bot(&self, bot: &BotId) -> Result<Vec<Routine>, DomainError> {
        let mut routines: Vec<Routine> = guard(&self.routines)?
            .values()
            .filter(|r| r.bot_id == *bot)
            .cloned()
            .collect();
        sort_routines(&mut routines);
        Ok(routines)
    }

    fn due_routines(&self, now: DateTime<Utc>) -> Result<Vec<Routine>, DomainError> {
        let mut routines: Vec<Routine> = guard(&self.routines)?
            .values()
            .filter(|r| {
                r.status == RoutineStatus::Active && r.next_run_at.is_some_and(|next| next <= now)
            })
            .cloned()
            .collect();
        sort_due_routines(&mut routines);
        Ok(routines)
    }

    fn delete_routine(&self, id: &RoutineId) -> Result<(), DomainError> {
        let mut routines = guard(&self.routines)?;
        if routines.remove(id).is_none() {
            return Err(not_found::routine(id));
        }
        // Cascade delete runs
        guard(&self.routine_runs)?.retain(|_, run| run.routine_id != *id);
        Ok(())
    }

    fn insert_run(&self, run: &RoutineRun) -> Result<(), DomainError> {
        let mut runs = guard(&self.routine_runs)?;
        if runs.contains_key(&run.id) {
            return Err(duplicate_id("routine_run", &run.id));
        }
        runs.insert(run.id.clone(), run.clone());
        Ok(())
    }

    fn update_run(&self, run: &RoutineRun) -> Result<(), DomainError> {
        let mut runs = guard(&self.routine_runs)?;
        if !runs.contains_key(&run.id) {
            return Err(not_found::routine_run(&run.id));
        }
        runs.insert(run.id.clone(), run.clone());
        Ok(())
    }

    fn get_run(&self, id: &RunId) -> Result<Option<RoutineRun>, DomainError> {
        Ok(guard(&self.routine_runs)?.get(id).cloned())
    }

    fn list_runs(
        &self,
        routine_id: &RoutineId,
        limit: u32,
    ) -> Result<Vec<RoutineRun>, DomainError> {
        let mut runs: Vec<RoutineRun> = guard(&self.routine_runs)?
            .values()
            .filter(|run| run.routine_id == *routine_id)
            .cloned()
            .collect();
        sort_runs(&mut runs);
        runs.truncate(limit as usize);
        Ok(runs)
    }
}
