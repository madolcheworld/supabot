//! One runner per bot: the loop that takes deliveries, runs turns, and posts
//! replies.
//!
//! The rules this file exists to keep:
//!
//! * **one mind per bot** — a single task owns the bot's session, so two
//!   conversations can never interleave inside one model call;
//! * **every turn belongs to exactly one conversation** — its replies, tool
//!   calls, cards and usage land there;
//! * **queue, don't interrupt** — a delivery for another conversation waits
//!   (FIFO); only an urgent delivery for the *same* conversation is steered
//!   into the running turn, and a bot message never interrupts anything;
//! * **nothing is silently dropped** — a delivery is only marked done once its
//!   turn actually ran.
//!
//! The turn runs as a separate future so the loop can keep taking commands
//! (steer, interrupt, more deliveries) while a model call is in flight. That is
//! why the turn is a free function over cloned `Arc`s rather than a `&mut self`
//! method: the loop and the turn must not borrow the same state.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use swarmbot_contracts::bot::{Bot, BotStatus};
use swarmbot_contracts::config::{BackendKind, Settings};
use swarmbot_contracts::conversation::{Conversation, ConversationKind};
use swarmbot_contracts::delivery::{Delivery, DeliveryKind, Urgency};
use swarmbot_contracts::entry::{Author, Entry, EntryKind, NoticeLevel, PostState, ToolCallStatus};
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{BotId, ConversationId, EntryId, TaskId, ToolCallId, TurnId};
use swarmbot_contracts::node::{NodeInfo, PolicyMap};
use swarmbot_contracts::repo::{Persistence, ToolCallRecord};
use swarmbot_contracts::task::Task;
use swarmbot_contracts::tool::{Invocation, ToolCallRequest, ToolResult, ToolSpec, Toolset};
use swarmbot_contracts::turn::{Turn, TurnInitiator, TurnStatus};
use swarmbot_llm::{
    ChatMessage, ChatRequest, ChatResponse, FinishReason, LlmBackend, LlmEvent, Role,
};
use swarmbot_tools::{ApprovalGate, NodeExecutor, ToolContext, ToolRegistry};

use crate::backends::BackendSet;
use crate::bus::{Change, LiveHub};
use crate::instructions::{InstructionRenderer, RenderOptions};
use crate::router::{GroupRouter, RouteReason, is_no_reply};
use crate::session::{Session, origin_tag};
use crate::settings::SettingsHandle;
use crate::swarm::{McpProviderSlot, McpTurnRegistration, McpTurnRequest};
use crate::transcript::Transcript;

/// How often a streaming reply is written to the transcript. Every delta would
/// be a write per character; a model that stops mid-word is still readable.
const STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(120);
/// …and at least this often when the model is fast.
const STREAM_FLUSH_CHARS: usize = 240;
/// How much of a group a bot is caught up on in one turn.
const CATCH_UP_LIMIT: u32 = 60;
const CATCH_UP_BYTES: usize = 8 * 1024;
/// Consecutive backend failures before a bot is marked `error`.
const FAILURE_LIMIT: u32 = 3;
/// The context budget assumed when a backend does not declare one.
const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

/// Commands a runner accepts while it lives.
#[derive(Debug)]
pub enum RunnerCmd {
    Deliver(Delivery),
    /// Push into the running turn. Refused when the runner is idle or working
    /// in another conversation — the caller then queues it instead.
    Steer(Delivery),
    /// Stop the running turn of one conversation, and drop that conversation's
    /// queued deliveries. `None` means "whatever is running".
    Interrupt(Option<ConversationId>),
    Shutdown,
}

/// Shared, lock-light state the roster and the gateway read.
#[derive(Default)]
pub struct SharedState {
    busy: AtomicBool,
    conversation: Mutex<Option<ConversationId>>,
    turn_cancel: Mutex<Option<CancellationToken>>,
}

impl SharedState {
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    pub fn active_conversation(&self) -> Option<ConversationId> {
        self.conversation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// The handle the rest of the brain holds.
pub struct RunnerHandle {
    bot_id: BotId,
    cmd: mpsc::UnboundedSender<RunnerCmd>,
    cancel: CancellationToken,
    state: Arc<SharedState>,
    finished: Arc<tokio::sync::Notify>,
}

impl RunnerHandle {
    pub fn bot_id(&self) -> &BotId {
        &self.bot_id
    }

    pub fn is_busy(&self) -> bool {
        self.state.is_busy()
    }

    pub fn active_conversation(&self) -> Option<ConversationId> {
        self.state.active_conversation()
    }

    pub fn deliver(&self, delivery: Delivery) -> Result<(), DomainError> {
        self.cmd
            .send(RunnerCmd::Deliver(delivery))
            .map_err(|_| DomainError::conflict(format!("the runner for {} is gone", self.bot_id)))
    }

    /// Steer into the running turn of the same conversation.
    pub fn steer(&self, delivery: Delivery) -> bool {
        if !self.is_busy() || self.active_conversation().as_ref() != Some(&delivery.conversation_id)
        {
            return false;
        }
        self.cmd.send(RunnerCmd::Steer(delivery)).is_ok()
    }

    /// Stop a turn and/or drop a conversation's queued deliveries. True when
    /// there was something to stop.
    pub fn interrupt(&self, conversation: Option<&ConversationId>) -> bool {
        let running = self.active_conversation();
        let stops_the_turn = match (conversation, &running) {
            (None, Some(_)) => true,
            (Some(wanted), Some(current)) => wanted == current,
            _ => false,
        };
        if stops_the_turn
            && let Some(token) = self
                .state
                .turn_cancel
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        {
            token.cancel();
        }
        let sent = self
            .cmd
            .send(RunnerCmd::Interrupt(conversation.cloned()))
            .is_ok();
        sent && (stops_the_turn || self.is_busy())
    }

    pub fn shutdown(&self) {
        self.cancel.cancel();
        let _ = self.cmd.send(RunnerCmd::Shutdown);
    }

    /// Wait for the runner task to end (a graceful shutdown).
    pub async fn joined(&self) {
        if self.cancel.is_cancelled() && !self.is_busy() {
            return;
        }
        self.finished.notified().await;
    }
}

/// Every runner, plus the deliveries that arrived before one existed.
///
/// Parking instead of failing is what breaks the construction cycle: messaging
/// and the task pump can hand work to a bot whose runner is not spawned yet,
/// and nothing is lost.
#[derive(Default)]
pub struct RunnerMap {
    handles: Mutex<HashMap<BotId, Arc<RunnerHandle>>>,
    parked: Mutex<HashMap<BotId, VecDeque<Delivery>>>,
}

impl RunnerMap {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn register(&self, handle: Arc<RunnerHandle>) {
        let bot_id = handle.bot_id().clone();
        let parked: Vec<Delivery> = self
            .parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&bot_id)
            .map(VecDeque::into)
            .unwrap_or_default();
        for delivery in parked {
            if let Err(err) = handle.deliver(delivery) {
                warn!(bot = %bot_id, error = %err, "cannot hand a parked delivery to its runner");
            }
        }
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(bot_id, handle);
    }

    pub fn unregister(&self, bot_id: &BotId) -> Option<Arc<RunnerHandle>> {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(bot_id)
    }

    pub fn get(&self, bot_id: &BotId) -> Option<Arc<RunnerHandle>> {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(bot_id)
            .cloned()
    }

    /// Hand a delivery to its bot, parking it when the runner is not up yet.
    pub fn deliver(&self, delivery: Delivery) -> Result<(), DomainError> {
        let bot_id = delivery.bot_id.clone();
        if let Some(handle) = self.get(&bot_id) {
            return handle.deliver(delivery);
        }
        self.parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(bot_id)
            .or_default()
            .push_back(delivery);
        Ok(())
    }

    pub fn steer(&self, bot_id: &BotId, delivery: Delivery) -> bool {
        self.get(bot_id).is_some_and(|h| h.steer(delivery))
    }

    pub fn interrupt(&self, bot_id: &BotId, conversation: Option<&ConversationId>) -> bool {
        self.get(bot_id).is_some_and(|h| h.interrupt(conversation))
    }

    /// The conversation a bot's running turn belongs to — what the tool gateway
    /// uses to place a card next to the work.
    pub fn active_turn(&self, bot_id: &BotId) -> Option<ConversationId> {
        self.get(bot_id).and_then(|h| h.active_conversation())
    }

    pub fn is_busy(&self, bot_id: &BotId) -> bool {
        self.get(bot_id).is_some_and(|h| h.is_busy())
    }

    pub fn bot_ids(&self) -> Vec<BotId> {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.handles.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many deliveries are waiting for a runner that does not exist yet.
    pub fn parked_count(&self) -> usize {
        self.parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(VecDeque::len)
            .sum()
    }

    pub async fn shutdown(&self) {
        let handles: Vec<Arc<RunnerHandle>> = self
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, handle)| handle)
            .collect();
        for handle in &handles {
            handle.shutdown();
        }
        for handle in &handles {
            handle.joined().await;
        }
    }
}

