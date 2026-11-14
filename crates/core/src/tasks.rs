//! Background and handed-off tasks.
//!
//! A task is one piece of work with a beginning, an end and a result that goes
//! back to whoever asked. Two ways in, one lifecycle: `start_task` (a bot's own
//! work) and `handoff_task` (somebody else's).
//!
//! Where a task runs, and where its noise stays: the turn runs in the **owner
//! bot's DM**, so a group is not filled with somebody's tool calls; the
//! **result** is delivered to the task's origin conversation, where the owner or
//! the requesting bot reads it and reports in their own words.
//!
//! Deviation from the OpenBot design, on purpose and recorded here: a task runs
//! in the bot's own session (one mind per bot) rather than in a separate
//! ephemeral one, and `max_running_tasks_per_bot` is therefore a queue depth,
//! not true parallelism — a bot still runs one turn at a time. The
//! `session_key` field is kept so a separate session can be introduced without
//! changing the shape of a task.

use std::sync::Arc;

use chrono::Utc;
use tracing::{debug, warn};

use swarmbot_contracts::bot::{Bot, BotStatus};
use swarmbot_contracts::delivery::Delivery;
use swarmbot_contracts::entry::NoticeLevel;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{BotId, ConversationId, TaskId};
use swarmbot_contracts::repo::{Persistence, RecoveryReport};
use swarmbot_contracts::task::{StepKind, Task, TaskStatus, TaskStep};

use crate::bus::{Change, LiveHub};
use crate::runners::RunnerMap;
use crate::settings::SettingsHandle;
use crate::transcript::Transcript;

pub struct BackgroundTasks {
    store: Arc<dyn Persistence>,
    hub: Arc<LiveHub>,
    transcript: Arc<Transcript>,
    settings: Arc<SettingsHandle>,
    runners: Arc<RunnerMap>,
}

