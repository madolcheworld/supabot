//! The token budget: what it actually stops, and what it deliberately does not.
//!
//! A budget bug is invisible — everything keeps working and the bill is the only
//! symptom — so these tests assert on the *machine*: how many times the model was
//! asked, whether a `Turn` row was written at all, and whether the delivery that
//! was dropped says why. A test that only checked "the transcript mentions the
//! budget" would pass on a brain that spent the money first and apologised after.
//!
//! Two caps, two different promises:
//!
//! * `max_tokens_per_turn` is checked **after** each round trip, because tokens
//!   are only known once the backend reports them. The promise is "no further
//!   round trip", not "never exceed" — the test asserts the call that does *not*
//!   happen, which is the only part a real model would let a brain promise.
//! * `max_tokens_per_day` is checked **before** a turn exists, so the test
//!   asserts that no `Turn` row was written and the model was never called.
//!
//! Everything runs in the lead's DM rather than the group: a group reply wakes
//! teammates, and a call count that includes other bots' turns would pass or
//! fail for the wrong reason.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use swarmbot_contracts::Usage;
use swarmbot_contracts::config::{BackendSpec, BrainConfig, Settings};
use swarmbot_contracts::delivery::Delivery;
use swarmbot_contracts::entry::EntryKind;
use swarmbot_contracts::id::{BotId, ConversationId};
use swarmbot_contracts::task::{Task, TaskStatus};
use swarmbot_contracts::turn::{Turn, TurnInitiator, TurnStatus};
use swarmbot_llm::ScriptedTurn;

use swarmbot_core::compose;
use swarmbot_core::swarm::Swarm;

const FAKE: &str = "fake-budget";

fn config() -> BrainConfig {
    let mut config = BrainConfig {
        default_backend: FAKE.to_string(),
        ..BrainConfig::default()
    };
    config.backends = vec![BackendSpec::fake(FAKE)];
    let mut team = BrainConfig::demo_team(FAKE);
    for bot in team.bots.iter_mut() {
        bot.backend = Some(FAKE.to_string());
    }
    config.team = Some(team);
    config
}

struct Rig {
    swarm: Arc<Swarm>,
    /// The lead's DM: one bot, so a call count belongs to the turn under test.
    dm: ConversationId,
    lead: BotId,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().expect("a temp dir");
    let swarm =
        compose::memory(config(), dir.path(), vec![], &BTreeMap::new()).expect("a swarm builds");
    swarm.boot().expect("the swarm boots");

    let lead = swarm
        .bots
        .by_name("lead")
        .expect("the demo team has a lead")
        .id;
    let dm = swarm
        .store
        .get_bot(&lead)
        .expect("readable")
        .expect("the lead exists")
        .direct_conversation_id;
    Rig {
        swarm,
        dm,
        lead,
        _dir: dir,
    }
}

impl Rig {
    fn set(&self, change: impl FnOnce(&mut Settings)) {
        self.swarm
            .settings
            .update(change)
            .expect("the settings are writable");
    }

    fn script(&self, turns: Vec<ScriptedTurn>) {
        let fake = self
            .swarm
            .backends
            .registry()
            .fake(FAKE)
            .expect("the budget backend is a fake");
        for turn in turns {
            fake.push_turn(turn);
        }
    }

    /// Scripted turns the model has not been asked for yet. This — not a call
    /// count alone — is what "the turn stopped early" means.
    fn unspent_script(&self) -> usize {
        self.swarm
            .backends
            .registry()
            .fake(FAKE)
            .expect("the budget backend is a fake")
            .queued_turns()
    }

    fn calls(&self) -> usize {
        self.swarm
            .backends
            .registry()
            .fake(FAKE)
            .expect("the budget backend is a fake")
            .request_count()
    }

    /// A turn already in the store that spent `tokens` today, i.e. inside the
    /// rolling 24-hour window the daily cap is measured over.
    fn seed_spend(&self, tokens: u64) {
        let mut earlier = Turn::start(
            self.lead.clone(),
            self.dm.clone(),
            TurnInitiator::Owner,
            None,
            None,
        );
        earlier.usage = Usage {
            input_tokens: tokens,
            output_tokens: 0,
            llm_calls: 1,
            tool_calls: 0,
        };
        earlier.complete(chrono::Utc::now());
        self.swarm
            .store
            .insert_turn(&earlier)
            .expect("the earlier turn lands");
    }

