//! End-to-end delegation: the acceptance test for the thing this prototype
//! exists to do.
//!
//! Everything here runs offline and deterministically — an in-memory store and
//! one scripted [`FakeBackend`] per bot — so it asserts on the *orchestration*,
//! not on a model's mood. What it proves:
//!
//! * a group message wakes the lead;
//! * the lead's `handoff_task` creates a task owned by a teammate, whose work
//!   happens in that teammate's own DM and whose result comes back to the group;
//! * the lead then answers the owner in the group, in its own words;
//! * a mention wakes a teammate once, in the group, with no private side channel;
//! * a chain that would oscillate is stopped by the hop limit, in the transcript;
//! * `NO_REPLY` is silence, not a message.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use swarmbot_contracts::config::{BackendSpec, BrainConfig};
use swarmbot_contracts::entry::EntryKind;
use swarmbot_contracts::id::{BotId, ConversationId};
use swarmbot_contracts::task::{TaskKind, TaskStatus};
use swarmbot_llm::ScriptedTurn;

use swarmbot_core::compose;
use swarmbot_core::swarm::Swarm;

/// One scripted backend per bot, so each role can be written separately.
const LEAD: &str = "fake-lead";
const SCOUT: &str = "fake-scout";
const DEV: &str = "fake-dev";
const SENTINEL: &str = "fake-sentinel";

fn team_config() -> BrainConfig {
    let mut config = BrainConfig {
        default_backend: LEAD.to_string(),
        ..BrainConfig::default()
    };
    config.backends = vec![
        BackendSpec::fake(LEAD),
        BackendSpec::fake(SCOUT),
        BackendSpec::fake(DEV),
        BackendSpec::fake(SENTINEL),
    ];
    let mut team = BrainConfig::demo_team(LEAD);
    for (bot, backend) in team.bots.iter_mut().zip([LEAD, SCOUT, DEV, SENTINEL]) {
        bot.backend = Some(backend.to_string());
    }
    config.team = Some(team);
    config
}

struct Rig {
    swarm: Arc<Swarm>,
    group: ConversationId,
    lead: BotId,
    scout: BotId,
    dev: BotId,
    sentinel: BotId,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().expect("a temp dir");
    let swarm = compose::memory(team_config(), dir.path(), vec![], &BTreeMap::new())
        .expect("a swarm builds");
    swarm.boot().expect("the swarm boots");
    swarm.start_expiry_ticker();

    let bots = |name: &str| {
        swarm
            .bots
            .by_name(name)
            .unwrap_or_else(|_| panic!("the team has a {name}"))
            .id
    };
    let group = swarm
        .conversations
        .list()
        .expect("conversations")
        .into_iter()
        .find(|c| c.title.as_deref() == Some("Team"))
        .expect("the demo team has a group")
        .id;
    Rig {
        lead: bots("lead"),
        scout: bots("scout"),
        dev: bots("dev"),
        sentinel: bots("sentinel"),
        swarm,
        group,
        _dir: dir,
    }
}

impl Rig {
    fn script(&self, backend: &str, turns: Vec<ScriptedTurn>) {
        let fake = self
            .swarm
            .backends
            .registry()
            .fake(backend)
            .unwrap_or_else(|| panic!("backend {backend} is a fake"));
        for turn in turns {
            fake.push_turn(turn);
        }
    }

    async fn settle(&self) {
        assert!(
            self.swarm.wait_for_idle(Duration::from_secs(20)).await,
            "the team did not go idle.\n{}",
            self.why_not_idle()
        );
    }

