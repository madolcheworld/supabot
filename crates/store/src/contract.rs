//! The shared contract suite: one body of assertions, run against every
//! [`Persistence`] implementation.
//!
//! Why this exists: the orchestration is written against the port and tested
//! against [`MemoryStore`](crate::MemoryStore), while production runs on
//! [`SqliteStore`](crate::SqliteStore) — the whole scheme only holds if no
//! caller can tell the two apart. This module pins the observable behaviour
//! (ordering, conflicts, counts, recovery) once, and both implementations
//! must pass it unchanged.
//!
//! The suite lives in the library (not under `#[cfg(test)]`) so any crate
//! can run it against its own double, and it therefore never panics: every
//! check returns through [`anyhow::Result`], and callers (the tests in
//! `tests/`) decide what a failure means.
//!
//! Each area is also exposed on its own (`contract_bots`, `contract_entries`,
//! …) so a focused test can run just the part it cares about.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use chrono::{Duration, Utc};
use serde_json::json;
use uuid::Uuid;

use swarmbot_contracts::id::{
    BotId, ConversationId, DeliveryId, RequestId, RoutineId, TaskId, TurnId,
};
use swarmbot_contracts::repo::{BotSessionRecord, Persistence, ToolCallRecord, recover};
use swarmbot_contracts::{
    ApprovalRule, Bot, CausalChain, Conversation, Decision, Delivery, DeliveryState, DomainError,
    Entry, EntryId, EntryKind, LiveCursor, LiveEvent, MessageEntry, NodeId, NodeInfo, NodeKind,
    NodeRoot, PairingRecord, PolicyCeiling, PolicyMap, PostState, RequestStatus, Responder,
    Routine, RoutineRun, RoutineSchedule, RoutineStatus, RuleEffect, RuleId, RuleScope, RunStatus,
    RuntimeRequest, Settings, StepKind, Surface, Task, TaskStatus, TaskStep, ToolCallId, Turn,
    TurnInitiator, TurnStatus, Urgency, Usage, normalize_name,
};

use crate::JsonlMirror;

/// Fails the suite with a readable message (the panic-free stand-in for
/// `assert!` — this module ships in the library, where panicking is banned).
macro_rules! ensure_that {
    ($cond:expr, $($msg:tt)*) => {
        if !($cond) {
            anyhow::bail!($($msg)*);
        }
    };
}

/// Fails the suite showing both sides (the panic-free `assert_eq!`). The
/// `match` scrutinee keeps temporaries (a `?`-chained call) alive for the
/// comparison without moving either side, exactly like `assert_eq!` does.
macro_rules! ensure_eq {
    ($left:expr, $right:expr, $($msg:tt)*) => {
        match (&$left, &$right) {
            (left, right) => {
                if left != right {
                    anyhow::bail!(
                        "{} — left: {:?}, right: {:?}",
                        format_args!($($msg)*),
                        left,
                        right
                    );
                }
            }
        }
    };
}

/// Run the whole contract suite against a fresh store per area.
///
/// `make` builds a brand-new, empty store; areas that need several stores
/// (recovery, the mirror rebuild) call it more than once.
pub fn run_contract_suite<S: Persistence>(mut make: impl FnMut() -> S) -> anyhow::Result<()> {
    contract_bots(&make()).context("bots")?;
    contract_conversations(&make()).context("conversations")?;
    contract_entries(&make()).context("entries")?;
    contract_turns(&make()).context("turns")?;
    contract_deliveries(&make()).context("deliveries")?;
    contract_tasks(&make()).context("tasks")?;
    contract_requests(&make()).context("requests")?;
    contract_rules(&make()).context("rules")?;
    contract_nodes(&make()).context("nodes")?;
    contract_sessions(&make()).context("sessions")?;
    contract_tool_calls(&make()).context("tool calls")?;
    contract_event_log(&make()).context("event log")?;
    contract_settings(&make()).context("settings")?;
    contract_routines(&make()).context("routines")?;
    contract_recovery(&make()).context("recovery")?;
    contract_mirror(&mut make).context("jsonl mirror")?;
    Ok(())
}

/// A unique scratch directory that removes itself. The suite cannot depend on
/// `tempfile` (a dev-dependency, invisible to library code), so it rolls the
/// two lines it needs: a unique name under the system temp dir, cleanup on
/// drop, errors ignored because a leftover scratch dir harms nobody.
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    /// Create `<tmp>/swarmbot-store-<tag>-<pid>-<uuid>`.
    pub fn new(tag: &str) -> anyhow::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "swarmbot-store-{tag}-{}-{}",
            std::process::id(),
            Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// --- bots -------------------------------------------------------------------

pub fn contract_bots<S: Persistence>(store: &S) -> anyhow::Result<()> {
    let base = Utc::now();
    let mut lead = Bot::new("lead", "Lead", "you lead", "fake", ConversationId::new());
    lead.created_at = base;
    let mut scout = Bot::new(
        "@Scout",
        "Scout",
        "you explore",
        "fake",
        ConversationId::new(),
    );
    // Explicitly apart: the suite pins list order, and "created a microsecond
    // apart" must not depend on the clock's mood.
    scout.created_at = base + Duration::seconds(1);

    store.insert_bot(&lead)?;
    store.insert_bot(&scout)?;

    ensure_eq!(
        store.get_bot(&lead.id)?,
        Some(lead.clone()),
        "a bot round-trips through insert/get"
    );
    ensure_that!(
        store.get_bot(&BotId::new())?.is_none(),
        "a missing id reads as Ok(None), never an error"
    );

    // `Bot::new` normalizes; lookups normalize too, so any spelling finds it.
    ensure_eq!(
        store.bot_by_name("@Lead")?.as_ref().map(|bot| &bot.id),
        Some(&lead.id),
        "bot_by_name matches a mention spelling"
    );
    ensure_eq!(
        store.bot_by_name("  LEAD  ")?.as_ref().map(|bot| &bot.id),
        Some(&lead.id),
        "bot_by_name trims and lowercases"
    );
    ensure_eq!(
        normalize_name("@Scout"),
        scout.name,
        "the stored handle is the normalized one"
    );
    ensure_that!(
        store.bot_by_name("nobody")?.is_none(),
        "an unknown handle finds nobody"
    );

    // Uniqueness: one row per id, one bot per handle — `bot_by_name` must
    // never have to guess.
    ensure_that!(
        matches!(store.insert_bot(&lead), Err(DomainError::Conflict { .. })),
        "inserting the same bot id twice is a conflict"
    );
    let twin = Bot::new("lead", "Second Lead", "", "fake", ConversationId::new());
    ensure_that!(
        matches!(store.insert_bot(&twin), Err(DomainError::Conflict { .. })),
        "inserting a second bot with the same name is a conflict"
    );

    let mut renamed = lead.clone();
    renamed.label = "Leader".into();
    store.update_bot(&renamed)?;
    ensure_eq!(
        store.get_bot(&lead.id)?.map(|bot| bot.label),
        Some("Leader".to_string()),
        "update_bot persists"
    );
    let ghost = Bot::new("ghost", "Ghost", "", "fake", ConversationId::new());
    ensure_that!(
        matches!(store.update_bot(&ghost), Err(DomainError::NotFound { .. })),
        "updating a bot that never existed is NotFound"
    );

    let bots = store.list_bots()?;
    ensure_eq!(bots.len(), 2, "list_bots returns every bot");
    ensure_eq!(
        bots.iter().map(|bot| bot.name.clone()).collect::<Vec<_>>(),
        vec!["lead".to_string(), "scout".to_string()],
        "list_bots is ordered by creation"
    );

    store.delete_bot(&scout.id)?;
    ensure_that!(store.get_bot(&scout.id)?.is_none(), "a deleted bot is gone");
    ensure_eq!(store.list_bots()?.len(), 1, "the list shrinks");
    store.delete_bot(&scout.id)?; // idempotent: deleting twice is fine
    Ok(())
}