/// Everything a runner needs. Built once by the brain, cloned per bot (all
/// `Arc`s, so a clone is cheap).
#[derive(Clone)]
pub struct RunnerEnv {
    pub store: Arc<dyn Persistence>,
    pub hub: Arc<LiveHub>,
    pub transcript: Arc<Transcript>,
    pub backends: Arc<BackendSet>,
    pub tools: Arc<ToolRegistry>,
    pub instructions: Arc<InstructionRenderer>,
    pub settings: Arc<SettingsHandle>,
    pub approval: Arc<dyn ApprovalGate>,
    pub nodes: Arc<dyn NodeExecutor>,
    pub runners: Arc<RunnerMap>,
    /// Where each bot's files live: `<root>/<bot name>`.
    pub workspaces_root: PathBuf,
    pub hooks: Arc<dyn RunnerHooks>,
    pub mcp_provider: McpProviderSlot,
}

/// The things a runner cannot do alone: wake other bots, settle tasks, and
/// coordinate DM reply forwarding across turns.
pub trait RunnerHooks: Send + Sync {
    /// A reply mentioned teammates in a group: wake each of them once.
    fn wake_mentioned(
        &self,
        conversation: &Conversation,
        from: &Bot,
        text: &str,
        entry: Option<&Entry>,
        hops: u8,
        woken: Vec<BotId>,
    );

    /// A task's turn ended: mark the task and deliver its result.
    fn settle_task(&self, task_id: &TaskId, result: &str, failed: bool);

    /// Take and clear targets explicitly messaged or handed off during `turn_id`.
    fn take_turn_targets(&self, _turn_id: &TurnId) -> HashSet<BotId> {
        HashSet::new()
    }

    /// Forward a DM reply back to the teammate that sent a `BotMessage`.
    fn forward_dm_reply(
        &self,
        _conversation: &Conversation,
        _from: &Bot,
        _to: &BotId,
        _text: &str,
        _source_entry: Option<EntryId>,
        _hops: u8,
    ) {
    }
}

/// No hooks, for tests that only care about one bot.
#[derive(Default)]
pub struct NoHooks;

impl RunnerHooks for NoHooks {
    fn wake_mentioned(
        &self,
        _conversation: &Conversation,
        _from: &Bot,
        _text: &str,
        _entry: Option<&Entry>,
        _hops: u8,
        _woken: Vec<BotId>,
    ) {
    }

    fn settle_task(&self, _task_id: &TaskId, _result: &str, _failed: bool) {}
}

/// What one turn produced.
#[derive(Clone, Debug, Default)]
pub struct TurnOutcome {
    /// The bot's final text, empty when it only called tools or said nothing.
    pub text: String,
    /// The entry that text was posted as, when it was posted.
    pub reply: Option<Entry>,
    /// True when the owner stopped it.
    pub aborted: bool,
    /// True when the reply was exactly `NO_REPLY`.
    pub silent: bool,
}

/// The per-bot task.
pub struct BotRunner {
    bot_id: BotId,
    env: RunnerEnv,
    rx: mpsc::UnboundedReceiver<RunnerCmd>,
    cancel: CancellationToken,
    state: Arc<SharedState>,
    queue: VecDeque<Delivery>,
    failures: u32,
}

impl BotRunner {
    /// Spawn a runner for one bot, register it, and return its handle.
    pub fn spawn(bot_id: BotId, env: RunnerEnv) -> Arc<RunnerHandle> {
        let (cmd_tx, rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let state = Arc::new(SharedState::default());
        let finished = Arc::new(tokio::sync::Notify::new());
        let handle = Arc::new(RunnerHandle {
            bot_id: bot_id.clone(),
            cmd: cmd_tx,
            cancel: cancel.clone(),
            state: Arc::clone(&state),
            finished: Arc::clone(&finished),
        });
        let runner = Self {
            bot_id,
            env,
            rx,
            cancel,
            state,
            queue: VecDeque::new(),
            failures: 0,
        };
        let runners = Arc::clone(&runner.env.runners);
        tokio::spawn(async move {
            runner.run().await;
            finished.notify_waiters();
        });
        // Registering after the spawn is fine: deliveries that arrive in
        // between are parked by the map and handed over here.
        runners.register(Arc::clone(&handle));
        handle
    }

    async fn run(mut self) {
        info!(bot = %self.bot_id, "bot runner started");
        loop {
            let Some(first) = self.next_delivery().await else {
                break;
            };
            let batch = self.take_batch(first);
            let steer: Arc<Mutex<VecDeque<Delivery>>> = Arc::new(Mutex::new(VecDeque::new()));

            let turn = run_turn(
                self.bot_id.clone(),
                self.env.clone(),
                Arc::clone(&self.state),
                batch,
                Arc::clone(&steer),
            );
            tokio::pin!(turn);

            let outcome: Result<(TurnOutcome, Turn), DomainError> = loop {
                tokio::select! {
                    biased;
                    () = self.cancel.cancelled() => {
                        cancel_current(&self.state);
                        break Err(DomainError::internal("the brain is shutting down"));
                    }
                    result = &mut turn => break result,
                    command = self.rx.recv() => match command {
                        Some(command) => {
                            if !self.handle_command(command, &steer) {
                                cancel_current(&self.state);
                                return;
                            }
                        }
                        None => {
                            cancel_current(&self.state);
                            return;
                        }
                    },
                }
            };

            {
                let mut leftover = steer.lock().unwrap_or_else(|e| e.into_inner());
                while let Some(delivery) = leftover.pop_front() {
                    self.queue.push_back(delivery);
                }
            }
            self.publish_queued();
            self.after_turn(outcome);
            if self.cancel.is_cancelled() {
                break;
            }
        }
        info!(bot = %self.bot_id, "bot runner stopped");
    }

    /// Wait for a delivery: from the queue, or from a command.
    async fn next_delivery(&mut self) -> Option<Delivery> {
        loop {
            if let Some(delivery) = self.queue.pop_front() {
                return Some(delivery);
            }
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => return None,
                command = self.rx.recv() => match command {
                    Some(RunnerCmd::Deliver(delivery)) => self.queue.push_back(delivery),
                    // Nothing is running, so a steer is just a delivery.
                    Some(RunnerCmd::Steer(delivery)) => self.queue.push_back(delivery),
                    Some(RunnerCmd::Interrupt(conversation)) => self.drop_queued(conversation.as_ref()),
                    Some(RunnerCmd::Shutdown) | None => return None,
                },
            }
        }
    }