impl BackgroundTasks {
    pub fn new(
        store: Arc<dyn Persistence>,
        hub: Arc<LiveHub>,
        transcript: Arc<Transcript>,
        settings: Arc<SettingsHandle>,
        runners: Arc<RunnerMap>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            hub,
            transcript,
            settings,
            runners,
        })
    }

    /// Queue a piece of the bot's own work. Returns at once: the point of a
    /// background task is that the bot stays free to chat.
    pub fn start(
        &self,
        bot: &Bot,
        origin: ConversationId,
        title: &str,
        brief: &str,
    ) -> Result<Task, DomainError> {
        let task = Task::own(bot.id.clone(), origin.clone(), title, brief);
        self.store.insert_task(&task)?;
        self.hub.publish(Change::Task(task.clone()));
        let _ = self.transcript.event(
            &origin,
            "task-started",
            format!("@{} started a task: \"{}\"", bot.name, task.title),
        );
        self.pump()?;
        Ok(task)
    }

    /// Submit an already-built task (a handoff) and let the pump place it.
    ///
    /// This is the only way a task starts running, which is what keeps a
    /// handoff's work in the owner bot's DM instead of in the conversation that
    /// asked for it: the pump decides where the turn happens.
    pub fn submit(&self, task: Task) -> Result<Task, DomainError> {
        self.store.insert_task(&task)?;
        self.hub.publish(Change::Task(task.clone()));
        self.pump()?;
        Ok(self.store.get_task(&task.id)?.unwrap_or(task))
    }

    /// Hand queued tasks to their bots while the per-bot cap allows it.
    /// Returns how many were started.
    pub fn pump(&self) -> Result<usize, DomainError> {
        let cap = self.settings.get().max_running_tasks_per_bot;
        let mut started = 0;
        for mut task in self.store.queued_tasks()? {
            // A task is marked Running when its delivery is handed over, so the
            // running count is the whole truth: counting its delivery too would
            // charge the same task twice and halve the cap.
            let active = self.store.running_task_count(&task.owner)?;
            if active >= cap {
                continue;
            }
            let Some(bot) = self.store.get_bot(&task.owner)? else {
                task.fail(Utc::now(), "the bot that owned this task is gone");
                self.store.update_task(&task)?;
                self.hub.publish(Change::Task(task));
                continue;
            };
            // A disabled bot has no runner and never will until the owner
            // switches it back on, so handing it a delivery parks the work for
            // nobody: the row says `Running`, the park map never empties, and
            // `wait_for_idle` — which requires `parked_count() == 0` — can never
            // report a quiet team again. The task is closed here instead, with
            // the reason naming the real cause (see `abandon_task` for why the
            // status is `Failed` and not `Interrupted`).
            if matches!(bot.status, BotStatus::Disabled) {
                abandon_task(
                    self.store.as_ref(),
                    &self.transcript,
                    &task,
                    &format!(
                        "the bot @{} is disabled, so this task did not run; enable the bot and \
                         start the task again",
                        bot.name
                    ),
                )?;
                continue;
            }
            // The work happens in the bot's own DM; the result travels later.
            let mut delivery = Delivery::task(
                bot.direct_conversation_id.clone(),
                bot.id.clone(),
                task.id.clone(),
                &task.title,
                task.brief.clone(),
            );
            // Carry the chain that produced the task, so the hop limit still
            // bounds a handoff loop (D4.3).
            delivery.causal = task.causal.clone();
            self.store.insert_delivery(&delivery)?;
            task.start(Utc::now(), None);
            self.store.update_task(&task)?;
            self.hub.publish(Change::Task(task.clone()));
            self.record_step(&task.id, StepKind::Notice, "queued for the bot")?;
            if let Err(err) = self.runners.deliver(delivery) {
                warn!(error = %err, task = %task.id, "cannot hand a task to its bot");
            }
            started += 1;
        }
        Ok(started)
    }

    /// A turn for this task ended: record the outcome and deliver the result.
    pub fn settle(&self, task_id: &TaskId, result: &str, failed: bool) -> Result<(), DomainError> {
        let Some(mut task) = self.store.get_task(task_id)? else {
            debug!(task = %task_id, "a task result arrived for a task that is gone");
            return Ok(());
        };
        if task.is_terminal() {
            return Ok(());
        }
        let now = Utc::now();
        if failed {
            task.fail(now, result);
        } else {
            task.succeed(now, result);
        }
        self.store.update_task(&task)?;
        self.hub.publish(Change::Task(task.clone()));
        self.record_step(
            task_id,
            StepKind::Result,
            if failed { "failed" } else { "succeeded" },
        )?;
        self.deliver_result(&task)
    }

    /// The result goes back to the origin conversation, to the bot that asked
    /// (or to the bot itself, for its own task).
    fn deliver_result(&self, task: &Task) -> Result<(), DomainError> {
        let recipient = match &task.requester {
            Some(id) => self.store.get_bot(id)?,
            None => self.store.get_bot(&task.owner)?,
        };
        let Some(recipient) = recipient else {
            let _ = self.transcript.notice(
                &task.origin,
                format!("The task \"{}\" finished but its bot is gone.", task.title),
                NoticeLevel::Warning,
            );
            return Ok(());
        };
        let mut delivery = Delivery::task_result(
            task.origin.clone(),
            recipient.id.clone(),
            task.id.clone(),
            &task.title,
            task.result_text(),
        );
        // A result returning to its requester is the same causal chain, not a
        // new one: resetting the count here would let two bots hand the same
        // work back and forth forever, because every round trip would look like
        // the first hop.
        delivery.causal = task.causal.clone();
        self.store.insert_delivery(&delivery)?;
        let _ = self.transcript.event(
            &task.origin,
            "task-result",
            format!(
                "Task \"{}\" {} — @{} is reading the result",
                task.title,
                task.status.label(),
                recipient.name
            ),
        );
        self.runners.deliver(delivery)?;
        Ok(())
    }

    /// Add to a running task's brief. Steered when it is running, otherwise
    /// recorded so the next turn sees it.
    pub fn update(&self, task_id: &TaskId, text: &str) -> Result<bool, DomainError> {
        let task = self
            .store
            .get_task(task_id)?
            .ok_or_else(|| swarmbot_contracts::not_found::task(task_id))?;
        if task.is_terminal() {
            return Err(DomainError::conflict(format!(
                "the task \"{}\" already {}",
                task.title,
                task.status.label()
            )));
        }
        self.record_step(task_id, StepKind::Update, text)?;
        if task.status != TaskStatus::Running {
            return Ok(false);
        }
        let bot = self
            .store
            .get_bot(&task.owner)?
            .ok_or_else(|| swarmbot_contracts::not_found::bot(&task.owner))?;
        let delivery = Delivery::task(
            bot.direct_conversation_id.clone(),
            bot.id.clone(),
            task.id.clone(),
            &task.title,
            text,
        );
        // A steer joins the running turn; when it cannot, the note waits.
        Ok(self.runners.steer(&bot.id, delivery))
    }

    /// Stop a task. `by_owner` decides whether the bot is told: an owner's
    /// cancel is reported, a bot's own `cancel_task` is not (it knows).
    pub fn cancel(&self, task_id: &TaskId, by_owner: bool) -> Result<Task, DomainError> {
        let mut task = self
            .store
            .get_task(task_id)?
            .ok_or_else(|| swarmbot_contracts::not_found::task(task_id))?;
        if task.is_terminal() {
            return Ok(task);
        }
        let was_running = task.status == TaskStatus::Running;
        task.cancel(Utc::now());
        self.store.update_task(&task)?;
        self.hub.publish(Change::Task(task.clone()));
        if was_running && let Some(bot) = self.store.get_bot(&task.owner)? {
            // Stopping the task stops the turn running it, which is in the
            // bot's DM — never a turn of another conversation.
            self.runners
                .interrupt(&bot.id, Some(&bot.direct_conversation_id));
        }
        if by_owner {
            let _ = self.transcript.event(
                &task.origin,
                "task-cancelled",
                format!("The owner cancelled the task \"{}\"", task.title),
            );
            let _ = self.deliver_result(&task);
        }
        Ok(task)
    }

    /// Re-run what a restart cut off, within the budget; report the rest.
    ///
    /// The budget is `settings.max_task_restarts`, read here rather than taken
    /// from `Task::can_restart`'s built-in default: the setting existed,
    /// validated, showed up on the Settings screen, and no code ever read it, so
    /// an owner who set it to zero still got two restart attempts. A zero
    /// budget means "do not re-run anything", and the task is reported instead
    /// of restarted.
    pub fn recover(&self, report: &RecoveryReport) -> Result<usize, DomainError> {
        let limit = self.settings.get().max_task_restarts;
        let mut restarted = 0;
        for task_id in &report.interrupted_tasks {
            let Some(mut task) = self.store.get_task(task_id)? else {
                continue;
            };
            if task.can_restart_with(limit) {
                task.bump_restart();
                self.store.update_task(&task)?;
                self.hub.publish(Change::Task(task));
                restarted += 1;
            } else if let Some(bot) = self.store.get_bot(&task.owner)? {
                let _ = self.transcript.notice(
                    &task.origin,
                    format!(
                        "The task \"{}\" was interrupted by a restart and could not be finished.",
                        task.title
                    ),
                    NoticeLevel::Warning,
                );
                let _ = bot;
            }
        }
        if restarted > 0 {
            self.pump()?;
        }
        Ok(restarted)
    }

    pub fn record_step(
        &self,
        task_id: &TaskId,
        kind: StepKind,
        text: &str,
    ) -> Result<(), DomainError> {
        let seq = self.store.task_steps(task_id)?.len() as u32 + 1;
        self.store
            .append_task_step(&TaskStep::new(task_id.clone(), seq, kind, text))
    }

    pub fn list(&self, bot: &BotId) -> Result<Vec<Task>, DomainError> {
        self.store.tasks_of(bot)
    }

    pub fn in_conversation(&self, conversation: &ConversationId) -> Result<Vec<Task>, DomainError> {
        self.store.tasks_in(conversation)
    }

    pub fn get(&self, task_id: &TaskId) -> Result<Option<Task>, DomainError> {
        self.store.get_task(task_id)
    }

    pub fn steps(&self, task_id: &TaskId) -> Result<Vec<TaskStep>, DomainError> {
        self.store.task_steps(task_id)
    }

    pub fn store(&self) -> Arc<dyn Persistence> {
        Arc::clone(&self.store)
    }
}

