//! Deliveries: the unit of work that wakes a bot.
//!
//! Every producer — the owner, another bot, a routine, an answered question, a
//! finished task — becomes a `Delivery` in the recipient bot's FIFO queue. One
//! runner per bot consumes it; the turn it starts belongs to the delivery's
//! conversation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::{BotId, ConversationId, DeliveryId, EntryId, RequestId, RoutineId, TaskId};

/// How many bot-to-bot hand-offs one causal chain may make before the system
/// stops it ("Stopped: too many hand-offs").
pub const HOP_LIMIT: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryKind {
    /// The owner typed it.
    OwnerMessage,
    /// Another bot sent it (`message_bot`, `post_to_group`).
    BotMessage,
    /// A routine fired.
    Routine,
    /// The brain restarted and is resuming work.
    SystemResume,
    /// Work to run as a task of the bot's own (`start_task`, `handoff_task`).
    Task,
    /// A background or handed-off task finished.
    TaskResult,
    /// An answered `ask_user` / `request_approval`.
    AsyncAnswer,
}

/// `Urgent` may be steered into a running turn of the *same* conversation;
/// `Background` always waits for the next turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    #[default]
    Background,
    Urgent,
}

/// Where a delivery came from, for the hop limit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalChain {
    /// The first delivery of the chain.
    pub root: DeliveryId,
    pub hops: u8,
}

impl CausalChain {
    pub fn new(root: DeliveryId) -> Self {
        Self { root, hops: 0 }
    }

    /// The start of a chain that nothing delegated: the owner asked, or a bot
    /// took work on itself.
    pub fn root() -> Self {
        Self::new(DeliveryId::new())
    }

    /// The chain one hop further on, or `None` at the limit.
    pub fn next(&self) -> Option<Self> {
        (self.hops < HOP_LIMIT).then(|| Self {
            root: self.root.clone(),
            hops: self.hops + 1,
        })
    }

