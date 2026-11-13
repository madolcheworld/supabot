//! Routines: scheduled triggers that wake a bot.
//!
//! A routine belongs to one bot. When its schedule is due, `RoutineScheduler`
//! creates a `DeliveryKind::Routine` delivery and queues it for the bot, waking
//! its runner to execute.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tracing::{debug, info, warn};

use swarmbot_contracts::delivery::Delivery;
use swarmbot_contracts::error::{DomainError, not_found};
use swarmbot_contracts::id::{BotId, RoutineId, RunId};
use swarmbot_contracts::repo::Persistence;
use swarmbot_contracts::routine::{Routine, RoutineRun, RoutineSchedule, RunStatus};

use crate::runners::RunnerMap;

pub const ROUTINE_TICK: Duration = Duration::from_secs(1);

pub struct RoutineScheduler {
    store: Arc<dyn Persistence>,
    runners: Arc<RunnerMap>,
}

impl RoutineScheduler {
    pub fn new(store: Arc<dyn Persistence>, runners: Arc<RunnerMap>) -> Arc<Self> {
        Arc::new(Self { store, runners })
    }

    pub fn create_routine(
        &self,
        bot_id: BotId,
        name: impl Into<String>,
        prompt: impl Into<String>,
        schedule: RoutineSchedule,
        now: DateTime<Utc>,
    ) -> Result<Routine, DomainError> {
        if self.store.get_bot(&bot_id)?.is_none() {
            return Err(not_found::bot(&bot_id));
        }
        let routine = Routine::new(bot_id, name, prompt, schedule, now)?;
        self.store.insert_routine(&routine)?;
        info!(id = %routine.id, name = %routine.name, "created routine");
        Ok(routine)
    }

    pub fn get_routine(&self, id: &RoutineId) -> Result<Option<Routine>, DomainError> {
        self.store.get_routine(id)
    }

    pub fn list_routines(&self) -> Result<Vec<Routine>, DomainError> {
        self.store.list_routines()
    }

    pub fn list_routines_for_bot(&self, bot_id: &BotId) -> Result<Vec<Routine>, DomainError> {
        self.store.list_routines_for_bot(bot_id)
    }

    pub fn pause_routine(&self, id: &RoutineId) -> Result<Routine, DomainError> {
        let mut routine = self
            .store
            .get_routine(id)?
            .ok_or_else(|| not_found::routine(id))?;
        routine.pause();
        self.store.update_routine(&routine)?;
        info!(id = %routine.id, "paused routine");
        Ok(routine)
    }

    pub fn resume_routine(
        &self,
        id: &RoutineId,
        now: DateTime<Utc>,
    ) -> Result<Routine, DomainError> {
        let mut routine = self
            .store
            .get_routine(id)?
            .ok_or_else(|| not_found::routine(id))?;
        routine.resume(now)?;
        self.store.update_routine(&routine)?;
        info!(id = %routine.id, "resumed routine");
        Ok(routine)
    }

    pub fn delete_routine(&self, id: &RoutineId) -> Result<(), DomainError> {
        self.store.delete_routine(id)?;
        info!(id = %id, "deleted routine");
        Ok(())
    }

    pub fn list_runs(
        &self,
        routine_id: &RoutineId,
        limit: u32,
    ) -> Result<Vec<RoutineRun>, DomainError> {
        self.store.list_runs(routine_id, limit)
    }

    /// Mark runs that have been `Running` for longer than `stale_after` as
    /// failed. Called at boot, where the only runs still marked running are the
    /// ones a crash cut off.
    pub fn reap_stale_runs(
        &self,
        now: DateTime<Utc>,
        stale_after: Duration,
    ) -> Result<usize, DomainError> {
        let cutoff =
            now - chrono::Duration::from_std(stale_after).unwrap_or(chrono::Duration::zero());
        let mut reaped = 0;
        for routine in self.store.list_routines()? {
            for mut run in self.store.list_runs(&routine.id, 64)? {
                if run.status == RunStatus::Running && run.started_at < cutoff {
                    run.fail(now, "the brain stopped before this run finished");
                    self.store.update_run(&run)?;
                    reaped += 1;
                }
            }
        }
        if reaped > 0 {
            warn!(reaped, "reaped routine runs a restart cut off");
        }
        Ok(reaped)
    }

    pub fn get_run(&self, run_id: &RunId) -> Result<Option<RoutineRun>, DomainError> {
        self.store.get_run(run_id)
    }

