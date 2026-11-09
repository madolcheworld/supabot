//! `swarmbot demo`: the delegation acceptance test, runnable with no
//! credentials and no network.
//!
//! The demo builds its own [`BrainConfig`] — one scripted [`FakeBackend`] per
//! bot (`fake-lead`, `fake-scout`, …) and the four-bot demo team pointed at
//! them — then scripts the story the product exists to tell:
//!
//! 1. the owner asks the Lead for something;
//! 2. the Lead calls `handoff_task` to Scout;
//! 3. Scout messages the Lead back (`message_bot`) and finishes the task;
//! 4. the Lead takes a `start_task` for itself;
//! 5. the task result comes back and the Lead writes the final answer.
//!
//! Every step streams to the terminal as the orchestration produces it —
//! nothing here writes transcript entries directly, so what you see is what
//! the real machinery did.
//!
//! **Honesty rule:** if this build's tool registry has no `handoff_task`
//! (`core/src/bot_tools.rs` is still a scaffold in some checkouts), the demo
//! says so in one line and falls back to a scripted group conversation that
//! still exercises the runner, the transcript, the hub and mention routing.
//! It never fakes a delegation that did not happen.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::style::Color;
use serde_json::json;
use swarmbot_contracts::config::{BackendSpec, BrainConfig};
use swarmbot_contracts::entry::{Author, EntryKind};
use swarmbot_contracts::id::ConversationId;
use swarmbot_contracts::task::Task;
use swarmbot_core::swarm::Swarm;
use swarmbot_llm::{FakeBackend, ScriptedTurn};

use crate::embedded::{BrainParams, Embedded, drain_quiescent, drain_synced, emit_event};
use crate::render::{Printer, Roster};
use crate::tmp::TempDir;

/// One scripted backend per bot, so each role plays its own script.
const LEAD: &str = "fake-lead";
const SCOUT: &str = "fake-scout";
const DEV: &str = "fake-dev";
const SENTINEL: &str = "fake-sentinel";

/// The per-bot fake-id convention this module introduced (`fake-lead`,
/// `fake-scout`, …): one fake — and therefore one script queue — per bot, so a
/// turn scripted for one bot can never be popped by another, whatever the
/// scheduler does. [`crate::config::per_bot_fakes_for_script`] reuses this for
/// `--script` runs, so per-bot fakes stay a single mechanism instead of two
/// that can drift.
pub(crate) fn per_bot_fake_id(bot_name: &str) -> String {
    format!("fake-{bot_name}")
}

/// The tool whose presence decides whether the delegation story can run.
/// `handoff_task` is the one the demo's plot hinges on; when the registry
/// lacks it, the rest of the bot tools are missing too (they register
/// together).
const DELEGATION_PROBE_TOOL: &str = "handoff_task";

pub struct DemoOptions {
    /// In-memory store instead of SQLite under `data_dir`.
    pub offline: bool,
    pub data_dir: PathBuf,
    /// Budget for the team to go idle; the demo reports a timeout honestly.
    pub wait: Duration,
}

/// What the demo did, for the summary — and for the tests, which assert on
/// the store rather than on stdout. The binary itself only reads `idle`; the
/// inspection fields exist for the demo test (and for callers that want to
/// verify the story), so they carry an explicit allow rather than a phantom
/// use somewhere in `main`.
#[allow(dead_code)]
pub struct DemoReport {
    pub swarm: Arc<Swarm>,
    /// The group the story ran in ("Team").
    pub conversation: ConversationId,
    /// Whether this build has the delegation tools.
    pub delegation_tools: bool,
    /// Whether the team went idle inside the budget.
    pub idle: bool,
    /// Keeps an `--offline` demo's directory alive until the report drops.
    _temp: Option<TempDir>,
}