    /// Batch the queued background deliveries of the same conversation into
    /// this turn, so five notes do not cost five turns.
    ///
    /// A **task** delivery is never batched: a turn settles exactly one task
    /// (the one it was started for), so folding two task deliveries into one
    /// turn would leave the second task marked `Running` forever — and a task
    /// stuck running occupies the per-bot cap and starves the queue behind it.
    fn take_batch(&mut self, first: Delivery) -> Vec<Delivery> {
        let mut batch = vec![first];
        if batch[0].task_id.is_some() {
            return batch;
        }
        let head_conversation = batch[0].conversation_id.clone();
        let mut index = 0;
        while index < self.queue.len() {
            let candidate = &self.queue[index];
            let same_conversation = candidate.conversation_id == head_conversation;
            let background = candidate.urgency == Urgency::Background;
            let not_the_owner = candidate.kind != DeliveryKind::OwnerMessage;
            let not_a_task = candidate.task_id.is_none();
            if same_conversation && background && not_the_owner && not_a_task {
                batch.push(self.queue.remove(index).expect("index in range"));
            } else {
                index += 1;
            }
        }
        batch
    }

    /// A command that arrived while a turn runs. False stops the runner.
    fn handle_command(
        &mut self,
        command: RunnerCmd,
        steer: &Arc<Mutex<VecDeque<Delivery>>>,
    ) -> bool {
        match command {
            RunnerCmd::Deliver(delivery) => {
                self.queue.push_back(delivery);
                self.publish_queued();
                true
            }
            RunnerCmd::Steer(delivery) => {
                steer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(delivery);
                true
            }
            RunnerCmd::Interrupt(conversation) => {
                let running = self.state.active_conversation();
                let matches = match (&conversation, &running) {
                    (None, Some(_)) => true,
                    (Some(wanted), Some(current)) => wanted == current,
                    _ => false,
                };
                if matches {
                    cancel_current(&self.state);
                }
                self.drop_queued(conversation.as_ref());
                true
            }
            RunnerCmd::Shutdown => false,
        }
    }

    fn drop_queued(&mut self, conversation: Option<&ConversationId>) {
        self.queue.retain(|delivery| match conversation {
            Some(id) => delivery.conversation_id != *id,
            None => true,
        });
        self.publish_queued();
    }

    fn publish_queued(&self) {
        if self.queue.is_empty() {
            return;
        }
        self.publish_status(BotStatus::Queued {
            count: self.queue.len() as u32,
        });
    }

    fn after_turn(&mut self, outcome: Result<(TurnOutcome, Turn), DomainError>) {
        match &outcome {
            Ok(_) => self.failures = 0,
            Err(_) => self.failures += 1,
        }
        let status = if self.failures >= FAILURE_LIMIT {
            BotStatus::Error {
                reason: outcome
                    .as_ref()
                    .err()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "repeated failures".into()),
            }
        } else if self.queue.is_empty() {
            BotStatus::Idle
        } else {
            BotStatus::Queued {
                count: self.queue.len() as u32,
            }
        };
        self.publish_status(status);
        if let Err(err) = &outcome {
            warn!(bot = %self.bot_id, error = %err, "a turn failed");
        }
    }

    fn publish_status(&self, status: BotStatus) {
        self.env.hub.publish(Change::BotStatus {
            bot_id: self.bot_id.clone(),
            status: status.clone(),
        });
        if let Ok(Some(mut bot)) = self.env.store.get_bot(&self.bot_id) {
            // A status is not worth a write when nothing changed.
            if bot.status != status {
                bot.status = status;
                bot.updated_at = chrono::Utc::now();
                if self.env.store.update_bot(&bot).is_ok() {
                    self.env.hub.publish(Change::Bot(bot));
                }
            }
        }
    }
}

fn cancel_current(state: &SharedState) {
    if let Some(token) = state
        .turn_cancel
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        token.cancel();
    }
}

/// Run one turn: everything the loop hands over, nothing it keeps.
async fn run_turn(
    bot_id: BotId,
    env: RunnerEnv,
    state: Arc<SharedState>,
    deliveries: Vec<Delivery>,
    steer: Arc<Mutex<VecDeque<Delivery>>>,
) -> Result<(TurnOutcome, Turn), DomainError> {
    let Some(first) = deliveries.first() else {
        return Err(DomainError::invalid("a turn needs at least one delivery"));
    };
    let conversation_id = first.conversation_id.clone();
    let store = Arc::clone(&env.store);

    let bot = store
        .get_bot(&bot_id)?
        .ok_or_else(|| swarmbot_contracts::not_found::bot(&bot_id))?;
    let conversation = store
        .get_conversation(&conversation_id)?
        .ok_or_else(|| swarmbot_contracts::not_found::conversation(&conversation_id))?;
    let settings = env.settings.get();
    let task = match first.task_id.as_ref() {
        Some(id) => store.get_task(id)?,
        None => None,
    };

    let mut turn = Turn::start(
        bot_id.clone(),
        conversation_id.clone(),
        initiator_of(first, task.as_ref()),
        Some(first.id.clone()),
        first.task_id.clone(),
    );
    store.insert_turn(&turn)?;

    let turn_cancel = CancellationToken::new();
    {
        *state.turn_cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(turn_cancel.clone());
        *state.conversation.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(conversation_id.clone());
        state.busy.store(true, Ordering::SeqCst);
    }
    publish_status(
        &env,
        &bot_id,
        BotStatus::Working {
            conversation_id: conversation_id.clone(),
            activity: activity_of(first),
        },
    );

    // Mark the deliveries running, so a restart can tell them from leftovers.
    for delivery in &deliveries {
        let mut record = delivery.clone();
        record.mark_running(chrono::Utc::now());
        let _ = store.update_delivery(&record);
    }

    let mut consumed_steers = Vec::new();
    let executed = execute_turn(
        &bot_id,
        &env,
        &bot,
        &conversation,
        &deliveries,
        task.as_ref(),
        &mut turn,
        &turn_cancel,
        &steer,
        &mut consumed_steers,
        &settings,
    )
    .await;

    let now = chrono::Utc::now();
    let aborted = turn_cancel.is_cancelled();
    let outcome = match executed {
        Ok(mut outcome) => {
            if aborted && !turn.status.is_terminal() {
                turn.abort(now);
                outcome.aborted = true;
            } else if !turn.status.is_terminal() {
                turn.complete(now);
            }
            Ok((outcome, turn.clone()))
        }
        Err(err) => {
            if aborted {
                turn.abort(now);
                let _ = env.transcript.notice(
                    &conversation_id,
                    "Stopped by the owner.",
                    NoticeLevel::Info,
                );
                Ok((
                    TurnOutcome {
                        aborted: true,
                        ..Default::default()
                    },
                    turn.clone(),
                ))
            } else {
                turn.fail(now, err.to_string());
                let _ = env.transcript.notice(
                    &conversation_id,
                    format!("This turn failed: {err}"),
                    NoticeLevel::Error,
                );
                Err(err)
            }
        }
    };

    // `turn` was moved into the success value, so write whichever we hold.
    let finished_turn = match &outcome {
        Ok((_, turn)) => turn.clone(),
        Err(_) => turn.clone(),
    };
    store.update_turn(&finished_turn)?;
    env.hub.publish(Change::Turn(finished_turn));

    for delivery in deliveries.iter().chain(consumed_steers.iter()) {
        let mut record = delivery.clone();
        record.mark_done(chrono::Utc::now());
        let _ = store.update_delivery(&record);
    }

    // A task's result goes back to whoever asked, not into this transcript.
    // A turn the owner stopped is a failure of the task, not an empty success:
    // the requester has to know the work did not finish.
    if let Some(task) = task {
        let (text, failed) = match &outcome {
            Ok((done, turn)) if done.aborted || turn.status == TurnStatus::Aborted => {
                ("the turn running this task was stopped".to_string(), true)
            }
            Ok((done, _)) => (done.text.clone(), false),
            Err(err) => (err.to_string(), true),
        };
        env.hooks.settle_task(&task.id, &text, failed);
    }

    state.busy.store(false, Ordering::SeqCst);
    *state.conversation.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *state.turn_cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let _ = env.hooks.take_turn_targets(&turn.id);

    outcome
}

