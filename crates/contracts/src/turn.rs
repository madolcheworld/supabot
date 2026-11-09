//! Turns: one delivery taken from the queue to its end.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::{BotId, ConversationId, DeliveryId, EntryId, RoutineId, TaskId, TurnId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    /// The model finished and its reply was posted (or was `NO_REPLY`).
    Completed,
    /// The owner pressed Stop.
    Aborted,
    /// The brain restarted mid-turn, or the session died underneath it.
    Interrupted,
    /// The backend or a tool error ended it.
    Failed,
}

impl TurnStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// Who started a turn. A harness-initiated turn is one the CLI began by
/// itself (a background command finishing); it is run as a system turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnInitiator {
    Owner,
    Bot {
        bot_id: BotId,
    },
    Routine {
        routine_id: RoutineId,
    },
    Task {
        task_id: TaskId,
    },
    /// An answered request woke the bot.
    Request,
    System,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Model round trips inside this turn.
    pub llm_calls: u32,
    pub tool_calls: u32,
}

impl Usage {
    pub fn merge(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.llm_calls += other.llm_calls;
        self.tool_calls += other.tool_calls;
    }

    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub id: TurnId,
    pub bot_id: BotId,
    /// Exactly one conversation owns a turn: its replies, tool calls, cards
    /// and usage all land there.
    pub conversation_id: ConversationId,
    pub delivery_id: Option<DeliveryId>,
    pub task_id: Option<TaskId>,
    pub initiator: TurnInitiator,
    pub status: TurnStatus,
    /// The entry the bot's final reply was posted as, when it posted one.
    pub reply_entry_id: Option<EntryId>,
    pub usage: Usage,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl Turn {
    pub fn start(
        bot_id: BotId,
        conversation_id: ConversationId,
        initiator: TurnInitiator,
        delivery_id: Option<DeliveryId>,
        task_id: Option<TaskId>,
    ) -> Self {
        Self {
            id: TurnId::new(),
            bot_id,
            conversation_id,
            delivery_id,
            task_id,
            initiator,
            status: TurnStatus::Running,
            reply_entry_id: None,
            usage: Usage::default(),
            error: None,
            started_at: Utc::now(),
            ended_at: None,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.status, TurnStatus::Running)
    }

    fn end(&mut self, status: TurnStatus, now: DateTime<Utc>) -> bool {
        if self.status.is_terminal() {
            return false;
        }
        self.status = status;
        self.ended_at = Some(now);
        true
    }

    pub fn complete(&mut self, now: DateTime<Utc>) -> bool {
        self.end(TurnStatus::Completed, now)
    }

    pub fn fail(&mut self, now: DateTime<Utc>, reason: impl Into<String>) -> bool {
        if self.status.is_terminal() {
            return false;
        }
        self.error = Some(reason.into());
        self.end(TurnStatus::Failed, now)
    }

    /// The owner stopped it.
    pub fn abort(&mut self, now: DateTime<Utc>) -> bool {
        self.end(TurnStatus::Aborted, now)
    }

    /// Recovery after a restart: honest about what may have happened.
    pub fn interrupt(&mut self, now: DateTime<Utc>, reason: impl Into<String>) -> bool {
        if self.status.is_terminal() {
            return false;
        }
        self.error = Some(reason.into());
        self.end(TurnStatus::Interrupted, now)
    }

    pub fn duration_ms(&self) -> Option<u64> {
        self.ended_at
            .map(|end| (end - self.started_at).num_milliseconds().max(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn() -> Turn {
        Turn::start(
            BotId::new(),
            ConversationId::new(),
            TurnInitiator::Owner,
            Some(DeliveryId::new()),
            None,
        )
    }

    #[test]
    fn a_turn_starts_running_and_ends_once() {
        let mut t = turn();
        assert!(t.is_running());
        let now = Utc::now();
        assert!(t.complete(now));
        assert_eq!(t.status, TurnStatus::Completed);
        assert!(t.ended_at.is_some());
        // A second end does not overwrite the first.
        assert!(!t.fail(now, "late error"));
        assert_eq!(t.status, TurnStatus::Completed);
        assert_eq!(t.error, None);
    }

    #[test]
    fn failures_and_interrupts_keep_their_reason() {
        let mut t = turn();
        t.fail(Utc::now(), "backend 402");
        assert_eq!(t.status, TurnStatus::Failed);
        assert_eq!(t.error.as_deref(), Some("backend 402"));

        let mut t2 = turn();
        t2.interrupt(Utc::now(), "brain restarted");
        assert_eq!(t2.status, TurnStatus::Interrupted);
        assert!(t2.error.is_some());
    }

    #[test]
    fn usage_merges_across_llm_calls() {
        let mut usage = Usage::default();
        usage.merge(Usage {
            input_tokens: 100,
            output_tokens: 20,
            llm_calls: 1,
            tool_calls: 2,
        });
        usage.merge(Usage {
            input_tokens: 150,
            output_tokens: 30,
            llm_calls: 1,
            tool_calls: 1,
        });
        assert_eq!(usage.total_tokens(), 300);
        assert_eq!(usage.llm_calls, 2);
        assert_eq!(usage.tool_calls, 3);
    }

    #[test]
    fn initiators_serialize_with_a_tag() {
        let json = serde_json::to_value(TurnInitiator::Task {
            task_id: TaskId::new(),
        })
        .unwrap();
        assert_eq!(json["kind"], "task");
        assert!(json["task_id"].is_string());
    }
}
