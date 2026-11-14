//! Operational numbers: `GET /api/metrics`.
//!
//! This is the answer to "what is this brain doing right now?" without reading
//! its log — the thing `swarmbot status` and the UI's status line print. Two rules
//! shape it, and both exist to keep it from becoming a dashboard of fiction:
//!
//! * **every number is a count of something the store or the runner map already
//!   knows.** Nothing here is an estimate, a rate or a projection: there is no
//!   "turns per minute" because this route keeps no history, and no memory or CPU
//!   figures because a number the brain cannot verify itself would be noise with
//!   authority. [`Metrics::turns_running`] comes from the *store* rather than from
//!   the in-memory runner map on purpose: the store is what `Swarm::wait_for_idle`
//!   trusts, and a turn the store still calls `Running` is the one kind of "still
//!   working" that is invisible everywhere else (`AGENTS.md` records a whole
//!   debugging session spent on exactly that).
//! * **a queue is reported as two numbers, not one.** A delivery *queued* for a
//!   running bot and a delivery *parked* until a runner exists are different
//!   facts — the second one is what makes `wait_for_idle` never return — so they
//!   are [`Metrics::deliveries_queued`] and [`Metrics::deliveries_parked`].
//!
//! What it deliberately does **not** do: it does not touch a backend, spawn a
//! probe or read the disk. `/api/metrics` has to stay cheap enough to poll, and a
//! route that dials an LLM to answer "how busy are you" is a health check that
//! costs money.
//!
//! One consequence of being honest: `bots_working` counts `BotStatus::Working` as
//! the roster row stores it. The runner updates that row at the start and end of a
//! turn, so in the window between a turn ending and its row being written the two
//! can disagree. `turns_running` is the number to trust when they do.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use swarmbot_contracts::bot::BotStatus;
use swarmbot_contracts::repo::Persistence;

use crate::error::ApiResult;
use crate::state::AppState;

/// One snapshot of what the brain is doing. Every field is documented at
/// [`Metrics`]; serialized as a flat JSON object with the same names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metrics {
    /// The brain's version (`CARGO_PKG_VERSION`), so a client can say which
    /// binary it is talking to.
    pub version: String,
    /// Seconds since this process started. Monotonic, not wall-clock.
    pub uptime_secs: u64,
    /// The live event log's epoch; a different value means the brain restarted
    /// and a stored cursor must resync instead of resuming.
    pub epoch: u64,

    /// Bots in the roster, including disabled ones.
    pub bots_total: u64,
    /// `BotStatus::Idle` — not working, nothing queued for it.
    pub bots_idle: u64,
    /// `BotStatus::Working` — mid-turn as the roster row records it.
    pub bots_working: u64,
    /// `BotStatus::Queued` — waiting for its runner.
    pub bots_queued: u64,
    /// `BotStatus::Error` — failed `FAILURE_LIMIT` turns in a row.
    pub bots_error: u64,
    /// `BotStatus::Disabled` — turned off by the owner; it gets no runner.
    pub bots_disabled: u64,
    /// Live runner tasks. A disabled or deleted bot has none, so this can be
    /// smaller than `bots_total`.
    pub runners: u64,

    /// Turns the **store** still calls `Running` — the signal that decides
    /// whether the swarm is idle.
    pub turns_running: u64,
    /// Deliveries waiting for a running bot, summed over every bot in the roster.
    pub deliveries_queued: u64,
    /// Deliveries waiting for a runner that does not exist (yet). This is the
    /// number that can keep `wait_for_idle` from ever returning.
    pub deliveries_parked: u64,
    /// Tasks in the queue, across every bot.
    pub tasks_queued: u64,
    /// Tasks currently running, across every bot.
    pub tasks_running: u64,
    /// Cards (approvals, questions, forms) still waiting for the owner.
    pub requests_pending: u64,

    /// Every roster row, revoked ones included.
    pub nodes_total: u64,
    /// Nodes that are `online && !revoked` — able to execute a tool call now.
    pub nodes_online: u64,
}

/// `GET /api/metrics`
pub async fn get(State(state): State<AppState>) -> ApiResult<Json<Metrics>> {
    // One blocking hop for the whole store half: a metrics poll must not queue
    // sixteen separate `spawn_blocking` jobs behind each other, and the numbers
    // ought to describe one consistent moment rather than sixteen.
    let counts = state.async_store().read(store_counts).await?;
    let runners = state.swarm.runners.len() as u64;
    let parked = state.swarm.runners.parked_count() as u64;
    Ok(Json(Metrics {
        version: state.version().to_string(),
        uptime_secs: state.uptime_secs(),
        epoch: state.epoch(),
        runners,
        deliveries_parked: parked,
        ..counts
    }))
}