    async fn settle(&self) {
        assert!(
            self.swarm.wait_for_idle(Duration::from_secs(20)).await,
            "the team did not go idle.\n{}",
            self.transcript()
        );
    }

    fn transcript(&self) -> String {
        self.swarm
            .conversations
            .history(&self.dm, 0, 200)
            .expect("history")
            .into_iter()
            .map(|entry| {
                format!(
                    "[{}] {}",
                    entry.kind.kind_str(),
                    entry.kind.text().unwrap_or_default().replace('\n', " ⏎ ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn notices(&self) -> Vec<String> {
        self.swarm
            .conversations
            .history(&self.dm, 0, 200)
            .expect("history")
            .into_iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Notice(notice) => Some(notice.text.clone()),
                _ => None,
            })
            .collect()
    }

    fn turns(&self) -> Vec<Turn> {
        self.swarm.store.turns_in(&self.dm, 200).expect("turns")
    }
}

/// A scripted turn that reports usage — the only way a fake can look like a real
/// model to the budget, which reads what the backend said it spent.
fn spends(text: &str, input: u64, output: u64) -> ScriptedTurn {
    ScriptedTurn::text(text).with_usage(Usage {
        input_tokens: input,
        output_tokens: output,
        llm_calls: 1,
        tool_calls: 0,
    })
}

#[tokio::test]
async fn a_turn_stops_calling_the_model_once_it_reaches_its_token_cap() {
    let rig = rig();
    rig.set(|settings| settings.max_tokens_per_turn = Some(100));
    // Three round trips are scripted. The first spends 60 (under the cap) and
    // asks for a tool, so the loop wants a second round. That second round spends
    // 60 more and crosses the cap — after which the scripted third turn must
    // stay unspent. That is the difference between a budget and an apology.
    rig.script(vec![
        spends("first", 40, 20).with_tool_call("list_bots", json!({})),
        // The second round asks for another tool on purpose: without the cap
        // the loop would take a third round, so "it stopped" and "it happened
        // to end anyway" cannot be confused.
        spends("second", 40, 20).with_tool_call("list_bots", json!({})),
        spends("third", 40, 20),
    ]);

    rig.swarm
        .send(&rig.dm, "what is everyone doing?")
        .expect("the owner's message is accepted");
    rig.settle().await;

    assert_eq!(
        rig.calls(),
        2,
        "the cap stops the third round trip.\n{}",
        rig.transcript()
    );
    assert_eq!(
        rig.unspent_script(),
        1,
        "the third scripted turn was never asked for.\n{}",
        rig.transcript()
    );
    let notices = rig.notices();
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("cap of 100") && notice.contains("120 token(s)")),
        "the transcript says what was spent and against what cap: {notices:?}\n{}",
        rig.transcript()
    );

    // The turn is a normal completion with a partial answer, not a failure:
    // nothing broke, the bot was cut off. The overshoot is recorded as it
    // happened rather than rounded down to the cap.
    let turns = rig.turns();
    let turn = turns.first().expect("one turn ran");
    assert_eq!(turn.status, TurnStatus::Completed, "{turn:?}");
    assert_eq!(turn.usage.total_tokens(), 120, "{turn:?}");
}

#[tokio::test]
async fn no_cap_means_no_cap() {
    let rig = rig();
    // Nothing set: all three scripted round trips must be spent. This guards the
    // shape where an absent cap is read as `Some(0)` or as "stop at the first
    // round".
    rig.script(vec![
        spends("one", 0, 0).with_tool_call("list_bots", json!({})),
        spends("two", 0, 0).with_tool_call("list_bots", json!({})),
        spends("three", 0, 0),
    ]);

    rig.swarm
        .send(&rig.dm, "what is everyone doing?")
        .expect("the owner's message is accepted");
    rig.settle().await;

    assert_eq!(rig.calls(), 3, "no cap, no cut.\n{}", rig.transcript());
    assert_eq!(rig.unspent_script(), 0);
    assert!(
        rig.notices().is_empty(),
        "a budget with no cap has nothing to announce: {:?}",
        rig.notices()
    );
}

