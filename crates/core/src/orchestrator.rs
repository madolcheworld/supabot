//! The orchestrator: what the owner's messages enter through, and the thing
//! that ties routing, deliveries, interrupts and recovery together.
//!
//! It is also the [`RunnerHooks`] implementation, which is how a runner wakes
//! mentioned teammates and settles tasks without knowing either service exists.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::Utc;
use tracing::{debug, info, warn};

use serde_json::Value;
use swarmbot_contracts::Seq;
use swarmbot_contracts::bot::{Bot, BotStatus};
use swarmbot_contracts::conversation::{Conversation, ConversationKind};
use swarmbot_contracts::delivery::{CausalChain, Delivery};
use swarmbot_contracts::entry::{Entry, PostState};
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{BotId, ConversationId, EntryId, RequestId, TaskId, TurnId};
use swarmbot_contracts::repo::{Persistence, RecoveryReport, recover};
use swarmbot_contracts::request::{Decision, Responder, RuntimeRequest};

use crate::approval::ApprovalService;
use crate::bus::{Change, LiveHub};
use crate::messaging::{Messaging, SendContext};
use crate::router::{GroupRouter, RouteDecision};
use crate::runners::{RunnerHooks, RunnerMap};
use crate::settings::SettingsHandle;
use crate::tasks::BackgroundTasks;
use crate::transcript::Transcript;

pub struct Orchestrator {
    store: Arc<dyn Persistence>,
    hub: Arc<LiveHub>,
    transcript: Arc<Transcript>,
    settings: Arc<SettingsHandle>,
    runners: Arc<RunnerMap>,
    messaging: Arc<Messaging>,
    tasks: Arc<BackgroundTasks>,
    approval: Arc<ApprovalService>,
}

/// What one owner message did.
#[derive(Clone, Debug)]
pub struct Sent {
    pub entry: Entry,
    pub route: RouteDecision,
    /// Bots that got the message but are busy elsewhere: their copy waits.
    pub queued: Vec<BotId>,
}