    /// What is still holding the team busy: the diagnostic a stuck test needs.
    fn why_not_idle(&self) -> String {
        let mut out = Vec::new();
        out.push(format!("busy bots: {:?}", self.busy_bots()));
        out.push(format!(
            "parked deliveries: {}",
            self.swarm.runners.parked_count()
        ));
        for bot in self.swarm.bots.list().unwrap_or_default() {
            let queued = self
                .swarm
                .store
                .queued_deliveries(&bot.id)
                .map(|d| d.len())
                .unwrap_or(0);
            let tasks = self.swarm.tasks.list(&bot.id).unwrap_or_default();
            let by_status =
                |wanted: TaskStatus| tasks.iter().filter(|t| t.status == wanted).count();
            out.push(format!(
                "@{}: queued={} tasks(queued={} running={} done={}) hops={:?}",
                bot.name,
                queued,
                by_status(TaskStatus::Queued),
                by_status(TaskStatus::Running),
                by_status(TaskStatus::Succeeded)
                    + by_status(TaskStatus::Failed)
                    + by_status(TaskStatus::Cancelled),
                tasks.iter().map(|t| t.causal.hops).collect::<Vec<_>>()
            ));
        }
        let queued_tasks = self.swarm.store.queued_tasks().unwrap_or_default().len();
        out.push(format!("queued tasks overall: {queued_tasks}"));
        out.join("\n")
    }

    fn busy_bots(&self) -> Vec<String> {
        self.swarm
            .bots
            .list()
            .unwrap_or_default()
            .iter()
            .filter(|bot| self.swarm.runners.is_busy(&bot.id))
            .map(|bot| bot.name.clone())
            .collect()
    }

    fn entries(&self, conversation: &ConversationId) -> Vec<swarmbot_contracts::entry::Entry> {
        self.swarm
            .conversations
            .history(conversation, 0, 200)
            .expect("history")
    }