// --- conversations ----------------------------------------------------------

pub fn contract_conversations<S: Persistence>(store: &S) -> anyhow::Result<()> {
    let base = Utc::now();
    let bot = BotId::new();
    let other = BotId::new();
    let mut dm = Conversation::direct(bot.clone());
    dm.created_at = base;
    let mut group = Conversation::group("Team", vec![bot.clone(), other.clone()], bot.clone())
        .context("a two-bot group is valid")?;
    group.created_at = base + Duration::seconds(1);

    store.insert_conversation(&dm)?;
    store.insert_conversation(&group)?;

    ensure_eq!(
        store.get_conversation(&dm.id)?,
        Some(dm.clone()),
        "a DM round-trips"
    );
    let stored_group = store
        .get_conversation(&group.id)?
        .context("the group exists")?;
    ensure_eq!(stored_group, group, "members, lead and title round-trip");
    ensure_that!(stored_group.kind.is_group(), "the group keeps its kind");

    ensure_eq!(
        store.direct_conversation(&bot)?,
        Some(dm.clone()),
        "direct_conversation finds the bot's DM"
    );
    ensure_that!(
        store.direct_conversation(&BotId::new())?.is_none(),
        "an unknown bot has no DM"
    );

    // The sequence counter is the no-holes invariant: it must survive the
    // write, because every entry seq is handed out from it.
    let mut counted = store.get_conversation(&dm.id)?.context("the DM exists")?;
    ensure_eq!(counted.next_seq(), 1, "sequences start at 1");
    ensure_eq!(counted.next_seq(), 2, "sequences count up");
    store.update_conversation(&counted)?;
    ensure_eq!(
        store.get_conversation(&dm.id)?.map(|c| c.last_seq),
        Some(2),
        "next_seq counters persist through update_conversation"
    );

    ensure_that!(
        matches!(
            store.insert_conversation(&dm),
            Err(DomainError::Conflict { .. })
        ),
        "inserting the same conversation twice is a conflict"
    );
    ensure_that!(
        matches!(
            store.update_conversation(&Conversation::direct(BotId::new())),
            Err(DomainError::NotFound { .. })
        ),
        "updating an unknown conversation is NotFound"
    );

    let listed = store.list_conversations()?;
    ensure_eq!(
        listed.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
        vec![dm.id.clone(), group.id.clone()],
        "list_conversations is ordered by creation"
    );

    store.delete_conversation(&group.id)?;
    ensure_that!(
        store.get_conversation(&group.id)?.is_none(),
        "a deleted group is gone"
    );
    ensure_eq!(
        store.get_conversation(&dm.id)?,
        Some(counted.clone()),
        "deleting a group leaves other conversations alone"
    );
    store.delete_conversation(&group.id)?; // idempotent
    Ok(())
}

// --- entries ----------------------------------------------------------------

pub fn contract_entries<S: Persistence>(store: &S) -> anyhow::Result<()> {
    let conversation = ConversationId::new();
    let entries: Vec<Entry> = (1..=5)
        .map(|seq| Entry::owner_message(conversation.clone(), seq, format!("line {seq}")))
        .collect();
    for entry in &entries {
        store.upsert_entry(entry)?;
    }

    let seqs = |list: &[Entry]| list.iter().map(|entry| entry.seq).collect::<Vec<_>>();
    ensure_eq!(
        seqs(&store.entries_after(&conversation, 0, 10)?),
        vec![1, 2, 3, 4, 5],
        "entries come back ascending by seq"
    );
    ensure_eq!(
        seqs(&store.entries_after(&conversation, 2, 2)?),
        vec![3, 4],
        "entries_after honours the cursor and the limit"
    );
    ensure_eq!(
        seqs(&store.entry_tail(&conversation, 3)?),
        vec![3, 4, 5],
        "entry_tail returns the newest window, ascending"
    );
    ensure_that!(
        store.entry_tail(&conversation, 0)?.is_empty(),
        "a zero limit reads nothing"
    );
    ensure_that!(
        store
            .entries_after(&ConversationId::new(), 0, 10)?
            .is_empty(),
        "an unknown conversation is an empty transcript"
    );
    ensure_eq!(
        store.get_entry(&entries[0].id)?,
        Some(entries[0].clone()),
        "get_entry round-trips"
    );
    ensure_that!(
        store.get_entry(&EntryId::new())?.is_none(),
        "a missing entry reads as Ok(None)"
    );

    // The revision guard: whole-entry upserts, newest wins, stale writes are
    // conflicts — this is what lets a streaming message be edited in place.
    let mut edited = entries[0].clone();
    edited.kind = EntryKind::Message(Box::new(MessageEntry {
        text: "edited".into(),
        attachments: vec![],
        reply_to: None,
        post: PostState::Posted,
        addressed_to: None,
        turn_id: None,
    }));
    ensure_that!(
        matches!(
            store.upsert_entry(&edited),
            Err(DomainError::Conflict { .. })
        ),
        "an equal revision is a conflict"
    );
    let mut older = edited.clone();
    older.revision = 0;
    ensure_that!(
        matches!(
            store.upsert_entry(&older),
            Err(DomainError::Conflict { .. })
        ),
        "an older revision is a conflict"
    );
    edited.revision = 2;
    store.upsert_entry(&edited)?;
    let stored = store
        .get_entry(&entries[0].id)?
        .context("the entry still exists")?;
    ensure_eq!(stored.id, entries[0].id, "the upsert kept the same id");
    ensure_eq!(stored.revision, 2, "the newer revision replaced the row");
    ensure_eq!(
        stored.kind.text(),
        Some("edited"),
        "the new payload is stored"
    );
    ensure_eq!(
        store.entries_after(&conversation, 0, 10)?.len(),
        5,
        "revising an entry writes no second row"
    );

    // One entry per (conversation, seq): sequences are handed out once.
    let clash = Entry::owner_message(conversation.clone(), 3, "clash");
    ensure_that!(
        matches!(
            store.upsert_entry(&clash),
            Err(DomainError::Conflict { .. })
        ),
        "a second entry at the same seq is a conflict"
    );

    ensure_eq!(
        store.delete_entries(&conversation)?,
        5,
        "delete_entries counts what it deleted"
    );
    ensure_that!(
        store.entries_after(&conversation, 0, 10)?.is_empty(),
        "the transcript is empty afterwards"
    );
    ensure_that!(
        store.get_entry(&entries[0].id)?.is_none(),
        "the revised entry went with the rest"
    );
    ensure_eq!(
        store.delete_entries(&conversation)?,
        0,
        "deleting an empty transcript is a no-op"
    );
    Ok(())
}

// --- turns ------------------------------------------------------------------