#[allow(clippy::too_many_arguments)]
async fn execute_turn(
    bot_id: &BotId,
    env: &RunnerEnv,
    bot: &Bot,
    conversation: &Conversation,
    deliveries: &[Delivery],
    task: Option<&Task>,
    turn: &mut Turn,
    turn_cancel: &CancellationToken,
    steer: &Arc<Mutex<VecDeque<Delivery>>>,
    consumed_steers: &mut Vec<Delivery>,
    settings: &Settings,
) -> Result<TurnOutcome, DomainError> {
    let store = Arc::clone(&env.store);
    let mut session = Session::load(store.as_ref(), bot_id)?;

    let workspace = crate::compose::ensure_bot_workspace(&env.workspaces_root, &bot.name)?;
    let spool_dir = workspace.join(".spool");

    let node = store
        .get_node(&bot.host_node())?
        .unwrap_or_else(|| NodeInfo::local("this machine", Vec::new(), PolicyMap::default()));
    let backend: Arc<dyn LlmBackend> = env.backends.backend(&bot.backend)?;
    let spec = env.backends.spec(&bot.backend);
    let in_task = task.is_some();

    let mut effective_toolsets = bot.toolsets.clone();
    if backend.kind() == BackendKind::Acp {
        // ACP runs fs/shell tools inside its own sandboxed harness per D2.5,
        // while keeping SwarmBot's Bot/Task toolsets scoped through MCP.
        effective_toolsets.retain(|toolset| *toolset != Toolset::Node);
    }
    let visible = env
        .tools
        .visible_specs(&effective_toolsets, Some(&node.capabilities()), in_task);

    let previous_elsewhere = session
        .last_conversation()
        .is_some_and(|last| last != &conversation.id);
    let turn_text = render_turn_text(
        env,
        bot,
        conversation,
        deliveries,
        task,
        &mut session,
        previous_elsewhere,
    )?;
    session.push_user(turn_text);
    session.note_conversation(&conversation.id);

    let memory_content = read_bot_memory(&workspace);
    let workspace_abs = std::fs::canonicalize(&workspace).unwrap_or_else(|_| workspace.clone());

    let mcp_registration: Option<McpTurnRegistration> = if backend.kind().owns_tool_loop() {
        let provider = env
            .mcp_provider
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match provider {
            Some(provider) => Some(provider.register_turn(McpTurnRequest {
                bot_id: bot_id.clone(),
                conversation_id: conversation.id.clone(),
                turn_id: turn.id.clone(),
                task_id: task.map(|t| t.id.clone()),
                workspace: workspace_abs.clone(),
                spool_dir: spool_dir.clone(),
                toolsets: effective_toolsets.clone(),
                node_caps: Some(node.capabilities()),
                in_task,
            })?),
            None => None,
        }
    } else {
        None
    };
    let mcp_servers = mcp_registration
        .as_ref()
        .map(|reg| reg.servers.clone())
        .unwrap_or_default();

    let placement = if node.id.is_local() {
        format!(
            "node `{}` (local); private workspace: `{}`",
            node.label,
            workspace_abs.display()
        )
    } else {
        format!("node `{}` (remote)", node.label)
    };
    let chat_cwd = node.id.is_local().then_some(workspace_abs);
    let prompt_tools: &[ToolSpec] = if !backend.kind().owns_tool_loop() || !mcp_servers.is_empty() {
        &visible
    } else {
        &[]
    };
    let instructions = env.instructions.render(
        bot,
        RenderOptions {
            task,
            tools: prompt_tools,
            memory_index: memory_content.as_deref(),
            placement: Some(&placement),
            messaging_enabled: settings.bot_messaging,
        },
    )?;

    let model = bot
        .model
        .clone()
        .or_else(|| spec.and_then(|s| s.model.clone()))
        .unwrap_or_else(|| "default".to_string());
    let context_window = spec
        .and_then(|s| s.options.get("context_window"))
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);

    let dm_addressed_to = if conversation.is_direct() {
        deliveries.iter().find_map(|d| {
            if d.kind == DeliveryKind::BotMessage {
                d.from_bot.clone()
            } else {
                None
            }
        })
    } else {
        None
    };

    let mut rounds = 0u32;
    let mut outcome = TurnOutcome::default();

    loop {
        rounds += 1;
        if rounds > settings.max_tool_rounds {
            let _ = env.transcript.notice(
                &conversation.id,
                format!(
                    "Stopped after {} tool rounds without an answer.",
                    settings.max_tool_rounds
                ),
                NoticeLevel::Warning,
            );
            break;
        }

        // Steers join the running turn between model rounds. A task session has
        // one piece of work and is never steered. Steers targeting other
        // conversations stay queued.
        if !in_task {
            for delivery in drain_matching_steers(steer, &conversation.id) {
                session.push_user(format!("[Added while you were working] {}", delivery.text));
                consumed_steers.push(delivery);
            }
        }

        let request = ChatRequest {
            model: model.clone(),
            instructions: instructions.clone(),
            messages: session.messages().to_vec(),
            tools: if backend.kind().owns_tool_loop() {
                Vec::new()
            } else {
                visible.clone()
            },
            mcp_servers: mcp_servers.clone(),
            max_tokens: None,
            temperature: None,
            extra_body: spec.map(|s| s.extra_body.clone()).unwrap_or_default(),
            extra_headers: spec.map(|s| s.extra_headers.clone()).unwrap_or_default(),
            cwd: chat_cwd.clone(),
        };

        let (event_tx, event_rx) = mpsc::channel(256);
        let renderer = StreamRenderer::new(
            Arc::clone(&env.transcript),
            conversation.id.clone(),
            bot_id.clone(),
            turn.id.clone(),
        );
        let (entry_tx, entry_rx) = oneshot::channel();
        tokio::spawn(stream_pump(renderer, event_rx, entry_tx));

        let response: ChatResponse = backend
            .chat(request, event_tx, turn_cancel.child_token())
            .await
            .map_err(|err| {
                if turn_cancel.is_cancelled() {
                    DomainError::conflict("stopped by the owner")
                } else {
                    DomainError::BackendUnavailable {
                        backend: bot.backend.clone(),
                        reason: err.to_string(),
                    }
                }
            })?;
        let streamed = entry_rx.await.unwrap_or_default();

        turn.usage.merge(response.usage);
        outcome.silent = is_no_reply(&response.text);
        outcome.text = response.text.clone();
        outcome.reply = finalize_reply(
            env,
            bot_id,
            conversation,
            streamed,
            &response.text,
            turn.id.clone(),
            dm_addressed_to.clone(),
        )?;

        if response.tool_calls.is_empty() {
            if !response.text.is_empty() && !outcome.silent {
                session.push(ChatMessage {
                    role: Role::Assistant,
                    content: response.text.clone(),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    name: None,
                });
            }
            if matches!(response.finish_reason, FinishReason::Length) {
                let _ = env.transcript.notice(
                    &conversation.id,
                    "The answer was cut off by the model's output limit.",
                    NoticeLevel::Warning,
                );
            }
            if !in_task && !turn_cancel.is_cancelled() && rounds < settings.max_tool_rounds {
                let mid_steers = drain_matching_steers(steer, &conversation.id);
                if !mid_steers.is_empty() {
                    for delivery in mid_steers {
                        session
                            .push_user(format!("[Added while you were working] {}", delivery.text));
                        consumed_steers.push(delivery);
                    }
                    continue;
                }
            }
            break;
        }
        if turn_cancel.is_cancelled() {
            break;
        }

        // The assistant message that asked for the tools, then each result.
        session.push(ChatMessage {
            role: Role::Assistant,
            content: response.text.clone(),
            tool_calls: response.tool_calls.clone(),
            tool_call_id: None,
            name: None,
        });

        for call in &response.tool_calls {
            if turn_cancel.is_cancelled() {
                break;
            }
            let call_id = ToolCallId::from_suffix(&stable_id(&call.id));
            let result_text = run_tool_call(
                env,
                bot_id,
                conversation,
                &node,
                turn,
                task,
                &workspace,
                &spool_dir,
                turn_cancel,
                &call_id,
                &call.name,
                call.arguments.clone(),
            )
            .await?;
            session.push(ChatMessage {
                role: Role::Tool,
                content: result_text,
                tool_calls: Vec::new(),
                tool_call_id: Some(call.id.clone()),
                name: None,
            });
            turn.usage.tool_calls += 1;
        }
    }

    drop(mcp_registration);
    let messaged_targets = env.hooks.take_turn_targets(&turn.id);

    // Wake the teammates a group reply mentioned, or forward a DM reply back
    // to a teammate that asked via `message_bot` when the bot only spoke text.
    if !outcome.text.is_empty() && !outcome.silent {
        if let ConversationKind::Group { members, .. } = &conversation.kind {
            let member_bots: Vec<Bot> = members
                .iter()
                .filter_map(|id| store.get_bot(id).ok().flatten())
                .collect();
            let already = already_addressed_in_group(
                store.as_ref(),
                bot_id,
                &member_bots,
                conversation,
                deliveries,
            );
            let woken = GroupRouter::wake_from_reply(&member_bots, &outcome.text, &already);
            if !woken.is_empty() {
                let hops = deliveries.iter().map(|d| d.causal.hops).max().unwrap_or(0);
                env.hooks.wake_mentioned(
                    conversation,
                    bot,
                    &outcome.text,
                    outcome.reply.as_ref(),
                    hops,
                    woken,
                );
            }
        } else if matches!(conversation.kind, ConversationKind::Direct { .. })
            && !in_task
            && !turn_cancel.is_cancelled()
        {
            let mut forwarded_to: HashSet<BotId> = HashSet::new();
            for delivery in deliveries.iter().chain(consumed_steers.iter()) {
                if delivery.kind != DeliveryKind::BotMessage {
                    continue;
                }
                let Some(sender_id) = delivery.from_bot.as_ref() else {
                    continue;
                };
                if sender_id == bot_id
                    || messaged_targets.contains(sender_id)
                    || !forwarded_to.insert(sender_id.clone())
                {
                    continue;
                }
                let Some(sender_bot) = store.get_bot(sender_id)? else {
                    continue;
                };
                // Forward back only when the DM reply explicitly addresses the
                // teammate (e.g. `@name`, `name:`, `name,`, or greeting). Plain
                // acknowledgements or third-person mentions stay local so turns
                // do not ping-pong.
                let addressed = GroupRouter::wake_from_reply(
                    std::slice::from_ref(&sender_bot),
                    &outcome.text,
                    std::slice::from_ref(bot_id),
                );
                if addressed.is_empty() {
                    continue;
                }
                let hops = deliveries
                    .iter()
                    .chain(consumed_steers.iter())
                    .filter(|d| {
                        d.kind == DeliveryKind::BotMessage && d.from_bot.as_ref() == Some(sender_id)
                    })
                    .map(|d| d.causal.hops)
                    .max()
                    .unwrap_or(delivery.causal.hops);
                env.hooks.forward_dm_reply(
                    conversation,
                    bot,
                    sender_id,
                    &outcome.text,
                    outcome.reply.as_ref().map(|e| e.id.clone()),
                    hops,
                );
            }
        }
    } else if outcome.silent {
        debug!(bot = %bot_id, "a group turn answered NO_REPLY");
        let _ = env.transcript.event(
            &conversation.id,
            "no-reply",
            format!("@{} had nothing to add", bot.name),
        );
    }

    // Keep a long-lived bot inside its budget.
    if session.needs_compaction(context_window, settings.compact_at_fraction) {
        let summary = compact_summary(&session.compaction_source());
        session.compact(summary);
    }
    session.save(store.as_ref())?;
    Ok(outcome)
}