    /// Check and trigger all routines that are due at `now`.
    pub fn fire_due(&self, now: DateTime<Utc>) -> Result<usize, DomainError> {
        let due = self.store.due_routines(now)?;
        let mut fired = 0;
        for mut routine in due {
            let dm = match self.store.direct_conversation(&routine.bot_id)? {
                Some(dm) => dm,
                None => {
                    warn!(
                        bot_id = %routine.bot_id,
                        routine_id = %routine.id,
                        "bot has no direct conversation; skipping routine"
                    );
                    continue;
                }
            };

            let mut run = RoutineRun::start(&routine, now);
            run.conversation_id = Some(dm.id.clone());

            let delivery = Delivery::routine(
                dm.id.clone(),
                routine.bot_id.clone(),
                routine.id.clone(),
                &routine.prompt,
            );
            run.delivery_id = Some(delivery.id.clone());

            self.store.insert_delivery(&delivery)?;
            self.store.insert_run(&run)?;

            routine.mark_run(now)?;
            self.store.update_routine(&routine)?;

            if let Err(err) = self.runners.deliver(delivery) {
                warn!(
                    error = %err,
                    routine = %routine.id,
                    "cannot hand routine delivery to its bot"
                );
            }
            fired += 1;
            debug!(
                routine = %routine.name,
                bot = %routine.bot_id,
                "routine fired and delivery queued"
            );
        }
        Ok(fired)
    }

    /// Spawn a background ticker that checks and fires due routines.
    pub fn spawn_ticker(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let scheduler = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(ROUTINE_TICK);
            loop {
                tick.tick().await;
                match scheduler.fire_due(Utc::now()) {
                    Ok(0) => {}
                    Ok(n) => debug!(fired = n, "fired due routines"),
                    Err(err) => warn!(error = %err, "cannot fire routines"),
                }
            }
        })
    }
}