pub fn contract_turns<S: Persistence>(store: &S) -> anyhow::Result<()> {
    let base = Utc::now();
    let bot = BotId::new();
    let conversation = ConversationId::new();

    let mut running = Turn::start(
        bot.clone(),
        conversation.clone(),
        TurnInitiator::Owner,
        None,
        None,
    );
    running.started_at = base;
    let mut done = Turn::start(
        bot.clone(),
        conversation.clone(),
        TurnInitiator::Owner,
        None,
        None,
    );
    done.started_at = base - Duration::seconds(5);
    done.complete(base - Duration::seconds(4));

    store.insert_turn(&running)?;
    store.insert_turn(&done)?;
    ensure_that!(
        matches!(
            store.insert_turn(&running),
            Err(DomainError::Conflict { .. })
        ),
        "inserting the same turn twice is a conflict"
    );

    let running_turns = store.running_turns()?;
    ensure_eq!(
        running_turns
            .iter()
            .map(|turn| turn.id.clone())
            .collect::<Vec<_>>(),
        vec![running.id.clone()],
        "running_turns returns only the running ones"
    );

    // An update has to persist every part the runner mutates mid-turn.
    running.usage.merge(Usage {
        input_tokens: 100,
        output_tokens: 20,
        llm_calls: 1,
        tool_calls: 2,
    });
    running.fail(base + Duration::seconds(10), "backend 402");
    store.update_turn(&running)?;
    let stored = store.get_turn(&running.id)?.context("the turn exists")?;
    ensure_eq!(stored.status, TurnStatus::Failed, "the status persisted");
    ensure_eq!(
        stored.error.as_deref(),
        Some("backend 402"),
        "the error persisted"
    );
    ensure_eq!(stored.usage.input_tokens, 100, "the usage persisted");
    ensure_eq!(stored.usage.tool_calls, 2, "all usage fields persisted");
    ensure_that!(
        store.running_turns()?.is_empty(),
        "a failed turn is no longer running"
    );

    let stranger = Turn::start(
        BotId::new(),
        conversation.clone(),
        TurnInitiator::System,
        None,
        None,
    );
    ensure_that!(
        matches!(
            store.update_turn(&stranger),
            Err(DomainError::NotFound { .. })
        ),
        "updating a turn that never existed is NotFound"
    );
    ensure_that!(
        store.get_turn(&TurnId::new())?.is_none(),
        "a missing turn reads as Ok(None)"
    );

    // turns_in is a context window: the newest `limit` turns, oldest first.
    let window_conversation = ConversationId::new();
    let mut started = Vec::new();
    for step in 0..5 {
        let mut turn = Turn::start(
            bot.clone(),
            window_conversation.clone(),
            TurnInitiator::Owner,
            None,
            None,
        );
        turn.started_at = base + Duration::seconds(step);
        turn.complete(base + Duration::seconds(step));
        store.insert_turn(&turn)?;
        started.push(turn.started_at);
    }
    let window = store.turns_in(&window_conversation, 3)?;
    ensure_eq!(
        window
            .iter()
            .map(|turn| turn.started_at)
            .collect::<Vec<_>>(),
        vec![started[2], started[3], started[4]],
        "turns_in returns the newest window in ascending order"
    );
    ensure_that!(
        store.turns_in(&window_conversation, 0)?.is_empty(),
        "a zero limit reads nothing"
    );
    Ok(())
}

// --- deliveries -------------------------------------------------------------

pub fn contract_deliveries<S: Persistence>(store: &S) -> anyhow::Result<()> {
    let base = Utc::now();
    let bot = BotId::new();
    let other_bot = BotId::new();
    let conv_a = ConversationId::new();
    let conv_b = ConversationId::new();

    let mut first = Delivery::owner_message(conv_a.clone(), bot.clone(), "one");
    first.created_at = base;
    let mut second = Delivery::bot_message(
        conv_b.clone(),
        other_bot.clone(),
        bot.clone(),
        "two",
        CausalChain::new(DeliveryId::new()),
        Urgency::Background,
    );
    second.created_at = base + Duration::seconds(1);
    let mut third = Delivery::owner_message(conv_a.clone(), bot.clone(), "three");
    third.created_at = base + Duration::seconds(2);
    let mut theirs = Delivery::owner_message(conv_b.clone(), other_bot.clone(), "not mine");
    theirs.created_at = base + Duration::seconds(3);

    for delivery in [&first, &second, &third, &theirs] {
        store.insert_delivery(delivery)?;
    }
    ensure_that!(
        matches!(
            store.insert_delivery(&first),
            Err(DomainError::Conflict { .. })
        ),
        "inserting the same delivery twice is a conflict"
    );

    // FIFO is the fairness rule — one queue per bot, across conversations.
    ensure_eq!(
        store
            .queued_deliveries(&bot)?
            .iter()
            .map(|delivery| delivery.id.clone())
            .collect::<Vec<_>>(),
        vec![first.id.clone(), second.id.clone(), third.id.clone()],
        "queued_deliveries is FIFO across conversations"
    );
    ensure_eq!(
        store
            .queued_deliveries(&other_bot)?
            .iter()
            .map(|delivery| delivery.id.clone())
            .collect::<Vec<_>>(),
        vec![theirs.id.clone()],
        "another bot has its own queue"
    );

    // Only `queued` deliveries are the queue; a settled one never comes back.
    let mut settled = second.clone();
    settled.mark_done(base + Duration::seconds(4));
    store.update_delivery(&settled)?;
    ensure_eq!(
        store
            .queued_deliveries(&bot)?
            .iter()
            .map(|delivery| delivery.id.clone())
            .collect::<Vec<_>>(),
        vec![first.id.clone(), third.id.clone()],
        "a done delivery leaves the queue"
    );
    ensure_eq!(
        store.get_delivery(&second.id)?.map(|d| d.state),
        Some(DeliveryState::Done),
        "update_delivery persisted the state"
    );

    let stranger = Delivery::owner_message(conv_a.clone(), bot.clone(), "ghost");
    ensure_that!(
        matches!(
            store.update_delivery(&stranger),
            Err(DomainError::NotFound { .. })
        ),
        "updating a delivery that never existed is NotFound"
    );
    ensure_that!(
        store.get_delivery(&DeliveryId::new())?.is_none(),
        "a missing delivery reads as Ok(None)"
    );

    // Dropping marks, never deletes: the row stays readable, the queue forgets.
    ensure_eq!(
        store.drop_deliveries(&conv_a, "group deleted")?,
        2,
        "drop_deliveries counts the queued deliveries of that conversation"
    );
    let dropped = store
        .get_delivery(&first.id)?
        .context("dropped deliveries are kept, not deleted")?;
    ensure_eq!(dropped.state, DeliveryState::Dropped, "marked dropped");
    ensure_that!(dropped.ended_at.is_some(), "dropping stamps the end time");
    ensure_that!(
        store.queued_deliveries(&bot)?.is_empty(),
        "the bot's queue is empty after the drop"
    );
    ensure_eq!(
        store
            .queued_deliveries(&other_bot)?
            .iter()
            .map(|delivery| delivery.id.clone())
            .collect::<Vec<_>>(),
        vec![theirs.id.clone()],
        "another conversation's queue is untouched"
    );
    ensure_eq!(
        store.drop_deliveries(&conv_a, "again")?,
        0,
        "a second drop finds nothing queued"
    );
    ensure_eq!(
        store.drop_deliveries(&conv_b, "cleanup")?,
        1,
        "the remaining queue drops as well"
    );
    Ok(())
}

