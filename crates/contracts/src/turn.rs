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

/// What one turn spent, **as the backend reported it**.
///
/// These are the numbers a model round trip came back with, summed — never an
/// estimate this runtime made. A backend that reports no usage leaves the
/// counters at zero, so `Usage::default()` means "nothing was reported" and
/// must not be read as "nothing was spent". See [`UsageReport`] for the window
/// view, which carries a turn count for exactly that reason.
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

/// One bot's share of a [`UsageReport`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotUsage {
    pub bot_id: BotId,
    /// The bot's stored `@handle`, or `None` when the bot has been deleted: its
    /// turns still count — dropping the row would understate the spend — they
    /// just have no name left to show.
    pub bot_name: Option<String>,
    pub usage: Usage,
}

/// What a window of turns cost, **as the backends reported it**.
///
/// Read this before trusting a number. Every field is the usage a backend
/// reported on each model round trip, summed; nothing here is estimated by this
/// runtime. A backend that reports nothing — the `fake` one reports
/// `llm_calls: 1` and zero tokens, and an ACP agent may report no usage at all —
/// therefore makes its turns look free. Zeros mean "nothing was reported", which
/// is **not** the same claim as "nothing was spent", and `turns` is carried so a
/// reader can tell an empty window from a window of unreported turns.
///
/// There is deliberately no cost-per-token table in this crate: turning tokens
/// into money needs the owner's per-model rates, and inventing them would be a
/// lie with a decimal point.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReport {
    /// Start of the window, inclusive.
    pub since: DateTime<Utc>,
    /// End of the window, as the moment the report was built.
    ///
    /// The window is `[since, until]`. This type does not enforce the upper
    /// bound — it folds whatever turns it is given — so a caller that wants
    /// "a turn that starts after this belongs to the next report" must filter
    /// its window to `started_at <= until` first. [`crate::turn::UsageReport::from_turns`]
    /// documents only what it does: sum, group, order.
    pub until: DateTime<Utc>,
    /// How many turns fell in the window — the denominator behind the zeros.
    pub turns: u64,
    /// Every bot's usage summed.
    pub totals: Usage,
    /// Per bot, biggest spender first (ties by bot id), so a reader sees where
    /// the tokens went without sorting, and a test can pin the order.
    pub by_bot: Vec<BotUsage>,
}

impl UsageReport {
    /// Fold a window of turns into a report.
    ///
    /// `name_of` resolves a bot's `@handle`; it is called once per distinct bot
    /// and its `None` is kept as `bot_name: None` rather than dropping the row.
    pub fn from_turns(
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        turns: &[Turn],
        mut name_of: impl FnMut(&BotId) -> Option<String>,
    ) -> Self {
        let mut totals = Usage::default();
        let mut by_bot: Vec<BotUsage> = Vec::new();
        for turn in turns {
            totals.merge(turn.usage);
            match by_bot.iter_mut().find(|row| row.bot_id == turn.bot_id) {
                Some(row) => row.usage.merge(turn.usage),
                None => by_bot.push(BotUsage {
                    bot_id: turn.bot_id.clone(),
                    bot_name: name_of(&turn.bot_id),
                    usage: turn.usage,
                }),
            }
        }
        by_bot.sort_by(|a, b| {
            b.usage
                .total_tokens()
                .cmp(&a.usage.total_tokens())
                .then_with(|| a.bot_id.cmp(&b.bot_id))
        });
        Self {
            since,
            until,
            turns: turns.len() as u64,
            totals,
            by_bot,
        }
    }

    /// No turn ran in the window. The honest "nothing to report" — distinct
    /// from a window full of turns whose backends reported no usage.
    pub fn is_empty(&self) -> bool {
        self.turns == 0
    }

    /// The report for `[since, until]`, read from `store`.
    ///
    /// This lives in `contracts` rather than in the HTTP route because **two
    /// front ends answer the same question**: the brain serves it at
    /// `GET /api/usage`, and the CLI's embedded mode reads its own store. If
    /// each wrote the window rule down itself, they would drift, and the one
    /// that drifted would be wrong in a way nobody notices — a report is only
    /// ever compared against a number somebody remembers.
    ///
    /// Three rules are shared because they are easy to get subtly wrong:
    ///
    /// * `since` is **inclusive** (`started_at >= since`), which is what the
    ///   store's `turns_since` pins in the contract suite;
    /// * `until` is an **upper bound enforced here**, after the read. Without
    ///   it a turn that started between taking `until` and the store answering
    ///   would be counted in this report *and* in a later one that started from
    ///   this `until` — and `until` would be a lie about what it covers;
    /// * a bot that no longer exists keeps its row with `bot_name: None`. Its
    ///   spend is real even when its name is gone, and a store error reading a
    ///   bot must not fail the report for the same reason.
    pub fn window(
        store: &dyn crate::repo::Persistence,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Self, crate::error::DomainError> {
        let turns: Vec<Turn> = store
            .turns_since(since)?
            .into_iter()
            .filter(|turn| turn.started_at <= until)
            .collect();
        Ok(Self::from_turns(since, until, &turns, |bot_id: &BotId| {
            store.get_bot(bot_id).ok().flatten().map(|bot| bot.name)
        }))
    }
}

/// The window a caller gets when it asks for none.
///
/// A day is the shortest span that answers "what did this team cost me today?"
/// without the caller having to know the brain's clock, and it keeps an
/// unadorned request cheap on a store that has been running for months.
pub const DEFAULT_WINDOW_HOURS: i64 = 24;

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