impl Orchestrator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn Persistence>,
        hub: Arc<LiveHub>,
        transcript: Arc<Transcript>,
        settings: Arc<SettingsHandle>,
        runners: Arc<RunnerMap>,
        messaging: Arc<Messaging>,
        tasks: Arc<BackgroundTasks>,
        approval: Arc<ApprovalService>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            hub,
            transcript,
            settings,
            runners,
            messaging,
            tasks,
            approval,
        })
    }

    /// The owner typed something. Route it, write it, and wake whoever it is for.
    pub fn send_owner_message(
        &self,
        conversation_id: &ConversationId,
        text: &str,
        addressed: &[BotId],
        reply_to: Option<&EntryId>,
    ) -> Result<Sent, DomainError> {
        if text.trim().is_empty() {
            return Err(DomainError::invalid("a message cannot be empty"));
        }
        let conversation = self
            .store
            .get_conversation(conversation_id)?
            .ok_or_else(|| swarmbot_contracts::not_found::conversation(conversation_id))?;

        let reply_author = match reply_to {
            Some(id) => self
                .store
                .get_entry(id)?
                .and_then(|entry| entry.author.bot_id().cloned()),
            None => None,
        };

        let route = self.route(&conversation, text, addressed, reply_author.as_ref())?;
        if route.targets.is_empty() {
            return Err(DomainError::conflict(
                "nobody in this conversation can answer (are its bots disabled?)",
            ));
        }

        let entry = self.transcript.owner_message(conversation_id, text)?;

        let mut queued = Vec::new();
        for target in &route.targets {
            let Some(bot) = self.store.get_bot(target)? else {
                continue;
            };
            if matches!(bot.status, BotStatus::Disabled) {
                let _ = self.transcript.event(
                    conversation_id,
                    "bot-disabled",
                    format!("@{} is disabled and was not woken", bot.name),
                );
                continue;
            }
            let mut delivery =
                Delivery::owner_message(conversation_id.clone(), bot.id.clone(), text);
            delivery.source_entry_id = Some(entry.id.clone());
            delivery.causal = CausalChain::new(delivery.id.clone());
            self.store.insert_delivery(&delivery)?;

            // Busy in another conversation: the owner sees it waiting.
            let busy_elsewhere = self
                .runners
                .active_turn(&bot.id)
                .is_some_and(|active| &active != conversation_id);
            if busy_elsewhere {
                queued.push(bot.id.clone());
            }
            if let Err(err) = self.runners.deliver(delivery) {
                warn!(error = %err, bot = %bot.name, "cannot hand a message to its bot");
            }
        }

        if !queued.is_empty() {
            let mut updated = entry.clone();
            if let Some(message) = updated.as_message_mut() {
                message.post = PostState::Queued;
            }
            updated.revision += 1;
            updated.updated_at = Utc::now();
            self.transcript.update(&updated)?;
        }

        Ok(Sent {
            entry,
            route,
            queued,
        })
    }

    /// Who a message is for.
    pub fn route(
        &self,
        conversation: &Conversation,
        text: &str,
        addressed: &[BotId],
        reply_to: Option<&BotId>,
    ) -> Result<RouteDecision, DomainError> {
        match &conversation.kind {
            ConversationKind::Direct { bot_id } => Ok(RouteDecision {
                targets: vec![bot_id.clone()],
                reason: crate::router::RouteReason::Addressed,
                handles: Vec::new(),
            }),
            ConversationKind::Group { members, lead } => {
                let mut bots = Vec::new();
                for id in members {
                    if let Some(bot) = self.store.get_bot(id)? {
                        bots.push(bot);
                    }
                }
                Ok(GroupRouter::route(&bots, text, addressed, reply_to, lead))
            }
        }
    }

    /// Stop a conversation's running turn and drop its queued deliveries.
    pub fn interrupt(&self, conversation_id: &ConversationId) -> Result<bool, DomainError> {
        let conversation = self
            .store
            .get_conversation(conversation_id)?
            .ok_or_else(|| swarmbot_contracts::not_found::conversation(conversation_id))?;
        let mut stopped = false;
        for bot_id in conversation.kind.participants() {
            if self.runners.interrupt(&bot_id, Some(conversation_id)) {
                stopped = true;
            }
        }
        if stopped {
            let _ = self
                .transcript
                .event(conversation_id, "stopped", "Stopped by the owner");
        }
        Ok(stopped)
    }

    /// The conversation a bot's running turn belongs to — what the tool gateway
    /// uses to place a card next to the work.
    pub fn active_turn(&self, bot: &BotId) -> Option<ConversationId> {
        self.runners.active_turn(bot)
    }

    /// Answer a card. The service does the work; this is the API's one call.
    pub fn respond_request(
        &self,
        id: &RequestId,
        decision: Option<Decision>,
        answer: Option<Value>,
    ) -> Result<RuntimeRequest, DomainError> {
        self.approval
            .respond(id, decision, answer, Responder::Owner)
    }

    /// Boot: settle what a restart cut off, then let tasks run again.
    pub fn boot(&self) -> Result<RecoveryReport, DomainError> {
        let report = recover(self.store.as_ref(), Utc::now())?;
        if !report.is_empty() {
            info!(summary = %report.summary(), "boot recovery");
            self.tasks.recover(&report)?;
        }
        // A roster that says "working" after a restart is a lie.
        for mut bot in self.store.list_bots()? {
            if bot.status.is_busy() {
                bot.status = BotStatus::Idle;
                bot.updated_at = Utc::now();
                self.store.update_bot(&bot)?;
                self.hub.publish(Change::Bot(bot));
            }
        }
        Ok(report)
    }

    /// A graceful stop: no turn is left marked running.
    pub async fn shutdown(&self) {
        self.runners.shutdown().await;
        let now = Utc::now();
        for mut turn in self.store.running_turns().unwrap_or_default() {
            turn.interrupt(now, "the brain stopped");
            let _ = self.store.update_turn(&turn);
        }
        for mut bot in self.store.list_bots().unwrap_or_default() {
            if bot.status.is_busy() {
                bot.status = BotStatus::Idle;
                let _ = self.store.update_bot(&bot);
            }
        }
        debug!("orchestrator stopped");
    }

    // -- accessors used by the API layer -------------------------------------

    pub fn store(&self) -> Arc<dyn Persistence> {
        Arc::clone(&self.store)
    }
    pub fn hub(&self) -> Arc<LiveHub> {
        Arc::clone(&self.hub)
    }
    pub fn transcript(&self) -> Arc<Transcript> {
        Arc::clone(&self.transcript)
    }
    pub fn messaging(&self) -> Arc<Messaging> {
        Arc::clone(&self.messaging)
    }
    pub fn tasks(&self) -> Arc<BackgroundTasks> {
        Arc::clone(&self.tasks)
    }
    pub fn approval(&self) -> Arc<ApprovalService> {
        Arc::clone(&self.approval)
    }
    pub fn runners(&self) -> Arc<RunnerMap> {
        Arc::clone(&self.runners)
    }
    pub fn settings(&self) -> Arc<SettingsHandle> {
        Arc::clone(&self.settings)
    }

    /// History paging for the API and the CLI.
    pub fn history(
        &self,
        conversation: &ConversationId,
        after_seq: Seq,
        limit: u32,
    ) -> Result<Vec<Entry>, DomainError> {
        self.store.entries_after(conversation, after_seq, limit)
    }

    /// A bot the API can name: by id, by handle or by label.
    pub fn bot(&self, key: &str) -> Result<Bot, DomainError> {
        if let Ok(id) = BotId::parse(key)
            && let Some(bot) = self.store.get_bot(&id)?
        {
            return Ok(bot);
        }
        self.messaging.resolve_bot(key)
    }
}

