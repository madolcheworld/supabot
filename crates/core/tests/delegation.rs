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
    // Assert that scout's DM has no messages (nothing was ever said to scout).
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

#[tokio::test]
async fn a_natural_greeting_wakes_a_teammate_in_the_group_without_at_symbol() {
    let rig = rig();
    rig.script(
        LEAD,
        vec![ScriptedTurn::text("halo dev, tolong buat parser test.")],
    );
    rig.script(
        DEV,
        vec![ScriptedTurn::text(
            "Siap, test parser sudah dibuat dan lolos.",
        )],
    );

    rig.swarm
        .send(&rig.group, "mulai kerjakan pengujian")
        .expect("sent");
    rig.settle().await;

    let group = rig.transcript(&rig.group);
    assert!(
        group.contains("halo dev, tolong buat parser test"),
        "{group}"
    );
    assert!(
        group.contains("Siap, test parser sudah dibuat dan lolos"),
        "the naturally greeted bot answers in the group: {group}"
    );

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn a_natural_greeting_in_dm_delegates_to_teammate_and_reports_back() {
    let rig = rig();
    let lead_dm = rig.dm_of(&rig.lead);

    // Lead receives greeting asking for scout in Lead's DM, delegates to Scout, then reports back.
    rig.script(
        LEAD,
        vec![
            ScriptedTurn::text("Saya minta bantuan scout untuk mencarikan file parser.")
                .with_tool_call(
                    "handoff_task",
                    json!({
                        "to": "scout",
                        "title": "cari file parser",
                        "task": "temukan letak file parser toml",
                        "context": "di direktori src/"
                    }),
                ),
            ScriptedTurn::text("Sedang didelegasikan ke scout..."),
            ScriptedTurn::text("Scout sudah menemukan file parser di src/parser.rs. Selesai!"),
        ],
    );
    rig.script(
        SCOUT,
        vec![ScriptedTurn::text(
            "File parser terletak di src/parser.rs baris 10.",
        )],
    );

    rig.swarm
        .send(&lead_dm, "halo scout tolong carikan file parser")
        .expect("sent");
    rig.settle().await;

    let dm_transcript = rig.transcript(&lead_dm);
    assert!(
        dm_transcript.contains("halo scout tolong carikan file parser"),
        "{dm_transcript}"
    );
    assert!(
        dm_transcript.contains("Scout sudah menemukan file parser di src/parser.rs"),
        "lead reports back scout's answer in dm: {dm_transcript}"
    );

    // Verify task was created for scout
    let tasks = rig.swarm.tasks.in_conversation(&lead_dm).expect("tasks");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].owner, rig.scout);
    assert_eq!(tasks[0].status, TaskStatus::Succeeded);

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn each_bot_turn_receives_its_own_private_workspace_cwd_and_instructions() {
    let rig = rig();
    rig.script(LEAD, vec![ScriptedTurn::text("Lead ready.")]);
    rig.script(SCOUT, vec![ScriptedTurn::text("Scout ready.")]);

    rig.swarm
        .send(&rig.dm_of(&rig.lead), "hi lead")
        .expect("sent");
    rig.swarm
        .send(&rig.dm_of(&rig.scout), "hi scout")
        .expect("sent");
    rig.settle().await;

    let lead_req = rig
        .swarm
        .backends
        .registry()
        .fake(LEAD)
        .expect("fake lead")
        .requests();
    let scout_req = rig
        .swarm
        .backends
        .registry()
        .fake(SCOUT)
        .expect("fake scout")
        .requests();

    assert_eq!(lead_req.len(), 1);
    assert_eq!(scout_req.len(), 1);

    let lead_cwd = lead_req[0].cwd.as_ref().expect("lead receives cwd");
    let scout_cwd = scout_req[0].cwd.as_ref().expect("scout receives cwd");
    assert_ne!(lead_cwd, scout_cwd, "each bot gets its own workspace");
    assert!(lead_cwd.ends_with("workspaces/lead"), "{lead_cwd:?}");
    assert!(scout_cwd.ends_with("workspaces/scout"), "{scout_cwd:?}");
    assert!(
        lead_req[0]
            .instructions
            .contains(&format!("private workspace: `{}`", lead_cwd.display())),
        "{}",
        lead_req[0].instructions
    );
    assert!(
        scout_req[0]
            .instructions
            .contains(&format!("private workspace: `{}`", scout_cwd.display())),
        "{}",
        scout_req[0].instructions
    );

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn group_multi_turn_a_to_b_to_a_retains_session_history_and_respects_explicit_dual_mention() {
    let rig = rig();

    // A -> B -> A inside the group: lead asks scout, scout replies to lead, lead finishes.
    rig.script(
        LEAD,
        vec![
            ScriptedTurn::text("@scout please verify Cargo.toml"),
            ScriptedTurn::text("Confirmed with scout: Cargo.toml is clean."),
        ],
    );
    rig.script(
        SCOUT,
        vec![ScriptedTurn::text("@lead Cargo.toml is clean and builds.")],
    );

    rig.swarm
        .send(&rig.group, "please check Cargo.toml")
        .expect("sent");
    rig.settle().await;

    let group_log = rig.transcript(&rig.group);
    assert!(
        group_log.contains("@scout please verify Cargo.toml"),
        "{group_log}"
    );
    assert!(
        group_log.contains("@lead Cargo.toml is clean and builds."),
        "{group_log}"
    );
    assert!(
        group_log.contains("Confirmed with scout: Cargo.toml is clean."),
        "{group_log}"
    );

    // Final replies must be persisted into each bot's session history.
    let lead_session = swarmbot_core::session::Session::load(rig.swarm.store.as_ref(), &rig.lead)
        .expect("session");
    let lead_assistant_msgs: Vec<&str> = lead_session
        .messages()
        .iter()
        .filter(|m| m.role == swarmbot_llm::Role::Assistant)
        .map(|m| m.content.as_str())
        .collect();
    assert!(
        lead_assistant_msgs.contains(&"@scout please verify Cargo.toml"),
        "lead session missed turn 1 final reply: {lead_assistant_msgs:?}"
    );
    assert!(
        lead_assistant_msgs.contains(&"Confirmed with scout: Cargo.toml is clean."),
        "lead session missed turn 2 final reply: {lead_assistant_msgs:?}"
    );

    let scout_session = swarmbot_core::session::Session::load(rig.swarm.store.as_ref(), &rig.scout)
        .expect("session");
    assert!(
        scout_session.messages().iter().any(|m| {
            m.role == swarmbot_llm::Role::Assistant
                && m.content == "@lead Cargo.toml is clean and builds."
        }),
        "scout session missed final reply"
    );

    // Next: owner explicitly mentions BOTH @lead and @scout. Even if lead says
    // "halo scout", scout must NOT be woken a second time for the same message.
    rig.script(
        LEAD,
        vec![ScriptedTurn::text("On it — halo scout, let's check docs.")],
    );
    rig.script(SCOUT, vec![ScriptedTurn::text("Docs checked.")]);
    let scout_calls_before = rig
        .swarm
        .backends
        .registry()
        .fake(SCOUT)
        .expect("fake scout")
        .requests()
        .len();

    rig.swarm
        .send(&rig.group, "@lead @scout check the docs")
        .expect("sent");
    rig.settle().await;

    let scout_calls_after = rig
        .swarm
        .backends
        .registry()
        .fake(SCOUT)
        .expect("fake scout")
        .requests()
        .len();
    assert_eq!(
        scout_calls_after - scout_calls_before,
        1,
        "scout should wake exactly once when owner already addressed @lead @scout"
    );

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn group_multi_turn_ping_pong_stops_at_hop_limit() {
    let rig = rig();
    rig.swarm
        .settings
        .update(|s| s.max_hops = 3)
        .expect("settings");

    rig.script(
        LEAD,
        (0..6)
            .map(|i| ScriptedTurn::text(format!("@scout ping {i}")))
            .collect(),
    );
    rig.script(
        SCOUT,
        (0..6)
            .map(|i| ScriptedTurn::text(format!("@lead pong {i}")))
            .collect(),
    );

    rig.swarm.send(&rig.group, "start ping pong").expect("sent");
    rig.settle().await;

    let group_log = rig.transcript(&rig.group);
    assert!(
        group_log.contains("too many hand-offs"),
        "hop limit should stop group oscillation: {group_log}"
    );
    assert!(
        rig.swarm
            .store
            .queued_deliveries(&rig.lead)
            .unwrap()
            .is_empty()
    );
    assert!(
        rig.swarm
            .store
            .queued_deliveries(&rig.scout)
            .unwrap()
            .is_empty()
    );

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn group_catchup_uses_message_prefix_and_does_not_duplicate_trigger() {
    let rig = rig();

    // First turn only wakes lead so scout misses both owner's message and lead's reply.
    rig.script(
        LEAD,
        vec![
            ScriptedTurn::text("I will ask scout once you confirm."),
            ScriptedTurn::text("@scout please verify the build"),
        ],
    );
    rig.script(SCOUT, vec![ScriptedTurn::text("Build looks green.")]);

    rig.swarm
        .send(&rig.group, "early context for lead only")
        .expect("sent");
    rig.settle().await;

    rig.swarm
        .send(&rig.group, "@lead proceed with step two")
        .expect("sent");
    rig.settle().await;

    let scout_requests = rig
        .swarm
        .backends
        .registry()
        .fake(SCOUT)
        .expect("fake scout")
        .requests();
    assert_eq!(scout_requests.len(), 1);
    let user_prompt = scout_requests[0]
        .messages
        .iter()
        .filter(|m| m.role == swarmbot_llm::Role::User)
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        user_prompt.contains("[Message from @lead] @scout please verify the build"),
        "group delivery must render with [Message from @...]: {user_prompt}"
    );
    assert!(
        !user_prompt.contains("[Direct message from @lead]"),
        "group delivery must never claim Direct message: {user_prompt}"
    );
    assert!(
        user_prompt.contains("[Messages in this group you have not seen]")
            && user_prompt.contains("I will ask scout once you confirm."),
        "earlier unseen messages appear in catch-up: {user_prompt}"
    );
    assert_eq!(
        user_prompt
            .matches("@scout please verify the build")
            .count(),
        1,
        "triggering message must not be duplicated in catch-up: {user_prompt}"
    );

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn dm_bot_to_bot_reply_via_plain_text_and_via_tool_avoid_deadlock() {
    let rig = rig();
    let lead_dm = rig.dm_of(&rig.lead);

    // 1. Plain-text reply addressing `@sender` from B back to A forwards without
    // requiring `message_bot`, while A's plain reply back to the owner does not
    // ping-pong back to B.
    rig.script(
        LEAD,
        vec![
            ScriptedTurn::text("Asking scout in DM.").with_tool_call(
                "message_bot",
                json!({"to": "scout", "text": "what is the port?", "urgency": "urgent"}),
            ),
            ScriptedTurn::text("Asked scout; waiting."),
            ScriptedTurn::text("Scout answered: port is 8080."),
        ],
    );
    rig.script(SCOUT, vec![ScriptedTurn::text("@lead Port is 8080.")]);

    rig.swarm
        .send(&lead_dm, "check the port with scout")
        .expect("sent");
    rig.settle().await;

    let lead_requests = rig
        .swarm
        .backends
        .registry()
        .fake(LEAD)
        .expect("fake lead")
        .requests();
    let got_dm_reply = lead_requests.iter().any(|req| {
        req.messages.iter().any(|m| {
            m.content
                .contains("[Direct message from @scout] @lead Port is 8080.")
        })
    });
    assert!(
        got_dm_reply,
        "lead must receive scout's @lead DM reply back without deadlock: {lead_requests:?}"
    );
    assert_eq!(
        rig.swarm
            .backends
            .registry()
            .fake(SCOUT)
            .expect("fake scout")
            .requests()
            .len(),
        1,
        "lead's owner-facing follow-up without @scout must not ping-pong back to scout"
    );

    let lead_dm_log = rig.transcript(&lead_dm);
    assert!(
        lead_dm_log.contains("Scout answered: port is 8080."),
        "{lead_dm_log}"
    );

    // 2. Reply via `message_bot` tool: B's closing text (even with `@lead`) must
    // NOT double-forward, and A's plain acknowledgement must NOT ping-pong back.
    rig.script(
        LEAD,
        vec![
            ScriptedTurn::text("Checking logs.").with_tool_call(
                "message_bot",
                json!({"to": "scout", "text": "check logs", "urgency": "urgent"}),
            ),
            ScriptedTurn::text("Sent question."),
            ScriptedTurn::text("Noted — I will fold scout's read."),
        ],
    );
    rig.script(
        SCOUT,
        vec![
            ScriptedTurn::text("Calling lead back.").with_tool_call(
                "message_bot",
                json!({"to": "lead", "text": "Logs are completely clean.", "urgency": "urgent"}),
            ),
            ScriptedTurn::text("@lead Closing note for my own DM only."),
        ],
    );

    rig.swarm
        .send(&lead_dm, "check logs with scout")
        .expect("sent");
    rig.settle().await;

    let updated_lead_dm = rig.transcript(&lead_dm);
    assert!(
        updated_lead_dm.contains("Logs are completely clean."),
        "{updated_lead_dm}"
    );
    assert!(
        !updated_lead_dm.contains("Closing note for my own DM only."),
        "must not double-forward closing text when message_bot already responded: {updated_lead_dm}"
    );
    assert_eq!(
        rig.swarm
            .backends
            .registry()
            .fake(SCOUT)
            .expect("fake scout")
            .requests()
            .len(),
        3,
        "lead's plain acknowledgement must not ping-pong back to scout (1 call in sub-case 1 + 2 tool-loop rounds in sub-case 2)"
    );

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn dm_plain_acknowledgement_and_third_person_do_not_ping_pong() {
    let rig = rig();
    let scout_dm = rig.dm_of(&rig.scout);
    let lead_dm = rig.dm_of(&rig.lead);

    // Case 1 & 2: scout sends DM updates to lead; lead replies in its own DM with
    // plain "LEAD-DM-ACK" and third-person "Noted — I will fold scout's early
    // read into the verdict." — neither explicitly addresses @scout, so neither
    // wakes scout.
    rig.script(
        SCOUT,
        vec![
            ScriptedTurn::text("Sharing first early read.")
                .with_tool_call("message_bot", json!({"to": "lead", "text": "early read 1"})),
            ScriptedTurn::text("First read sent."),
            ScriptedTurn::text("Sharing second early read.")
                .with_tool_call("message_bot", json!({"to": "lead", "text": "early read 2"})),
            ScriptedTurn::text("Second read sent."),
        ],
    );
    rig.script(
        LEAD,
        vec![
            ScriptedTurn::text("LEAD-DM-ACK"),
            ScriptedTurn::text("Noted — I will fold scout's early read into the verdict."),
        ],
    );

    rig.swarm.send(&scout_dm, "share read 1").expect("sent");
    rig.settle().await;
    rig.swarm.send(&scout_dm, "share read 2").expect("sent");
    rig.settle().await;

    let scout_turns = rig
        .swarm
        .store
        .turns_in(&scout_dm, 20)
        .expect("scout turns");
    assert_eq!(
        scout_turns.len(),
        2,
        "plain ack and third-person possessive mention must not create extra DM turns for scout: {scout_turns:?}"
    );
    let scout_reqs = rig
        .swarm
        .backends
        .registry()
        .fake(SCOUT)
        .expect("fake scout")
        .requests();
    assert_eq!(
        scout_reqs.len(),
        4,
        "2 DM turns * (tool call round + answer round) = 4 calls; zero ping-pong turns"
    );
    let scout_dm_log = rig.transcript(&scout_dm);
    assert!(!scout_dm_log.contains("LEAD-DM-ACK"), "{scout_dm_log}");
    assert!(
        !scout_dm_log.contains("fold scout's early read"),
        "{scout_dm_log}"
    );

    // Case 3: lead uses `post_to_group` mentioning @scout while answering scout's
    // DM, then writes `@scout Moved to Team.` in its DM closing text; recording
    // post_to_group targets prevents a duplicate DM forward into scout's DM.
    rig.script(
        SCOUT,
        vec![
            ScriptedTurn::text("Asking where to continue.").with_tool_call(
                "message_bot",
                json!({"to": "lead", "text": "where should we follow up?"}),
            ),
            ScriptedTurn::text("Asked lead."),
            ScriptedTurn::text("Following up in Team."),
        ],
    );
    rig.script(
        LEAD,
        vec![
            ScriptedTurn::text("Posting in group.").with_tool_call(
                "post_to_group",
                json!({"group": "Team", "text": "@scout let's continue in Team"}),
            ),
            ScriptedTurn::text("@scout Moved to Team."),
        ],
    );

    rig.swarm
        .send(&scout_dm, "ask lead where next")
        .expect("sent");
    rig.settle().await;

    let scout_dm_turns = rig
        .swarm
        .store
        .turns_in(&scout_dm, 20)
        .expect("scout dm turns");
    assert_eq!(
        scout_dm_turns.len(),
        3,
        "scout's DM only ran the 3 owner-triggered turns, no duplicate DM forward from lead: {scout_dm_turns:?}"
    );
    let scout_reqs = rig
        .swarm
        .backends
        .registry()
        .fake(SCOUT)
        .expect("fake scout")
        .requests();
    assert_eq!(
        scout_reqs.len(),
        7,
        "4 prior calls + 2 rounds for Case 3 DM turn + 1 Team group turn = 7"
    );
    let group_log = rig.transcript(&rig.group);
    assert!(group_log.contains("let's continue in Team"), "{group_log}");
    assert!(group_log.contains("Following up in Team."), "{group_log}");
    assert!(
        !rig.transcript(&scout_dm).contains("Moved to Team."),
        "must not forward DM closing text when post_to_group already reached scout"
    );
    assert!(
        rig.transcript(&lead_dm).contains("LEAD-DM-ACK"),
        "lead's local DM retained its own acknowledgement"
    );

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn mcp_turn_provider_attaches_servers_for_acp_filters_node_tools_and_drops_on_finish() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use swarmbot_contracts::DomainError;
    use swarmbot_contracts::config::BackendKind;
    use swarmbot_contracts::tool::Toolset;
    use swarmbot_core::{McpTurnProvider, McpTurnRegistration, McpTurnRequest};
    use swarmbot_llm::{
        BackendStatus, Capabilities, ChatRequest, ChatResponse, FinishReason, LlmBackend, LlmError,
        LlmEvent, McpServerSpec,
    };

    struct RecordedCall {
        request: ChatRequest,
        dropped_during_chat: bool,
    }

    struct MockAcpBackend {
        id: String,
        drop_flag: Arc<AtomicBool>,
        calls: Mutex<Vec<RecordedCall>>,
    }

    #[async_trait]
    impl LlmBackend for MockAcpBackend {
        fn id(&self) -> &str {
            &self.id
        }

        fn kind(&self) -> BackendKind {
            BackendKind::Acp
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                streaming: false,
                tool_calls: false,
                reasoning: false,
                steer: false,
                survives_restart: false,
            }
        }

        async fn chat(
            &self,
            request: ChatRequest,
            events: mpsc::Sender<LlmEvent>,
            _cancel: CancellationToken,
        ) -> Result<ChatResponse, LlmError> {
            let dropped_during_chat = self.drop_flag.load(Ordering::SeqCst);
            self.calls.lock().unwrap().push(RecordedCall {
                request,
                dropped_during_chat,
            });
            let mut response = ChatResponse::empty();
            response.text = "ACP finished.".to_string();
            let _ = events
                .send(LlmEvent::Finished {
                    finish_reason: FinishReason::Stop,
                    usage: response.usage,
                    text: response.text.clone(),
                    reasoning: String::new(),
                    tool_calls: Vec::new(),
                })
                .await;
            Ok(response)
        }

        async fn probe(&self) -> Result<BackendStatus, LlmError> {
            Ok(BackendStatus {
                backend: self.id.clone(),
                ok: true,
                detail: "mock acp ok".to_string(),
                model: None,
                version: None,
            })
        }
    }

    struct TestMcpProvider {
        requests: Mutex<Vec<McpTurnRequest>>,
        drop_flag: Arc<AtomicBool>,
    }

    impl McpTurnProvider for TestMcpProvider {
        fn register_turn(
            &self,
            request: McpTurnRequest,
        ) -> Result<McpTurnRegistration, DomainError> {
            self.requests.lock().unwrap().push(request);
            let flag = Arc::clone(&self.drop_flag);
            Ok(McpTurnRegistration::new(
                vec![McpServerSpec::http(
                    "swarmbot",
                    "http://127.0.0.1:49999/mcp/turn",
                )],
                move || {
                    flag.store(true, Ordering::SeqCst);
                },
            ))
        }
    }

    // Sub-case 1: ACP bot with NO MCP provider gets honest instructions without
    // claiming SwarmBot messaging/task tool names.
    let rig = rig();
    let bare_flag = Arc::new(AtomicBool::new(false));
    let acp = Arc::new(MockAcpBackend {
        id: "mock-acp".to_string(),
        drop_flag: Arc::clone(&bare_flag),
        calls: Mutex::new(Vec::new()),
    });
    rig.swarm
        .backends
        .register("mock-acp", Arc::clone(&acp) as Arc<dyn LlmBackend>);
    let mut lead_bot = rig.swarm.bots.get(&rig.lead).expect("lead bot");
    lead_bot.backend = "mock-acp".to_string();
    rig.swarm
        .bots
        .update(&lead_bot)
        .expect("switch lead to mock-acp");

    rig.swarm
        .send(&rig.dm_of(&rig.lead), "first bare acp turn")
        .expect("sent");
    rig.settle().await;

    {
        let calls = acp.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].request.mcp_servers.is_empty());
        assert!(calls[0].request.tools.is_empty());
        let instr = &calls[0].request.instructions;
        assert!(!instr.contains("handoff_task"), "{instr}");
        assert!(!instr.contains("message_bot"), "{instr}");
        assert!(!instr.contains("post_to_group"), "{instr}");
        assert!(instr.contains("@name"), "{instr}");
    }

    // Sub-case 2: Attach McpTurnProvider and run another ACP turn.
    let reg_drop_flag = Arc::new(AtomicBool::new(false));
    let acp_with_mcp = Arc::new(MockAcpBackend {
        id: "mock-acp-mcp".to_string(),
        drop_flag: Arc::clone(&reg_drop_flag),
        calls: Mutex::new(Vec::new()),
    });
    rig.swarm.backends.register(
        "mock-acp-mcp",
        Arc::clone(&acp_with_mcp) as Arc<dyn LlmBackend>,
    );
    lead_bot.backend = "mock-acp-mcp".to_string();
    rig.swarm
        .bots
        .update(&lead_bot)
        .expect("switch lead to mock-acp-mcp");

    let provider = Arc::new(TestMcpProvider {
        requests: Mutex::new(Vec::new()),
        drop_flag: Arc::clone(&reg_drop_flag),
    });
    rig.swarm
        .set_mcp_provider(Arc::clone(&provider) as Arc<dyn McpTurnProvider>);

    rig.swarm
        .send(&rig.dm_of(&rig.lead), "second acp turn with mcp")
        .expect("sent");
    rig.settle().await;

    {
        let reqs = provider.requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].bot_id, rig.lead);
        assert!(!reqs[0].in_task);
        assert!(
            !reqs[0].toolsets.contains(&Toolset::Node),
            "ACP must keep its own fs/shell tools and omit Toolset::Node: {:?}",
            reqs[0].toolsets
        );
        assert!(reqs[0].toolsets.contains(&Toolset::Bot));
        assert!(reqs[0].toolsets.contains(&Toolset::Task));
    }
    {
        let calls = acp_with_mcp.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].request.mcp_servers.len(), 1);
        assert!(calls[0].request.tools.is_empty());
        assert!(
            !calls[0].dropped_during_chat,
            "MCP registration must stay active during backend.chat()"
        );
        let instr = &calls[0].request.instructions;
        assert!(instr.contains("handoff_task"), "{instr}");
        assert!(instr.contains("message_bot"), "{instr}");
        assert!(instr.contains("post_to_group"), "{instr}");
    }
    assert!(
        reg_drop_flag.load(Ordering::SeqCst),
        "MCP registration handle must drop cleanly when the turn ends"
    );

    rig.swarm.shutdown().await;
}