// --- tasks ------------------------------------------------------------------

pub fn contract_tasks<S: Persistence>(store: &S) -> anyhow::Result<()> {
    let base = Utc::now();
    let owner = BotId::new();
    let origin = ConversationId::new();

    let mut queued = Task::own(owner.clone(), origin.clone(), "queued one", "q");
    queued.created_at = base;
    let mut running = Task::own(owner.clone(), origin.clone(), "running one", "r");
    running.created_at = base + Duration::seconds(1);
    running.start(base + Duration::seconds(2), Some(TurnId::new()));
    let mut finished = Task::own(owner.clone(), origin.clone(), "done one", "d");
    finished.created_at = base + Duration::seconds(3);
    finished.succeed(base + Duration::seconds(4), "all green");

    for task in [&queued, &running, &finished] {
        store.insert_task(task)?;
    }
    ensure_that!(
        matches!(
            store.insert_task(&queued),
            Err(DomainError::Conflict { .. })
        ),
        "inserting the same task twice is a conflict"
    );

    ensure_eq!(
        store
            .queued_tasks()?
            .iter()
            .map(|task| task.id.clone())
            .collect::<Vec<_>>(),
        vec![queued.id.clone()],
        "queued_tasks returns only the queued ones"
    );
    ensure_eq!(
        store
            .running_tasks()?
            .iter()
            .map(|task| task.id.clone())
            .collect::<Vec<_>>(),
        vec![running.id.clone()],
        "running_tasks returns only the running ones"
    );
    ensure_eq!(
        store.running_task_count(&owner)?,
        1,
        "the pump counts running tasks per owner"
    );
    ensure_eq!(
        store.running_task_count(&BotId::new())?,
        0,
        "an unknown owner runs nothing"
    );
    ensure_eq!(
        store.tasks_of(&owner)?.len(),
        3,
        "tasks_of lists the owner's tasks"
    );
    ensure_eq!(
        store.tasks_in(&origin)?.len(),
        3,
        "tasks_in lists the conversation's tasks"
    );

    // FIFO across bots: the pump picks the oldest queued task first.
    let mut earlier = Task::own(BotId::new(), origin.clone(), "earlier", "e");
    earlier.created_at = base - Duration::seconds(1);
    store.insert_task(&earlier)?;
    ensure_eq!(
        store
            .queued_tasks()?
            .iter()
            .map(|task| task.id.clone())
            .collect::<Vec<_>>(),
        vec![earlier.id.clone(), queued.id.clone()],
        "queued_tasks is FIFO by creation"
    );

    // An update persists the whole lifecycle move (status, result, cleared
    // turn binding) — recovery and the pump both read these back.
    let mut concluded = running.clone();
    concluded.succeed(base + Duration::seconds(9), "shipped");
    store.update_task(&concluded)?;
    let stored = store.get_task(&running.id)?.context("the task exists")?;
    ensure_eq!(stored.status, TaskStatus::Succeeded, "the status persisted");
    ensure_eq!(
        stored.result.as_deref(),
        Some("shipped"),
        "the result persisted"
    );
    ensure_that!(
        stored.turn_id.is_none(),
        "finishing clears the turn binding"
    );
    ensure_that!(
        store.running_tasks()?.is_empty(),
        "no task is running afterwards"
    );
    ensure_eq!(store.running_task_count(&owner)?, 0, "the count follows");

    let stranger = Task::own(BotId::new(), origin.clone(), "ghost", "g");
    ensure_that!(
        matches!(
            store.update_task(&stranger),
            Err(DomainError::NotFound { .. })
        ),
        "updating a task that never existed is NotFound"
    );
    ensure_that!(
        store.get_task(&TaskId::new())?.is_none(),
        "a missing task reads as Ok(None)"
    );

    // Steps append out of order and read back ordered — the task pane renders
    // them by seq, whatever order the runner produced them in.
    let steps = [
        TaskStep::new(queued.id.clone(), 2, StepKind::Message, "second"),
        TaskStep::new(queued.id.clone(), 1, StepKind::Reasoning, "first"),
        TaskStep::tool(queued.id.clone(), 3, "shell", "third"),
    ];
    for step in &steps {
        store.append_task_step(step)?;
    }
    let read_back = store.task_steps(&queued.id)?;
    ensure_eq!(
        read_back.iter().map(|step| step.seq).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "steps read back in seq order"
    );
    ensure_eq!(read_back[0].text, "first", "the step payloads survived");
    ensure_eq!(
        read_back[2].tool.as_deref(),
        Some("shell"),
        "tool steps keep their tool"
    );
    ensure_that!(
        store.task_steps(&TaskId::new())?.is_empty(),
        "an unknown task has no steps"
    );
    ensure_that!(
        matches!(
            store.append_task_step(&TaskStep::new(TaskId::new(), 1, StepKind::Notice, "orphan")),
            Err(DomainError::NotFound { .. })
        ),
        "a step for a missing task is NotFound, never an orphan row"
    );
    Ok(())
}

// --- requests ---------------------------------------------------------------

pub fn contract_requests<S: Persistence>(store: &S) -> anyhow::Result<()> {
    let base = Utc::now();
    let bot = BotId::new();
    let conversation = ConversationId::new();
    let turn = TurnId::new();

    let mut pending =
        RuntimeRequest::question(bot.clone(), conversation.clone(), "which one?", vec![]);
    pending.turn_id = Some(turn.clone());
    pending.created_at = base;

    let mut answered = RuntimeRequest::approval(
        bot.clone(),
        conversation.clone(),
        Surface::Shell,
        "shell",
        "cargo test",
        None,
    );
    answered.turn_id = Some(turn.clone());
    answered.created_at = base + Duration::seconds(1);
    answered.respond(
        base + Duration::seconds(2),
        Some(Decision::AllowOnce),
        None,
        Responder::Owner,
    );

    let mut expired =
        RuntimeRequest::question(bot.clone(), conversation.clone(), "too late?", vec![]);
    expired.created_at = base + Duration::seconds(3);
    expired.expire(base + Duration::seconds(4));

    for request in [&pending, &answered, &expired] {
        store.insert_request(request)?;
    }
    ensure_that!(
        matches!(
            store.insert_request(&pending),
            Err(DomainError::Conflict { .. })
        ),
        "inserting the same request twice is a conflict"
    );

    ensure_eq!(
        store
            .pending_requests()?
            .iter()
            .map(|request| request.id.clone())
            .collect::<Vec<_>>(),
        vec![pending.id.clone()],
        "pending_requests excludes answered and expired ones"
    );
    ensure_eq!(
        store
            .requests_in(&conversation)?
            .iter()
            .map(|request| request.id.clone())
            .collect::<Vec<_>>(),
        vec![pending.id.clone(), answered.id.clone(), expired.id.clone()],
        "requests_in lists every request of the conversation, oldest first"
    );
    ensure_that!(
        store.get_request(&RequestId::new())?.is_none(),
        "a missing request reads as Ok(None)"
    );

    // A pending request of *another* turn must survive the cancel.
    let other_turn = TurnId::new();
    let mut elsewhere =
        RuntimeRequest::question(bot.clone(), conversation.clone(), "other?", vec![]);
    elsewhere.turn_id = Some(other_turn.clone());
    elsewhere.created_at = base + Duration::seconds(5);
    store.insert_request(&elsewhere)?;

    ensure_eq!(
        store.cancel_requests_of_turn(&turn, "turn stopped")?,
        1,
        "only the pending request of that turn is cancelled"
    );
    let cancelled = store
        .get_request(&pending.id)?
        .context("the cancelled request is kept")?;
    ensure_eq!(
        cancelled.status,
        RequestStatus::Cancelled,
        "the cancel persisted"
    );
    ensure_that!(
        matches!(cancelled.responded_by, Some(Responder::System { .. })),
        "the responder records why"
    );
    ensure_eq!(
        store.get_request(&answered.id)?.map(|r| r.status),
        Some(RequestStatus::Answered),
        "an answered request of the same turn is history, not state"
    );
    ensure_eq!(
        store
            .pending_requests()?
            .iter()
            .map(|request| request.id.clone())
            .collect::<Vec<_>>(),
        vec![elsewhere.id.clone()],
        "another turn's request is untouched"
    );
    ensure_eq!(
        store.cancel_requests_of_turn(&turn, "again")?,
        0,
        "cancelling twice finds nothing pending"
    );

    let stranger = RuntimeRequest::question(BotId::new(), conversation.clone(), "ghost", vec![]);
    ensure_that!(
        matches!(
            store.update_request(&stranger),
            Err(DomainError::NotFound { .. })
        ),
        "updating a request that never existed is NotFound"
    );

    let mut edited = elsewhere.clone();
    edited.prompt = Some("rephrased".into());
    store.update_request(&edited)?;
    ensure_eq!(
        store.get_request(&elsewhere.id)?.and_then(|r| r.prompt),
        Some("rephrased".to_string()),
        "update_request persists"
    );
    Ok(())
}