/// One tool call: idempotency, a transcript card, the dispatch, and the result
/// the model gets back.
#[allow(clippy::too_many_arguments)]
async fn run_tool_call(
    env: &RunnerEnv,
    bot_id: &BotId,
    conversation: &Conversation,
    node: &NodeInfo,
    turn: &mut Turn,
    task: Option<&Task>,
    workspace: &Path,
    spool_dir: &Path,
    turn_cancel: &CancellationToken,
    call_id: &ToolCallId,
    name: &str,
    args: serde_json::Value,
) -> Result<String, DomainError> {
    let store = Arc::clone(&env.store);

    // A resumed harness may replay a call id; never run it twice.
    if let Some(record) = store.find_tool_call(&turn.id, call_id)? {
        return Ok(record.result_text);
    }

    let mut entry = env.transcript.tool_call(
        &conversation.id,
        Some(turn.id.clone()),
        call_id.clone(),
        name,
        args.clone(),
        Some(node.id.clone()),
        task.map(|t| t.id.clone()),
    )?;

    let started = Instant::now();
    let ctx = ToolContext {
        invocation: Invocation {
            id: swarmbot_contracts::id::InvocationId::new(),
            bot_id: bot_id.clone(),
            conversation_id: conversation.id.clone(),
            turn_id: Some(turn.id.clone()),
            task_id: task.map(|t| t.id.clone()),
            tool_call_id: Some(call_id.clone()),
            node_id: Some(node.id.clone()),
        },
        workspace: workspace.to_path_buf(),
        node_id: node.id.clone(),
        approval: Arc::clone(&env.approval),
        nodes: Arc::clone(&env.nodes),
        cancel: turn_cancel.clone(),
        spool_dir: spool_dir.to_path_buf(),
    };

    let request = ToolCallRequest {
        id: call_id.clone(),
        name: name.to_string(),
        args: args.clone(),
    };
    let result: ToolResult = env.tools.dispatch(&ctx, request).await;
    let duration = started.elapsed().as_millis() as u64;
    let text = result.text_only();
    let spill = result
        .meta
        .get("spill_path")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let status = if result.is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };
    env.transcript.finish_tool_call(
        &mut entry,
        status,
        Some(text.clone()),
        spill,
        Some(duration),
    )?;

    let _ = store.insert_tool_call(&ToolCallRecord {
        turn_id: turn.id.clone(),
        call_id: call_id.clone(),
        tool: name.to_string(),
        args,
        result_text: text.clone(),
        is_error: result.is_error,
        node_id: Some(node.id.clone()),
        created_at: chrono::Utc::now(),
    });
    Ok(text)
}