#[tokio::test]
async fn a_spent_day_refuses_the_turn_before_the_model_is_ever_called() {
    let rig = rig();
    rig.seed_spend(1_200);
    rig.set(|settings| settings.max_tokens_per_day = Some(1_000));
    rig.script(vec![spends("should never be asked", 1, 1)]);

    rig.swarm
        .send(&rig.dm, "morning, what is on today?")
        .expect("the owner's message is accepted");
    rig.settle().await;

    assert_eq!(
        rig.calls(),
        0,
        "a spent budget refuses the turn before spending more.\n{}",
        rig.transcript()
    );
    assert_eq!(rig.unspent_script(), 1);
    assert_eq!(
        rig.turns().len(),
        1,
        "no refused turn is written; the one row is the seeded spend.\n{}",
        rig.transcript()
    );
    let notices = rig.notices();
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("1200 token(s)") && notice.contains("cap of 1000")),
        "the refusal names the spend and the cap: {notices:?}\n{}",
        rig.transcript()
    );

    // The delivery the refused turn was for is closed, not left `Pending` for
    // boot recovery to replay into the same refusal on the next start.
    assert!(
        rig.swarm
            .store
            .queued_deliveries(&rig.lead)
            .expect("queued")
            .is_empty(),
        "the refused delivery was settled, not left pending"
    );
}

#[tokio::test]
async fn a_spent_day_fails_a_routine_run_instead_of_leaving_it_running() {
    let rig = rig();
    // The day has to be *spent* for a refusal to happen at all: a cap of 50 with
    // 50 already spent is refused, and a cap with nothing spent is not.
    rig.seed_spend(50);
    rig.set(|settings| settings.max_tokens_per_day = Some(50));

    // A routine that has just fired: `fire_due` writes the run row and hands the
    // delivery to the runner, exactly as the ticker does in a real brain. If the
    // budget refuses that delivery without settling the run, the Routines screen
    // shows "Running" forever — a permanent lie and a row that never closes.
    let now = chrono::Utc::now();
    let routine = rig
        .swarm
        .routines
        .create_routine(
            rig.lead.clone(),
            "morning check",
            "check the inbox",
            swarmbot_contracts::routine::RoutineSchedule::Once {
                run_at: now + chrono::Duration::seconds(1),
            },
            now,
        )
        .expect("a routine");
    let fired = rig
        .swarm
        .routines
        .fire_due(now + chrono::Duration::seconds(2))
        .expect("the ticker runs");
    assert_eq!(fired, 1, "the routine was due");
    rig.settle().await;

    assert_eq!(rig.calls(), 0, "the refused routine spent nothing");
    let runs = rig
        .swarm
        .store
        .list_runs(&routine.id, 10)
        .expect("runs are readable");
    let run = runs.first().expect("the fired run is recorded");
    assert_eq!(
        run.status,
        swarmbot_contracts::routine::RunStatus::Failed,
        "a run the budget refused is not left Running: {run:?}"
    );
    let reason = run.error.as_deref().unwrap_or_default();
    assert!(
        reason.contains("Token budget") && reason.contains("cap of 50"),
        "the run says why it did not execute: {run:?}"
    );
}

#[tokio::test]
async fn a_spent_day_fails_a_task_instead_of_leaving_it_running() {
    let rig = rig();
    rig.seed_spend(50);
    rig.set(|settings| settings.max_tokens_per_day = Some(50));

    // A task already queued for the lead. The refusal must close it with the
    // budget's own words: a task left `Running` holds its per-bot slot and
    // starves everything queued behind it (the D9.20 leak, one layer up).
    let task = Task::own(
        rig.lead.clone(),
        rig.dm.clone(),
        "the thing",
        "do the thing",
    );
    rig.swarm.store.insert_task(&task).expect("the task");
    let delivery = Delivery::task(
        rig.dm.clone(),
        rig.lead.clone(),
        task.id.clone(),
        "the thing",
        "do the thing",
    );
    rig.swarm
        .store
        .insert_delivery(&delivery)
        .expect("the delivery");
    rig.swarm
        .runners
        .get(&rig.lead)
        .expect("the lead runner")
        .deliver(delivery)
        .expect("accepted");
    rig.settle().await;

    assert_eq!(rig.calls(), 0, "nothing was spent on it");
    let stored = rig
        .swarm
        .store
        .get_task(&task.id)
        .expect("readable")
        .expect("still there");
    assert_eq!(stored.status, TaskStatus::Failed, "{stored:?}");
    // The reason has to be readable, not just present: a task marked `Failed`
    // with no words sends the requester looking for a model error that never
    // happened. Either field carrying the budget's sentence is fine — which one
    // is the task layer's choice — but one of them must.
    let reason = stored
        .error
        .as_deref()
        .or(stored.result.as_deref())
        .unwrap_or_default();
    assert!(
        reason.contains("cap of 50") && reason.contains("Token budget"),
        "the task says why it did not run: {stored:?}"
    );
}