/// The half of [`Metrics`] that is one `Persistence` read.
///
/// Every field is overwritten by the caller except the ones it cannot know; the
/// `Default` in `..counts` is therefore only for the fields this closure sets.
/// Named rather than inlined so the route body stays one readable expression.
fn store_counts(store: &dyn Persistence) -> Result<Metrics, swarmbot_contracts::DomainError> {
    let bots = store.list_bots()?;
    let mut bots_idle = 0;
    let mut bots_working = 0;
    let mut bots_queued = 0;
    let mut bots_error = 0;
    let mut bots_disabled = 0;
    let mut deliveries_queued = 0u64;
    for bot in &bots {
        match bot.status {
            BotStatus::Idle => bots_idle += 1,
            BotStatus::Working { .. } => bots_working += 1,
            BotStatus::Queued { .. } => bots_queued += 1,
            BotStatus::Error { .. } => bots_error += 1,
            BotStatus::Disabled => bots_disabled += 1,
        }
        // Per bot, because the port is per bot: a brain with twenty bots does
        // twenty cheap indexed reads, which is the honest way to get this number
        // without a new repository method.
        deliveries_queued += store.queued_deliveries(&bot.id)?.len() as u64;
    }
    let nodes = store.list_nodes()?;
    let nodes_online = nodes
        .iter()
        .filter(|node| node.online && !node.revoked)
        .count() as u64;
    Ok(Metrics {
        // Filled in by the caller, which is the only place that knows them.
        version: String::new(),
        uptime_secs: 0,
        epoch: 0,
        bots_total: bots.len() as u64,
        bots_idle,
        bots_working,
        bots_queued,
        bots_error,
        bots_disabled,
        // The in-memory runner map and the parked queue are the caller's.
        runners: 0,
        turns_running: store.running_turns()?.len() as u64,
        deliveries_queued,
        deliveries_parked: 0,
        tasks_queued: store.queued_tasks()?.len() as u64,
        tasks_running: store.running_tasks()?.len() as u64,
        requests_pending: store.pending_requests()?.len() as u64,
        nodes_total: nodes.len() as u64,
        nodes_online,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use swarmbot_contracts::repo::{BotRepo as _, ConversationRepo as _};
    use swarmbot_contracts::{Bot, Conversation};

    fn store_with(bots: usize, disabled: usize) -> Arc<dyn Persistence> {
        let store = swarmbot_store::MemoryStore::new();
        for index in 0..bots {
            let conversation = Conversation::direct(swarmbot_contracts::BotId::new());
            store.insert_conversation(&conversation).expect("a DM");
            let mut bot = Bot::new(
                format!("bot-{index}"),
                "Bot",
                "instructions",
                "fake",
                conversation.id,
            );
            if index < disabled {
                bot.status = BotStatus::Disabled;
            }
            store.insert_bot(&bot).expect("a bot");
        }
        Arc::new(store)
    }

    #[test]
    fn the_store_half_counts_every_status_and_a_disabled_bot_is_not_idle() {
        // Two enabled bots and one disabled: the two statuses are different
        // numbers here, which is the whole point of counting them apart.
        let counts = store_counts(store_with(3, 1).as_ref()).expect("counts");
        assert_eq!(counts.bots_total, 3);
        assert_eq!(counts.bots_disabled, 1);
        assert_eq!(counts.bots_idle, 2);
        assert_eq!(counts.bots_working, 0);
        assert_eq!(counts.bots_queued, 0);
        assert_eq!(counts.bots_error, 0);
        assert_eq!(counts.deliveries_queued, 0);
        assert_eq!(counts.turns_running, 0);
        assert_eq!(counts.tasks_queued, 0);
        assert_eq!(counts.tasks_running, 0);
        assert_eq!(counts.requests_pending, 0);
    }

    #[test]
    fn an_empty_brain_reports_zeros_rather_than_failing() {
        let counts = store_counts(store_with(0, 0).as_ref()).expect("counts");
        assert_eq!(counts.bots_total, 0);
        assert_eq!(counts.nodes_total, 0, "no node registered yet");
        assert_eq!(counts.nodes_online, 0);
    }

    #[test]
    fn the_documented_fields_are_exactly_the_ones_a_client_may_read() {
        // The client crate mirrors this struct field for field; a rename here is
        // a broken front end there, and the contract test in
        // `tests/client_contract.rs` is what catches it end to end.
        let value = serde_json::to_value(Metrics {
            version: "0.0.0".into(),
            uptime_secs: 1,
            epoch: 2,
            bots_total: 3,
            bots_idle: 4,
            bots_working: 5,
            bots_queued: 6,
            bots_error: 7,
            bots_disabled: 8,
            runners: 9,
            turns_running: 10,
            deliveries_queued: 11,
            deliveries_parked: 12,
            tasks_queued: 13,
            tasks_running: 14,
            requests_pending: 15,
            nodes_total: 16,
            nodes_online: 17,
        })
        .expect("serializes");
        let object = value.as_object().expect("an object");
        assert_eq!(object.len(), 18, "{value}");
        for key in [
            "version",
            "uptime_secs",
            "epoch",
            "bots_total",
            "bots_idle",
            "bots_working",
            "bots_queued",
            "bots_error",
            "bots_disabled",
            "runners",
            "turns_running",
            "deliveries_queued",
            "deliveries_parked",
            "tasks_queued",
            "tasks_running",
            "requests_pending",
            "nodes_total",
            "nodes_online",
        ] {
            assert!(object.get(key).is_some(), "missing `{key}` in {value}");
        }
        assert_eq!(
            serde_json::from_value::<Metrics>(value).expect("round trips"),
            Metrics {
                version: "0.0.0".into(),
                uptime_secs: 1,
                epoch: 2,
                bots_total: 3,
                bots_idle: 4,
                bots_working: 5,
                bots_queued: 6,
                bots_error: 7,
                bots_disabled: 8,
                runners: 9,
                turns_running: 10,
                deliveries_queued: 11,
                deliveries_parked: 12,
                tasks_queued: 13,
                tasks_running: 14,
                requests_pending: 15,
                nodes_total: 16,
                nodes_online: 17,
            }
        );
    }
}