/// Build the text of a turn: where it came from, what it missed, and the
/// deliveries themselves, each attributed.
fn render_turn_text(
    env: &RunnerEnv,
    bot: &Bot,
    conversation: &Conversation,
    deliveries: &[Delivery],
    task: Option<&Task>,
    session: &mut Session,
    previous_elsewhere: bool,
) -> Result<String, DomainError> {
    let mut parts: Vec<String> = Vec::new();

    let (members, lead) = match &conversation.kind {
        ConversationKind::Group { members, lead } => (members.clone(), Some(lead.clone())),
        ConversationKind::Direct { .. } => (Vec::new(), None),
    };
    let mut member_names = Vec::new();
    for id in &members {
        if let Some(member) = env.store.get_bot(id)? {
            member_names.push(member.name);
        }
    }
    let lead_name = match &lead {
        Some(id) => env.store.get_bot(id)?.map(|b| b.name),
        None => None,
    };

    if let Some(task) = task {
        parts.push(format!("[Task \"{}\"]", task.title));
    }

    let from_bot_name = match deliveries.first().and_then(|d| d.from_bot.as_ref()) {
        Some(id) => env.store.get_bot(id)?.map(|b| b.name),
        None => None,
    };
    let owner_dm = conversation.is_direct()
        && deliveries
            .first()
            .is_some_and(|d| d.kind == DeliveryKind::OwnerMessage);
    let tag = origin_tag(
        conversation.title.as_deref(),
        &member_names,
        lead_name.as_deref(),
        from_bot_name.as_deref(),
        owner_dm,
        previous_elsewhere,
    );
    if !tag.is_empty() {
        parts.push(tag);
    }

    // Group catch-up: what this bot has not seen, oldest first, capped.
    if conversation.kind.is_group() {
        let seen = session.seen(&conversation.id);
        let unseen = env
            .transcript
            .since(&conversation.id, seen, CATCH_UP_LIMIT)?;
        let delivery_source_ids: HashSet<EntryId> = deliveries
            .iter()
            .filter_map(|d| d.source_entry_id.clone())
            .collect();
        let mut lines = Vec::new();
        let mut bytes = 0usize;
        let mut last_processed_seq = seen;
        let mut truncated = false;
        for entry in &unseen {
            if delivery_source_ids.contains(&entry.id) {
                last_processed_seq = last_processed_seq.max(entry.seq);
                continue;
            }
            let Some(message) = entry.as_message() else {
                last_processed_seq = last_processed_seq.max(entry.seq);
                continue;
            };
            if entry.author.bot_id() == Some(&bot.id) {
                last_processed_seq = last_processed_seq.max(entry.seq);
                continue; // never its own words
            }
            let author = match &entry.author {
                Author::Owner => "the owner".to_string(),
                Author::Bot { id } => env
                    .store
                    .get_bot(id)?
                    .map(|b| format!("@{}", b.name))
                    .unwrap_or_else(|| "@unknown".to_string()),
                Author::System => {
                    last_processed_seq = last_processed_seq.max(entry.seq);
                    continue;
                }
            };
            let line = format!("[{author}]: {}", message.text);
            if !lines.is_empty() && bytes + line.len() > CATCH_UP_BYTES {
                truncated = true;
                break;
            }
            bytes += line.len();
            last_processed_seq = last_processed_seq.max(entry.seq);
            lines.push(line);
        }
        if !lines.is_empty() {
            parts.push(format!(
                "[Messages in this group you have not seen]\n{}",
                lines.join("\n")
            ));
        }
        if last_processed_seq > seen {
            session.mark_seen(&conversation.id, last_processed_seq);
        }
        // A delivery's own entry is already in the transcript; mark the batch
        // seen only when catch-up fit completely so older unseen entries are
        // never leaped past.
        if !truncated {
            for delivery in deliveries {
                if let Some(entry_id) = &delivery.source_entry_id
                    && let Ok(Some(entry)) = env.store.get_entry(entry_id)
                {
                    session.mark_seen(&conversation.id, entry.seq);
                }
            }
        }
    }

    let is_group = conversation.kind.is_group();
    for (index, delivery) in deliveries.iter().enumerate() {
        let sender_name = match &delivery.from_bot {
            Some(id) => env.store.get_bot(id)?.map(|b| b.name),
            None => None,
        };
        let prefix = match (&delivery.kind, &delivery.from_bot, is_group) {
            (DeliveryKind::OwnerMessage, _, _) => String::new(),
            (DeliveryKind::BotMessage, Some(_), true) => match &sender_name {
                Some(name) => format!("[Message from @{name}] "),
                None => "[Message from a teammate] ".to_string(),
            },
            (DeliveryKind::BotMessage, Some(_), false) => match &sender_name {
                Some(name) => format!("[Direct message from @{name}] "),
                None => "[Direct message from a teammate] ".to_string(),
            },
            (DeliveryKind::Task, _, _) => String::new(),
            (DeliveryKind::TaskResult, _, _) => String::new(),
            (DeliveryKind::AsyncAnswer, _, _) => "[An answer to what you asked] ".to_string(),
            (DeliveryKind::Routine, _, _) => "[Routine run] ".to_string(),
            (DeliveryKind::SystemResume, _, _) => "[System] ".to_string(),
            (DeliveryKind::BotMessage, None, _) => String::new(),
        };
        // The first delivery reads as the request itself; anything batched
        // behind it is a list item.
        if deliveries.len() == 1 || index == 0 {
            parts.push(format!("{prefix}{}", delivery.text));
        } else {
            parts.push(format!("- {prefix}{}", delivery.text));
        }
    }

    Ok(parts.join("\n\n"))
}

fn drain_matching_steers(
    steer: &Arc<Mutex<VecDeque<Delivery>>>,
    conversation_id: &ConversationId,
) -> Vec<Delivery> {
    let mut pending = steer.lock().unwrap_or_else(|e| e.into_inner());
    let mut kept = VecDeque::with_capacity(pending.len());
    let mut matching = Vec::new();
    while let Some(delivery) = pending.pop_front() {
        if &delivery.conversation_id == conversation_id {
            matching.push(delivery);
        } else {
            kept.push_back(delivery);
        }
    }
    *pending = kept;
    matching
}

fn already_addressed_in_group(
    store: &dyn Persistence,
    self_bot_id: &BotId,
    member_bots: &[Bot],
    conversation: &Conversation,
    deliveries: &[Delivery],
) -> Vec<BotId> {
    let mut excluded: HashSet<BotId> = HashSet::new();
    excluded.insert(self_bot_id.clone());
    let lead = match &conversation.kind {
        ConversationKind::Group { lead, .. } => lead,
        ConversationKind::Direct { .. } => return excluded.into_iter().collect(),
    };
    for delivery in deliveries {
        let Some(entry_id) = &delivery.source_entry_id else {
            continue;
        };
        let Ok(Some(entry)) = store.get_entry(entry_id) else {
            continue;
        };
        let Some(message) = entry.as_message() else {
            continue;
        };
        if let Some(target) = &message.addressed_to
            && target != self_bot_id
        {
            excluded.insert(target.clone());
        }
        if entry.author.is_owner() {
            let decision = GroupRouter::route(member_bots, &message.text, &[], None, lead);
            if matches!(
                decision.reason,
                RouteReason::Mention | RouteReason::Everyone | RouteReason::Addressed
            ) {
                for target in decision.targets {
                    if target != *self_bot_id {
                        excluded.insert(target);
                    }
                }
            }
        }
    }
    excluded.into_iter().collect()
}

/// Turn the streamed entry into the final reply, or drop it when the answer was
/// `NO_REPLY` or empty.
fn finalize_reply(
    env: &RunnerEnv,
    bot_id: &BotId,
    conversation: &Conversation,
    streamed: StreamedEntries,
    text: &str,
    turn_id: TurnId,
    addressed_to: Option<BotId>,
) -> Result<Option<Entry>, DomainError> {
    if text.trim().is_empty() || is_no_reply(text) {
        return Ok(None);
    }
    let mut entry = match streamed.message {
        Some(entry) => entry,
        None => env
            .transcript
            .begin_bot_message(&conversation.id, bot_id, turn_id)?,
    };
    if let EntryKind::Message(message) = &mut entry.kind {
        message.text = text.to_string();
        message.post = PostState::Posted;
        if message.addressed_to.is_none() {
            message.addressed_to = addressed_to;
        }
    }
    entry.revision += 1;
    entry.updated_at = chrono::Utc::now();
    env.transcript.update(&entry)?;
    Ok(Some(entry))
}

fn publish_status(env: &RunnerEnv, bot_id: &BotId, status: BotStatus) {
    env.hub.publish(Change::BotStatus {
        bot_id: bot_id.clone(),
        status: status.clone(),
    });
    if let Ok(Some(mut bot)) = env.store.get_bot(bot_id)
        && bot.status != status
    {
        bot.status = status;
        bot.updated_at = chrono::Utc::now();
        if env.store.update_bot(&bot).is_ok() {
            env.hub.publish(Change::Bot(bot));
        }
    }
}

/// The entries a streaming turn created, handed back when the stream ends.
#[derive(Clone, Debug, Default)]
pub struct StreamedEntries {
    pub message: Option<Entry>,
    pub reasoning: Option<Entry>,
}

/// Renders deltas into transcript entries, throttled, and holds back anything
/// that could still turn out to be `NO_REPLY`.
struct StreamRenderer {
    transcript: Arc<Transcript>,
    conversation: ConversationId,
    bot: BotId,
    turn: TurnId,
    message: Option<Entry>,
    reasoning: Option<Entry>,
    text: String,
    reasoning_text: String,
    last_flush: Instant,
    pending_bytes: usize,
}

