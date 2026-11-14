//! Two notices, both about a run whose output would otherwise mislead: the
//! unscripted-`fake` one, and the embedded-ACP-without-team-tools one.
//!
//! ## The unscripted-`fake` notice: one honest word before the first message
//!
//! `swarmbot run "…" --backend fake` and `swarmbot chat … --backend fake` start
//! a real brain on the scripted [`FakeBackend`] — but nothing in those commands
//! scripts it, so every reply is the fake's own placeholder,
//! `[fake] no scripted turn left`. That output is *true* and still misleads: it
//! reads like a team that failed rather than a model nobody gave a script. This
//! module prints the missing context to **stderr** before the first message —
//! never to stdout (`run --json` keeps stdout pure JSON), never per message, and
//! never from `demo`, which scripts its own team and therefore never asks.
//!
//! The decision is made from the resolved backends, never from reply text:
//!
//! * **embedded** ([`embedded_unscripted_fake`]): the script queues are visible
//!   in-process (`FakeBackend::queued_turns`), so the notice is printed only
//!   when no fake the conversation can wake has a scripted turn — the case
//!   where the placeholder really is what the next turn answers. A `--script`
//!   run splits a shared `fake` into per-bot fakes, and a script covering any
//!   woken bot silences the notice; an echo fake (which answers with the
//!   owner's own text and never produces the placeholder) is never a reason
//!   to warn.
//! * **remote** ([`ask_remote_uses_fake`]): the daemon's redacted
//!   `GET /api/backends` view carries the *kind* and nothing else — whether a
//!   remote fake has scripted turns left is not knowable from here, so the
//!   notice is due whenever a bot the conversation can wake reports
//!   `kind == fake`. A lookup that fails decides "do not warn": the notice must
//!   never be the reason a command fails.
//!
//! ## The embedded-ACP notice: a bot that cannot delegate
//!
//! A backend that owns its own tool loop (ACP, `claude -p`) reaches SwarmBot's
//! roster tools only through the brain's MCP gateway. `swarmbot` without
//! `--brain` never composes one, so the agent is handed no `mcpServers` while its
//! instructions describe `handoff_task` and friends. The run is not broken — the
//! bot answers for itself — but a claim that it delegated has nothing behind it,
//! and the closing `delegated:` line would contradict it silently. So
//! [`embedded_agent_without_team_tools`] decides from the roster + registry
//! kinds, and [`warn_agent_on_stderr`] says the two lines once per command.
//! D9.29 records why the fix is a disclosure and not a refusal.

use std::io::Write;

use swarmbot_client::BackendView;
use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::config::BackendKind;
use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::id::{BotId, ConversationId};
use swarmbot_core::swarm::Swarm;
use swarmbot_llm::{EMPTY_QUEUE_REPLY, FakeBackend};

use crate::remote::Remote;
use crate::resolve::Target;

/// The notice, as the two lines that go to stderr.
///
/// Built from the fake's own [`EMPTY_QUEUE_REPLY`] constant so the quoted
/// placeholder can never drift from the reply it explains.
pub fn lines() -> [String; 2] {
    [
        format!(
            "swarmbot: the `fake` backend only answers what a script tells it to — with no \
             script it replies \"{EMPTY_QUEUE_REPLY}\"."
        ),
        "swarmbot: run `swarmbot demo` for the scripted delegation story, or configure a real \
         backend (`--backend <id>` with `base_url`/`api_key_env`) for real answers."
            .to_string(),
    ]
}

/// Print the notice to `sink` unless it is suppressed by `--quiet` or nothing
/// is due. Returns whether the full notice was printed, so a caller (or a
/// test) can tell what actually reached the stream.
///
/// A sink that refuses the bytes — a closed stderr — is not worth failing a
/// run over: the notice is a courtesy, so a write error ends the attempt
/// quietly instead of becoming the command's error.
pub fn warn<W: Write>(sink: &mut W, quiet: bool, due: bool) -> bool {
    emit(sink, quiet, due, &lines())
}