// --- rules ------------------------------------------------------------------

pub fn contract_rules<S: Persistence>(store: &S) -> anyhow::Result<()> {
    let base = Utc::now();
    let mut allow_cargo = ApprovalRule::new(RuleScope::Global, RuleEffect::Allow);
    allow_cargo.surface = Some(Surface::Shell);
    allow_cargo.command_prefix = Some("cargo".into());
    allow_cargo.created_at = base;
    let mut deny_rm = ApprovalRule::new(RuleScope::Bot(BotId::new()), RuleEffect::Deny);
    deny_rm.command_prefix = Some("rm -rf".into());
    deny_rm.created_at = base + Duration::seconds(1);

    store.insert_rule(&allow_cargo)?;
    store.insert_rule(&deny_rm)?;
    ensure_that!(
        matches!(
            store.insert_rule(&allow_cargo),
            Err(DomainError::Conflict { .. })
        ),
        "inserting the same rule twice is a conflict"
    );

    let rules = store.list_rules()?;
    ensure_eq!(
        rules.iter().map(|rule| rule.id.clone()).collect::<Vec<_>>(),
        vec![allow_cargo.id.clone(), deny_rm.id.clone()],
        "list_rules is ordered by creation"
    );
    ensure_eq!(
        rules[0].effect,
        RuleEffect::Allow,
        "the rule payload round-trips"
    );
    ensure_eq!(rules[1].scope, deny_rm.scope, "scopes round-trip");

    store.delete_rule(&allow_cargo.id)?;
    ensure_eq!(store.list_rules()?.len(), 1, "the deleted rule is gone");
    store.delete_rule(&allow_cargo.id)?; // idempotent
    store.delete_rule(&RuleId::new())?; // deleting an unknown rule is fine
    Ok(())
}

// --- nodes ------------------------------------------------------------------

pub fn contract_nodes<S: Persistence>(store: &S) -> anyhow::Result<()> {
    let now = Utc::now();
    let mut policy = PolicyMap::default();
    policy.set(Surface::Shell, PolicyCeiling::Always);
    policy.set(Surface::WriteFile, PolicyCeiling::Never);
    let roots = vec![
        NodeRoot::read_write("work", "/tmp/work"),
        NodeRoot::read_only("etc", "/etc"),
    ];
    let local = NodeInfo::local("this machine", roots, policy);
    store.insert_node(&local)?;

    let stored = store.get_node(&local.id)?.context("the node round-trips")?;
    ensure_eq!(
        stored,
        local,
        "capabilities, roots and policy survive the round trip"
    );
    ensure_eq!(
        stored.policy.get(Surface::Shell),
        PolicyCeiling::Always,
        "per-surface ceilings survive"
    );
    ensure_eq!(
        stored.policy.get(Surface::WriteFile),
        PolicyCeiling::Never,
        "a stricter-than-default ceiling survives"
    );
    ensure_that!(stored.can("shell"), "effective capabilities survive");
    ensure_that!(
        stored.roots.iter().any(|root| root.id == "work"),
        "roots survive"
    );

    ensure_that!(
        matches!(store.insert_node(&local), Err(DomainError::Conflict { .. })),
        "inserting the same node twice is a conflict"
    );

    let mut remote = local.clone();
    remote.id = NodeId::new();
    remote.kind = NodeKind::Remote;
    remote.paired_at = local.paired_at + Duration::seconds(1);
    store.insert_node(&remote)?;

    let mut offline = local.clone();
    offline.online = false;
    store.update_node(&offline)?;
    ensure_eq!(
        store.get_node(&local.id)?.map(|node| node.online),
        Some(false),
        "update_node persists presence"
    );
    let mut stranger = NodeInfo::local("ghost", vec![], PolicyMap::default());
    stranger.id = NodeId::new();
    ensure_that!(
        matches!(
            store.update_node(&stranger),
            Err(DomainError::NotFound { .. })
        ),
        "updating a node that never existed is NotFound"
    );
    ensure_that!(
        store.get_node(&NodeId::new())?.is_none(),
        "a missing node reads as Ok(None)"
    );
    ensure_eq!(
        store
            .list_nodes()?
            .iter()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>(),
        vec![local.id.clone(), remote.id.clone()],
        "list_nodes is ordered by pairing time"
    );

    // Pairings: only *unused* codes can expire; a spent code is history.
    let live = PairingRecord {
        code_hash: "aa11".into(),
        created_at: now - Duration::minutes(1),
        expires_at: now + Duration::minutes(10),
        used: false,
        node_id: None,
    };
    let stale = PairingRecord {
        code_hash: "bb22".into(),
        created_at: now - Duration::minutes(30),
        expires_at: now - Duration::minutes(1),
        used: false,
        node_id: None,
    };
    let spent = PairingRecord {
        code_hash: "cc33".into(),
        created_at: now - Duration::minutes(30),
        expires_at: now - Duration::minutes(1),
        used: true,
        node_id: Some(remote.id.clone()),
    };
    for record in [&live, &stale, &spent] {
        store.insert_pairing(record)?;
    }
    ensure_that!(
        matches!(
            store.insert_pairing(&live),
            Err(DomainError::Conflict { .. })
        ),
        "inserting the same pairing code twice is a conflict"
    );
    ensure_eq!(
        store
            .expired_pairings(now)?
            .iter()
            .map(|record| record.code_hash.clone())
            .collect::<Vec<_>>(),
        vec!["bb22".to_string()],
        "expired_pairings returns only expired, unused codes"
    );
    ensure_eq!(
        store.get_pairing("aa11")?,
        Some(live.clone()),
        "get_pairing round-trips by hash"
    );
    ensure_that!(
        store.get_pairing("missing")?.is_none(),
        "an unknown hash finds nothing"
    );

    let mut used = live.clone();
    used.used = true;
    used.node_id = Some(remote.id.clone());
    store.update_pairing(&used)?;
    let stored = store
        .get_pairing("aa11")?
        .context("the pairing survives its update")?;
    ensure_that!(stored.used, "marking a pairing used persists");
    ensure_eq!(
        stored.node_id,
        Some(remote.id.clone()),
        "the bound node persists"
    );
    ensure_eq!(
        store
            .expired_pairings(now)?
            .iter()
            .map(|record| record.code_hash.clone())
            .collect::<Vec<_>>(),
        vec!["bb22".to_string()],
        "a used code never shows up as expired"
    );
    let mut unknown_pairing = live.clone();
    unknown_pairing.code_hash = "zz99".into();
    ensure_that!(
        matches!(
            store.update_pairing(&unknown_pairing),
            Err(DomainError::NotFound { .. })
        ),
        "updating an unknown pairing is NotFound"
    );

    // Node keys live apart from the roster and upsert on re-pairing.
    ensure_that!(
        store.node_key(&remote.id)?.is_none(),
        "an unpinned node has no key"
    );
    store.put_node_key(&remote.id, "a2V5LW9uZQ")?;
    store.put_node_key(&remote.id, "a2V5LXR3bw")?;
    ensure_eq!(
        store.node_key(&remote.id)?,
        Some("a2V5LXR3bw".to_string()),
        "re-pinning replaces the key"
    );
    store.put_node_key(&local.id, "bG9jYWwta2V5")?;
    let keys = store.list_node_keys()?;
    ensure_eq!(keys.len(), 2, "every pinned node is listed");
    ensure_that!(
        keys.windows(2).all(|pair| pair[0].0 <= pair[1].0),
        "list_node_keys is ordered by node id"
    );
    ensure_that!(
        keys.iter()
            .any(|(node, key)| *node == remote.id && key == "a2V5LXR3bw"),
        "the list carries the base64url key strings"
    );
    Ok(())
}