impl StreamRenderer {
    fn new(
        transcript: Arc<Transcript>,
        conversation: ConversationId,
        bot: BotId,
        turn: TurnId,
    ) -> Self {
        Self {
            transcript,
            conversation,
            bot,
            turn,
            message: None,
            reasoning: None,
            text: String::new(),
            reasoning_text: String::new(),
            last_flush: Instant::now(),
            pending_bytes: 0,
        }
    }

    /// True while the text so far is still a prefix of `NO_REPLY`: posting then
    /// unposting would be worse than waiting one round trip.
    fn holding_back(&self) -> bool {
        let upper = self.text.trim().to_uppercase();
        !upper.is_empty() && is_no_reply_prefix(&upper)
    }

    fn on_event(&mut self, event: LlmEvent) {
        match event {
            LlmEvent::TextDelta(delta) => {
                self.text.push_str(&delta);
                self.pending_bytes += delta.len();
                self.maybe_flush();
            }
            LlmEvent::ReasoningDelta(delta) => {
                self.reasoning_text.push_str(&delta);
                self.maybe_flush();
            }
            LlmEvent::ToolCallStart { .. } | LlmEvent::ToolCallArgsDelta { .. } => self.flush(),
            LlmEvent::Finished { .. } => self.flush(),
        }
    }

    fn maybe_flush(&mut self) {
        if self.pending_bytes >= STREAM_FLUSH_CHARS
            || self.last_flush.elapsed() >= STREAM_FLUSH_INTERVAL
        {
            self.flush();
        }
    }

    fn flush(&mut self) {
        self.pending_bytes = 0;
        self.last_flush = Instant::now();

        if !self.reasoning_text.is_empty() {
            match &mut self.reasoning {
                None => match self.transcript.reasoning(
                    &self.conversation,
                    &self.bot,
                    self.reasoning_text.clone(),
                    Some(self.turn.clone()),
                ) {
                    Ok(entry) => self.reasoning = Some(entry),
                    Err(err) => warn!(error = %err, "cannot record reasoning"),
                },
                Some(entry) => {
                    if let EntryKind::Reasoning(reasoning) = &mut entry.kind {
                        reasoning.text = self.reasoning_text.clone();
                    }
                    entry.revision += 1;
                    entry.updated_at = chrono::Utc::now();
                    if let Err(err) = self.transcript.update(entry) {
                        warn!(error = %err, "cannot update reasoning");
                    }
                }
            }
        }

        if self.text.is_empty() || self.holding_back() {
            return;
        }
        match &mut self.message {
            None => match self.transcript.begin_bot_message(
                &self.conversation,
                &self.bot,
                self.turn.clone(),
            ) {
                Ok(mut entry) => {
                    if let EntryKind::Message(message) = &mut entry.kind {
                        message.text = self.text.clone();
                    }
                    entry.revision += 1;
                    match self.transcript.update(&entry) {
                        Ok(()) => self.message = Some(entry),
                        Err(err) => warn!(error = %err, "cannot write the first streamed chunk"),
                    }
                }
                Err(err) => warn!(error = %err, "cannot start a streamed reply"),
            },
            Some(entry) => {
                if let EntryKind::Message(message) = &mut entry.kind {
                    message.text = self.text.clone();
                }
                entry.revision += 1;
                entry.updated_at = chrono::Utc::now();
                if let Err(err) = self.transcript.update(entry) {
                    warn!(error = %err, "cannot update a streamed reply");
                }
            }
        }
    }

    fn finish(mut self) -> StreamedEntries {
        self.flush();
        StreamedEntries {
            message: self.message,
            reasoning: self.reasoning,
        }
    }
}

fn is_no_reply_prefix(upper: &str) -> bool {
    crate::router::NO_REPLY.starts_with(upper)
}

async fn stream_pump(
    mut renderer: StreamRenderer,
    mut rx: mpsc::Receiver<LlmEvent>,
    tx: oneshot::Sender<StreamedEntries>,
) {
    while let Some(event) = rx.recv().await {
        renderer.on_event(event);
    }
    let _ = tx.send(renderer.finish());
}

fn initiator_of(delivery: &Delivery, task: Option<&Task>) -> TurnInitiator {
    if let Some(task) = task {
        return TurnInitiator::Task {
            task_id: task.id.clone(),
        };
    }
    match delivery.kind {
        DeliveryKind::OwnerMessage => TurnInitiator::Owner,
        DeliveryKind::BotMessage => TurnInitiator::Bot {
            bot_id: delivery
                .from_bot
                .clone()
                .unwrap_or_else(|| delivery.bot_id.clone()),
        },
        DeliveryKind::Routine => delivery
            .routine_id
            .clone()
            .map_or(TurnInitiator::System, |routine_id| TurnInitiator::Routine {
                routine_id,
            }),
        DeliveryKind::Task => task
            .map(|t| TurnInitiator::Task {
                task_id: t.id.clone(),
            })
            .unwrap_or(TurnInitiator::System),
        DeliveryKind::TaskResult => TurnInitiator::System,
        DeliveryKind::AsyncAnswer => TurnInitiator::Request,
        DeliveryKind::SystemResume => TurnInitiator::System,
    }
}

fn activity_of(delivery: &Delivery) -> String {
    match delivery.kind {
        DeliveryKind::OwnerMessage => "Answering the owner",
        DeliveryKind::BotMessage => "Answering a teammate",
        DeliveryKind::Routine => "Running a routine",
        DeliveryKind::Task => "Working on a task",
        DeliveryKind::TaskResult => "Reading a task result",
        DeliveryKind::AsyncAnswer => "Reading an answer",
        DeliveryKind::SystemResume => "Resuming",
    }
    .to_string()
}

/// Tool-call ids come from the model; fold them into our id shape without
/// losing the one-to-one relation (so idempotency still holds).
fn stable_id(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '0' })
        .take(32)
        .collect();
    format!("{cleaned:0<32}")
}

/// A compaction summary without spending a model call: the head and tail of
/// what is being folded away. Honest about the gap in the middle.
pub fn compact_summary(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    if lines.len() <= 12 {
        return source.to_string();
    }
    let head: Vec<&str> = lines.iter().take(6).copied().collect();
    let mut tail: Vec<&str> = lines.iter().rev().take(6).copied().collect();
    tail.reverse();
    format!(
        "{}\n… ({} earlier lines folded away) …\n{}",
        head.join("\n"),
        lines.len() - 12,
        tail.join("\n")
    )
}