/// The demo's config: four fake backends, and the demo team with each bot
/// pointed at its own — that is what makes per-role scripting possible.
pub fn demo_config() -> BrainConfig {
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

/// Does this build have the delegation tools? Asked, not assumed: the answer
/// decides which script plays and what the summary may claim.
pub fn delegation_tools_present(swarm: &Swarm) -> bool {
    swarm.tools.get(DELEGATION_PROBE_TOOL).is_some()
}

/// Run the demo end to end. Returns the report (and keeps the swarm alive in
/// it) so callers — and tests — can inspect what really happened.
pub async fn run_demo<W: Write>(
    options: &DemoOptions,
    printer: &mut Printer<W>,
) -> Result<DemoReport> {
    // An offline demo still needs a real directory for the bot workspaces and
    // the audit log; a temp one keeps `./data` clean.
    let temp = options.offline.then(|| TempDir::new("demo")).transpose()?;
    let data_dir = match &temp {
        Some(dir) => dir.path().to_path_buf(),
        None => options.data_dir.clone(),
    };

    let embedded = Embedded::build(
        BrainParams {
            config: demo_config(),
            data_dir: data_dir.clone(),
            roots: Vec::new(),
            secrets: std::collections::BTreeMap::new(),
            offline: options.offline,
        },
        true,
    )?;
    let swarm = Arc::clone(&embedded.swarm);

    let group = swarm
        .conversations
        .find_by_title("Team")
        .context("cannot read the roster")?
        .ok_or_else(|| anyhow!("the demo team has no \"Team\" group — the build is broken"))?;

    let delegation_tools = delegation_tools_present(&swarm);

    printer.heading("── swarmbot demo ──")?;
    printer.note(&format!(
        "team: @lead @scout @dev @sentinel · group \"Team\" · backends: {LEAD}, {SCOUT}, {DEV}, {SENTINEL} (scripted fakes, no network)"
    ))?;
    printer.note(&format!(
        "store: {} · data dir: {}",
        if options.offline {
            "in-memory (--offline)"
        } else {
            "durable"
        },
        data_dir.display()
    ))?;
    if !delegation_tools {
        // One honest line: no delegation will happen, and the demo will not
        // pretend otherwise in its summary either.
        printer.line(&printer.theme.paint(
            "notice: delegation tools are not in this build yet (no `handoff_task` in the \
             registry) — running the group-routing fallback script instead; nothing will be \
             handed off.",
            Color::Yellow,
        ))?;
    }

    script(&swarm, delegation_tools)?;

    // Subscribe to *everything*: the handoff's work happens in Scout's DM and
    // the Lead's own task in the Lead's DM, and hiding those would hide half
    // the delegation story. Off-focus lines get a `[dm @scout]` label.
    let mut rx = embedded.subscribe(None);
    let mut roster = Roster::new(
        swarm.bots.list()?,
        swarm.conversations.list()?,
        Some(group.id.clone()),
    );
    drain_synced(&mut rx, printer, &mut roster, false, false).await?;

    let owner_text = if delegation_tools {
        "please find out what changed under src/ and give me a final verdict."
    } else {
        "@scout what can you see in the workspace?"
    };
    printer.note(&format!("sending to \"Team\": {owner_text}"))?;
    embedded.send(&group.id, owner_text)?;

    // Wait for the team to settle, streaming while we wait.
    let (idle_tx, mut idle_rx) = tokio::sync::oneshot::channel::<bool>();
    let waiter = Arc::clone(&swarm);
    let budget = options.wait;
    tokio::spawn(async move {
        let _ = idle_tx.send(waiter.wait_for_idle(budget).await);
    });
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Some(event) => emit_event(printer, &mut roster, &event, false, false)?,
                None => break,
            },
            _ = &mut idle_rx => break,
        }
    }
    let idle = match idle_rx.try_recv() {
        Ok(idle) => idle,
        // The select fired on an event in the same tick; ask again with a
        // fresh (short) budget — the fakes are instantaneous, so this only
        // covers the scheduling gap, it never papers over a stuck team.
        Err(_) => swarm.wait_for_idle(Duration::from_secs(5)).await,
    };
    drain_quiescent(
        &mut rx,
        printer,
        &mut roster,
        false,
        Duration::from_millis(250),
    )
    .await?;
    printer.finish()?;

    print_summary(&embedded, &group.id, delegation_tools, idle, printer)?;
    embedded.shutdown().await;

    Ok(DemoReport {
        swarm,
        conversation: group.id,
        delegation_tools,
        idle,
        _temp: temp,
    })
}