#[tokio::test]
async fn mid_turn_steer_on_single_round_and_other_conversation_steer_both_run() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use tokio::sync::{Notify, mpsc};
    use tokio_util::sync::CancellationToken;

    use swarmbot_contracts::config::BackendKind;
    use swarmbot_contracts::delivery::Delivery;
    use swarmbot_llm::{
        BackendStatus, Capabilities, ChatRequest, ChatResponse, FinishReason, LlmBackend, LlmError,
        LlmEvent,
    };

    struct CoordinatedBackend {
        started: Arc<Notify>,
        release: Arc<Notify>,
        calls: AtomicUsize,
        requests: Mutex<Vec<ChatRequest>>,
    }

    #[async_trait]
    impl LlmBackend for CoordinatedBackend {
        fn id(&self) -> &str {
            "coord"
        }

        fn kind(&self) -> BackendKind {
            BackendKind::Fake
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                streaming: false,
                tool_calls: true,
                reasoning: false,
                steer: true,
                survives_restart: false,
            }
        }

        async fn chat(
            &self,
            request: ChatRequest,
            events: mpsc::Sender<LlmEvent>,
            _cancel: CancellationToken,
        ) -> Result<ChatResponse, LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(request);
            if n == 0 {
                self.started.notify_one();
                self.release.notified().await;
            }
            let mut response = ChatResponse::empty();
            response.text = format!("reply-{n}");
            let _ = events
                .send(LlmEvent::Finished {
                    finish_reason: FinishReason::Stop,
                    usage: response.usage,
                    text: response.text.clone(),
                    reasoning: String::new(),
                    tool_calls: Vec::new(),
                })
                .await;
            Ok(response)
        }

        async fn probe(&self) -> Result<BackendStatus, LlmError> {
            Ok(BackendStatus {
                backend: "coord".to_string(),
                ok: true,
                detail: "ok".to_string(),
                model: None,
                version: None,
            })
        }
    }

    let rig = rig();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let coord = Arc::new(CoordinatedBackend {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
        calls: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    rig.swarm
        .backends
        .register("coord", Arc::clone(&coord) as Arc<dyn LlmBackend>);
    let mut lead_bot = rig.swarm.bots.get(&rig.lead).expect("lead bot");
    lead_bot.backend = "coord".to_string();
    rig.swarm
        .bots
        .update(&lead_bot)
        .expect("switch lead to coord");

    let lead_dm = rig.dm_of(&rig.lead);
    // Start a turn in lead's DM.
    rig.swarm.send(&lead_dm, "initial dm prompt").expect("sent");
    started.notified().await;

    // While round 1 is paused in backend.chat(), verify:
    // 1) steering ANOTHER conversation (`rig.group`) is refused by `steer` and queued via `deliver`;
    // 2) steering `lead_dm` joins mid-turn and triggers round 2 even after a 0-tool-call round!
    let mut group_steer =
        Delivery::owner_message(rig.group.clone(), rig.lead.clone(), "group note");
    group_steer.urgency = swarmbot_contracts::delivery::Urgency::Urgent;
    rig.swarm
        .store
        .insert_delivery(&group_steer)
        .expect("insert group delivery");
    assert!(!rig.swarm.runners.steer(&rig.lead, group_steer.clone()));
    rig.swarm
        .runners
        .deliver(group_steer)
        .expect("deliver group");

    let mut dm_steer =
        Delivery::owner_message(lead_dm.clone(), rig.lead.clone(), "mid-turn adjustment");
    dm_steer.urgency = swarmbot_contracts::delivery::Urgency::Urgent;
    rig.swarm
        .store
        .insert_delivery(&dm_steer)
        .expect("insert dm delivery");
    assert!(rig.swarm.runners.steer(&rig.lead, dm_steer));

    release.notify_one();
    rig.settle().await;

    {
        let reqs = coord.requests.lock().unwrap();
        assert_eq!(
            reqs.len(),
            3,
            "round 1 (initial dm), round 2 (mid-turn dm steer), and turn 2 (preserved group steer): {:?}",
            reqs.len()
        );
        assert!(
            reqs[1]
                .messages
                .iter()
                .any(|m| { m.role == swarmbot_llm::Role::Assistant && m.content == "reply-0" })
        );
        assert!(reqs[1].messages.iter().any(|m| {
            m.content
                .contains("[Added while you were working] mid-turn adjustment")
        }));
        assert!(
            reqs[2]
                .messages
                .iter()
                .any(|m| { m.content.contains("group note") })
        );
    }

    rig.swarm.shutdown().await;
}