    pub fn at_limit(&self) -> bool {
        self.hops >= HOP_LIMIT
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Queued,
    Running,
    Done,
    /// Dropped: hop limit, bot deleted, group deleted, kill switch.
    Dropped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Delivery {
    pub id: DeliveryId,
    pub conversation_id: ConversationId,
    /// The bot that has to take it.
    pub bot_id: BotId,
    pub kind: DeliveryKind,
    pub text: String,
    pub urgency: Urgency,
    pub causal: CausalChain,
    /// The entry this delivery was made from, so a reply can be labelled.
    pub source_entry_id: Option<EntryId>,
    /// Who wrote it, when another bot did.
    pub from_bot: Option<BotId>,
    pub task_id: Option<TaskId>,
    pub request_id: Option<RequestId>,
    pub routine_id: Option<RoutineId>,
    pub state: DeliveryState,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl Delivery {
    fn new(
        conversation_id: ConversationId,
        bot_id: BotId,
        kind: DeliveryKind,
        text: String,
    ) -> Self {
        let id = DeliveryId::new();
        Self {
            causal: CausalChain::new(id.clone()),
            id,
            conversation_id,
            bot_id,
            kind,
            text,
            urgency: Urgency::Background,
            source_entry_id: None,
            from_bot: None,
            task_id: None,
            request_id: None,
            routine_id: None,
            state: DeliveryState::Queued,
            created_at: Utc::now(),
            started_at: None,
            ended_at: None,
        }
    }

    pub fn owner_message(
        conversation_id: ConversationId,
        bot_id: BotId,
        text: impl Into<String>,
    ) -> Self {
        let mut delivery = Self::new(
            conversation_id,
            bot_id,
            DeliveryKind::OwnerMessage,
            text.into(),
        );
        // The owner may always steer into the bot's current turn.
        delivery.urgency = Urgency::Urgent;
        delivery
    }

    pub fn bot_message(
        conversation_id: ConversationId,
        from: BotId,
        to: BotId,
        text: impl Into<String>,
        causal: CausalChain,
        urgency: Urgency,
    ) -> Self {
        let mut delivery = Self::new(conversation_id, to, DeliveryKind::BotMessage, text.into());
        delivery.from_bot = Some(from);
        delivery.causal = causal;
        delivery.urgency = urgency;
        delivery
    }

    /// One piece of work for the receiving bot to run as a task.
    pub fn task(
        conversation_id: ConversationId,
        bot_id: BotId,
        task_id: TaskId,
        title: &str,
        brief: impl Into<String>,
    ) -> Self {
        let mut delivery = Self::new(
            conversation_id,
            bot_id,
            DeliveryKind::Task,
            format!("[Task \"{title}\"]\n{}", brief.into()),
        );
        delivery.task_id = Some(task_id);
        delivery
    }

    pub fn task_result(
        conversation_id: ConversationId,
        bot_id: BotId,
        task_id: TaskId,
        title: &str,
        result: impl Into<String>,
    ) -> Self {
        let mut delivery = Self::new(
            conversation_id,
            bot_id,
            DeliveryKind::TaskResult,
            format!("[Result of \"{title}\"]\n{}", result.into()),
        );
        delivery.task_id = Some(task_id);
        delivery
    }

    pub fn async_answer(
        conversation_id: ConversationId,
        bot_id: BotId,
        request_id: RequestId,
        text: impl Into<String>,
    ) -> Self {
        let mut delivery = Self::new(
            conversation_id,
            bot_id,
            DeliveryKind::AsyncAnswer,
            text.into(),
        );
        delivery.request_id = Some(request_id);
        delivery.urgency = Urgency::Urgent;
        delivery
    }

    pub fn routine(
        conversation_id: ConversationId,
        bot_id: BotId,
        routine_id: RoutineId,
        text: impl Into<String>,
    ) -> Self {
        let mut delivery = Self::new(conversation_id, bot_id, DeliveryKind::Routine, text.into());
        delivery.routine_id = Some(routine_id);
        delivery
    }

    pub fn system_resume(
        conversation_id: ConversationId,
        bot_id: BotId,
        text: impl Into<String>,
    ) -> Self {
        Self::new(
            conversation_id,
            bot_id,
            DeliveryKind::SystemResume,
            text.into(),
        )
    }

    pub fn is_from_bot(&self) -> bool {
        matches!(self.kind, DeliveryKind::BotMessage)
    }

    /// A bot message never interrupts a turn; only the owner's can.
    pub fn may_interrupt(&self) -> bool {
        matches!(self.kind, DeliveryKind::OwnerMessage)
    }

    /// The chain this delivery would pass on, or `None` when the hop limit is
    /// reached and the chain must stop here.
    pub fn next_chain(&self) -> Option<CausalChain> {
        self.causal.next()
    }

    pub fn mark_running(&mut self, now: DateTime<Utc>) {
        self.state = DeliveryState::Running;
        self.started_at = Some(now);
    }

    pub fn mark_done(&mut self, now: DateTime<Utc>) {
        self.state = DeliveryState::Done;
        self.ended_at = Some(now);
    }

    pub fn mark_dropped(&mut self, now: DateTime<Utc>) {
        self.state = DeliveryState::Dropped;
        self.ended_at = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_limit_stops_a_chain_at_eight() {
        let mut chain = CausalChain::new(DeliveryId::new());
        for hop in 1..=HOP_LIMIT {
            chain = chain
                .next()
                .unwrap_or_else(|| panic!("hop {hop} should be allowed"));
            assert_eq!(chain.hops, hop);
        }
        assert!(chain.at_limit());
        assert!(chain.next().is_none());
    }

    #[test]
    fn owner_messages_may_interrupt_but_bot_messages_may_not() {
        let owner = Delivery::owner_message(ConversationId::new(), BotId::new(), "hi");
        assert!(owner.may_interrupt());
        assert_eq!(owner.urgency, Urgency::Urgent);
        assert_eq!(owner.causal.hops, 0);

        let bot = Delivery::bot_message(
            ConversationId::new(),
            BotId::new(),
            BotId::new(),
            "your turn",
            owner.causal.next().unwrap(),
            Urgency::Background,
        );
        assert!(!bot.may_interrupt());
        assert!(bot.is_from_bot());
        assert_eq!(bot.causal.hops, 1);
    }

    #[test]
    fn task_results_carry_their_title() {
        let delivery = Delivery::task_result(
            ConversationId::new(),
            BotId::new(),
            TaskId::new(),
            "audit the diff",
            "3 issues found",
        );
        assert_eq!(delivery.kind, DeliveryKind::TaskResult);
        assert!(delivery.text.starts_with("[Result of \"audit the diff\"]"));
        assert!(delivery.text.contains("3 issues found"));
        assert!(delivery.task_id.is_some());
    }

    #[test]
    fn state_transitions_stamp_times() {
        let mut delivery = Delivery::owner_message(ConversationId::new(), BotId::new(), "x");
        assert_eq!(delivery.state, DeliveryState::Queued);
        let now = Utc::now();
        delivery.mark_running(now);
        assert!(delivery.started_at.is_some());
        delivery.mark_done(now);
        assert_eq!(delivery.state, DeliveryState::Done);
        assert!(delivery.ended_at.is_some());
    }

    #[test]
    fn serde_round_trips() {
        let delivery = Delivery::owner_message(ConversationId::new(), BotId::new(), "x");
        let json = serde_json::to_value(&delivery).unwrap();
        assert_eq!(json["kind"], "owner_message");
        assert_eq!(json["urgency"], "urgent");
        assert_eq!(serde_json::from_value::<Delivery>(json).unwrap(), delivery);
    }
}