/// Push the scripted turns. `ScriptedTurn` is not `Clone` and a turn with a
/// tool call makes the runner ask the model *again*, so every tool round is
/// followed by a text-only round that ends the turn — the same discipline the
/// core's own delegation tests use.
fn script(swarm: &Swarm, delegation_tools: bool) -> Result<()> {
    if delegation_tools {
        script_delegation(swarm)
    } else {
        script_fallback(swarm)
    }
}

fn fake(swarm: &Swarm, id: &str) -> Result<Arc<FakeBackend>> {
    swarm
        .backends
        .registry()
        .fake(id)
        .ok_or_else(|| anyhow!("backend `{id}` is configured as a fake but the registry lost it"))
}

/// The full story. Turn order per backend is forced by the orchestration's
/// FIFO queues, which makes the whole script deterministic:
///
/// * lead: group turn (handoff + "waiting"), DM turn (scout's message),
///   group turn (task result + start_task), DM task turn (the verdict),
///   group turn (the final answer);
/// * scout: DM task turn (message_bot back, then the finding as the task
///   result).
fn script_delegation(swarm: &Swarm) -> Result<()> {
    let lead = fake(swarm, LEAD)?;
    // Group turn 1, round 1: delegate. No `@scout` in the text — a mention in
    // a posted group reply would wake Scout a second time and desynchronize
    // the queue.
    lead.push_turn(
        ScriptedTurn::text("On it — handing the search to Scout.")
            .with_reasoning("The owner wants a verdict; searching is scout's job.")
            .with_tool_call(
                "handoff_task",
                json!({
                    "to": "scout",
                    "title": "find what changed under src/",
                    "task": "list the files that changed under src/ and say whether anything looks risky",
                    "context": "the owner asked for a final verdict"
                }),
            ),
    );
    // Group turn 1, round 2: no tool call, so the turn ends and the Lead
    // waits for the result instead of inventing one.
    lead.push_turn(ScriptedTurn::text(
        "I have asked scout; I will report when the result lands.",
    ));
    // Lead's DM turn, woken by scout's message_bot.
    lead.push_turn(ScriptedTurn::text(
        "Noted — I will fold scout's early read into the verdict.",
    ));
    // Group turn 2 (the task result), round 1: take the summary task.
    lead.push_turn(
        ScriptedTurn::text("Scout's finding is in — starting the summary task.").with_tool_call(
            "start_task",
            json!({
                "title": "write the final verdict",
                "brief": "summarize what changed under src/ and give the owner a verdict"
            }),
        ),
    );
    // Group turn 2, round 2: end the turn; the task runs in the Lead's DM.
    lead.push_turn(ScriptedTurn::text(
        "The summary task is running; I will report when it lands.",
    ));
    // Lead's DM task turn: its text becomes the task's result.
    lead.push_turn(ScriptedTurn::text(
        "Verdict: three files changed under src/ (config.rs, render.rs, main.rs); no destructive commands; safe to keep.",
    ));
    // Group turn 3 (the task result): the final answer to the owner.
    lead.push_turn(ScriptedTurn::text(
        "Done. Scout found three changed files under src/ and nothing risky; the summary task \
         confirms it. Verdict: safe to keep.",
    ));

    let scout = fake(swarm, SCOUT)?;
    // Scout's DM task turn, round 1: report back to the Lead mid-task.
    scout.push_turn(ScriptedTurn::default().with_tool_call(
        "message_bot",
        json!({
            "to": "lead",
            "text": "early read: three files changed under src/, nothing destructive so far"
        }),
    ));
    // Round 2: the finding, which becomes the task result delivered to the
    // Lead in the group.
    scout.push_turn(ScriptedTurn::text(
        "Three files changed: src/config.rs, src/render.rs, src/main.rs. No destructive commands in the diff.",
    ));
    Ok(())
}

