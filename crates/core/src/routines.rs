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
use swarmbot_contracts::routine::{Routine, RoutineRun, RoutineSchedule};

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
}
