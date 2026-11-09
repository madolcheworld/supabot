//! Routines: scheduled triggers that wake a bot.
//!
//! A routine is an instruction or prompt that runs on an interval, at a fixed
//! time, or on a cron schedule. When triggered, it creates a `Delivery` of
//! kind `DeliveryKind::Routine` for the target bot.

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::id::{BotId, ConversationId, DeliveryId, RoutineId, RunId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineStatus {
    Active,
    Paused,
    Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Success,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoutineSchedule {
    Interval { every_secs: u64 },
    Once { run_at: DateTime<Utc> },
    Cron { expr: String },
}

impl RoutineSchedule {
    /// Compute the next execution time strictly after `after`.
    pub fn next_run_after(
        &self,
        after: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, DomainError> {
        match self {
            Self::Interval { every_secs } => {
                if *every_secs == 0 {
                    return Err(DomainError::invalid("interval every_secs must be > 0"));
                }
                let secs = i64::try_from(*every_secs)
                    .map_err(|_| DomainError::invalid("interval every_secs is too large"))?;
                Ok(Some(after + Duration::seconds(secs)))
            }
            Self::Once { run_at } => {
                if *run_at > after {
                    Ok(Some(*run_at))
                } else {
                    Ok(None)
                }
            }
            Self::Cron { expr } => cron_next_after(expr, after),
        }
    }
}

fn parse_cron_part(part: &str, min: u32, max: u32) -> Result<Vec<u32>, DomainError> {
    let mut values = Vec::new();
    for sub in part.split(',') {
        let sub = sub.trim();
        if sub.is_empty() {
            return Err(DomainError::invalid(format!(
                "invalid cron field: '{part}'"
            )));
        }
        if sub == "*" {
            values.extend(min..=max);
        } else if let Some(step_str) = sub.strip_prefix("*/") {
            let step: u32 = step_str
                .parse()
                .map_err(|_| DomainError::invalid(format!("invalid cron step: '{sub}'")))?;
            if step == 0 {
                return Err(DomainError::invalid("cron step must be > 0"));
            }
            let mut v = min;
            while v <= max {
                values.push(v);
                v += step;
            }
        } else if sub.contains('-') {
            let (range_str, step) = if let Some((r, s)) = sub.split_once('/') {
                let step: u32 = s
                    .parse()
                    .map_err(|_| DomainError::invalid(format!("invalid cron step: '{sub}'")))?;
                if step == 0 {
                    return Err(DomainError::invalid("cron step must be > 0"));
                }
                (r, step)
            } else {
                (sub, 1)
            };
            let (start_str, end_str) = range_str
                .split_once('-')
                .ok_or_else(|| DomainError::invalid(format!("invalid cron range: '{sub}'")))?;
            let start: u32 = start_str.parse().map_err(|_| {
                DomainError::invalid(format!("invalid cron range start: '{start_str}'"))
            })?;
            let end: u32 = end_str.parse().map_err(|_| {
                DomainError::invalid(format!("invalid cron range end: '{end_str}'"))
            })?;
            if start > end || start < min || end > max {
                return Err(DomainError::invalid(format!(
                    "cron range {start}-{end} out of bounds ({min}-{max})"
                )));
            }
            let mut v = start;
            while v <= end {
                values.push(v);
                v += step;
            }
        } else {
            let val: u32 = sub
                .parse()
                .map_err(|_| DomainError::invalid(format!("invalid cron value: '{sub}'")))?;
            if val < min || val > max {
                return Err(DomainError::invalid(format!(
                    "cron value {val} out of bounds ({min}-{max})"
                )));
            }
            values.push(val);
        }
    }
    values.sort_unstable();
    values.dedup();
    Ok(values)
}

fn cron_next_after(expr: &str, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>, DomainError> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(DomainError::invalid(format!(
            "cron expression must have 5 fields (min hour dom mon dow), got {}",
            parts.len()
        )));
    }
    let minutes = parse_cron_part(parts[0], 0, 59)?;
    let hours = parse_cron_part(parts[1], 0, 23)?;
    let doms = parse_cron_part(parts[2], 1, 31)?;
    let months = parse_cron_part(parts[3], 1, 12)?;
    let mut dows = parse_cron_part(parts[4], 0, 7)?;
    if dows.contains(&7) {
        if !dows.contains(&0) {
            dows.push(0);
        }
        dows.retain(|&x| x != 7);
        dows.sort_unstable();
    }

    let start = (after + Duration::minutes(1))
        .with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(after + Duration::minutes(1));

    let max_minutes = 527040;
    let mut candidate = start;
    let mut checked = 0;

    while checked < max_minutes {
        let mon = candidate.month();
        if !months.contains(&mon)
            && let Some(next_month) = candidate
                .with_day(1)
                .and_then(|c| c.with_hour(0))
                .and_then(|c| c.with_minute(0))
        {
            let jumped = (next_month + Duration::days(32))
                .with_day(1)
                .unwrap_or(candidate + Duration::days(1));
            let diff = (jumped - candidate).num_minutes();
            checked += if diff > 0 { diff as usize } else { 1 };
            candidate = jumped;
            continue;
        }

        let dom = candidate.day();
        let dow = candidate.weekday().num_days_from_sunday();
        let dom_match = doms.contains(&dom);
        let dow_match = dows.contains(&dow);
        let dom_is_star = parts[2] == "*";
        let dow_is_star = parts[4] == "*";

        let day_match = match (dom_is_star, dow_is_star) {
            (true, true) => true,
            (false, true) => dom_match,
            (true, false) => dow_match,
            (false, false) => dom_match || dow_match,
        };

        if !day_match && let Some(next_day) = candidate.with_hour(0).and_then(|c| c.with_minute(0))
        {
            let jumped = next_day + Duration::days(1);
            let diff = (jumped - candidate).num_minutes();
            checked += if diff > 0 { diff as usize } else { 1 };
            candidate = jumped;
            continue;
        }

        let hour = candidate.hour();
        if !hours.contains(&hour)
            && let Some(next_hour) = candidate.with_minute(0)
        {
            let jumped = next_hour + Duration::hours(1);
            let diff = (jumped - candidate).num_minutes();
            checked += if diff > 0 { diff as usize } else { 1 };
            candidate = jumped;
            continue;
        }

        let minute = candidate.minute();
        if minutes.contains(&minute) {
            return Ok(Some(candidate));
        }

        candidate += Duration::minutes(1);
        checked += 1;
    }

    Ok(None)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Routine {
    pub id: RoutineId,
    pub bot_id: BotId,
    pub name: String,
    pub description: String,
    pub schedule: RoutineSchedule,
    pub status: RoutineStatus,
    pub prompt: String,
    pub next_run_at: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Routine {
    pub fn new(
        bot_id: BotId,
        name: impl Into<String>,
        prompt: impl Into<String>,
        schedule: RoutineSchedule,
        now: DateTime<Utc>,
    ) -> Result<Self, DomainError> {
        let name = name.into().trim().to_string();
        if name.is_empty() {
            return Err(DomainError::invalid("routine name cannot be empty"));
        }
        let prompt = prompt.into();
        let next_run_at = schedule.next_run_after(now)?;
        let status = if next_run_at.is_some() {
            RoutineStatus::Active
        } else {
            RoutineStatus::Completed
        };
        Ok(Self {
            id: RoutineId::new(),
            bot_id,
            name,
            description: String::new(),
            schedule,
            status,
            prompt,
            next_run_at,
            last_run_at: None,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn pause(&mut self) {
        self.status = RoutineStatus::Paused;
        self.updated_at = Utc::now();
    }

    pub fn resume(&mut self, now: DateTime<Utc>) -> Result<(), DomainError> {
        if self.status == RoutineStatus::Paused {
            self.next_run_at = self.schedule.next_run_after(now)?;
            self.status = if self.next_run_at.is_some() {
                RoutineStatus::Active
            } else {
                RoutineStatus::Completed
            };
            self.updated_at = now;
        }
        Ok(())
    }

    pub fn mark_run(&mut self, now: DateTime<Utc>) -> Result<(), DomainError> {
        self.last_run_at = Some(now);
        self.next_run_at = self.schedule.next_run_after(now)?;
        if self.next_run_at.is_none() {
            self.status = RoutineStatus::Completed;
        }
        self.updated_at = now;
        Ok(())
    }

    pub fn is_due(&self, now: DateTime<Utc>) -> bool {
        self.status == RoutineStatus::Active && self.next_run_at.is_some_and(|next| next <= now)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutineRun {
    pub id: RunId,
    pub routine_id: RoutineId,
    pub bot_id: BotId,
    pub conversation_id: Option<ConversationId>,
    pub delivery_id: Option<DeliveryId>,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

impl RoutineRun {
    pub fn start(routine: &Routine, started_at: DateTime<Utc>) -> Self {
        Self {
            id: RunId::new(),
            routine_id: routine.id.clone(),
            bot_id: routine.bot_id.clone(),
            conversation_id: None,
            delivery_id: None,
            status: RunStatus::Running,
            started_at,
            ended_at: None,
            error: None,
        }
    }

    pub fn complete(&mut self, ended_at: DateTime<Utc>) {
        self.status = RunStatus::Success;
        self.ended_at = Some(ended_at);
    }

    pub fn fail(&mut self, ended_at: DateTime<Utc>, error: impl Into<String>) {
        self.status = RunStatus::Failed;
        self.ended_at = Some(ended_at);
        self.error = Some(error.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn interval_schedule_computes_next() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let sched = RoutineSchedule::Interval { every_secs: 60 };
        let next = sched.next_run_after(now).unwrap();
        assert_eq!(next, Some(now + Duration::seconds(60)));
    }

    #[test]
    fn once_schedule_past_returns_none() {
        let t1 = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2026, 1, 1, 13, 0, 0).unwrap();
        let sched = RoutineSchedule::Once { run_at: t1 };
        assert_eq!(sched.next_run_after(t2).unwrap(), None);
        assert_eq!(
            sched.next_run_after(t1 - Duration::seconds(10)).unwrap(),
            Some(t1)
        );
    }

    #[test]
    fn cron_schedule_every_minute() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 30).unwrap();
        let sched = RoutineSchedule::Cron {
            expr: "* * * * *".into(),
        };
        let next = sched.next_run_after(now).unwrap().unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 1, 12, 1, 0).unwrap());
    }

    #[test]
    fn cron_schedule_specific_hour() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 8, 30, 0).unwrap();
        let sched = RoutineSchedule::Cron {
            expr: "0 9 * * *".into(),
        };
        let next = sched.next_run_after(now).unwrap().unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 1, 9, 0, 0).unwrap());
    }

    #[test]
    fn routine_lifecycle() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let mut routine = Routine::new(
            BotId::new(),
            "Daily report",
            "Generate summary",
            RoutineSchedule::Interval { every_secs: 3600 },
            now,
        )
        .unwrap();
        assert_eq!(routine.status, RoutineStatus::Active);
        assert!(!routine.is_due(now));
        assert!(routine.is_due(now + Duration::seconds(3600)));

        routine.mark_run(now + Duration::seconds(3600)).unwrap();
        assert_eq!(routine.last_run_at, Some(now + Duration::seconds(3600)));
        assert_eq!(routine.next_run_at, Some(now + Duration::seconds(7200)));

        routine.pause();
        assert_eq!(routine.status, RoutineStatus::Paused);
        assert!(!routine.is_due(now + Duration::seconds(7200)));

        routine.resume(now + Duration::seconds(7200)).unwrap();
        assert_eq!(routine.status, RoutineStatus::Active);
    }
}