/// The fallback when the bot tools are missing: one owner mention wakes Scout,
/// Scout's reply mentions the Lead, the Lead closes. That still exercises the
/// runner, the transcript, the live hub and group mention routing — it just
/// delegates nothing, and the summary says exactly that.
fn script_fallback(swarm: &Swarm) -> Result<()> {
    let scout = fake(swarm, SCOUT)?;
    scout.push_turn(ScriptedTurn::text(
        "I can see three files under src/: main.rs, config.rs, render.rs. @lead over to you.",
    ));
    let lead = fake(swarm, LEAD)?;
    // No mention in the Lead's reply: it would wake Scout again, and an
    // unscripted wake answers with the fake's empty-queue placeholder.
    lead.push_turn(ScriptedTurn::text(
        "Thanks — that is the picture. Owner: three source files under src/, nothing risky in sight.",
    ));
    Ok(())
}

/// The closing block: what was delegated to whom (from the tasks and the
/// event entries), and whether the team settled.
fn print_summary<W: Write>(
    embedded: &Embedded,
    conversation: &ConversationId,
    delegation_tools: bool,
    idle: bool,
    printer: &mut Printer<W>,
) -> Result<()> {
    let swarm = &embedded.swarm;
    printer.heading("── summary ──")?;

    let lines = delegation_lines(swarm, conversation);
    if lines.is_empty() {
        if delegation_tools {
            printer.line("delegated: nothing (the script did not reach a handoff)")?;
        } else {
            printer.line("delegated: nothing — the delegation tools are not in this build")?;
        }
    } else {
        printer.line("delegated:")?;
        for line in lines {
            printer.line(&format!("  {line}"))?;
        }
    }

    // The event entries are the transcript's own account; counting them is a
    // cross-check that the tasks and the visible story agree.
    let entries = swarm
        .conversations
        .tail(conversation, 500)
        .context("cannot read the transcript")?;
    let mut counts = [
        ("handoff", 0usize),
        ("task-started", 0),
        ("task-result", 0),
        ("bot-messaged", 0),
        ("no-reply", 0),
        ("hop-limit", 0),
    ];
    let mut owner_messages = 0usize;
    let mut bot_messages = 0usize;
    for entry in &entries {
        match &entry.kind {
            EntryKind::Event(event) => {
                for (key, count) in &mut counts {
                    if event.event == *key {
                        *count += 1;
                    }
                }
            }
            EntryKind::Message(message) if !message.text.is_empty() => match entry.author {
                Author::Owner => owner_messages += 1,
                Author::Bot { .. } => bot_messages += 1,
                Author::System => {}
            },
            _ => {}
        }
    }
    let events: Vec<String> = counts
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(key, count)| format!("{count} {key}"))
        .collect();
    printer.note(&format!(
        "entries in \"Team\": {owner_messages} owner message(s), {bot_messages} bot reply(ies){}",
        if events.is_empty() {
            String::new()
        } else {
            format!(", events: {}", events.join(", "))
        }
    ))?;

    // Every DM that saw action, so the summary is honest about work that
    // happened off the group stage.
    let mut dm_lines = Vec::new();
    for bot in swarm.bots.list()? {
        let dm_entries = swarm.conversations.tail(&bot.direct_conversation_id, 500)?;
        let messages = dm_entries
            .iter()
            .filter(|entry| match &entry.kind {
                EntryKind::Message(message) => !message.text.is_empty(),
                _ => false,
            })
            .count();
        if messages > 0 {
            dm_lines.push(format!("dm @{}: {messages} message(s)", bot.name));
        }
    }
    if !dm_lines.is_empty() {
        printer.note(&format!("behind the scenes — {}", dm_lines.join("; ")))?;
    }

    if idle {
        printer.note("the team went idle; demo complete.")?;
    } else {
        printer.line(&printer.theme.paint(
            &format!(
                "TIMED OUT — still running: {}",
                embedded.still_running().join("; ")
            ),
            Color::Red,
        ))?;
    }
    Ok(())
}

