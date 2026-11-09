//! Tasks: long work a bot hands off, or hands to itself.
//!
//! Two ways in, one lifecycle:
//!
//! * `handoff_task {to, task, context}` — work given to **another** bot; the
//!   result comes back to the requester's conversation as a delivery.
//! * `start_task {title, brief}` — work a bot gives **itself**, in a separate
//!   short-lived session, so it stays free to chat.
//!
//! Both keep their steps out of the transcript (they live in the task pane and
//! drive the UI's delegation graph) and both end by delivering one result.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::delivery::CausalChain;
use crate::id::{BotId, ConversationId, TaskId, TurnId};

/// How many running tasks one bot may have at once.
pub const MAX_RUNNING_TASKS_PER_BOT: u32 = 3;

/// A task interrupted by a restart is re-run at most this often.
pub const MAX_TASK_RESTARTS: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskKind {
    /// The owner's bot started it for itself.
    Own,
    /// Another bot handed it over.
    Handoff { from: BotId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    /// The brain restarted while it ran.
    Interrupted,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }

    /// The one-line label the UI shows.
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    /// The bot doing the work.
    pub owner: BotId,
    /// Where the result goes back to.
    pub origin: ConversationId,
    pub kind: TaskKind,
    pub title: String,
    /// What has to be done, in the requester's words.
    pub brief: String,
    /// Extra context handed over with a handoff.
    pub context: Option<String>,
    pub status: TaskStatus,
    /// Who asked for it (`None` for the owner's own task).
    pub requester: Option<BotId>,
    /// A task this one spawned; makes the delegation tree.
    pub parent_task_id: Option<TaskId>,
    /// The delegation chain that produced this task.
    ///
    /// A task runs as a turn of its owner's, and that turn's deliveries are what
    /// the hop limit is read from. Without the chain travelling *with* the task,
    /// a `handoff_task` loop (A → B → A → …) would restart the count at zero on
    /// every hop and never reach `max_hops`.
    #[serde(default = "CausalChain::root")]
    pub causal: CausalChain,
    /// The session key of the ephemeral session running it.
    pub session_key: String,
    pub restarts: u8,
    pub result: Option<String>,
    pub error: Option<String>,
    /// The turn currently running it.
    pub turn_id: Option<TurnId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl Task {
    pub fn own(
        owner: BotId,
        origin: ConversationId,
        title: impl Into<String>,
        brief: impl Into<String>,
    ) -> Self {
        Self::base(owner, origin, TaskKind::Own, title, brief)
    }

    pub fn handoff(
        from: BotId,
        owner: BotId,
        origin: ConversationId,
        title: impl Into<String>,
        brief: impl Into<String>,
        context: Option<String>,
    ) -> Self {
        let mut task = Self::base(
            owner,
            origin,
            TaskKind::Handoff { from: from.clone() },
            title,
            brief,
        );
        task.requester = Some(from);
        task.context = context;
        task
    }

    fn base(
        owner: BotId,
        origin: ConversationId,
        kind: TaskKind,
        title: impl Into<String>,
        brief: impl Into<String>,
    ) -> Self {
        let id = TaskId::new();
        let now = Utc::now();
        Self {
            session_key: format!("{owner}:task:{id}"),
            id,
            owner,
            origin,
            kind,
            title: title.into(),
            brief: brief.into(),
            context: None,
            status: TaskStatus::Queued,
            requester: None,
            parent_task_id: None,
            causal: CausalChain::root(),
            restarts: 0,
            result: None,
            error: None,
            turn_id: None,
            created_at: now,
            updated_at: now,
            started_at: None,
            ended_at: None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    pub fn start(&mut self, now: DateTime<Utc>, turn_id: Option<TurnId>) -> bool {
        if self.status.is_terminal() || self.status == TaskStatus::Running {
            return false;
        }
        self.status = TaskStatus::Running;
        self.started_at = Some(now);
        self.turn_id = turn_id;
        self.updated_at = now;
        true
    }

    pub fn succeed(&mut self, now: DateTime<Utc>, result: impl Into<String>) -> bool {
        self.finish(now, TaskStatus::Succeeded, Some(result.into()), None)
    }

    pub fn fail(&mut self, now: DateTime<Utc>, reason: impl Into<String>) -> bool {
        self.finish(now, TaskStatus::Failed, None, Some(reason.into()))
    }

    pub fn cancel(&mut self, now: DateTime<Utc>) -> bool {
        self.finish(now, TaskStatus::Cancelled, None, Some("cancelled".into()))
    }

    /// Restart recovery: honest that the work was cut off.
    pub fn interrupt(&mut self, now: DateTime<Utc>) -> bool {
        self.finish(
            now,
            TaskStatus::Interrupted,
            None,
            Some("interrupted by a restart".into()),
        )
    }

    fn finish(
        &mut self,
        now: DateTime<Utc>,
        status: TaskStatus,
        result: Option<String>,
        error: Option<String>,
    ) -> bool {
        if self.status.is_terminal() {
            return false;
        }
        self.status = status;
        self.result = result;
        self.error = error;
        self.ended_at = Some(now);
        self.updated_at = now;
        self.turn_id = None;
        true
    }

    /// Re-run after a restart, if the budget allows it.
    pub fn can_restart(&self) -> bool {
        matches!(self.status, TaskStatus::Interrupted) && self.restarts < MAX_TASK_RESTARTS
    }

    pub fn bump_restart(&mut self) {
        self.restarts += 1;
        self.status = TaskStatus::Queued;
        self.started_at = None;
        self.ended_at = None;
        self.error = None;
        self.updated_at = Utc::now();
    }

    /// The text delivered back to the origin conversation.
    pub fn result_text(&self) -> String {
        match self.status {
            TaskStatus::Succeeded => self
                .result
                .clone()
                .unwrap_or_else(|| "(the task finished without a written result)".into()),
            TaskStatus::Failed => format!(
                "FAILED: {}",
                self.error.clone().unwrap_or_else(|| "unknown error".into())
            ),
            TaskStatus::Cancelled => "CANCELLED before it finished.".into(),
            TaskStatus::Interrupted => "INTERRUPTED by a restart; it may be re-run.".into(),
            other => format!("still {}", other.label()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Message,
    Reasoning,
    Tool,
    /// The owner or the bot updated the brief while it ran.
    Update,
    Result,
    Notice,
}

/// One line of a task's own progress. Steps never become transcript entries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskStep {
    pub task_id: TaskId,
    pub seq: u32,
    pub kind: StepKind,
    pub text: String,
    pub tool: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl TaskStep {
    pub fn new(task_id: TaskId, seq: u32, kind: StepKind, text: impl Into<String>) -> Self {
        Self {
            task_id,
            seq,
            kind,
            text: text.into(),
            tool: None,
            created_at: Utc::now(),
        }
    }

    pub fn tool(
        task_id: TaskId,
        seq: u32,
        tool: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        let mut step = Self::new(task_id, seq, StepKind::Tool, text);
        step.tool = Some(tool.into());
        step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_starts_queued_and_runs_once() {
        let mut task = Task::own(
            BotId::new(),
            ConversationId::new(),
            "audit",
            "check the diff",
        );
        assert_eq!(task.status, TaskStatus::Queued);
        assert!(!task.is_terminal());
        assert!(task.session_key.contains(":task:"));

        let now = Utc::now();
        assert!(task.start(now, Some(TurnId::new())));
        assert_eq!(task.status, TaskStatus::Running);
        assert!(
            !task.start(now, None),
            "a running task does not start again"
        );
    }

    #[test]
    fn results_read_as_results() {
        let mut task = Task::own(BotId::new(), ConversationId::new(), "audit", "x");
        task.succeed(Utc::now(), "3 issues");
        assert_eq!(task.result_text(), "3 issues");
        assert!(task.is_terminal());

        let mut failed = Task::own(BotId::new(), ConversationId::new(), "build", "x");
        failed.fail(Utc::now(), "cargo exited 101");
        assert!(failed.result_text().starts_with("FAILED: cargo exited 101"));

        let mut cancelled = Task::own(BotId::new(), ConversationId::new(), "deploy", "x");
        cancelled.cancel(Utc::now());
        assert_eq!(cancelled.result_text(), "CANCELLED before it finished.");
    }

    #[test]
    fn only_one_finish_wins() {
        let mut task = Task::own(BotId::new(), ConversationId::new(), "audit", "x");
        assert!(task.succeed(Utc::now(), "done"));
        assert!(!task.fail(Utc::now(), "too late"));
        assert_eq!(task.status, TaskStatus::Succeeded);
        assert_eq!(task.result.as_deref(), Some("done"));
    }

    #[test]
    fn handoffs_remember_their_requester() {
        let from = BotId::new();
        let owner = BotId::new();
        let task = Task::handoff(
            from.clone(),
            owner.clone(),
            ConversationId::new(),
            "review the diff",
            "look for security holes",
            Some("the diff is in /tmp/x.patch".into()),
        );
        assert_eq!(task.kind, TaskKind::Handoff { from: from.clone() });
        assert_eq!(task.requester.as_ref(), Some(&from));
        assert_eq!(task.owner, owner);
        assert!(task.context.is_some());
    }

    #[test]
    fn restarts_are_budgeted() {
        let mut task = Task::own(BotId::new(), ConversationId::new(), "audit", "x");
        task.interrupt(Utc::now());
        assert!(task.can_restart());
        task.bump_restart();
        assert_eq!(task.status, TaskStatus::Queued);
        assert_eq!(task.restarts, 1);
        task.interrupt(Utc::now());
        task.bump_restart();
        task.interrupt(Utc::now());
        assert_eq!(task.restarts, MAX_TASK_RESTARTS);
        assert!(!task.can_restart(), "the budget is spent");
    }

    #[test]
    fn steps_stay_out_of_the_transcript_shape() {
        let task_id = TaskId::new();
        let step = TaskStep::tool(task_id.clone(), 1, "shell", "cargo test");
        assert_eq!(step.kind, StepKind::Tool);
        assert_eq!(step.tool.as_deref(), Some("shell"));
        let json = serde_json::to_value(&step).unwrap();
        assert_eq!(json["kind"], "tool");
        assert_eq!(json["task_id"], serde_json::json!(task_id.to_string()));
    }
}