// --- sessions ---------------------------------------------------------------

pub fn contract_sessions<S: Persistence>(store: &S) -> anyhow::Result<()> {
    let bot = BotId::new();
    ensure_that!(
        store.get_session(&bot)?.is_none(),
        "a bot without a session reads as Ok(None)"
    );

    let conv_a = ConversationId::new();
    let conv_b = ConversationId::new();
    let mut record = BotSessionRecord::new(bot.clone());
    record.native_session_id = Some("native-123".into());
    record.history = json!([{"role": "user", "content": "hi"}]);
    record.seen.insert(conv_a.clone(), 5);
    record.seen.insert(conv_b.clone(), 7);
    record.token_estimate = 42;
    store.put_session(&record)?;

    ensure_eq!(
        store.get_session(&bot)?,
        Some(record.clone()),
        "the session round-trips, including the seen map and the opaque history"
    );

    record.token_estimate = 99;
    store.put_session(&record)?;
    ensure_eq!(
        store
            .get_session(&bot)?
            .map(|session| session.token_estimate),
        Some(99),
        "put_session replaces the whole record"
    );

    // "New session": the backend forgets, the read cursors stay — a reset
    // must never make a group re-show old messages.
    let seen_before = record.seen.clone();
    store.reset_session(&bot)?;
    let reset = store
        .get_session(&bot)?
        .context("a reset keeps the session row")?;
    ensure_that!(
        reset.native_session_id.is_none(),
        "reset drops the native session id"
    );
    ensure_eq!(
        reset.history,
        serde_json::Value::Array(vec![]),
        "reset clears the history"
    );
    ensure_eq!(
        reset.token_estimate,
        0,
        "the token estimate follows the history"
    );
    ensure_eq!(reset.seen, seen_before, "reset keeps the seen cursors");

    store.reset_session(&BotId::new())?; // no session, no problem
    store.delete_session(&bot)?;
    ensure_that!(
        store.get_session(&bot)?.is_none(),
        "delete_session removes the record"
    );
    store.delete_session(&bot)?; // idempotent
    Ok(())
}

// --- tool calls -------------------------------------------------------------

pub fn contract_tool_calls<S: Persistence>(store: &S) -> anyhow::Result<()> {
    let turn = TurnId::new();
    let call = ToolCallId::new();
    ensure_that!(
        store.find_tool_call(&turn, &call)?.is_none(),
        "find_tool_call is None before the call ran"
    );

    let record = ToolCallRecord {
        turn_id: turn.clone(),
        call_id: call.clone(),
        tool: "shell".into(),
        args: json!({"command": "ls"}),
        result_text: "Cargo.toml".into(),
        is_error: false,
        node_id: Some(NodeId::local()),
        created_at: Utc::now(),
    };
    store.insert_tool_call(&record)?;
    ensure_eq!(
        store.find_tool_call(&turn, &call)?,
        Some(record.clone()),
        "after the insert the stored result comes back — a replayed call id is answered from the store"
    );

    // The key is (turn, call): the same call id in another turn is a
    // different fact.
    let other_turn = ToolCallRecord {
        turn_id: TurnId::new(),
        ..record.clone()
    };
    store.insert_tool_call(&other_turn)?;
    ensure_eq!(
        store.find_tool_call(&turn, &call)?,
        Some(record.clone()),
        "records are keyed by turn and call id"
    );

    let mut rerun = record.clone();
    rerun.result_text = "ran again".into();
    store.insert_tool_call(&rerun)?;
    ensure_eq!(
        store.find_tool_call(&turn, &call)?,
        Some(rerun),
        "re-recording a call replaces its row instead of failing"
    );
    Ok(())
}

// --- event log --------------------------------------------------------------