impl RunnerHooks for Orchestrator {
    fn wake_mentioned(
        &self,
        conversation: &Conversation,
        from: &Bot,
        text: &str,
        entry: Option<&Entry>,
        hops: u8,
        woken: Vec<BotId>,
    ) {
        self.messaging
            .wake_mentioned(conversation, from, text, entry, hops, woken);
    }

    fn settle_task(&self, task_id: &TaskId, result: &str, failed: bool) {
        if let Err(err) = self.tasks.settle(task_id, result, failed) {
            warn!(error = %err, task = %task_id, "cannot settle a task");
        }
        // A finished task may free room for a queued one.
        if let Err(err) = self.tasks.pump() {
            debug!(error = %err, "cannot pump the task queue");
        }
    }

    fn take_turn_targets(&self, turn_id: &TurnId) -> HashSet<BotId> {
        self.messaging.take_turn_targets(turn_id)
    }

    fn forward_dm_reply(
        &self,
        conversation: &Conversation,
        from: &Bot,
        to: &BotId,
        text: &str,
        source_entry: Option<EntryId>,
        hops: u8,
    ) {
        if !self.settings.get().bot_messaging {
            return;
        }
        let ctx = SendContext {
            from: from.clone(),
            conversation: conversation.clone(),
            hops,
            source_entry,
        };
        // The answer is forwarded once as a `BotReply`: the receiving turn has
        // been answered and never forwards one back, which is what ends the
        // exchange (D4.8).
        if let Err(err) = self.messaging.forward_answer(&ctx, to, text) {
            warn!(
                error = %err,
                from = %from.name,
                to = %to,
                "cannot forward DM answer back to teammate"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;
    use swarmbot_contracts::turn::{Turn, TurnInitiator, TurnStatus};

    fn rig() -> (Arc<Orchestrator>, Arc<dyn Persistence>, Bot, Conversation) {
        let fixture = testing::Fixture::memory();
        let lead = fixture.bot("lead");
        let scout = fixture.bot("scout");
        let group = fixture.group("Team", &["lead", "scout"]);
        let settings = SettingsHandle::new(fixture.store.clone());
        let runners = RunnerMap::new();
        let tasks = BackgroundTasks::new(
            fixture.store.clone(),
            fixture.hub.clone(),
            fixture.transcript.clone(),
            settings.clone(),
            runners.clone(),
        );
        let messaging = Messaging::new(
            fixture.store.clone(),
            fixture.transcript.clone(),
            settings.clone(),
            runners.clone(),
            tasks.clone(),
        );
        let approval = ApprovalService::new(
            fixture.store.clone(),
            fixture.hub.clone(),
            fixture.transcript.clone(),
            settings.clone(),
            runners.clone(),
        );
        let orchestrator = Orchestrator::new(
            fixture.store.clone(),
            fixture.hub.clone(),
            fixture.transcript.clone(),
            settings,
            runners,
            messaging,
            tasks,
            approval,
        );
        let _ = scout;
        (orchestrator, fixture.store, lead, group)
    }

    #[test]
    fn a_dm_goes_to_its_bot() {
        let (orchestrator, store, lead, _) = rig();
        let sent = orchestrator
            .send_owner_message(&lead.direct_conversation_id, "hello", &[], None)
            .unwrap();
        assert_eq!(sent.route.targets, vec![lead.id.clone()]);
        assert!(sent.queued.is_empty());
        let deliveries = store.queued_deliveries(&lead.id).unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].text, "hello");
        assert_eq!(deliveries[0].source_entry_id, Some(sent.entry.id.clone()));
    }

    #[test]
    fn an_empty_message_is_refused() {
        let (orchestrator, _, lead, _) = rig();
        let err = orchestrator
            .send_owner_message(&lead.direct_conversation_id, "   ", &[], None)
            .unwrap_err();
        assert_eq!(err.code(), "invalid_input");
    }

    #[test]
    fn a_group_message_routes_to_the_lead_when_nobody_is_named() {
        let (orchestrator, store, lead, group) = rig();
        let sent = orchestrator
            .send_owner_message(&group.id, "what now?", &[], None)
            .unwrap();
        assert_eq!(sent.route.reason, crate::router::RouteReason::Lead);
        assert_eq!(sent.route.targets, vec![lead.id.clone()]);
        assert_eq!(store.queued_deliveries(&lead.id).unwrap().len(), 1);
    }

    #[test]
    fn a_mention_routes_to_the_named_bot() {
        let (orchestrator, store, _, group) = rig();
        let sent = orchestrator
            .send_owner_message(&group.id, "@scout find it", &[], None)
            .unwrap();
        assert_eq!(sent.route.reason, crate::router::RouteReason::Mention);
        let scout = store.bot_by_name("scout").unwrap().unwrap();
        assert_eq!(sent.route.targets, vec![scout.id.clone()]);
        assert_eq!(store.queued_deliveries(&scout.id).unwrap().len(), 1);
    }

    #[test]
    fn everyone_wakes_everyone() {
        let (orchestrator, _, _, group) = rig();
        let sent = orchestrator
            .send_owner_message(&group.id, "@everyone standup", &[], None)
            .unwrap();
        assert_eq!(sent.route.reason, crate::router::RouteReason::Everyone);
        assert_eq!(sent.route.targets.len(), 2);
    }

    #[test]
    fn a_disabled_bot_is_not_woken_but_the_owner_is_told() {
        let (orchestrator, store, lead, _) = rig();
        let mut disabled = lead.clone();
        disabled.status = BotStatus::Disabled;
        store.update_bot(&disabled).unwrap();
        let sent = orchestrator
            .send_owner_message(&lead.direct_conversation_id, "hello?", &[], None)
            .unwrap();
        assert!(store.queued_deliveries(&lead.id).unwrap().is_empty());
        let entries = store.entry_tail(&lead.direct_conversation_id, 10).unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.kind.text().unwrap_or_default().contains("disabled"))
        );
        assert_eq!(sent.route.targets.len(), 1);
    }

    #[test]
    fn boot_marks_a_running_turn_interrupted_and_clears_the_roster() {
        let (orchestrator, store, lead, _) = rig();
        let mut turn = Turn::start(
            lead.id.clone(),
            lead.direct_conversation_id.clone(),
            TurnInitiator::Owner,
            None,
            None,
        );
        store.insert_turn(&turn).unwrap();
        let mut busy = lead.clone();
        busy.status = BotStatus::Working {
            conversation_id: lead.direct_conversation_id.clone(),
            activity: "Thinking".into(),
        };
        store.update_bot(&busy).unwrap();
        turn.id = turn.id.clone();

        let report = orchestrator.boot().unwrap();
        assert_eq!(report.interrupted_turns.len(), 1);
        let stored = store.get_turn(&turn.id).unwrap().unwrap();
        assert_eq!(stored.status, TurnStatus::Interrupted);
        let bot = store.get_bot(&lead.id).unwrap().unwrap();
        assert_eq!(bot.status, BotStatus::Idle, "the roster does not lie");
    }

    #[test]
    fn history_pages_by_sequence() {
        let (orchestrator, _, lead, _) = rig();
        for i in 0..5 {
            orchestrator
                .send_owner_message(&lead.direct_conversation_id, &format!("m{i}"), &[], None)
                .unwrap();
        }
        let all = orchestrator
            .history(&lead.direct_conversation_id, 0, 10)
            .unwrap();
        assert_eq!(all.len(), 5);
        let after = orchestrator
            .history(&lead.direct_conversation_id, 3, 10)
            .unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].seq, 4);
    }

    #[test]
    fn a_bot_resolves_by_handle_or_id() {
        let (orchestrator, _, lead, _) = rig();
        assert_eq!(orchestrator.bot("lead").unwrap().id, lead.id);
        assert_eq!(orchestrator.bot("@Lead").unwrap().id, lead.id);
        assert_eq!(orchestrator.bot(lead.id.as_str()).unwrap().id, lead.id);
        assert!(orchestrator.bot("nobody").is_err());
    }

    #[tokio::test]
    async fn shutdown_leaves_nothing_running() {
        let (orchestrator, store, lead, _) = rig();
        let turn = Turn::start(
            lead.id.clone(),
            lead.direct_conversation_id.clone(),
            TurnInitiator::Owner,
            None,
            None,
        );
        store.insert_turn(&turn).unwrap();
        orchestrator.shutdown().await;
        let stored = store.get_turn(&turn.id).unwrap().unwrap();
        assert_eq!(stored.status, TurnStatus::Interrupted);
    }
}