/// The notice for an embedded run whose bot owns its own tool loop: two lines to
/// stderr, the same sink and the same `--quiet` rule as [`warn`].
pub fn warn_agent<W: Write>(sink: &mut W, quiet: bool, due: bool) -> bool {
    emit(sink, quiet, due, &agent_lines())
}

/// The message an embedded `run`/`chat` on an ACP (or `claude -p`) backend is
/// missing when its bot is asked to work as part of a team.
///
/// Read this together with the module docs: the same rule applies — the notice
/// must never be the reason a command fails, and it must only describe what is
/// actually true of this process.
pub fn agent_lines() -> [String; 2] {
    [
        "swarmbot: embedded mode hands an ACP agent no SwarmBot team tools — `handoff_task`, \
         `message_bot` and `post_to_group` never reach it, so it can only answer for itself."
            .to_string(),
        "swarmbot: run a `swarmbot-brain` and pass `--brain <url>` (or use `swarmbot-ui`, which \
         spawns one) when bots must delegate to each other; the brain composes the MCP gateway \
         the embedded mode has no listener for."
            .to_string(),
    ]
}

fn emit<W: Write>(sink: &mut W, quiet: bool, due: bool, lines: &[String; 2]) -> bool {
    if quiet || !due {
        return false;
    }
    for line in lines {
        if writeln!(sink, "{line}").is_err() {
            return false;
        }
    }
    true
}

/// The production entry point: the notice on this process's stderr, which is
/// where diagnostics live so stdout can stay prose (or pure JSON under
/// `run --json`).
pub fn warn_on_stderr(quiet: bool, due: bool) {
    warn(&mut std::io::stderr(), quiet, due);
}

/// [`warn_agent`] on this process's stderr, for the same reason as
/// [`warn_on_stderr`].
pub fn warn_agent_on_stderr(quiet: bool, due: bool) {
    warn_agent(&mut std::io::stderr(), quiet, due);
}

/// Embedded mode: does a bot this conversation can wake own its own tool loop
/// (an ACP agent, or `claude -p`)?
///
/// Those backends reach SwarmBot's roster tools — `handoff_task`,
/// `message_bot`, `post_to_group` — only through the MCP gateway, and the
/// gateway is composed by the **brain** (`Swarm::set_mcp_provider`, which
/// `crates/brain/src/state.rs` calls): the embedded CLI never sets a provider,
/// so the agent is handed `mcpServers: []` and its team instructions describe
/// tools it does not have.
///
/// This is a disclosure, not a capability: the fix is to run a brain (D9.11's
/// shape — the same reason the embedded CLI refuses remote node calls). It is
/// due only for a backend that owns its loop: an `openai_compat` backend is
/// driven by the runner, which calls our tools in-process and needs no gateway.
///
/// Observed 2026-11-14 (opencode, no `--brain`): a `run` asked to let `@scout`
/// count files answered "Sudah dikerjakan scout" while the run's own summary
/// said `delegated: nothing` — the model claimed a handoff nothing performed.
/// The notice is what stops that reading from being the only one available.
pub fn embedded_agent_without_team_tools(swarm: &Swarm, conversation: &ConversationId) -> bool {
    let participants = match swarm.store.get_conversation(conversation) {
        Ok(Some(conversation)) => conversation.kind.participants(),
        _ => return false,
    };
    let bots = match swarm.bots.list() {
        Ok(bots) => bots,
        Err(_) => return false,
    };
    let backends = swarm.backends.registry().list();
    participants.iter().any(|id| {
        bots.iter()
            .find(|bot| &bot.id == id)
            .and_then(|bot| backends.iter().find(|info| info.id == bot.backend))
            .is_some_and(|info| info.kind.owns_tool_loop())
    })
}