/// Read long-term memory for a bot from its workspace (`MEMORY.md` or `memory.md`).
fn read_bot_memory(workspace: &Path) -> Option<String> {
    for name in &["MEMORY.md", "memory.md"] {
        let path = workspace.join(name);
        if let Ok(content) = std::fs::read_to_string(&path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use swarmbot_contracts::config::{BackendSpec, BrainConfig};
    use swarmbot_contracts::delivery::CausalChain;
    use swarmbot_contracts::id::DeliveryId;

    fn test_swarm() -> (Arc<crate::swarm::Swarm>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut config = BrainConfig {
            default_backend: "fake".to_string(),
            backends: vec![BackendSpec::fake("fake")],
            ..BrainConfig::default()
        };
        config.team = Some(BrainConfig::demo_team("fake"));
        let swarm =
            crate::compose::memory(config, dir.path(), vec![], &BTreeMap::new()).expect("swarm");
        swarm.provision_team().expect("provision");
        (swarm, dir)
    }

    #[test]
    fn drain_matching_steers_preserves_other_conversations() {
        let conv_a = ConversationId::new();
        let conv_b = ConversationId::new();
        let bot = BotId::new();
        let steer = Arc::new(Mutex::new(VecDeque::from([
            Delivery::owner_message(conv_a.clone(), bot.clone(), "a1"),
            Delivery::owner_message(conv_b.clone(), bot.clone(), "b1"),
            Delivery::owner_message(conv_a.clone(), bot.clone(), "a2"),
        ])));

        let drained = drain_matching_steers(&steer, &conv_a);
        let texts: Vec<&str> = drained.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(texts, vec!["a1", "a2"]);

        let remaining: Vec<String> = steer
            .lock()
            .unwrap()
            .iter()
            .map(|d| d.text.clone())
            .collect();
        assert_eq!(remaining, vec!["b1".to_string()]);
    }

    #[test]
    fn render_turn_text_respects_catch_up_bytes_without_skipping_unshown_entries() {
        let (swarm, _dir) = test_swarm();
        let env = swarm.runner_env();
        let lead = swarm.bots.by_name("lead").unwrap();
        let scout = swarm.bots.by_name("scout").unwrap();
        let group = swarm
            .conversations
            .find_by_title("Team")
            .unwrap()
            .expect("Team group");

        let chunk_1 = format!("MSG_ONE {}", "a".repeat(3500));
        let chunk_2 = format!("MSG_TWO {}", "b".repeat(3500));
        let chunk_3 = format!("MSG_THREE {}", "c".repeat(3500));
        let e1 = swarm.transcript.owner_message(&group.id, &chunk_1).unwrap();
        let e2 = swarm.transcript.owner_message(&group.id, &chunk_2).unwrap();
        let e3 = swarm.transcript.owner_message(&group.id, &chunk_3).unwrap();
        let trigger = swarm
            .transcript
            .bot_message(
                &group.id,
                &lead.id,
                "@scout please check",
                None,
                Some(scout.id.clone()),
            )
            .unwrap();

        let mut delivery = Delivery::bot_message(
            group.id.clone(),
            lead.id.clone(),
            scout.id.clone(),
            "please check".to_string(),
            CausalChain::new(DeliveryId::new()),
            Urgency::Background,
        );
        delivery.source_entry_id = Some(trigger.id.clone());

        let mut session = Session::load(swarm.store.as_ref(), &scout.id).unwrap();
        let first_prompt = render_turn_text(
            &env,
            &scout,
            &group,
            std::slice::from_ref(&delivery),
            None,
            &mut session,
            false,
        )
        .unwrap();

        assert!(first_prompt.contains("MSG_ONE"));
        assert!(first_prompt.contains("MSG_TWO"));
        assert!(!first_prompt.contains("MSG_THREE"));
        assert!(first_prompt.contains("[Message from @lead] please check"));
        assert!(!first_prompt.contains("[Direct message from @lead]"));
        assert_eq!(
            session.seen(&group.id),
            e2.seq,
            "mark_seen must stop at the last message shown ({}) and never jump past e3 ({})",
            e2.seq,
            e3.seq
        );
        assert!(session.seen(&group.id) > e1.seq);

        // Next turn sees MSG_THREE without anything dropped, and does not
        // duplicate `trigger` inside catch-up.
        let second_prompt = render_turn_text(
            &env,
            &scout,
            &group,
            std::slice::from_ref(&delivery),
            None,
            &mut session,
            false,
        )
        .unwrap();
        assert!(second_prompt.contains("MSG_THREE"));
        assert_eq!(
            second_prompt.matches("please check").count(),
            1,
            "triggering delivery must appear once, not in catch-up: {second_prompt}"
        );
        assert_eq!(session.seen(&group.id), trigger.seq);
    }

    #[test]
    fn render_turn_text_resolves_bot_sender_per_delivery_in_batch() {
        let (swarm, _dir) = test_swarm();
        let env = swarm.runner_env();
        let lead = swarm.bots.by_name("lead").unwrap();
        let dev = swarm.bots.by_name("dev").unwrap();
        let scout = swarm.bots.by_name("scout").unwrap();
        let group = swarm
            .conversations
            .find_by_title("Team")
            .unwrap()
            .expect("Team group");

        let d1 = Delivery::bot_message(
            group.id.clone(),
            lead.id.clone(),
            scout.id.clone(),
            "first note".to_string(),
            CausalChain::new(DeliveryId::new()),
            Urgency::Background,
        );
        let d2 = Delivery::bot_message(
            group.id.clone(),
            dev.id.clone(),
            scout.id.clone(),
            "second note".to_string(),
            CausalChain::new(DeliveryId::new()),
            Urgency::Background,
        );

        let mut session = Session::load(swarm.store.as_ref(), &scout.id).unwrap();
        let prompt =
            render_turn_text(&env, &scout, &group, &[d1, d2], None, &mut session, false).unwrap();
        assert!(prompt.contains("[Message from @lead] first note"));
        assert!(prompt.contains("[Message from @dev] second note"));
        assert!(!prompt.contains("[Direct message from"));
    }

    #[tokio::test]
    async fn runner_cmd_steer_for_another_conversation_is_not_dropped() {
        use std::sync::atomic::AtomicUsize;
        use tokio::sync::Notify;

        struct GateBackend {
            started: Arc<Notify>,
            release: Arc<Notify>,
            calls: AtomicUsize,
            prompts: Mutex<Vec<String>>,
        }

        #[async_trait::async_trait]
        impl LlmBackend for GateBackend {
            fn id(&self) -> &str {
                "gate"
            }

            fn kind(&self) -> BackendKind {
                BackendKind::Fake
            }

            fn capabilities(&self) -> swarmbot_llm::Capabilities {
                swarmbot_llm::Capabilities {
                    streaming: false,
                    tool_calls: false,
                    reasoning: false,
                    steer: true,
                    survives_restart: false,
                }
            }

            async fn chat(
                &self,
                request: ChatRequest,
                events: mpsc::Sender<LlmEvent>,
                _cancel: CancellationToken,
            ) -> Result<ChatResponse, swarmbot_llm::LlmError> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                let user_text = request
                    .messages
                    .iter()
                    .filter(|m| m.role == Role::User)
                    .map(|m| m.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                self.prompts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(user_text);
                if n == 0 {
                    self.started.notify_one();
                    self.release.notified().await;
                }
                let mut response = ChatResponse::empty();
                response.text = format!("done-{n}");
                let _ = events
                    .send(LlmEvent::Finished {
                        finish_reason: FinishReason::Stop,
                        usage: response.usage,
                        text: response.text.clone(),
                        reasoning: String::new(),
                        tool_calls: Vec::new(),
                    })
                    .await;
                Ok(response)
            }

            async fn probe(&self) -> Result<swarmbot_llm::BackendStatus, swarmbot_llm::LlmError> {
                Ok(swarmbot_llm::BackendStatus {
                    backend: "gate".to_string(),
                    ok: true,
                    detail: "ok".to_string(),
                    model: None,
                    version: None,
                })
            }
        }

        let (swarm, _dir) = test_swarm();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let gate = Arc::new(GateBackend {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            calls: AtomicUsize::new(0),
            prompts: Mutex::new(Vec::new()),
        });
        swarm
            .backends
            .register("gate", Arc::clone(&gate) as Arc<dyn LlmBackend>);

        let mut lead = swarm.bots.by_name("lead").unwrap();
        lead.backend = "gate".to_string();
        swarm.bots.update(&lead).unwrap();
        swarm.ensure_runners().unwrap();

        let group = swarm
            .conversations
            .find_by_title("Team")
            .unwrap()
            .expect("Team group");

        swarm
            .send(&lead.direct_conversation_id, "first dm")
            .unwrap();
        started.notified().await;

        // Simulate a TOCTOU race where `RunnerCmd::Steer` for ANOTHER
        // conversation lands directly in the running turn's `steer` buffer.
        let mut other_steer =
            Delivery::owner_message(group.id.clone(), lead.id.clone(), "steer-other-conv");
        other_steer.urgency = Urgency::Urgent;
        swarm.store.insert_delivery(&other_steer).unwrap();
        let handle = swarm.runners.get(&lead.id).unwrap();
        handle.cmd.send(RunnerCmd::Steer(other_steer)).unwrap();

        release.notify_one();
        assert!(swarm.wait_for_idle(Duration::from_secs(5)).await);

        {
            let prompts = gate.prompts.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(prompts.len(), 2, "second turn must run the preserved steer");
            assert!(prompts[1].contains("steer-other-conv"), "{prompts:?}");
        }

        swarm.shutdown().await;
    }
}