pub fn contract_event_log<S: Persistence>(store: &S) -> anyhow::Result<()> {
    let epoch = store.epoch()?;
    ensure_eq!(epoch, store.epoch()?, "the epoch is stable across reads");
    ensure_eq!(
        store.current_cursor()?,
        LiveCursor::new(epoch, 0),
        "an empty log sits at sequence 0"
    );

    let mut cursors = Vec::new();
    for step in 0..3u64 {
        let event = LiveEvent::Synchronized {
            cursor: LiveCursor::new(epoch, step),
        };
        cursors.push(store.append_event(&event)?);
    }
    ensure_eq!(
        cursors,
        vec![
            LiveCursor::new(epoch, 1),
            LiveCursor::new(epoch, 2),
            LiveCursor::new(epoch, 3)
        ],
        "cursors ascend from 1"
    );
    ensure_eq!(
        store.current_cursor()?,
        LiveCursor::new(epoch, 3),
        "the tip follows the last append"
    );

    let all = store.events_since(LiveCursor::new(epoch, 0), 10)?;
    ensure_eq!(
        all.iter().map(|(cursor, _)| *cursor).collect::<Vec<_>>(),
        cursors,
        "events_since replays everything after the cursor, ascending"
    );
    ensure_that!(
        matches!(
            all.first().map(|(_, event)| event),
            Some(LiveEvent::Synchronized { cursor }) if *cursor == LiveCursor::new(epoch, 0)
        ),
        "the event payload round-trips"
    );
    ensure_eq!(
        store
            .events_since(LiveCursor::new(epoch, 1), 10)?
            .iter()
            .map(|(cursor, _)| cursor.sequence)
            .collect::<Vec<_>>(),
        vec![2, 3],
        "the cursor is honoured"
    );
    ensure_eq!(
        store
            .events_since(LiveCursor::new(epoch, 1), 1)?
            .iter()
            .map(|(cursor, _)| cursor.sequence)
            .collect::<Vec<_>>(),
        vec![2],
        "the limit is honoured"
    );
    ensure_that!(
        store
            .events_since(LiveCursor::new(epoch, 3), 10)?
            .is_empty(),
        "nothing lives after the tip"
    );
    ensure_eq!(
        store.events_since(LiveCursor::new(0, 0), 10)?.len(),
        3,
        "a cursor from an older epoch sees the whole log"
    );

    // The ring is bounded: truncate keeps the newest and reports the rest.
    ensure_eq!(
        store.truncate_events(2)?,
        1,
        "truncating to 2 deletes the oldest event"
    );
    ensure_eq!(
        store
            .events_since(LiveCursor::new(epoch, 0), 10)?
            .iter()
            .map(|(cursor, _)| cursor.sequence)
            .collect::<Vec<_>>(),
        vec![2, 3],
        "the newest events survive the truncate"
    );
    ensure_eq!(
        store.truncate_events(10)?,
        0,
        "keeping more than exists deletes nothing"
    );
    let next = store.append_event(&LiveEvent::Synchronized {
        cursor: LiveCursor::new(epoch, 0),
    })?;
    ensure_eq!(
        next,
        LiveCursor::new(epoch, 4),
        "sequences continue past a partial truncate"
    );
    ensure_eq!(
        store.truncate_events(0)?,
        3,
        "keeping nothing empties the log"
    );
    ensure_that!(
        store
            .events_since(LiveCursor::new(epoch, 0), 10)?
            .is_empty(),
        "the log is empty afterwards"
    );
    ensure_eq!(
        store.current_cursor()?,
        LiveCursor::new(epoch, 0),
        "the tip resets with the log"
    );
    Ok(())
}

// --- settings ---------------------------------------------------------------

pub fn contract_settings<S: Persistence>(store: &S) -> anyhow::Result<()> {
    ensure_eq!(
        store.settings()?,
        Settings::default(),
        "an unset store answers with the defaults"
    );
    let mut settings = Settings {
        bot_messaging: false,
        max_hops: 3,
        compact_at_fraction: 0.5,
        ..Settings::default()
    };
    store.put_settings(&settings)?;
    ensure_eq!(
        store.settings()?,
        settings,
        "settings round-trip through put/settings"
    );
    settings.max_tool_rounds = 7;
    store.put_settings(&settings)?;
    ensure_eq!(
        store.settings()?,
        settings,
        "a second put replaces the first"
    );
    Ok(())
}

// --- recovery ---------------------------------------------------------------

pub fn contract_recovery<S: Persistence>(store: &S) -> anyhow::Result<()> {
    // A clean store has nothing to settle — boot recovery must say so.
    let clean = recover(store, Utc::now())?;
    ensure_that!(
        clean.is_empty(),
        "recovery on a clean store reports nothing"
    );
    ensure_eq!(clean.summary(), "nothing to recover", "the summary agrees");

    let now = Utc::now();
    let conversation = Conversation::direct(BotId::new());
    let bot = conversation
        .direct_bot()
        .cloned()
        .context("a DM has a bot")?;
    store.insert_conversation(&conversation)?;

    let running_turn = Turn::start(
        bot.clone(),
        conversation.id.clone(),
        TurnInitiator::Owner,
        None,
        None,
    );
    store.insert_turn(&running_turn)?;
    let mut finished_turn = Turn::start(
        bot.clone(),
        conversation.id.clone(),
        TurnInitiator::Owner,
        None,
        None,
    );
    finished_turn.complete(now);
    store.insert_turn(&finished_turn)?;

    let mut request =
        RuntimeRequest::question(bot.clone(), conversation.id.clone(), "which one?", vec![]);
    request.turn_id = Some(running_turn.id.clone());
    store.insert_request(&request)?;

    let mut running_task = Task::own(bot.clone(), conversation.id.clone(), "build", "x");
    running_task.start(now, Some(TurnId::new()));
    store.insert_task(&running_task)?;
    let queued_task = Task::own(bot.clone(), conversation.id.clone(), "later", "y");
    store.insert_task(&queued_task)?;
    let mut finished_task = Task::own(bot.clone(), conversation.id.clone(), "earlier", "z");
    finished_task.succeed(now, "done");
    store.insert_task(&finished_task)?;

    let owner_delivery = Delivery::owner_message(conversation.id.clone(), bot.clone(), "stale");
    let routine_delivery = Delivery::routine(
        conversation.id.clone(),
        bot.clone(),
        RoutineId::new(),
        "daily",
    );
    store.insert_delivery(&owner_delivery)?;
    store.insert_delivery(&routine_delivery)?;

    let report = recover(store, now + Duration::seconds(1))?;
    ensure_eq!(
        report.interrupted_turns,
        vec![running_turn.id.clone()],
        "the running turn was interrupted"
    );
    ensure_eq!(
        report.expired_requests,
        vec![request.id.clone()],
        "the pending request expired"
    );
    ensure_eq!(
        report.interrupted_tasks.len(),
        2,
        "the running and the queued task were interrupted"
    );
    ensure_that!(
        report.interrupted_tasks.contains(&running_task.id)
            && report.interrupted_tasks.contains(&queued_task.id)
            && !report.interrupted_tasks.contains(&finished_task.id),
        "terminal tasks are left alone"
    );
    ensure_eq!(
        report.dropped_deliveries,
        2,
        "both queued deliveries were dropped"
    );
    ensure_that!(!report.is_empty(), "the report says something happened");
    ensure_that!(
        report.summary().contains("1 turn(s) interrupted"),
        "the summary names the counts: {}",
        report.summary()
    );

    // The report is a promise about the persisted state — verify the state.
    let turn = store
        .get_turn(&running_turn.id)?
        .context("the turn is kept")?;
    ensure_eq!(
        turn.status,
        TurnStatus::Interrupted,
        "interrupted, not running"
    );
    ensure_that!(turn.error.is_some(), "an interrupted turn says why");
    ensure_eq!(
        store.get_turn(&finished_turn.id)?.map(|t| t.status),
        Some(TurnStatus::Completed),
        "a completed turn stays completed"
    );
    ensure_eq!(
        store.get_request(&request.id)?.map(|r| r.status),
        Some(RequestStatus::Expired),
        "the request expired in storage"
    );
    let task = store
        .get_task(&running_task.id)?
        .context("the task is kept")?;
    ensure_eq!(
        task.status,
        TaskStatus::Interrupted,
        "interrupted in storage"
    );
    ensure_that!(task.can_restart(), "an interrupted task may be re-run");
    ensure_eq!(
        store.get_task(&queued_task.id)?.map(|t| t.status),
        Some(TaskStatus::Interrupted),
        "the queued task was interrupted too"
    );
    ensure_eq!(
        store.get_task(&finished_task.id)?.map(|t| t.status),
        Some(TaskStatus::Succeeded),
        "the succeeded task keeps its result"
    );
    ensure_eq!(
        store.get_delivery(&owner_delivery.id)?.map(|d| d.state),
        Some(DeliveryState::Dropped),
        "the owner delivery was dropped, not deleted"
    );
    ensure_eq!(
        store.get_delivery(&routine_delivery.id)?.map(|d| d.state),
        Some(DeliveryState::Dropped),
        "a routine delivery is dropped like any other"
    );
    ensure_that!(
        store.queued_deliveries(&bot)?.is_empty(),
        "no stale queue can wake the bot after recovery"
    );

    let second = recover(store, now + Duration::seconds(2))?;
    ensure_that!(
        second.is_empty(),
        "recovery is idempotent — a second pass settles nothing: {second:?}"
    );
    Ok(())
}