/// Close the run a finished turn was executing, if any.
///
/// A free function rather than a method because the runner has the store
/// and the finished deliveries but not the scheduler: threading the
/// scheduler into `RunnerEnv` would make the runner depend on routines,
/// when the dependency is the other way round.
///
/// `Ok(false)` means "this delivery was not a routine run", which is the
/// common case and not an error.
pub fn settle_delivery(
    store: &dyn Persistence,
    delivery: &Delivery,
    ok: bool,
    error: Option<&str>,
) -> Result<bool, DomainError> {
    let Some(routine_id) = &delivery.routine_id else {
        return Ok(false);
    };
    // The run is the newest `Running` row of this routine whose delivery is
    // this one; a routine that fired twice has two rows and they are
    // distinguishable by delivery id.
    let runs = store.list_runs(routine_id, 32)?;
    let Some(mut run) = runs.into_iter().find(|run| {
        run.delivery_id.as_ref() == Some(&delivery.id) && run.status == RunStatus::Running
    }) else {
        return Ok(false);
    };
    let now = Utc::now();
    if ok {
        run.complete(now);
    } else {
        run.fail(now, error.unwrap_or("the turn failed"));
    }
    store.update_run(&run)?;
    debug!(run = %run.id, routine = %routine_id, ok, "routine run settled");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;
    use chrono::Duration;
    use swarmbot_contracts::delivery::DeliveryKind;
    use swarmbot_contracts::routine::RoutineStatus;

    #[test]
    fn routine_creation_and_firing() {
        let store = testing::memory_store();
        let bot = testing::seed_bot(&store, "lead");
        let runners = RunnerMap::new();
        let scheduler = RoutineScheduler::new(store.clone(), runners);

        let now = Utc::now();
        let mut routine = scheduler
            .create_routine(
                bot.id.clone(),
                "Morning Report",
                "Summarize yesterday's work",
                RoutineSchedule::Interval { every_secs: 60 },
                now,
            )
            .unwrap();

        assert_eq!(routine.name, "Morning Report");
        assert_eq!(routine.status, RoutineStatus::Active);

        // Not due yet
        let fired = scheduler.fire_due(now).unwrap();
        assert_eq!(fired, 0);

        // Force it due
        routine.next_run_at = Some(now - Duration::seconds(1));
        store.update_routine(&routine).unwrap();

        let fired = scheduler.fire_due(now).unwrap();
        assert_eq!(fired, 1);

        // Check delivery queued
        let queued = store.queued_deliveries(&bot.id).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].kind, DeliveryKind::Routine);
        assert_eq!(queued[0].routine_id, Some(routine.id.clone()));
        assert_eq!(queued[0].text, "Summarize yesterday's work");

        // Check run recorded
        let runs = scheduler.list_runs(&routine.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].routine_id, routine.id);
        assert_eq!(runs[0].delivery_id, Some(queued[0].id.clone()));

        // Check routine next_run_at advanced
        let updated = scheduler.get_routine(&routine.id).unwrap().unwrap();
        assert!(updated.last_run_at.is_some());
        assert!(updated.next_run_at.unwrap() > now);

        // Pause routine
        let paused = scheduler.pause_routine(&routine.id).unwrap();
        assert_eq!(paused.status, RoutineStatus::Paused);

        // Resume routine
        let resumed = scheduler.resume_routine(&routine.id, now).unwrap();
        assert_eq!(resumed.status, RoutineStatus::Active);

        // Delete routine
        scheduler.delete_routine(&routine.id).unwrap();
        assert_eq!(scheduler.get_routine(&routine.id).unwrap(), None);
    }

    #[test]
    fn a_fired_run_is_closed_when_its_turn_ends() {
        // The bug this pins: a run was inserted as `Running` and nothing ever
        // called `update_run`, so every run the API listed was a permanent lie
        // and the table grew one dead row per tick.
        let store = testing::memory_store();
        let bot = testing::seed_bot(&store, "lead");
        let scheduler = RoutineScheduler::new(store.clone(), RunnerMap::new());

        let now = Utc::now();
        let mut routine = scheduler
            .create_routine(
                bot.id.clone(),
                "Nightly",
                "check the build",
                RoutineSchedule::Interval { every_secs: 60 },
                now,
            )
            .unwrap();
        routine.next_run_at = Some(now - Duration::seconds(1));
        store.update_routine(&routine).unwrap();
        assert_eq!(scheduler.fire_due(now).unwrap(), 1);

        let delivery = store
            .queued_deliveries(&bot.id)
            .unwrap()
            .into_iter()
            .find(|d| d.routine_id.as_ref() == Some(&routine.id))
            .expect("the routine's delivery");
        let run = scheduler.list_runs(&routine.id, 10).unwrap().remove(0);
        assert_eq!(run.status, RunStatus::Running);

        assert!(
            settle_delivery(store.as_ref(), &delivery, true, None).unwrap(),
            "a routine delivery must settle its run"
        );
        let run = scheduler.list_runs(&routine.id, 10).unwrap().remove(0);
        assert_eq!(run.status, RunStatus::Success);
        assert!(run.ended_at.is_some());

        // Settling twice is a no-op, not a second write: a turn's deliveries
        // can be walked more than once and a terminal row must stay terminal.
        assert!(!settle_delivery(store.as_ref(), &delivery, false, None).unwrap());
        let run = scheduler.list_runs(&routine.id, 10).unwrap().remove(0);
        assert_eq!(run.status, RunStatus::Success);
    }

    #[test]
    fn a_failed_routine_turn_records_why() {
        let store = testing::memory_store();
        let bot = testing::seed_bot(&store, "lead");
        let scheduler = RoutineScheduler::new(store.clone(), RunnerMap::new());
        let now = Utc::now();
        let mut routine = scheduler
            .create_routine(
                bot.id.clone(),
                "Nightly",
                "check the build",
                RoutineSchedule::Interval { every_secs: 60 },
                now,
            )
            .unwrap();
        routine.next_run_at = Some(now - Duration::seconds(1));
        store.update_routine(&routine).unwrap();
        scheduler.fire_due(now).unwrap();

        let delivery = store
            .queued_deliveries(&bot.id)
            .unwrap()
            .into_iter()
            .find(|d| d.routine_id.is_some())
            .expect("delivery");
        assert!(settle_delivery(store.as_ref(), &delivery, false, Some("backend 402")).unwrap());
        let run = scheduler.list_runs(&routine.id, 10).unwrap().remove(0);
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.error.as_deref(), Some("backend 402"));
    }

    #[test]
    fn a_non_routine_delivery_settles_nothing() {
        let store = testing::memory_store();
        let bot = testing::seed_bot(&store, "lead");
        let delivery =
            Delivery::owner_message(bot.direct_conversation_id.clone(), bot.id.clone(), "hi");
        assert!(!settle_delivery(store.as_ref(), &delivery, true, None).unwrap());
    }

    #[test]
    fn a_run_a_crash_cut_off_is_reaped_at_boot() {
        let store = testing::memory_store();
        let bot = testing::seed_bot(&store, "lead");
        let scheduler = RoutineScheduler::new(store.clone(), RunnerMap::new());
        let now = Utc::now();
        let mut routine = scheduler
            .create_routine(
                bot.id.clone(),
                "Nightly",
                "check the build",
                RoutineSchedule::Interval { every_secs: 60 },
                now,
            )
            .unwrap();
        routine.next_run_at = Some(now - Duration::seconds(1));
        store.update_routine(&routine).unwrap();
        scheduler.fire_due(now).unwrap();

        // Not stale yet: a run that just started is legitimately running.
        assert_eq!(
            scheduler
                .reap_stale_runs(now, std::time::Duration::from_secs(6 * 3600))
                .unwrap(),
            0
        );

        let later = now + Duration::hours(7);
        assert_eq!(
            scheduler
                .reap_stale_runs(later, std::time::Duration::from_secs(6 * 3600))
                .unwrap(),
            1
        );
        let run = scheduler.list_runs(&routine.id, 10).unwrap().remove(0);
        assert_eq!(run.status, RunStatus::Failed);
        assert!(
            run.error.as_deref().unwrap_or_default().contains("stopped"),
            "{:?}",
            run.error
        );
    }
}