    /// Every transcript line, as one readable string, for assertions and for
    /// the failure messages that make a broken flow diagnosable.
    fn transcript(&self, conversation: &ConversationId) -> String {
        self.entries(conversation)
            .iter()
            .map(|entry| {
                let author = match &entry.author {
                    swarmbot_contracts::entry::Author::Owner => "owner".to_string(),
                    swarmbot_contracts::entry::Author::Bot { id } => self
                        .swarm
                        .store
                        .get_bot(id)
                        .ok()
                        .flatten()
                        .map(|bot| format!("@{}", bot.name))
                        .unwrap_or_else(|| "@?".to_string()),
                    swarmbot_contracts::entry::Author::System => "system".to_string(),
                };
                format!(
                    "[{}] {}: {}",
                    entry.kind.kind_str(),
                    author,
                    entry.kind.text().unwrap_or_default().replace('\n', " ⏎ ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn dm_of(&self, bot: &BotId) -> ConversationId {
        self.swarm
            .store
            .get_bot(bot)
            .ok()
            .flatten()
            .expect("the bot exists")
            .direct_conversation_id
    }
}

#[tokio::test]
async fn a_group_request_is_handed_off_and_reported_back() {
    let rig = rig();

    // The lead delegates, then answers once the result comes back.
    rig.script(
        LEAD,
        vec![
            // Round 1 of the first turn: hand the work over.
            ScriptedTurn::text("Looking into it — scout will find where the config is parsed.")
                .with_tool_call(
                    "handoff_task",
                    json!({
                        "to": "scout",
                        "title": "find the config parser",
                        "task": "locate where the TOML config is parsed and report file and line",
                        "context": "start in src/"
                    }),
                ),
            // Round 2 of the same turn: no tool call, so the turn ends and the
            // lead waits for the result instead of guessing it.
            ScriptedTurn::text("I have asked scout; I will report when it lands."),
            // A new turn, woken by the task-result delivery.
            ScriptedTurn::text("Scout found it: the config is parsed in src/config.rs. Done."),
        ],
    );
    rig.script(
        SCOUT,
        vec![ScriptedTurn::text(
            "It is parsed in src/config.rs at line 42, by `load()`.",
        )],
    );

    rig.swarm
        .send(&rig.group, "where is the config parsed?")
        .expect("the owner's message is accepted");
    rig.settle().await;

    let group = rig.transcript(&rig.group);
    println!("{group}");

    // 1. The lead answered the group, and the owner's question is there.
    assert!(group.contains("where is the config parsed?"), "{group}");
    assert!(
        group.contains("Scout found it: the config is parsed in src/config.rs"),
        "the lead reports back in the group: {group}"
    );

    // 2. The handoff is visible as an event line, not as invisible machinery.
    assert!(
        group.contains("Handed off \"find the config parser\" to @scout"),
        "the delegation is recorded: {group}"
    );

    // 3. The task exists, is owned by scout, came from the lead, belongs to the
    //    group, and succeeded with scout's own words.
    let tasks = rig.swarm.tasks.in_conversation(&rig.group).expect("tasks");
    assert_eq!(tasks.len(), 1, "one handoff");
    let task = &tasks[0];
    assert_eq!(task.owner, rig.scout);
    assert_eq!(
        task.kind,
        TaskKind::Handoff {
            from: rig.lead.clone()
        }
    );
    assert_eq!(task.requester.as_ref(), Some(&rig.lead));
    assert_eq!(task.origin, rig.group);
    assert_eq!(task.status, TaskStatus::Succeeded);
    assert!(
        task.result
            .as_deref()
            .is_some_and(|r| r.contains("src/config.rs at line 42")),
        "the result is scout's answer: {:?}",
        task.result
    );

    // 4. The work happened in scout's DM, so the group is not full of its noise.
    let scout_dm = rig.transcript(&rig.dm_of(&rig.scout));
    assert!(
        scout_dm.contains("src/config.rs at line 42"),
        "scout worked in its own DM: {scout_dm}"
    );
    // The handoff itself is visible next to the work (that is where a card or a
    // delegation belongs); what must not leak is the teammate's own node work.
    for tool in ["shell", "read_file", "write_file", "grep", "glob"] {
        let quoted = format!("{}{}{}", '"', tool, '"');
        assert!(
            !group.contains(&quoted),
            "the group is not full of scout's tool calls ({tool}): {group}"
        );
    }
    assert!(
        group.contains("I have asked scout"),
        "the lead says it is waiting rather than inventing an answer: {group}"
    );

    // 5. The lead read the result: a task-result turn ran in the group.
    let lead_turns = rig.swarm.store.turns_in(&rig.group, 20).expect("turns");
    assert!(
        lead_turns.iter().any(|turn| turn.bot_id == rig.lead
            && matches!(
                turn.initiator,
                swarmbot_contracts::turn::TurnInitiator::Task { .. }
            )),
        "the lead got a task turn: {lead_turns:?}"
    );

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn a_mention_wakes_a_teammate_in_the_group_without_a_side_channel() {
    let rig = rig();
    rig.script(
        LEAD,
        vec![ScriptedTurn::text("@dev please write the parser tests.")],
    );
    rig.script(
        DEV,
        vec![ScriptedTurn::text(
            "On it — I added three tests and they pass.",
        )],
    );

    rig.swarm
        .send(&rig.group, "get the tests written")
        .expect("sent");
    rig.settle().await;

    let group = rig.transcript(&rig.group);
    assert!(
        group.contains("@dev please write the parser tests"),
        "{group}"
    );
    assert!(
        group.contains("I added three tests and they pass"),
        "the woken bot answers in the group: {group}"
    );

    // The team never splits into private side channels.
    let dev_dm = rig.entries(&rig.dm_of(&rig.dev));
    assert!(
        dev_dm
            .iter()
            .all(|entry| !matches!(entry.kind, EntryKind::Message(_))),
        "nothing was DM'd behind the group's back: {:?}",
        rig.transcript(&rig.dm_of(&rig.dev))
    );

    // Each member is woken at most once per reply.
    let deliveries = rig
        .swarm
        .store
        .queued_deliveries(&rig.dev)
        .expect("deliveries");
    assert!(deliveries.is_empty(), "the queue drained");
    let dev_turns = rig.swarm.store.turns_in(&rig.group, 20).expect("turns");
    let dev_group_turns = dev_turns
        .iter()
        .filter(|turn| turn.bot_id == rig.dev)
        .count();
    assert_eq!(dev_group_turns, 1, "one reply, one wake");

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn a_chain_that_would_oscillate_is_stopped_by_the_hop_limit() {
    let rig = rig();
    // Two bots handing the same work back and forth, forever if allowed.
    // `ScriptedTurn` is not Clone, so each turn is built fresh.
    let ping = |to: &str| {
        ScriptedTurn::text("").with_tool_call(
            "message_bot",
            json!({"to": to, "text": "your turn", "urgency": "background"}),
        )
    };
    // Hops advance once per *turn*, so each ping is followed by a text-only
    // turn that ends it: that is also how a real bot behaves (say it, stop,
    // wait for the answer).
    let volley = |to: &str| vec![ping(to), ScriptedTurn::text("waiting")];
    rig.script(LEAD, (0..8).flat_map(|_| volley("scout")).collect());
    rig.script(SCOUT, (0..8).flat_map(|_| volley("lead")).collect());

    rig.swarm
        .send(&rig.dm_of(&rig.lead), "start the loop")
        .expect("sent");
    rig.settle().await;

    let lead_dm = rig.transcript(&rig.dm_of(&rig.lead));
    let scout_dm = rig.transcript(&rig.dm_of(&rig.scout));
    assert!(
        lead_dm.contains("too many hand-offs") || scout_dm.contains("too many hand-offs"),
        "the chain was stopped and the transcript says so:\nlead: {lead_dm}\nscout: {scout_dm}"
    );

    // The chain really stopped: no delivery is left waiting to continue it.
    let waiting: usize = [rig.lead.clone(), rig.scout.clone()]
        .iter()
        .map(|bot| {
            rig.swarm
                .store
                .queued_deliveries(bot)
                .map(|d| d.len())
                .unwrap_or(0)
        })
        .sum();
    assert_eq!(waiting, 0, "nothing is queued to keep the loop going");

    // And it did not run away: the messages exchanged stay within the limit.
    let messaged = scout_dm.matches("Messaged @").count() + lead_dm.matches("Messaged @").count();
    assert!(
        messaged <= 2 * (swarmbot_contracts::HOP_LIMIT as usize + 2),
        "the loop was bounded, got {messaged} messages"
    );

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn a_handoff_loop_is_bounded_by_the_hop_limit_too() {
    // The subtle case: a task runs as a turn of its own, and a turn reads its
    // hop count from the delivery that woke it. If the chain did not travel with
    // the task, every handoff would restart the count at zero and two bots could
    // hand the same work back and forth forever.
    let rig = rig();
    // A handoff chain branches (every result wakes its requester, who may hand
    // off again), so with the default limit of 8 the scripts would run out long
    // before the deepest branch got there. A small limit makes the bound itself
    // the thing under test.
    rig.swarm
        .settings
        .update(|settings| settings.max_hops = 3)
        .expect("settings update");
    let handoff = |to: &str| {
        ScriptedTurn::text("").with_tool_call(
            "handoff_task",
            json!({"to": to, "title": "pass it on", "task": "please look at this"}),
        )
    };
    // One handoff per turn, then a text-only turn to end it (a turn that ends
    // with a tool call asks the model again).
    let volley = |to: &str| vec![handoff(to), ScriptedTurn::text("waiting on the other side")];
    rig.script(LEAD, (0..12).flat_map(|_| volley("scout")).collect());
    rig.script(SCOUT, (0..12).flat_map(|_| volley("lead")).collect());

    rig.swarm
        .send(&rig.dm_of(&rig.lead), "start passing it around")
        .expect("sent");
    rig.settle().await;

    let stopped =
        |conversation: &ConversationId| rig.transcript(conversation).contains("too many hand-offs");
    assert!(
        stopped(&rig.dm_of(&rig.lead)) || stopped(&rig.dm_of(&rig.scout)),
        "the handoff loop was stopped and the transcript says so:\nlead: {}\nscout: {}",
        rig.transcript(&rig.dm_of(&rig.lead)),
        rig.transcript(&rig.dm_of(&rig.scout))
    );

    // Bounded work: the hops allowed, times the two directions, plus the
    // slack a batched turn can produce. Not "as many as the script offered".
    let tasks = |bot: &BotId| rig.swarm.tasks.list(bot).expect("tasks").len();
    let total = tasks(&rig.lead) + tasks(&rig.scout);
    let bound = 2 * (rig.swarm.settings.get().max_hops as usize + 4);
    assert!(
        total <= bound,
        "the loop created {total} tasks, more than the {bound} the hop limit allows"
    );

    // And every task carries the chain that made it, so the count is auditable.
    let mut hops: Vec<u8> = rig
        .swarm
        .tasks
        .list(&rig.scout)
        .expect("tasks")
        .iter()
        .map(|task| task.causal.hops)
        .collect();
    hops.sort_unstable();
    assert!(
        hops.iter().any(|h| *h > 0),
        "a handed-off task is never a fresh chain: {hops:?}"
    );
    let limit = rig.swarm.settings.get().max_hops;
    assert!(
        *hops.last().expect("at least one task") <= limit,
        "no task went past the limit of {limit}: {hops:?}"
    );

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn no_reply_is_silence_and_the_line_says_why() {
    let rig = rig();
    rig.script(LEAD, vec![ScriptedTurn::text("Answered already.")]);
    rig.script(SCOUT, vec![ScriptedTurn::text("NO_REPLY")]);
    rig.script(DEV, vec![ScriptedTurn::text("NO_REPLY")]);
    rig.script(SENTINEL, vec![ScriptedTurn::text("NO_REPLY")]);

    rig.swarm
        .send(&rig.group, "@lead @scout @dev @sentinel status?")
        .expect("sent");
    rig.settle().await;

    let group = rig.transcript(&rig.group);
    assert!(group.contains("Answered already."), "{group}");
    assert!(
        !group.contains("NO_REPLY"),
        "the token is never posted: {group}"
    );
    assert!(
        group.contains("had nothing to add"),
        "but the silence is recorded: {group}"
    );
    // Three of the four stayed silent, and none of them posted anywhere else.
    for bot in [&rig.scout, &rig.dev, &rig.sentinel] {
        let dm = rig.entries(&rig.dm_of(bot));
        assert!(
            dm.iter().all(|entry| !entry.is_message()),
            "a silent bot said nothing in its DM either: {}",
            rig.transcript(&rig.dm_of(bot))
        );
    }

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn a_bot_can_take_long_work_off_its_own_hands() {
    let rig = rig();
    rig.script(
        LEAD,
        vec![
            ScriptedTurn::text("Starting the audit in the background.").with_tool_call(
                "start_task",
                json!({"title": "audit the diff", "brief": "check every hunk"}),
            ),
            ScriptedTurn::text("The audit came back clean."),
        ],
    );

    rig.swarm
        .send(&rig.dm_of(&rig.lead), "audit the diff please")
        .expect("sent");
    rig.settle().await;

    let tasks = rig.swarm.tasks.list(&rig.lead).expect("tasks");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].kind, TaskKind::Own);
    assert_eq!(tasks[0].status, TaskStatus::Succeeded);
    assert_eq!(tasks[0].origin, rig.dm_of(&rig.lead));

    let dm = rig.transcript(&rig.dm_of(&rig.lead));
    assert!(dm.contains("started a task"), "{dm}");
    assert!(dm.contains("The audit came back clean."), "{dm}");

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn the_kill_switch_stops_delegation_in_its_tracks() {
    let rig = rig();
    rig.swarm
        .settings
        .update(|settings| settings.bot_messaging = false)
        .expect("settings update");
    rig.script(
        LEAD,
        vec![
            ScriptedTurn::text("I could not reach the team.")
                .with_tool_call("message_bot", json!({"to": "scout", "text": "help"})),
        ],
    );

    rig.swarm
        .send(&rig.dm_of(&rig.lead), "ask scout")
        .expect("sent");
    rig.settle().await;

    // The tool call failed with the reason, and nothing was delivered.
    let dm = rig.transcript(&rig.dm_of(&rig.lead));
    assert!(
        dm.contains("switched off"),
        "the refusal is visible in the transcript: {dm}"
    );
    assert!(
        rig.swarm
            .store
            .queued_deliveries(&rig.scout)
            .expect("deliveries")
            .is_empty()
    );
    // Every DM carries a "@x joined the team" line, so assert on messages:
    // nothing was ever said to scout.
    let scout_dm = rig.entries(&rig.dm_of(&rig.scout));
    assert!(
        scout_dm.iter().all(|entry| !entry.is_message()),
        "scout was never woken: {}",
        rig.transcript(&rig.dm_of(&rig.scout))
    );

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn a_delivery_for_another_conversation_queues_instead_of_interrupting() {
    let rig = rig();
    // The lead is busy in its DM when a group message arrives for it.
    rig.script(
        LEAD,
        vec![
            ScriptedTurn::text("Working on the DM request.")
                .with_delay(Duration::from_millis(150))
                .with_tool_call("list_bots", json!({})),
            ScriptedTurn::text("Now the group question."),
            ScriptedTurn::text("And back to the DM."),
        ],
    );

    rig.swarm
        .send(&rig.dm_of(&rig.lead), "first, in the DM")
        .expect("sent");
    // Give the runner a moment to pick the DM turn up.
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.swarm
        .send(&rig.group, "then, in the group")
        .expect("sent");
    rig.settle().await;

    // Both were answered, each in its own conversation: one mind, no crossing.
    let dm = rig.transcript(&rig.dm_of(&rig.lead));
    let group = rig.transcript(&rig.group);
    assert!(dm.contains("Working on the DM request."), "{dm}");
    assert!(group.contains("then, in the group"), "{group}");
    assert!(
        !group.contains("Working on the DM request."),
        "a DM answer never lands in the group: {group}"
    );

    // Every turn belongs to exactly one conversation.
    let running = rig.swarm.store.running_turns().expect("turn lookup");
    assert!(running.is_empty(), "a turn was left running: {running:?}");
    let dm_turns = rig
        .swarm
        .store
        .turns_in(&rig.dm_of(&rig.lead), 20)
        .expect("turns");
    assert!(
        dm_turns
            .iter()
            .all(|t| t.conversation_id == rig.dm_of(&rig.lead)),
        "{dm_turns:?}"
    );

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn the_instructions_a_bot_gets_name_its_real_team_and_tools() {
    let rig = rig();
    rig.script(LEAD, vec![ScriptedTurn::text("Ready.")]);
    rig.swarm.send(&rig.dm_of(&rig.lead), "hi").expect("sent");
    rig.settle().await;

    let fake = rig.swarm.backends.registry().fake(LEAD).expect("fake");
    let requests = fake.requests();
    assert!(!requests.is_empty(), "the lead was called");
    let instructions = &requests[0].instructions;
    assert!(instructions.contains("You are a Bot"), "{instructions}");
    for name in ["@lead", "@scout", "@dev", "@sentinel"] {
        assert!(instructions.contains(name), "the roster names {name}");
    }
    assert!(
        instructions.contains("## Your groups"),
        "it knows its groups: {instructions}"
    );
    for tool in ["handoff_task", "message_bot", "start_task", "shell"] {
        assert!(
            instructions.contains(&format!("`{tool}`")),
            "it is told about {tool}"
        );
    }
    // The tools the model was actually offered match the instructions.
    let offered: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert!(offered.contains(&"handoff_task"), "{offered:?}");
    assert!(offered.contains(&"shell"), "{offered:?}");

    rig.swarm.shutdown().await;
}