/// One line per task that belongs to this conversation:
/// `@lead → @scout "find what changed" (succeeded)`. Shared with `run`'s
/// `delegated:` summary so both commands tell the truth the same way.
pub fn delegation_lines(swarm: &Swarm, conversation: &ConversationId) -> Vec<String> {
    let name_of = |id: &swarmbot_contracts::id::BotId| {
        swarm
            .store
            .get_bot(id)
            .ok()
            .flatten()
            .map(|bot| format!("@{}", bot.name))
            .unwrap_or_else(|| "@unknown".to_string())
    };
    let mut lines = Vec::new();
    for task in swarm
        .tasks
        .in_conversation(conversation)
        .unwrap_or_default()
    {
        lines.push(task_line(&task, &name_of));
    }
    lines
}

/// One task's `delegated:` line: `@lead → @scout "find what changed" (succeeded)`.
///
/// Shared by the embedded and the remote half of the seam, so the sentence a run
/// prints cannot drift between the two modes — only the source of the tasks and
/// of the names differs.
pub(crate) fn task_line(
    task: &Task,
    name_of: &dyn Fn(&swarmbot_contracts::id::BotId) -> String,
) -> String {
    let from = task
        .requester
        .as_ref()
        .map(name_of)
        .unwrap_or_else(|| name_of(&task.owner));
    format!(
        "{from} → {} \"{}\" ({})",
        name_of(&task.owner),
        task.title,
        task.status.label()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Theme;

    /// The demo must run end to end in memory: an owner entry, a bot reply,
    /// and — when this build has the delegation tools — a real handoff or
    /// task-started event entry. When it does not, the honest notice instead.
    /// Assertions are on the store, never on stdout.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_demo_runs_end_to_end_in_memory() {
        let mut buffer = Vec::new();
        let report = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            run_demo(
                &DemoOptions {
                    offline: true,
                    data_dir: PathBuf::from("unused-offline"),
                    wait: Duration::from_secs(30),
                },
                &mut printer,
            )
            .await
            .expect("the demo runs")
        };

        assert!(report.idle, "the team must settle inside the budget");
        let entries = report
            .swarm
            .store
            .entry_tail(&report.conversation, 500)
            .expect("entries");

        // (a) at least one owner entry
        assert!(
            entries
                .iter()
                .any(|entry| entry.author == Author::Owner && entry.is_message()),
            "the owner's message is in the transcript"
        );
        // (b) at least one bot reply
        assert!(
            entries
                .iter()
                .any(|entry| matches!(entry.author, Author::Bot { .. })
                    && entry.kind.text().is_some_and(|text| !text.is_empty())),
            "a bot replied in the group"
        );

        let out = String::from_utf8(buffer).expect("utf8");
        if report.delegation_tools {
            // (c) a real delegation event, produced by the orchestration.
            assert!(
                entries.iter().any(|entry| matches!(&entry.kind,
                    EntryKind::Event(event) if event.event == "handoff" || event.event == "task-started")),
                "a handoff or task-started event entry exists"
            );
            let tasks = report
                .swarm
                .tasks
                .in_conversation(&report.conversation)
                .expect("tasks");
            assert!(
                tasks
                    .iter()
                    .any(|task| task.status == swarmbot_contracts::task::TaskStatus::Succeeded),
                "at least one task really ran: {tasks:?}"
            );
            assert!(out.contains("delegated:"), "{out}");
            assert!(
                !out.contains("notice: delegation tools are not in this build"),
                "with the tools present the fallback notice must not appear: {out}"
            );
        } else {
            assert!(
                out.contains("not in this build yet"),
                "the honest notice is printed: {out}"
            );
            assert!(
                out.contains("delegated: nothing"),
                "and the summary does not claim a delegation: {out}"
            );
        }
    }

    /// The fallback story must work whichever branch runs: a mention wakes
    /// Scout, Scout's reply wakes the Lead, both answers land in the group.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_fallback_script_alone_still_moves_the_team() {
        let dir = TempDir::new("demo-fallback").expect("temp");
        let swarm = swarmbot_core::compose::memory(
            demo_config(),
            dir.path(),
            Vec::new(),
            &std::collections::BTreeMap::new(),
        )
        .expect("a swarm");
        swarm.boot().expect("boots");
        let group = swarm
            .conversations
            .find_by_title("Team")
            .expect("list")
            .expect("the group");
        script_fallback(&swarm).expect("scripts");

        swarm
            .send(&group.id, "@scout what can you see in the workspace?")
            .expect("sends");
        assert!(
            swarm.wait_for_idle(Duration::from_secs(20)).await,
            "the team settles"
        );

        let entries = swarm.store.entry_tail(&group.id, 100).expect("entries");
        let text = entries
            .iter()
            .filter_map(|entry| entry.kind.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("main.rs, config.rs, render.rs"), "{text}");
        assert!(text.contains("that is the picture"), "{text}");
        let scout = swarm.bots.by_name("scout").expect("scout");
        let turns = swarm.store.turns_in(&group.id, 20).expect("turns");
        assert!(
            turns.iter().any(|turn| turn.bot_id == scout.id),
            "the mention really woke scout: {turns:?}"
        );
        swarm.shutdown().await;
    }

    #[test]
    fn the_demo_config_points_every_bot_at_its_own_fake() {
        let config = demo_config();
        config.validate().expect("the demo config is valid");
        let team = config.team.as_ref().expect("a team");
        for bot in &team.bots {
            let backend = bot.backend.as_deref().expect("a backend per bot");
            assert_eq!(backend, format!("fake-{}", bot.name));
            assert_eq!(backend, per_bot_fake_id(&bot.name), "one convention");
            assert!(config.backend(backend).is_some(), "{backend} is configured");
        }
        assert_eq!(config.default_backend, LEAD);
        assert!(
            config
                .backends
                .iter()
                .all(|backend| backend.kind == swarmbot_contracts::config::BackendKind::Fake)
        );
    }

    /// The demo scripts its own team, so the unscripted-`fake` notice (which
    /// `run`/`chat` print to stderr before their first message) must never
    /// appear in a demo — `cmd_demo` never asks for it, and this pins the
    /// observable half: no notice text in anything the demo prints.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_demo_never_prints_the_unscripted_fake_notice() {
        let mut buffer = Vec::new();
        let report = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            run_demo(
                &DemoOptions {
                    offline: true,
                    data_dir: PathBuf::from("unused-offline"),
                    wait: Duration::from_secs(30),
                },
                &mut printer,
            )
            .await
            .expect("the demo runs")
        };
        assert!(report.idle, "the team settles");
        let out = String::from_utf8(buffer).expect("utf8");
        for line in &crate::notice::lines() {
            assert!(
                !out.contains(line.as_str()),
                "the demo prints no unscripted-fake notice: {out}"
            );
        }
    }

    #[test]
    fn task_lines_read_as_delegations() {
        let owner = swarmbot_contracts::id::BotId::from_suffix("0".repeat(32).as_str());
        let requester = swarmbot_contracts::id::BotId::from_suffix("1".repeat(32).as_str());
        let task = Task::handoff(
            requester.clone(),
            owner.clone(),
            ConversationId::new(),
            "review the diff",
            "look for risk",
            None,
        );
        let names = |id: &swarmbot_contracts::id::BotId| {
            if *id == owner {
                "@scout".to_string()
            } else {
                "@lead".to_string()
            }
        };
        assert_eq!(
            task_line(&task, &names),
            "@lead → @scout \"review the diff\" (queued)"
        );
        let own = Task::own(owner.clone(), ConversationId::new(), "summarize", "x");
        assert_eq!(
            task_line(&own, &names),
            "@scout → @scout \"summarize\" (queued)"
        );
    }
}