// --- jsonl mirror -----------------------------------------------------------

pub fn contract_mirror<S: Persistence>(make: &mut impl FnMut() -> S) -> anyhow::Result<()> {
    let scratch = ScratchDir::new("mirror")?;
    // The mirror writes into a directory that does not exist yet: `append`
    // must create it (the brain starts with an empty data dir).
    let dir = scratch.child("transcripts");
    let mirror = JsonlMirror::new(dir.clone());

    let conversation = ConversationId::new();
    let entries: Vec<Entry> = (1..=3)
        .map(|seq| Entry::owner_message(conversation.clone(), seq, format!("line {seq}")))
        .collect();
    for entry in &entries {
        mirror.append(&conversation, entry)?;
    }

    let path = mirror.path_for(&conversation);
    ensure_that!(
        path.starts_with(&dir) && path.extension().is_some_and(|ext| ext == "jsonl"),
        "the transcript lives at <dir>/<conversation id>.jsonl, got {}",
        path.display()
    );
    let raw = std::fs::read_to_string(&path)?;
    ensure_eq!(raw.lines().count(), 3, "one line per appended entry");
    ensure_that!(
        raw.lines()
            .all(|line| line.starts_with('{') && !line.contains('\n')),
        "each line is one compact JSON object"
    );
    ensure_eq!(
        mirror.read(&conversation)?,
        entries,
        "read returns the entries in order"
    );
    ensure_that!(
        mirror.read(&ConversationId::new())?.is_empty(),
        "a conversation that never wrote reads as empty"
    );

    // A crash mid-write leaves a torn tail; recovery must not stumble on it.
    let mut torn = raw.clone();
    torn.push_str("{\"id\": \"ent_torn\nnot json at all\n");
    std::fs::write(&path, torn)?;
    ensure_eq!(
        mirror.read(&conversation)?,
        entries,
        "corrupt lines are skipped, good lines survive"
    );

    // The escape hatch: the SQLite file is gone, the transcripts are not.
    let rebuilt = make();
    ensure_eq!(
        JsonlMirror::rebuild(&dir, &rebuilt)?,
        3,
        "rebuild counts the entries it wrote (torn lines do not count)"
    );
    ensure_eq!(
        rebuilt.entries_after(&conversation, 0, 10)?,
        entries,
        "rebuild restores the transcript into a fresh store"
    );
    // Replaying an intact store hits the revision guard on every entry and
    // must skip, not fail and not duplicate.
    ensure_eq!(
        JsonlMirror::rebuild(&dir, &rebuilt)?,
        0,
        "a second rebuild writes nothing — conflicts are skipped"
    );
    ensure_eq!(
        rebuilt.entries_after(&conversation, 0, 10)?.len(),
        3,
        "and nothing was duplicated"
    );
    ensure_eq!(
        JsonlMirror::rebuild(&scratch.child("never-created"), &rebuilt)?,
        0,
        "rebuilding from a missing directory is a no-op, not an error"
    );
    Ok(())
}

// --- routines ---------------------------------------------------------------

pub fn contract_routines(store: &impl Persistence) -> anyhow::Result<()> {
    let now = Utc::now();
    let bot_a = BotId::new();
    let bot_b = BotId::new();

    let mut r1 = Routine::new(
        bot_a.clone(),
        "Daily sync",
        "Summarize updates",
        RoutineSchedule::Interval { every_secs: 3600 },
        now,
    )?;
    r1.next_run_at = Some(now - Duration::seconds(10));
    store.insert_routine(&r1)?;

    ensure_that!(
        store.insert_routine(&r1).is_err(),
        "duplicate routine id must conflict"
    );

    let r2 = Routine::new(
        bot_a.clone(),
        "Weekly digest",
        "Digest newsletter",
        RoutineSchedule::Interval {
            every_secs: 86400 * 7,
        },
        now + Duration::seconds(1),
    )?;
    store.insert_routine(&r2)?;

    let r3 = Routine::new(
        bot_b.clone(),
        "Healthcheck",
        "Ping services",
        RoutineSchedule::Interval { every_secs: 60 },
        now + Duration::seconds(2),
    )?;
    store.insert_routine(&r3)?;

    ensure_eq!(
        store.get_routine(&r1.id)?,
        Some(r1.clone()),
        "r1 retrieved by id"
    );
    ensure_eq!(
        store.get_routine(&RoutineId::new())?,
        None,
        "unknown routine returns None"
    );

    let all = store.list_routines()?;
    ensure_eq!(all.len(), 3, "three routines listed");

    let bot_a_routines = store.list_routines_for_bot(&bot_a)?;
    ensure_eq!(bot_a_routines.len(), 2, "bot_a has two routines");

    let due = store.due_routines(now)?;
    ensure_eq!(due.len(), 1, "only r1 is due");
    ensure_eq!(due[0].id, r1.id, "r1 is the due routine");

    r1.pause();
    store.update_routine(&r1)?;
    ensure_eq!(
        store.get_routine(&r1.id)?.map(|r| r.status),
        Some(RoutineStatus::Paused),
        "r1 is paused"
    );
    ensure_eq!(
        store.due_routines(now)?.len(),
        0,
        "paused routine is not due"
    );

    let mut fake = r1.clone();
    fake.id = RoutineId::new();
    ensure_that!(
        store.update_routine(&fake).is_err(),
        "updating non-existent routine must error"
    );

    let mut run1 = RoutineRun::start(&r1, now);
    store.insert_run(&run1)?;

    ensure_that!(
        store.insert_run(&run1).is_err(),
        "duplicate run id must conflict"
    );

    ensure_eq!(
        store.get_run(&run1.id)?,
        Some(run1.clone()),
        "run retrieved by id"
    );

    run1.complete(now + Duration::seconds(5));
    store.update_run(&run1)?;
    ensure_eq!(
        store.get_run(&run1.id)?.map(|r| r.status),
        Some(RunStatus::Success),
        "run completed successfully"
    );

    let run2 = RoutineRun::start(&r1, now + Duration::seconds(10));
    store.insert_run(&run2)?;

    let runs = store.list_runs(&r1.id, 10)?;
    ensure_eq!(runs.len(), 2, "two runs listed for r1");
    ensure_eq!(runs[0].id, run2.id, "runs ordered newest first");

    let runs_limit = store.list_runs(&r1.id, 1)?;
    ensure_eq!(runs_limit.len(), 1, "limit respected");
    ensure_eq!(runs_limit[0].id, run2.id, "limit kept newest");

    store.delete_routine(&r1.id)?;
    ensure_eq!(store.get_routine(&r1.id)?, None, "deleted routine is gone");
    ensure_eq!(
        store.list_runs(&r1.id, 10)?.len(),
        0,
        "runs were cascade deleted"
    );

    ensure_that!(
        store.delete_routine(&r1.id).is_err(),
        "deleting non-existent routine must error"
    );

    Ok(())
}