    fn turn_of(bot: &BotId, usage: Usage, started_at: DateTime<Utc>) -> Turn {
        let mut turn = Turn::start(
            bot.clone(),
            ConversationId::new(),
            TurnInitiator::Owner,
            None,
            None,
        );
        turn.started_at = started_at;
        turn.usage = usage;
        turn
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            llm_calls: 1,
            tool_calls: 0,
        }
    }

    #[test]
    fn a_usage_report_sums_per_bot_and_orders_by_spend() {
        let since = Utc::now();
        let lead = BotId::new();
        let scout = BotId::new();
        let turns = vec![
            turn_of(&scout, usage(10, 1), since),
            turn_of(&lead, usage(100, 20), since),
            turn_of(&lead, usage(5, 5), since),
        ];

        let report = UsageReport::from_turns(since, since, &turns, |bot| {
            if *bot == lead {
                Some("lead".to_string())
            } else {
                Some("scout".to_string())
            }
        });

        assert_eq!(report.turns, 3, "every turn in the window is counted");
        assert_eq!(report.totals.input_tokens, 115, "the totals sum the turns");
        assert_eq!(report.totals.output_tokens, 26);
        assert_eq!(report.totals.llm_calls, 3);
        assert_eq!(report.by_bot.len(), 2, "one row per distinct bot");
        assert_eq!(report.by_bot[0].bot_id, lead, "biggest spender first");
        assert_eq!(report.by_bot[0].usage.input_tokens, 105);
        assert_eq!(report.by_bot[1].bot_id, scout);
        assert_eq!(report.by_bot[1].bot_name.as_deref(), Some("scout"));
        assert!(!report.is_empty());
    }

    #[test]
    fn a_deleted_bot_keeps_its_row_with_no_name() {
        let since = Utc::now();
        let gone = BotId::new();
        let turns = vec![turn_of(&gone, usage(40, 2), since)];

        let report = UsageReport::from_turns(since, since, &turns, |_| None);

        // The spend is still reported: a deleted bot does not un-spend tokens.
        assert_eq!(report.totals.input_tokens, 40);
        assert_eq!(report.by_bot.len(), 1);
        assert_eq!(report.by_bot[0].bot_id, gone);
        assert_eq!(report.by_bot[0].bot_name, None);
    }

    #[test]
    fn an_empty_window_is_zeros_not_an_error() {
        let since = Utc::now();
        let report = UsageReport::from_turns(since, since, &[], |_| None);
        assert!(report.is_empty());
        assert_eq!(report.turns, 0);
        assert_eq!(report.totals, Usage::default());
        assert!(report.by_bot.is_empty());
    }

    #[test]
    fn a_report_of_unreported_usage_is_zeros_with_turns() {
        // The honesty case: the turn ran, the backend reported nothing, so the
        // totals are zero — and `turns` is what tells that apart from an empty
        // window. Nothing here may be invented to make the numbers look better.
        let since = Utc::now();
        let bot = BotId::new();
        let turns = vec![turn_of(&bot, Usage::default(), since)];
        let report = UsageReport::from_turns(since, since, &turns, |_| Some("lead".into()));
        assert_eq!(report.totals.total_tokens(), 0);
        assert_eq!(report.turns, 1, "a zero-token turn is still a turn");
        assert!(!report.is_empty());
    }

    #[test]
    fn the_report_serializes_with_the_documented_keys() {
        let since = Utc::now();
        let bot = BotId::new();
        let turns = vec![turn_of(&bot, usage(7, 3), since)];
        let report = UsageReport::from_turns(since, since, &turns, |_| Some("lead".into()));

        let json = serde_json::to_value(&report).unwrap();
        // `serde_json::Value` sorts object keys, so the set is what is pinned;
        // the order on the wire comes from the struct definition.
        assert_eq!(
            json.as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            ["since", "until", "turns", "totals", "by_bot"]
                .map(str::to_string)
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "{json}"
        );
        assert_eq!(json["totals"]["input_tokens"], 7);
        assert_eq!(json["by_bot"][0]["bot_name"], "lead");
        assert_eq!(json["by_bot"][0]["usage"]["output_tokens"], 3);
        // And it decodes back into the same value, which is what the client does.
        assert_eq!(serde_json::from_value::<UsageReport>(json).unwrap(), report);
    }
}