/// Embedded mode: is this a run on `fake` backends that **nothing scripted**?
///
/// "Can wake" is every group member (a mention wakes any of them, and the lead
/// answers an unmentioned message) or the one bot of a DM. The queue depth is
/// asked of the live registry, not guessed: when anything scripted the fakes —
/// a test, or `--script` — this stays silent, and the notice only ever
/// describes the case it quotes.
///
/// One shared queue or one per bot, the question is the same but the shape of
/// the answer differs: a `--script` run splits a shared `fake` into per-bot
/// fakes (`config::per_bot_fakes_for_script`), and a script rarely covers every
/// member of a group — so "any woken fake has an empty queue" would warn on a
/// scripted run whose unscripted members can only ever answer with the
/// placeholder anyway. The honest reading is the notice's own claim ("with no
/// script…"): warn exactly when *no* fake this conversation can wake has a
/// scripted turn. A script for any woken bot means the run is scripted; a
/// conversation whose woken fakes are all empty is not, and says so.
///
/// Anything unreadable (a conversation or roster read that fails) decides "do
/// not warn": the command itself reports broken reads as errors, and a notice
/// that guesses would be the exact dishonesty this module exists to remove.
pub fn embedded_unscripted_fake(swarm: &Swarm, conversation: &ConversationId) -> bool {
    let participants = match swarm.store.get_conversation(conversation) {
        Ok(Some(conversation)) => conversation.kind.participants(),
        _ => return false,
    };
    let bots = match swarm.bots.list() {
        Ok(bots) => bots,
        Err(_) => return false,
    };
    let mut queues = 0usize;
    let mut scripted = 0usize;
    for id in &participants {
        let Some(bot) = bots.iter().find(|bot| &bot.id == id) else {
            continue;
        };
        if let Some(fake) = scriptable_fake(swarm, &bot.backend) {
            queues += 1;
            if fake.queued_turns() > 0 {
                scripted += 1;
            }
        }
    }
    queues > 0 && scripted == 0
}

/// The scriptable fake handle for a backend id — `None` for anything that is
/// not a plain (non-echo) `fake`.
fn scriptable_fake(swarm: &Swarm, backend_id: &str) -> Option<std::sync::Arc<FakeBackend>> {
    // An echo fake (`options.echo = true`) answers with the owner's own text
    // and never produces the placeholder, so the notice would be wrong about
    // it — asking the queue alone would warn on every echo turn.
    if swarm.config.backend(backend_id).is_some_and(|spec| {
        spec.options.get("echo").and_then(|value| value.as_bool()) == Some(true)
    }) {
        return None;
    }
    // The registry's `fake(id)` is Some exactly for backends of kind `fake`
    // (it is where scriptable handles live), so this reads the *kind* and the
    // queue in one step — never a reply text. With per-bot fakes the id asked
    // is simply the bot's own (`fake-lead`, …): each bot's handle answers for
    // its own queue.
    swarm.backends.registry().fake(backend_id)
}

/// Remote mode: does any bot among `participants` use a backend the daemon
/// reports as `fake`?
///
/// Pure over the two lists the daemon already served, so both remote commands
/// decide the same way and the decision is testable without a socket.
pub fn remote_uses_fake(backends: &[BackendView], bots: &[Bot], participants: &[BotId]) -> bool {
    participants.iter().any(|id| {
        bots.iter()
            .find(|bot| &bot.id == id)
            .and_then(|bot| backends.iter().find(|view| view.id == bot.backend))
            .is_some_and(|view| view.kind == BackendKind::Fake)
    })
}

/// Ask the daemon for its backend list and decide. A lookup that fails
/// answers `false` — "do not warn", never "fail the command": the run or chat
/// itself will report a brain it cannot reach, loudly and with the URL.
pub fn ask_remote_uses_fake(remote: &Remote, bots: &[Bot], participants: &[BotId]) -> bool {
    match remote.backends() {
        Ok(backends) => remote_uses_fake(&backends, bots, participants),
        Err(_) => false,
    }
}