/// Close a task whose work will never run, and tell the origin conversation.
///
/// A free function rather than a method, for the same reason as
/// [`crate::routines::settle_delivery`]: the other caller is the bot lifecycle
/// (`BotService::set_enabled(false)` and `delete`), which holds the store and
/// the transcript but must not own `BackgroundTasks` — that would put tasks
/// underneath bots in the dependency order to serve one cleanup.
///
/// **The status is `Failed`, deliberately.** `Interrupted` in this codebase
/// means one specific thing — `Task::interrupt` writes "interrupted by a
/// restart", and boot recovery is what re-runs those, from its own report — so a
/// task closed here would wear a label naming a cause that did not happen, and
/// would look recoverable when nothing would ever recover it. `Cancelled` would
/// claim somebody cancelled it on purpose. `Failed` is terminal (so re-enabling
/// the bot cannot make this row claim success, and `pump` never picks it up
/// again) and `Task::error` carries the real reason, which `result_text()`
/// surfaces to the reader as `FAILED: …`.
///
/// `Ok(())` for a task that is already terminal: this runs while a bot is being
/// switched off, and a row that has already ended must not be rewritten.
pub fn abandon_task(
    store: &dyn Persistence,
    transcript: &Transcript,
    task: &Task,
    reason: &str,
) -> Result<(), DomainError> {
    let Some(mut task) = store.get_task(&task.id)? else {
        return Ok(());
    };
    if task.is_terminal() {
        return Ok(());
    }
    task.fail(Utc::now(), reason.to_string());
    store.update_task(&task)?;
    transcript.hub().publish(Change::Task(task.clone()));
    let seq = store.task_steps(&task.id)?.len() as u32 + 1;
    store.append_task_step(&TaskStep::new(
        task.id.clone(),
        seq,
        StepKind::Notice,
        reason,
    ))?;
    transcript.notice(
        &task.origin,
        format!("The task \"{}\" did not run: {reason}", task.title),
        NoticeLevel::Warning,
    )?;
    debug!(task = %task.id, reason, "closed a task whose work will never run");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    struct Rig {
        tasks: Arc<BackgroundTasks>,
        store: Arc<dyn Persistence>,
        runners: Arc<RunnerMap>,
        settings: Arc<SettingsHandle>,
        bot: Bot,
        origin: ConversationId,
    }

    fn rig() -> Rig {
        let fixture = testing::Fixture::memory();
        let bot = fixture.bot("lead");
        let settings = SettingsHandle::new(fixture.store.clone());
        let runners = RunnerMap::new();
        let tasks = BackgroundTasks::new(
            fixture.store.clone(),
            fixture.hub.clone(),
            fixture.transcript.clone(),
            settings.clone(),
            runners.clone(),
        );
        Rig {
            tasks,
            store: fixture.store,
            runners,
            settings,
            origin: bot.direct_conversation_id.clone(),
            bot,
        }
    }

    #[test]
    fn a_task_a_bot_starts_for_itself_begins_a_fresh_chain() {
        let rig = rig();
        let task = rig
            .tasks
            .start(&rig.bot, rig.origin.clone(), "audit", "x")
            .unwrap();
        assert_eq!(task.causal.hops, 0, "own work is not a delegation hop");
        let deliveries = rig.store.queued_deliveries(&rig.bot.id).unwrap();
        assert_eq!(deliveries[0].causal.hops, 0);
    }

    #[test]
    fn starting_a_task_queues_it_and_hands_it_over() {
        let rig = rig();
        let task = rig
            .tasks
            .start(
                &rig.bot,
                rig.origin.clone(),
                "audit the diff",
                "look for holes",
            )
            .unwrap();
        assert_eq!(task.status, TaskStatus::Queued);
        // The pump started it: the bot has a delivery, in its own DM.
        let stored = rig.store.get_task(&task.id).unwrap().unwrap();
        assert_eq!(stored.status, TaskStatus::Running);
        assert_eq!(rig.runners.parked_count(), 1);
        let deliveries = rig.store.queued_deliveries(&rig.bot.id).unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(
            deliveries[0].conversation_id,
            rig.bot.direct_conversation_id
        );
        assert!(deliveries[0].text.contains("audit the diff"));
    }

    #[test]
    fn the_per_bot_cap_holds_extra_tasks_back() {
        let rig = rig();
        for i in 0..5 {
            rig.tasks
                .start(&rig.bot, rig.origin.clone(), &format!("task {i}"), "x")
                .unwrap();
        }
        let running = rig.store.running_task_count(&rig.bot.id).unwrap();
        assert_eq!(running, 3, "the default cap is three");
        let queued = rig.store.queued_tasks().unwrap();
        assert_eq!(queued.len(), 2);
    }

    #[test]
    fn settling_a_task_delivers_the_result_to_the_origin() {
        let rig = rig();
        let task = rig
            .tasks
            .start(&rig.bot, rig.origin.clone(), "audit", "x")
            .unwrap();
        rig.tasks.settle(&task.id, "3 issues found", false).unwrap();
        let stored = rig.store.get_task(&task.id).unwrap().unwrap();
        assert_eq!(stored.status, TaskStatus::Succeeded);
        assert_eq!(stored.result.as_deref(), Some("3 issues found"));

        let deliveries = rig.store.queued_deliveries(&rig.bot.id).unwrap();
        let result = deliveries
            .iter()
            .find(|d| d.kind == swarmbot_contracts::delivery::DeliveryKind::TaskResult)
            .expect("a result delivery");
        assert_eq!(result.conversation_id, rig.origin);
        assert!(result.text.contains("[Result of \"audit\"]"));
        assert!(result.text.contains("3 issues found"));
    }

    #[test]
    fn a_handoffs_result_goes_to_the_bot_that_asked() {
        let rig = rig();
        let requester = testing::seed_bot(&rig.store, "lead");
        let worker = testing::seed_bot(&rig.store, "sentinel");
        let task = Task::handoff(
            requester.id.clone(),
            worker.id.clone(),
            rig.origin.clone(),
            "review",
            "check it",
            None,
        );
        rig.store.insert_task(&task).unwrap();
        rig.tasks.settle(&task.id, "looks safe", false).unwrap();

        let deliveries = rig.store.queued_deliveries(&requester.id).unwrap();
        assert_eq!(deliveries.len(), 1, "the requester gets the result");
        assert_eq!(deliveries[0].conversation_id, rig.origin);
        assert!(
            rig.store
                .queued_deliveries(&worker.id)
                .unwrap()
                .iter()
                .all(|d| d.kind != swarmbot_contracts::delivery::DeliveryKind::TaskResult)
        );
    }

    #[test]
    fn a_failed_task_says_it_failed() {
        let rig = rig();
        let task = rig
            .tasks
            .start(&rig.bot, rig.origin.clone(), "build", "x")
            .unwrap();
        rig.tasks
            .settle(&task.id, "cargo exited 101", true)
            .unwrap();
        let stored = rig.store.get_task(&task.id).unwrap().unwrap();
        assert_eq!(stored.status, TaskStatus::Failed);
        let deliveries = rig.store.queued_deliveries(&rig.bot.id).unwrap();
        let result = deliveries
            .iter()
            .find(|d| d.kind == swarmbot_contracts::delivery::DeliveryKind::TaskResult)
            .unwrap();
        assert!(result.text.contains("FAILED: cargo exited 101"));
    }

    #[test]
    fn settling_twice_changes_nothing() {
        let rig = rig();
        let task = rig
            .tasks
            .start(&rig.bot, rig.origin.clone(), "audit", "x")
            .unwrap();
        rig.tasks.settle(&task.id, "first", false).unwrap();
        rig.tasks.settle(&task.id, "second", true).unwrap();
        let stored = rig.store.get_task(&task.id).unwrap().unwrap();
        assert_eq!(stored.result.as_deref(), Some("first"));
    }

    #[test]
    fn updating_a_queued_task_records_a_step() {
        let rig = rig();
        let task = Task::own(rig.bot.id.clone(), rig.origin.clone(), "audit", "x");
        rig.store.insert_task(&task).unwrap();
        assert!(!rig.tasks.update(&task.id, "also check tests").unwrap());
        let steps = rig.tasks.steps(&task.id).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].kind, StepKind::Update);
    }

    #[test]
    fn a_finished_task_cannot_be_updated() {
        let rig = rig();
        let task = rig
            .tasks
            .start(&rig.bot, rig.origin.clone(), "audit", "x")
            .unwrap();
        rig.tasks.settle(&task.id, "done", false).unwrap();
        let err = rig.tasks.update(&task.id, "one more thing").unwrap_err();
        assert_eq!(err.code(), "conflict");
    }

    #[test]
    fn cancelling_stops_the_turn_that_runs_it() {
        let rig = rig();
        let task = rig
            .tasks
            .start(&rig.bot, rig.origin.clone(), "audit", "x")
            .unwrap();
        let cancelled = rig.tasks.cancel(&task.id, true).unwrap();
        assert_eq!(cancelled.status, TaskStatus::Cancelled);
        // The owner is told, and the result delivery says it was cancelled.
        let deliveries = rig.store.queued_deliveries(&rig.bot.id).unwrap();
        assert!(
            deliveries
                .iter()
                .any(|d| d.text.contains("CANCELLED before it finished"))
        );
    }

    #[test]
    fn recovery_restarts_within_the_budget_and_reports_the_rest() {
        let rig = rig();
        let task = rig
            .tasks
            .start(&rig.bot, rig.origin.clone(), "audit", "x")
            .unwrap();
        let mut report = RecoveryReport::default();
        report.interrupted_tasks.push(task.id.clone());
        // The store still says running; recovery works from the report.
        let mut stored = rig.store.get_task(&task.id).unwrap().unwrap();
        stored.interrupt(Utc::now());
        rig.store.update_task(&stored).unwrap();

        assert_eq!(rig.tasks.recover(&report).unwrap(), 1);
        let after = rig.store.get_task(&task.id).unwrap().unwrap();
        assert_eq!(after.restarts, 1);
        assert_eq!(
            after.status,
            TaskStatus::Running,
            "the pump picked it up again"
        );
    }

    #[test]
    fn a_task_whose_budget_is_spent_is_reported_not_restarted() {
        let rig = rig();
        let mut task = Task::own(rig.bot.id.clone(), rig.origin.clone(), "audit", "x");
        task.restarts = swarmbot_contracts::task::MAX_TASK_RESTARTS;
        task.interrupt(Utc::now());
        rig.store.insert_task(&task).unwrap();
        let mut report = RecoveryReport::default();
        report.interrupted_tasks.push(task.id.clone());
        assert_eq!(rig.tasks.recover(&report).unwrap(), 0);
        let entries = rig.store.entry_tail(&rig.origin, 10).unwrap();
        assert!(entries.iter().any(|e| {
            e.kind
                .text()
                .unwrap_or_default()
                .contains("could not be finished")
        }));
    }

    #[test]
    fn the_configured_restart_budget_is_the_one_that_counts() {
        // `settings.max_task_restarts` validated, showed up on the Settings
        // screen, and no code read it: the decision used the built-in
        // `MAX_TASK_RESTARTS` constant, so an owner who set the budget to zero
        // still got two restart attempts.
        let rig = rig();
        rig.settings
            .update(|settings| settings.max_task_restarts = 0)
            .unwrap();

        let mut task = Task::own(rig.bot.id.clone(), rig.origin.clone(), "audit", "x");
        task.interrupt(Utc::now());
        rig.store.insert_task(&task).unwrap();
        let mut report = RecoveryReport::default();
        report.interrupted_tasks.push(task.id.clone());

        assert_eq!(
            rig.tasks.recover(&report).unwrap(),
            0,
            "a budget of zero means do not re-run anything"
        );
        let after = rig.store.get_task(&task.id).unwrap().unwrap();
        assert_eq!(after.restarts, 0);
        assert_eq!(after.status, TaskStatus::Interrupted, "it stays as it was");
        let entries = rig.store.entry_tail(&rig.origin, 10).unwrap();
        assert!(
            entries.iter().any(|e| {
                e.kind
                    .text()
                    .unwrap_or_default()
                    .contains("could not be finished")
            }),
            "and the origin conversation is told, rather than left guessing"
        );

        // The other direction too: a budget above the built-in default is
        // honoured, so the setting (not the constant) decides.
        rig.settings
            .update(|settings| settings.max_task_restarts = 4)
            .unwrap();
        let mut patient = Task::own(rig.bot.id.clone(), rig.origin.clone(), "second", "x");
        patient.restarts = swarmbot_contracts::task::MAX_TASK_RESTARTS;
        patient.interrupt(Utc::now());
        rig.store.insert_task(&patient).unwrap();
        let mut report = RecoveryReport::default();
        report.interrupted_tasks.push(patient.id.clone());
        assert_eq!(rig.tasks.recover(&report).unwrap(), 1);
        let after = rig.store.get_task(&patient.id).unwrap().unwrap();
        assert_eq!(
            after.restarts,
            swarmbot_contracts::task::MAX_TASK_RESTARTS + 1
        );
    }

    #[test]
    fn a_disabled_bots_task_is_closed_instead_of_parked() {
        // The pump used to check only that the owner bot *exists*. A disabled bot
        // has no runner and `ensure_runners` skips it, so its delivery parked
        // forever while the task row said `Running`: `wait_for_idle` requires
        // `parked_count() == 0`, so one disabled bot froze the whole team's idea
        // of "quiet", and the task pane showed work in progress that nobody was
        // doing.
        let rig = rig();
        let mut disabled = rig.bot.clone();
        disabled.status = BotStatus::Disabled;
        rig.store.update_bot(&disabled).unwrap();

        let task = rig
            .tasks
            .start(&disabled, rig.origin.clone(), "audit", "x")
            .unwrap();

        assert_eq!(
            rig.runners.parked_count(),
            0,
            "a disabled bot's work is not parked for a runner that will never exist"
        );
        assert!(
            rig.store
                .queued_deliveries(&disabled.id)
                .unwrap()
                .is_empty(),
            "and no delivery was written either"
        );
        let after = rig.store.get_task(&task.id).unwrap().unwrap();
        assert_eq!(
            after.status,
            TaskStatus::Failed,
            "the row does not claim it ran"
        );
        assert!(
            after
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("disabled"),
            "the reason names the real cause: {:?}",
            after.error
        );
        // `Interrupted` would be the other candidate and is wrong on purpose: it
        // means "a restart cut this off" everywhere else in the codebase, and
        // boot recovery is what re-runs those.
        assert_ne!(after.status, TaskStatus::Interrupted);
        // The origin conversation is told, the way `recover` tells it.
        let entries = rig.store.entry_tail(&rig.origin, 10).unwrap();
        assert!(
            entries.iter().any(|e| {
                let text = e.kind.text().unwrap_or_default();
                text.contains("did not run") && text.contains("disabled")
            }),
            "the requester is told the work did not run"
        );
    }

    #[test]
    fn a_closed_task_is_not_revived_when_the_bot_is_enabled_again() {
        // Requirement (c): re-enabling a bot must not turn work that never ran
        // into a success — or into a queue entry that silently starts later.
        let rig = rig();
        let mut disabled = rig.bot.clone();
        disabled.status = BotStatus::Disabled;
        rig.store.update_bot(&disabled).unwrap();
        let task = rig
            .tasks
            .start(&disabled, rig.origin.clone(), "audit", "x")
            .unwrap();
        assert_eq!(
            rig.store.get_task(&task.id).unwrap().unwrap().status,
            TaskStatus::Failed
        );

        let mut enabled = disabled.clone();
        enabled.status = BotStatus::Idle;
        rig.store.update_bot(&enabled).unwrap();
        assert_eq!(rig.tasks.pump().unwrap(), 0, "nothing is queued to start");

        let after = rig.store.get_task(&task.id).unwrap().unwrap();
        assert_eq!(
            after.status,
            TaskStatus::Failed,
            "a failure stays a failure: the work never ran"
        );
        assert!(after.result.is_none(), "and it never claims a result");
        assert_eq!(rig.runners.parked_count(), 0);
        assert!(
            rig.tasks
                .steps(&task.id)
                .unwrap()
                .iter()
                .any(|s| { s.kind == StepKind::Notice && s.text.contains("disabled") })
        );
    }

    #[test]
    fn steps_count_up_from_one() {
        let rig = rig();
        let task = rig
            .tasks
            .start(&rig.bot, rig.origin.clone(), "audit", "x")
            .unwrap();
        rig.tasks
            .record_step(&task.id, StepKind::Tool, "cargo test")
            .unwrap();
        rig.tasks
            .record_step(&task.id, StepKind::Message, "found it")
            .unwrap();
        let steps = rig.tasks.steps(&task.id).unwrap();
        assert_eq!(
            steps.iter().map(|s| s.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }
}