/// The bots a resolved chat target can wake: the one bot of a DM, or every
/// member of a group.
pub fn target_participants(target: &Target) -> Vec<BotId> {
    match target {
        Target::Bot { bot } => vec![bot.id.clone()],
        Target::Group { conversation } => conversation.kind.participants(),
    }
}

/// The bots a run's resolved conversation can wake. An id the roster does not
/// contain decides "do not warn" — the run reports missing conversations
/// itself.
pub fn conversation_participants(
    conversations: &[Conversation],
    conversation: &ConversationId,
) -> Vec<BotId> {
    conversations
        .iter()
        .find(|candidate| &candidate.id == conversation)
        .map(|candidate| candidate.kind.participants())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use swarmbot_client::testing::StubBrain;
    use swarmbot_contracts::config::{BackendSpec, BrainConfig};
    use swarmbot_llm::ScriptedTurn;

    use super::*;
    use crate::embedded::{BrainParams, Embedded};
    use crate::session::Session;
    use crate::tmp::TempDir;

    fn embedded(dir: &Path, config: BrainConfig) -> Embedded {
        Embedded::build(
            BrainParams {
                config,
                data_dir: dir.to_path_buf(),
                roots: Vec::new(),
                secrets: BTreeMap::new(),
                offline: true,
            },
            true,
        )
        .expect("a brain builds")
    }

    fn demo_on(backend: &str) -> BrainConfig {
        BrainConfig {
            team: Some(BrainConfig::demo_team(backend)),
            ..BrainConfig::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_embedded_decision_reads_the_script_queue_not_a_reply() {
        let dir = TempDir::new("notice-embedded").expect("temp");
        let embedded = embedded(dir.path(), demo_on("fake"));
        let swarm = swarm_of(&embedded);
        let group = team_group(&swarm);

        // Nothing scripted the always-present fake: the placeholder is what the
        // next turn really answers, so the notice is due — in the group and in
        // a DM alike.
        assert!(embedded_unscripted_fake(&swarm, &group.id));
        let lead = swarm.bots.by_name("lead").expect("the demo lead");
        assert!(embedded_unscripted_fake(
            &swarm,
            &lead.direct_conversation_id
        ));

        // One scripted turn silences it: the queue is visible in-process, so
        // the notice only ever describes the case it quotes.
        swarm
            .backends
            .registry()
            .fake("fake")
            .expect("the fake is always registered")
            .push_turn(ScriptedTurn::text("looking now"));
        assert!(!embedded_unscripted_fake(&swarm, &group.id));

        // A conversation the store does not know decides "do not warn" rather
        // than guessing.
        assert!(!embedded_unscripted_fake(&swarm, &ConversationId::new()));
        embedded.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_embedded_acp_run_says_its_bots_have_no_team_tools() {
        let dir = TempDir::new("notice-agent").expect("temp");
        // An ACP backend that owns its own tool loop. Nothing spawns: building
        // a spec registers a backend and dials nothing (the comment on
        // `a_real_backend_never_warns_in_either_mode` makes the same point).
        let mut spec = BackendSpec::fake("agent");
        spec.kind = BackendKind::Acp;
        spec.options.insert(
            "command".to_string(),
            serde_json::json!("/nonexistent/swarmbot-test/agent"),
        );
        let config = BrainConfig {
            default_backend: "agent".into(),
            backends: vec![spec],
            ..demo_on("agent")
        };
        let embedded = embedded(dir.path(), config);
        let swarm = swarm_of(&embedded);
        let group = team_group(&swarm);
        // The group and the DM both can wake that bot: the notice is due.
        assert!(embedded_agent_without_team_tools(&swarm, &group.id));
        let lead = swarm.bots.by_name("lead").expect("the demo lead");
        assert!(embedded_agent_without_team_tools(
            &swarm,
            &lead.direct_conversation_id
        ));
        // A conversation the store does not know decides "do not warn".
        assert!(!embedded_agent_without_team_tools(
            &swarm,
            &ConversationId::new()
        ));

        // Two lines, to the sink, and `--quiet` suppresses them.
        let mut sink = Vec::new();
        assert!(warn_agent(&mut sink, false, true));
        assert_eq!(
            sink.iter().filter(|byte| **byte == b'\n').count(),
            2,
            "{}",
            String::from_utf8_lossy(&sink)
        );
        let mut quiet = Vec::new();
        assert!(!warn_agent(&mut quiet, true, true));
        assert!(quiet.is_empty());
        embedded.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_echo_fake_needs_no_script_so_it_does_not_warn() {
        let dir = TempDir::new("notice-echo").expect("temp");
        let mut spec = BackendSpec::fake("echo-y");
        spec.options
            .insert("echo".to_string(), serde_json::Value::Bool(true));
        let config = BrainConfig {
            default_backend: "echo-y".into(),
            backends: vec![spec],
            ..demo_on("echo-y")
        };
        let embedded = embedded(dir.path(), config);
        let swarm = swarm_of(&embedded);
        let group = team_group(&swarm);
        // An echo fake answers with the owner's own text; the placeholder the
        // notice quotes can never occur, so warning would be a lie.
        assert!(!embedded_unscripted_fake(&swarm, &group.id));
        embedded.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_real_backend_never_warns_in_either_mode() {
        let dir = TempDir::new("notice-real").expect("temp");
        let config = BrainConfig {
            default_backend: "inferhub".into(),
            backends: vec![BackendSpec::openai_compat(
                "inferhub",
                "https://api.example.invalid/v1",
                "SWARMBOT_CLI_TEST_UNSET_KEY",
                "m",
            )],
            ..demo_on("inferhub")
        };
        let embedded = embedded(dir.path(), config);
        let swarm = swarm_of(&embedded);
        let group = team_group(&swarm);
        // Kind `open_ai_compat` with an empty script queue is still a real
        // backend: the notice is about the fake's placeholder, not about
        // scripts in general. (No request is ever made; building a spec dials
        // nothing.)
        assert!(!embedded_unscripted_fake(&swarm, &group.id));
        // And it is driven by the runner, which calls our tools in-process:
        // the missing-MCP notice is not due. This is the line that keeps the
        // two notices distinct — a runner-driven backend needs no gateway.
        assert!(!embedded_agent_without_team_tools(&swarm, &group.id));
        embedded.shutdown().await;

        // The remote decision agrees: kind is what it reads.
        let lead = Bot::new("lead", "Lead", "", "inferhub", ConversationId::new());
        let view = BackendView {
            id: "inferhub".into(),
            kind: BackendKind::OpenAiCompat,
            base_url: Some("https://api.example.invalid/v1".into()),
            model: Some("m".into()),
            api_key_env: Some("SWARMBOT_CLI_TEST_UNSET_KEY".into()),
            options: Default::default(),
        };
        assert!(!remote_uses_fake(
            &[view],
            std::slice::from_ref(&lead),
            std::slice::from_ref(&lead.id)
        ));
    }

    #[test]
    fn the_remote_decision_warns_when_any_woken_bot_reports_fake() {
        let fake_view = BackendView {
            id: "fake".into(),
            kind: BackendKind::Fake,
            base_url: None,
            model: Some("fake".into()),
            api_key_env: None,
            options: Default::default(),
        };
        let real_view = BackendView {
            id: "inferhub".into(),
            kind: BackendKind::OpenAiCompat,
            base_url: Some("https://api.example.invalid/v1".into()),
            model: Some("m".into()),
            api_key_env: Some("K".into()),
            options: Default::default(),
        };
        let views = [fake_view, real_view];
        let lead = Bot::new("lead", "Lead", "", "fake", ConversationId::new());
        let dev = Bot::new("dev", "Dev", "", "inferhub", ConversationId::new());
        let ghost = Bot::new("ghost", "Ghost", "", "missing", ConversationId::new());
        let bots = [lead.clone(), dev.clone(), ghost.clone()];

        // A DM wakes its one bot.
        assert!(remote_uses_fake(
            &views,
            &bots,
            std::slice::from_ref(&lead.id)
        ));
        assert!(!remote_uses_fake(
            &views,
            &bots,
            std::slice::from_ref(&dev.id)
        ));
        // A backend the daemon did not list decides "do not warn".
        assert!(!remote_uses_fake(
            &views,
            &bots,
            std::slice::from_ref(&ghost.id)
        ));
        // A group warns when *any* member a mention could wake is on a fake.
        assert!(remote_uses_fake(
            &views,
            &bots,
            &[dev.id.clone(), lead.id.clone()]
        ));
        assert!(!remote_uses_fake(
            &views,
            &bots,
            std::slice::from_ref(&dev.id)
        ));
        // An empty participant list warns about nobody.
        assert!(!remote_uses_fake(&views, &bots, &[]));
    }

    #[test]
    fn a_remote_lookup_that_fails_decides_do_not_warn() {
        // Port 1 on loopback refuses instantly; nothing is sent anywhere. The
        // premise is asserted so the test cannot silently degrade into
        // "the lookup worked and answered no fake".
        let remote = Remote::without_probe("http://127.0.0.1:1", "tok").expect("builds unprobed");
        assert!(remote.backends().is_err(), "the lookup really fails");
        let lead = Bot::new("lead", "Lead", "", "fake", ConversationId::new());
        assert!(!ask_remote_uses_fake(
            &remote,
            std::slice::from_ref(&lead),
            std::slice::from_ref(&lead.id)
        ));
    }

    #[test]
    fn a_remote_fake_backend_is_asked_from_the_daemons_own_list() {
        let stub = StubBrain::start();
        let remote = Remote::connect(&stub.url(), Some("tok"), Path::new("."), false)
            .expect("the stub answers health");
        let bots = Session::bots(&remote).expect("bots");
        let conversations = Session::conversations(&remote).expect("conversations");
        let conversation = conversations.first().expect("the stub serves one");
        let participants = conversation.kind.participants();
        // The stub's bot runs on backend `fake`, and `GET /api/backends` says
        // so — that answer, not a reply text, decides the notice.
        assert!(ask_remote_uses_fake(&remote, &bots, &participants));
        assert!(
            stub.last_request("GET", "/api/backends").is_some(),
            "the decision really came from the daemon's backend list"
        );
        let mut sink = Vec::new();
        assert!(warn(&mut sink, false, true));
    }

    #[test]
    fn the_notice_is_two_lines_on_the_sink_and_quiet_suppresses_it() {
        let mut sink = Vec::new();
        assert!(warn(&mut sink, false, true));
        let text = String::from_utf8(sink).expect("utf8");
        assert_eq!(text.lines().count(), 2, "one notice, two lines: {text}");
        // The placeholder is quoted from the fake's own constant, and both
        // ways out are named.
        assert!(text.contains(EMPTY_QUEUE_REPLY), "{text}");
        assert!(text.contains("swarmbot demo"), "{text}");
        assert!(text.contains("--backend <id>"), "{text}");
        assert!(
            text.contains("base_url") && text.contains("api_key_env"),
            "{text}"
        );

        // `--quiet` means only bot text: the notice stays off.
        let mut quiet_sink = Vec::new();
        assert!(!warn(&mut quiet_sink, true, true));
        assert!(quiet_sink.is_empty(), "--quiet suppresses the notice");

        // Nothing due, nothing printed.
        let mut idle_sink = Vec::new();
        assert!(!warn(&mut idle_sink, false, false));
        assert!(idle_sink.is_empty());
    }

    // Small helpers, because `expect` in a test names the invariant that
    // cannot fail rather than the step that did.
    fn swarm_of(embedded: &Embedded) -> std::sync::Arc<Swarm> {
        std::sync::Arc::clone(&embedded.swarm)
    }

    fn team_group(swarm: &Swarm) -> Conversation {
        swarm
            .conversations
            .find_by_title("Team")
            .expect("the roster reads")
            .expect("the demo team has its group")
    }
}
