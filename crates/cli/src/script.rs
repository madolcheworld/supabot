//! Scripting a `run`/`chat` from a file: real delegation, offline and
//! deterministic, without pretending anything.
//!
//! `swarmbot demo` proves the delegation story with four scripted
//! [`FakeBackend`]s it builds itself. `swarmbot run "…" --backend fake` could
//! not: with nothing scripting the fake, every reply is the placeholder
//! `[fake] no scripted turn left`, so a team that is fine reads as broken. This
//! module closes that gap — an operator writes the model turns in a TOML file
//! and `--script FILE` pushes them into the fake backends a run's bots resolve
//! to, *after* the swarm boots and the team is provisioned and *before* the
//! first message is sent.
//!
//! # The file
//!
//! One `[[turns]]` entry per model turn, consumed **in order** by the named
//! bot's backend. Each turn names a roster handle (`bot`), and carries optional
//! assistant `text`, optional `reasoning` (streamed before the text), and zero
//! or more `[[turns.tool_calls]]` (in order). A turn with **no tool calls ends
//! the model round**, which is how a bot stops and waits for a result instead of
//! being asked for another turn immediately — so every tool-call turn must be
//! followed by a text-only turn for the same bot. `arguments` is arbitrary TOML,
//! converted to the JSON a tool call carries.
//!
//! ```toml
//! [[turns]]
//! bot = "lead"
//! text = "On it — handing the search to scout."
//! [[turns.tool_calls]]
//! name = "handoff_task"
//! arguments = { to = "scout", title = "find what changed", task = "list the files that changed under src/" }
//!
//! [[turns]]
//! bot = "lead"                 # no tool calls: the round ends, the lead waits
//! text = "I have asked scout."
//! ```
//!
//! # One queue per scripted bot
//!
//! A `fake` backend is one FIFO queue, and `--backend fake` puts the whole
//! team on the single built-in `fake`. If every scripted turn shared that one
//! queue, a bot woken mid-delegation could pop a turn scripted for another bot
//! — a narrow scheduling window under a multi-threaded runtime, but a real one,
//! and a scripted run is required to be deterministic. So when `--script` is
//! given, [`crate::config::per_bot_fakes_for_script`] upgrades a shared fake
//! into one fake per scripted bot (`fake-lead`, `fake-scout`, …, exactly the
//! `demo`'s own idiom) **before the swarm is composed**, and the pushes below
//! land in each bot's own queue: every scripted queue then has exactly one
//! consumer, under any scheduler. A script naming a single bot splits nothing
//! — one bot, one queue, no race. A bot on a *real* backend is never moved to
//! a synthesised fake; such a script is refused (see Honesty below).
//!
//! # Honesty
//!
//! A script is only worth anything if it is honoured exactly or refused loudly,
//! so **every** problem is reported at load time — with the offending turn index
//! and bot name — and nothing is pushed until the whole file validates. Unknown
//! bot handles, bots on a backend that is not a `fake` (scripting a real model is
//! not a thing this does), turns with nothing to send, unknown fields (a typo
//! like `tool_call` must not silently do nothing), and calls to a tool the swarm
//! does not register (named with its closest real tools) all fail the load. This
//! module never edits `swarmbot-llm`: it converts into the existing
//! [`ScriptedTurn`] builders and pushes them through the registry's scriptable
//! `fake(id)` handles, exactly as `demo` does.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use swarmbot_contracts::bot::{Bot, normalize_name};
use swarmbot_contracts::config::BackendKind;
use swarmbot_core::swarm::Swarm;
use swarmbot_llm::ScriptedTurn;

/// The whole file: an ordered list of turns. `deny_unknown_fields` is what turns
/// a typo (`[[turn]]`, `bots = …`) into a load-time error instead of a script
/// that silently does nothing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptFile {
    /// One entry per model turn, consumed in order.
    #[serde(default)]
    turns: Vec<TurnSpec>,
}

/// One `[[turns]]` entry, as written. Converted to a [`ScriptedTurn`] by
/// [`TurnSpec::to_scripted_turn`]; validated against a live swarm by
/// [`validate_turns`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnSpec {
    /// A roster handle (`lead`, `@Scout`); resolved to that bot's backend id.
    bot: String,
    /// Optional assistant text, streamed in small deltas.
    #[serde(default)]
    text: Option<String>,
    /// Optional reasoning, streamed before the text.
    #[serde(default)]
    reasoning: Option<String>,
    /// Optional tool calls, announced in order. A turn with none ends the round.
    #[serde(default)]
    tool_calls: Vec<ToolCallSpec>,
}

/// One `[[turns.tool_calls]]` entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallSpec {
    /// The tool name; must be one the swarm's registry knows.
    name: String,
    /// Arbitrary TOML arguments, converted to the JSON a tool call carries. An
    /// absent table means "no arguments" (an empty object).
    #[serde(default)]
    arguments: Option<toml::Value>,
}

impl TurnSpec {
    /// Convert into the [`ScriptedTurn`] the fake backend plays. Pure: it only
    /// reshapes TOML into the builder, so a parse test can assert the exact
    /// sequence without a swarm.
    fn to_scripted_turn(&self) -> Result<ScriptedTurn> {
        let mut turn = ScriptedTurn::default();
        if let Some(reasoning) = &self.reasoning {
            turn = turn.with_reasoning(reasoning.clone());
        }
        if let Some(text) = &self.text {
            // `text` is a public field of the builder; there is no `with_text`.
            turn.text = Some(text.clone());
        }
        for call in &self.tool_calls {
            let arguments = match &call.arguments {
                Some(value) => serde_json::to_value(value).with_context(|| {
                    format!("the arguments of `{}` cannot be read as JSON", call.name)
                })?,
                None => serde_json::json!({}),
            };
            turn = turn.with_tool_call(call.name.clone(), arguments);
        }
        Ok(turn)
    }
}

/// A parsed turn paired with the roster handle it named, so validation and
/// pushing can both see who it belongs to. The [`ScriptedTurn`] is the value the
/// fake backend will play.
#[derive(Debug)]
pub struct ParsedTurn {
    /// The `bot` handle exactly as written (before normalization), for messages.
    pub bot: String,
    /// The converted turn.
    pub turn: ScriptedTurn,
}

/// Parse a script into its turns, in file order. This is the half that needs no
/// swarm: it deserializes (so `deny_unknown_fields` and TOML syntax errors
/// surface here, with the parser's own line numbers) and converts each turn into
/// a [`ScriptedTurn`].
pub fn parse(text: &str) -> Result<Vec<ParsedTurn>> {
    let file: ScriptFile = toml::from_str(text)?;
    let mut parsed = Vec::with_capacity(file.turns.len());
    for spec in &file.turns {
        parsed.push(ParsedTurn {
            bot: spec.bot.clone(),
            turn: spec.to_scripted_turn()?,
        });
    }
    Ok(parsed)
}

/// Load `path`, validate every turn against `swarm`, and push the turns into the
/// fake backends they resolve to. Returns the number of turns pushed.
///
/// Fails loudly and pushes **nothing** if any turn is invalid: a script that is
/// half-applied would leave a run in a state nobody asked for, which is exactly
/// the silent wrongness this module exists to prevent.
pub fn load(swarm: &Swarm, path: &Path) -> Result<usize> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the script file {}", path.display()))?;
    load_str(swarm, &text, path)
}

/// [`load`] from an already-read string, with `origin` named in any error. Split
/// out so the validation is testable without touching the filesystem.
pub fn load_str(swarm: &Swarm, text: &str, origin: &Path) -> Result<usize> {
    let turns = parse(text)
        .with_context(|| format!("cannot parse the script file {}", origin.display()))?;
    let bots = swarm
        .bots
        .list()
        .map_err(|err| anyhow!("{err}"))
        .context("cannot read the roster to check the script")?;
    let problems = validate_turns(swarm, &bots, &turns);
    if !problems.is_empty() {
        return Err(invalid(origin, &problems));
    }
    push(swarm, &bots, turns)
}

/// Validate every turn against the live roster, the backends and the tool
/// registry, returning **all** problems (empty means the script is valid). Each
/// problem names the turn index and the bot handle, because "the script is
/// invalid" without a location sends an operator hunting.
fn validate_turns(swarm: &Swarm, bots: &[Bot], turns: &[ParsedTurn]) -> Vec<String> {
    let registry = swarm.backends.registry();
    let roster: Vec<String> = bots.iter().map(|bot| format!("@{}", bot.name)).collect();
    let roster_list = if roster.is_empty() {
        "(the roster is empty — a team is provisioned when the brain boots)".to_string()
    } else {
        roster.join(", ")
    };

    let mut problems = Vec::new();
    for (index, parsed) in turns.iter().enumerate() {
        let handle = parsed.bot.as_str();
        let normalized = normalize_name(handle);
        match bots.iter().find(|bot| bot.name == normalized) {
            None => problems.push(format!(
                "turn {index} (bot \"{handle}\"): there is no bot called @{normalized} — the \
                 roster has {roster_list}"
            )),
            Some(bot) => {
                // The registry's `fake(id)` handle is Some exactly for backends
                // of kind `fake`; anything else cannot be scripted from here.
                if registry.fake(&bot.backend).is_none() {
                    let kind = swarm
                        .backends
                        .kind(&bot.backend)
                        .map(backend_kind_label)
                        .unwrap_or("unregistered");
                    problems.push(format!(
                        "turn {index} (bot \"{handle}\"): its backend \"{}\" is kind {kind}, not \
                         `fake` — this command scripts only the deterministic `fake` backend, \
                         never a real model",
                        bot.backend
                    ));
                }
            }
        }

        // A turn with nothing to send produces an empty round: neither an answer
        // nor a tool call, so the runner would just ask again. Refuse it.
        if parsed.turn.text.is_none() && parsed.turn.tool_calls.is_empty() {
            problems.push(format!(
                "turn {index} (bot \"{handle}\"): a turn needs `text` or at least one \
                 `[[turns.tool_calls]]` — there is nothing for the model to say or do"
            ));
        }

        for (call_index, call) in parsed.turn.tool_calls.iter().enumerate() {
            if swarm.tools.get(&call.name).is_none() {
                problems.push(format!(
                    "turn {index} (bot \"{handle}\"), tool call {call_index}: `{}` is not a tool \
                     this swarm registers{}",
                    call.name,
                    suggest(&call.name, &swarm.tools.tool_names())
                ));
            }
        }
    }
    problems
}

/// Push every turn into the fake backend its bot resolves to, in file order, so
/// each backend's FIFO queue plays the turns the script wrote for it. Only called
/// after [`validate_turns`] passed, so the lookups here cannot fail — but they
/// return errors rather than panic, because a swarm that changed underneath us is
/// still not a reason to take a bot's mind down.
fn push(swarm: &Swarm, bots: &[Bot], turns: Vec<ParsedTurn>) -> Result<usize> {
    let registry = swarm.backends.registry();
    let mut count = 0usize;
    for parsed in turns {
        let normalized = normalize_name(&parsed.bot);
        let bot = bots
            .iter()
            .find(|bot| bot.name == normalized)
            .ok_or_else(|| anyhow!("there is no bot called @{normalized}"))?;
        let fake = registry
            .fake(&bot.backend)
            .ok_or_else(|| anyhow!("backend {} is not a scriptable fake", bot.backend))?;
        fake.push_turn(parsed.turn);
        count += 1;
    }
    Ok(count)
}

/// The load-time error that lists every problem, one indented line each. A
/// multi-line message is right here (unlike the one-line refusals in `remote`):
/// the point is that an operator sees all of what is wrong in one pass, not one
/// problem per re-run.
fn invalid(origin: &Path, problems: &[String]) -> anyhow::Error {
    let mut message = format!(
        "the script {} cannot be honoured ({} problem(s)):",
        origin.display(),
        problems.len()
    );
    for problem in problems {
        message.push_str("\n  ");
        message.push_str(problem);
    }
    anyhow!(message)
}

/// The wire name of a backend kind — the same spelling `config show` and the
/// remote `backends` list use, so a refusal reads alike whichever way it came.
fn backend_kind_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::OpenAiCompat => "open_ai_compat",
        BackendKind::ClaudeCli => "claude_cli",
        BackendKind::CodexAppServer => "codex_app_server",
        BackendKind::Acp => "acp",
        BackendKind::Fake => "fake",
    }
}

/// " — did you mean `handoff_task`?" for a mistyped tool name, or an empty
/// string when nothing registered is close enough to guess. Ranks the registry's
/// own tool names by character-level similarity (`similar`), so a suggestion is
/// never a hardcoded list that drifts from the tools that actually exist.
fn suggest(name: &str, tools: &[String]) -> String {
    let mut scored: Vec<(f32, &String)> = tools
        .iter()
        .map(|tool| (similar::TextDiff::from_chars(name, tool).ratio(), tool))
        .filter(|(ratio, _)| *ratio >= 0.5)
        .collect();
    if scored.is_empty() {
        return String::new();
    }
    // Highest ratio first; ties broken by name so the suggestion is stable
    // instead of depending on registry order.
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let names: Vec<String> = scored
        .iter()
        .take(3)
        .map(|(_, tool)| format!("`{tool}`"))
        .collect();
    let list = match names.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [head @ .., last] => format!("{} or {last}", head.join(", ")),
    };
    format!(" — did you mean {list}?")
}

/// `--script` drives an **in-process** brain's `fake` backends by pushing turns
/// into their queues. A daemon's backends live in the daemon's process, so there
/// is nothing here to script — refused in one honest line rather than silently
/// ignored (which would leave the operator watching an unscripted fake answer
/// with placeholders and wondering why their file did nothing).
pub fn brain_refusal(url: &str) -> anyhow::Error {
    anyhow!(
        "`--script` pushes turns into an in-process brain's `fake` backends — the CLI cannot \
         script a daemon's in-process backends at {url}; drop --brain to script a run here"
    )
}

/// `demo` already scripts its own four-bot delegation story. Accepting a second
/// script and ignoring it is how an operator loses an afternoon, so it is refused
/// in one line that says what the demo does instead.
pub fn demo_refusal(path: &Path) -> anyhow::Error {
    anyhow!(
        "`demo` scripts its own four-bot delegation story and cannot also load {} — drop \
         --script, or use `swarmbot run … --backend fake --script {}` to drive your own",
        path.display(),
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid script yields the expected `ScriptedTurn` sequence, in order,
    /// grouped by the bot each turn named — asserted on the converted builders
    /// (their fields are public), so a drift in the conversion fails here rather
    /// than as a confusing run.
    #[test]
    fn a_valid_script_parses_into_the_expected_turns_in_order() {
        let text = r#"
[[turns]]
bot = "lead"
reasoning = "the owner wants a verdict"
text = "On it — handing the search to scout."
[[turns.tool_calls]]
name = "handoff_task"
arguments = { to = "scout", title = "find what changed", task = "list the files" }

[[turns]]
bot = "lead"
text = "I have asked scout."

[[turns]]
bot = "scout"
[[turns.tool_calls]]
name = "message_bot"
arguments = { to = "lead", text = "early read" }
"#;
        let parsed = parse(text).expect("a valid script parses");
        assert_eq!(parsed.len(), 3, "three turns, in file order");

        // Turn 0: lead, reasoning + text + one tool call.
        assert_eq!(parsed[0].bot, "lead");
        assert_eq!(
            parsed[0].turn.reasoning.as_deref(),
            Some("the owner wants a verdict")
        );
        assert_eq!(
            parsed[0].turn.text.as_deref(),
            Some("On it — handing the search to scout.")
        );
        assert_eq!(parsed[0].turn.tool_calls.len(), 1);
        assert_eq!(parsed[0].turn.tool_calls[0].name, "handoff_task");
        assert_eq!(
            parsed[0].turn.tool_calls[0].arguments,
            serde_json::json!({
                "to": "scout",
                "title": "find what changed",
                "task": "list the files"
            })
        );

        // Turn 1: lead, text only, no tool calls — this is what ends a round.
        assert_eq!(parsed[1].bot, "lead");
        assert_eq!(parsed[1].turn.text.as_deref(), Some("I have asked scout."));
        assert!(parsed[1].turn.reasoning.is_none());
        assert!(parsed[1].turn.tool_calls.is_empty());

        // Turn 2: scout, no text, one tool call, absent arguments default to {}.
        assert_eq!(parsed[2].bot, "scout");
        assert!(parsed[2].turn.text.is_none());
        assert_eq!(parsed[2].turn.tool_calls.len(), 1);
        assert_eq!(parsed[2].turn.tool_calls[0].name, "message_bot");
        assert_eq!(
            parsed[2].turn.tool_calls[0].arguments,
            serde_json::json!({ "to": "lead", "text": "early read" })
        );

        // A tool call with no `arguments` at all becomes an empty object.
        let bare = parse("[[turns]]\nbot=\"lead\"\n[[turns.tool_calls]]\nname=\"list_bots\"\n")
            .expect("parses");
        assert_eq!(bare[0].turn.tool_calls[0].arguments, serde_json::json!({}));
    }

    /// `deny_unknown_fields` is the guard against a typo silently doing nothing:
    /// `tool_call` (singular) must be a load-time error that names the field and
    /// the parser's line, not a turn that quietly drops its only tool call.
    #[test]
    fn an_unknown_field_is_refused_not_ignored() {
        // A singular `tool_call` on a turn.
        let err = parse(
            "[[turns]]\nbot = \"lead\"\ntext = \"x\"\n[[turns.tool_call]]\nname = \"shell\"\n",
        )
        .expect_err("a typo must not parse");
        let text = format!("{err:#}");
        assert!(
            text.contains("tool_call") || text.contains("unknown field"),
            "the typo is named: {text}"
        );

        // An unknown key directly on a turn.
        let err = parse("[[turns]]\nbot = \"lead\"\ntextt = \"x\"\n").expect_err("typo");
        let text = format!("{err:#}");
        assert!(
            text.contains("textt") || text.contains("unknown field"),
            "{text}"
        );

        // An unknown key on a tool call.
        let err = parse("[[turns]]\nbot=\"lead\"\n[[turns.tool_calls]]\nname=\"shell\"\nargz={}\n")
            .expect_err("typo");
        let text = format!("{err:#}");
        assert!(
            text.contains("argz") || text.contains("unknown field"),
            "{text}"
        );
    }

    /// A malformed TOML document reports the parser's own location rather than a
    /// bare "invalid script", so an operator can find the line.
    #[test]
    fn a_syntax_error_keeps_the_toml_line() {
        let err = parse("[[turns]]\nbot = \n").expect_err("broken toml");
        let text = format!("{err:#}");
        assert!(text.contains("line"), "the TOML line survives: {text}");
    }

    /// `suggest` ranks the registry's real names, so a near-miss is corrected and
    /// a wild guess offers nothing (rather than a misleading "did you mean").
    #[test]
    fn suggestions_name_the_closest_real_tools() {
        let tools: Vec<String> = ["handoff_task", "message_bot", "start_task", "shell"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let hint = suggest("handoff_taks", &tools);
        assert!(hint.contains("handoff_task"), "closest match: {hint}");
        assert!(hint.contains("did you mean"), "{hint}");
        // Nothing close: no guess is better than a wrong one.
        assert!(suggest("zzzzzzzzzzz", &tools).is_empty());
    }

    // ---- swarm-backed tests: validation, the shipped example, the hop limit ----

    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use swarmbot_contracts::config::{BackendSpec, BrainConfig};
    use swarmbot_contracts::entry::{Author, EntryKind};
    use swarmbot_contracts::task::TaskKind;

    use crate::demo::demo_config;
    use crate::embedded::{BrainParams, Embedded};
    use crate::render::{Printer, Theme};
    use crate::tmp::TempDir;
    use crate::{RunOnce, run_once};

    /// An in-memory brain on `config`, booted (team provisioned, runners up).
    fn embedded_with(dir: &Path, config: BrainConfig) -> Embedded {
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

    /// The demo team with every bot on the single always-present `fake`
    /// backend — what `run … --backend fake` resolves to *before* a
    /// `--script` upgrades the shared queue (see [`scripted_run_config`]).
    /// The validation tests use it directly: the five load-time refusals read
    /// the roster and the registry, which look the same whichever layout the
    /// fakes have.
    fn shared_fake_config() -> BrainConfig {
        BrainConfig {
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        }
    }

    /// The config a scripted run really gets: the shared-`fake` resolution
    /// upgraded by `config::per_bot_fakes_for_script` exactly the way
    /// `cmd_run`/`cmd_chat` upgrade it *before* composing — one fake per
    /// scripted bot (`fake-lead`, `fake-scout`, …, the demo's idiom), so no
    /// scheduler can let one bot pop a turn queued for another.
    fn scripted_run_config(script: &Path) -> BrainConfig {
        let mut config = shared_fake_config();
        crate::config::per_bot_fakes_for_script(&mut config, script);
        config
    }

    fn team_group(swarm: &Swarm) -> swarmbot_contracts::conversation::Conversation {
        swarm
            .conversations
            .find_by_title("Team")
            .expect("the roster reads")
            .expect("the demo team has its group")
    }

    /// The path of the shipped example, resolved from this crate's manifest dir so
    /// the test does not depend on the working directory `cargo test` happens to
    /// use.
    fn shipped_example() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/demo-script.example.toml")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_bot_handle_is_reported_with_the_roster() {
        let dir = TempDir::new("script-unknown-bot").expect("temp");
        let embedded = embedded_with(dir.path(), shared_fake_config());
        let swarm = Arc::clone(&embedded.swarm);
        let err = load_str(
            &swarm,
            "[[turns]]\nbot = \"ghost\"\ntext = \"hi\"\n",
            Path::new("s.toml"),
        )
        .expect_err("an unknown bot must be refused");
        let text = format!("{err:#}");
        assert!(text.contains("turn 0"), "names the index: {text}");
        assert!(text.contains("ghost"), "names the handle: {text}");
        assert!(
            text.contains("@lead") && text.contains("@scout"),
            "lists the bots that exist: {text}"
        );
        embedded.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_on_a_real_backend_cannot_be_scripted() {
        let dir = TempDir::new("script-real-backend").expect("temp");
        // A demo team pointed at an OpenAI-compatible backend. Building the spec
        // dials nothing; no message is ever sent, so no request is made.
        let config = BrainConfig {
            default_backend: "inferhub".into(),
            backends: vec![BackendSpec::openai_compat(
                "inferhub",
                "https://api.example.invalid/v1",
                "SWARMBOT_CLI_TEST_UNSET_KEY",
                "m",
            )],
            team: Some(BrainConfig::demo_team("inferhub")),
            ..BrainConfig::default()
        };
        let embedded = embedded_with(dir.path(), config);
        let swarm = Arc::clone(&embedded.swarm);
        let err = load_str(
            &swarm,
            "[[turns]]\nbot = \"lead\"\ntext = \"hi\"\n",
            Path::new("s.toml"),
        )
        .expect_err("a real model cannot be scripted");
        let text = format!("{err:#}");
        assert!(text.contains("turn 0"), "{text}");
        assert!(text.contains("lead"), "names the bot: {text}");
        assert!(text.contains("inferhub"), "names the backend: {text}");
        assert!(text.contains("open_ai_compat"), "names its kind: {text}");
        embedded.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_with_nothing_to_send_is_refused() {
        let dir = TempDir::new("script-empty-turn").expect("temp");
        let embedded = embedded_with(dir.path(), shared_fake_config());
        let swarm = Arc::clone(&embedded.swarm);
        // Reasoning alone is neither text nor a tool call: the round would emit
        // nothing and the runner would just ask again.
        let err = load_str(
            &swarm,
            "[[turns]]\nbot = \"lead\"\nreasoning = \"just thinking\"\n",
            Path::new("s.toml"),
        )
        .expect_err("an empty turn must be refused");
        let text = format!("{err:#}");
        assert!(text.contains("turn 0"), "{text}");
        assert!(text.contains("lead"), "{text}");
        assert!(
            text.contains("text") && text.contains("tool_calls"),
            "says what a turn needs: {text}"
        );
        embedded.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_tool_is_refused_with_a_suggestion_and_a_real_one_passes() {
        let dir = TempDir::new("script-unknown-tool").expect("temp");
        let embedded = embedded_with(dir.path(), shared_fake_config());
        let swarm = Arc::clone(&embedded.swarm);

        // A near-miss tool name is refused with the closest real tool named —
        // otherwise the call would fail deep inside a turn and look like a bug.
        let err = load_str(
            &swarm,
            "[[turns]]\nbot = \"lead\"\n[[turns.tool_calls]]\nname = \"handoff_taks\"\n",
            Path::new("s.toml"),
        )
        .expect_err("an unregistered tool must be refused");
        let text = format!("{err:#}");
        assert!(text.contains("turn 0"), "{text}");
        assert!(text.contains("lead"), "{text}");
        assert!(text.contains("handoff_taks"), "names the typo: {text}");
        assert!(
            text.contains("handoff_task") && text.contains("did you mean"),
            "suggests the real tool: {text}"
        );

        // The same turn with the real tool name loads and pushes cleanly.
        let pushed = load_str(
            &swarm,
            "[[turns]]\nbot = \"lead\"\ntext = \"delegating\"\n[[turns.tool_calls]]\nname = \
             \"handoff_task\"\narguments = { to = \"scout\", title = \"t\", task = \"x\" }\n",
            Path::new("s.toml"),
        )
        .expect("a registered tool passes");
        assert_eq!(pushed, 1, "one turn pushed");
        embedded.shutdown().await;
    }

    /// Every problem is reported at once, each with its index and bot, so an
    /// operator fixes the whole file in one pass instead of one error per re-run.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_problem_is_reported_in_one_pass() {
        let dir = TempDir::new("script-all-problems").expect("temp");
        let embedded = embedded_with(dir.path(), shared_fake_config());
        let swarm = Arc::clone(&embedded.swarm);
        let script = "\
[[turns]]
bot = \"ghost\"
text = \"nobody\"
[[turns]]
bot = \"lead\"
[[turns]]
bot = \"scout\"
[[turns.tool_calls]]
name = \"frobnicate\"
";
        let err = load_str(&swarm, script, Path::new("s.toml")).expect_err("three problems");
        let text = format!("{err:#}");
        assert!(text.contains("3 problem(s)"), "counts them: {text}");
        assert!(text.contains("turn 0") && text.contains("ghost"), "{text}");
        assert!(text.contains("turn 1") && text.contains("lead"), "{text}");
        assert!(
            text.contains("turn 2") && text.contains("frobnicate"),
            "{text}"
        );
        embedded.shutdown().await;
    }

    /// A loaded script fills the fake's queue, which is exactly the condition the
    /// unscripted-`fake` notice checks — so loading one silences the notice, and
    /// not loading one keeps it. This is the observable half of point 3.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_loaded_script_silences_the_unscripted_fake_notice() {
        let dir = TempDir::new("script-notice").expect("temp");
        let embedded = embedded_with(dir.path(), shared_fake_config());
        let swarm = Arc::clone(&embedded.swarm);
        let group = team_group(&swarm);

        // Nothing scripted yet: the placeholder is what the next turn answers.
        assert!(
            crate::notice::embedded_unscripted_fake(&swarm, &group.id),
            "an unscripted fake owes the notice"
        );
        // Load one turn; the shared fake's queue is now non-empty.
        load_str(
            &swarm,
            "[[turns]]\nbot = \"lead\"\ntext = \"looking now\"\n",
            Path::new("s.toml"),
        )
        .expect("loads");
        assert!(
            !crate::notice::embedded_unscripted_fake(&swarm, &group.id),
            "a loaded script silences the notice"
        );
        embedded.shutdown().await;
    }

    /// The acceptance test for the whole feature: resolve the config exactly
    /// the way `run … --backend fake --script …` does (the shared `fake`
    /// upgraded to per-bot fakes *before* composition), load the *shipped*
    /// example, send the owner's message, and assert the delegation really
    /// happened — a `handoff` event, a `task-result` event, the lead's final
    /// verdict, a `Task` of kind `Handoff`, and a `delegated:` summary that
    /// names both delegations. Modelled on `core/tests/delegation.rs`.
    ///
    /// Multi-threaded on purpose: this is the regression test for the
    /// scheduling race the per-bot split fixes. With one shared `fake` queue a
    /// bot woken mid-delegation could pop a turn scripted for another bot —
    /// before the split, 7 of 25 full-suite runs on a 4-core box failed
    /// exactly that way (the lead's group round answered with its own DM turn,
    /// "Noted — I will fold scout's early read into the verdict."). One queue
    /// per scripted bot removes the window under any scheduler. If this test
    /// ever goes flaky, the split regressed — pinning the runtime back to
    /// current-thread would hide the race instead of fixing it, so do not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_shipped_example_runs_a_real_delegation_end_to_end() {
        let dir = TempDir::new("script-e2e").expect("temp");
        let example = shipped_example();
        let embedded = embedded_with(dir.path(), scripted_run_config(&example));
        let swarm = Arc::clone(&embedded.swarm);
        let group = team_group(&swarm);

        // The split really happened on the way here: the two scripted bots are
        // on their own fakes; dev and sentinel, whom the script never names,
        // stay on the shared one.
        let roster = swarm.bots.list().expect("roster");
        let backend_of = |name: &str| {
            roster
                .iter()
                .find(|bot| bot.name == name)
                .map(|bot| bot.backend.clone())
                .unwrap_or_default()
        };
        assert_eq!(backend_of("lead"), "fake-lead");
        assert_eq!(backend_of("scout"), "fake-scout");
        assert_eq!(backend_of("dev"), "fake", "an unscripted bot is not moved");

        let pushed = load(&swarm, &example).expect("the shipped example loads");
        assert_eq!(pushed, 9, "the whole nine-turn script was pushed: {pushed}");
        // A loaded script silences the notice, asserted on the real file and
        // on the per-bot layout: lead's and scout's queues are filled, and the
        // conversation's fakes are therefore not all empty.
        assert!(
            !crate::notice::embedded_unscripted_fake(&swarm, &group.id),
            "the shipped example silences the unscripted-fake notice"
        );

        let mut buffer = Vec::new();
        let code = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            run_once(
                &embedded,
                &RunOnce {
                    text: "find out what changed under src/ and give me a verdict",
                    conversation: group.id.clone(),
                    wait: Duration::from_secs(30),
                    json: false,
                },
                &mut printer,
            )
            .await
            .expect("runs")
        };
        assert_eq!(code, 0, "an idle team exits 0");

        let entries = swarm.store.entry_tail(&group.id, 500).expect("entries");
        // A handoff event and a task-result event, produced by the orchestration.
        assert!(
            entries.iter().any(|entry| matches!(&entry.kind,
                EntryKind::Event(event) if event.event == "handoff")),
            "a handoff event is in the group transcript"
        );
        assert!(
            entries.iter().any(|entry| matches!(&entry.kind,
                EntryKind::Event(event) if event.event == "task-result")),
            "a task-result event is in the group transcript"
        );
        // The lead's final verdict is the last bot message in the group.
        let final_reply = entries
            .iter()
            .rev()
            .find(|entry| {
                matches!(entry.author, Author::Bot { .. })
                    && entry.kind.text().is_some_and(|text| !text.is_empty())
            })
            .and_then(|entry| entry.kind.text());
        assert!(
            final_reply.is_some_and(|text| text.contains("Verdict: safe to keep")),
            "the lead's final answer is the verdict: {final_reply:?}"
        );

        // A Task of kind Handoff really exists (the delegation is not a claim).
        let tasks = swarm.tasks.in_conversation(&group.id).expect("tasks");
        assert!(
            tasks
                .iter()
                .any(|task| matches!(task.kind, TaskKind::Handoff { .. })),
            "a handoff task exists: {tasks:?}"
        );

        // The delegation summary the CLI prints names both delegations.
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("delegated:"), "{out}");
        assert!(
            out.contains("@lead → @scout \"find what changed under src/\""),
            "the handoff is summarized: {out}"
        );
        assert!(
            out.contains("@lead → @lead \"write the final verdict\""),
            "the lead's own task is summarized: {out}"
        );
        embedded.shutdown().await;
    }

    /// The proof that the split separates the queues: a script whose lead and
    /// scout turns would be ambiguous in one shared FIFO — the lead's first
    /// turn hands off to the scout, the scout's first turn messages the lead
    /// back, so the lead is woken twice (its DM, then the group's task result)
    /// while the scout is mid-task. That crossing is exactly what popped the
    /// wrong turn under the shared queue. Each bot must consume **its own**
    /// turns: asserted per backend id on `FakeBackend::requests()` (only the
    /// bot that owns a backend ever asked it, once per scripted turn — the
    /// persona in the request instructions says who asked) and on the
    /// resulting transcript (every reply landed where the script put it).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn each_scripted_bot_consumes_its_own_queue() {
        let dir = TempDir::new("script-own-queue").expect("temp");
        let script_path = dir.path().join("story.toml");
        std::fs::write(
            &script_path,
            r#"
[[turns]]
bot = "lead"
text = "LEAD-GROUP-ONE"
[[turns.tool_calls]]
name = "handoff_task"
arguments = { to = "scout", title = "probe the workspace", task = "look and report" }

[[turns]]
bot = "lead"
text = "LEAD-GROUP-TWO"

[[turns]]
bot = "scout"
[[turns.tool_calls]]
name = "message_bot"
arguments = { to = "lead", text = "SCOUT-EARLY" }

[[turns]]
bot = "scout"
text = "SCOUT-RESULT"

[[turns]]
bot = "lead"
text = "LEAD-DM-ACK"

[[turns]]
bot = "lead"
text = "LEAD-GROUP-FINAL"
"#,
        )
        .expect("the script file writes");
        let embedded = embedded_with(dir.path(), scripted_run_config(&script_path));
        let swarm = Arc::clone(&embedded.swarm);
        let group = team_group(&swarm);

        let pushed = load(&swarm, &script_path).expect("the script loads");
        assert_eq!(pushed, 6, "every turn was pushed");
        // The turns landed in the per-bot queues, and the shared fake — which
        // dev and sentinel stay on — got none.
        let lead_fake = swarm
            .backends
            .registry()
            .fake("fake-lead")
            .expect("the lead's own fake");
        let scout_fake = swarm
            .backends
            .registry()
            .fake("fake-scout")
            .expect("the scout's own fake");
        let shared_fake = swarm
            .backends
            .registry()
            .fake("fake")
            .expect("the shared fake is always registered");
        assert_eq!(lead_fake.queued_turns(), 4, "the lead's four turns");
        assert_eq!(scout_fake.queued_turns(), 2, "the scout's two turns");
        assert_eq!(
            shared_fake.queued_turns(),
            0,
            "nothing was queued for dev or sentinel"
        );

        let mut buffer = Vec::new();
        let code = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            run_once(
                &embedded,
                &RunOnce {
                    text: "probe the workspace and report back",
                    conversation: group.id.clone(),
                    wait: Duration::from_secs(30),
                    json: false,
                },
                &mut printer,
            )
            .await
            .expect("runs")
        };
        assert_eq!(code, 0, "an idle team exits 0");

        // Per backend: exactly the scripted number of requests, all carrying
        // the owning bot's persona — a crossed queue would show up as a wrong
        // count or a foreign persona, and a shared queue as requests on `fake`.
        let lead_requests = lead_fake.requests();
        assert_eq!(
            lead_requests.len(),
            4,
            "the lead consumed its own four turns and asked nobody else's backend"
        );
        assert!(
            lead_requests
                .iter()
                .all(|request| request.instructions.contains("You lead a small team")),
            "only the lead ever asked fake-lead"
        );
        let scout_requests = scout_fake.requests();
        assert_eq!(
            scout_requests.len(),
            2,
            "the scout consumed its own two turns"
        );
        assert!(
            scout_requests
                .iter()
                .all(|request| request.instructions.contains("You explore")),
            "only the scout ever asked fake-scout"
        );
        assert!(
            shared_fake.requests().is_empty(),
            "no scripted turn was ever served through the shared queue"
        );
        assert_eq!(lead_fake.queued_turns(), 0, "the lead's queue drained");
        assert_eq!(scout_fake.queued_turns(), 0, "the scout's queue drained");

        // The transcript: each scripted reply landed where the script put it.
        // The crossing the shared queue allowed showed up as the lead's group
        // round answering with its own DM turn.
        let entries = swarm.store.entry_tail(&group.id, 500).expect("entries");
        let last_group_reply = entries
            .iter()
            .rev()
            .find(|entry| {
                matches!(entry.author, Author::Bot { .. })
                    && entry.kind.text().is_some_and(|text| !text.is_empty())
            })
            .and_then(|entry| entry.kind.text());
        assert_eq!(
            last_group_reply,
            Some("LEAD-GROUP-FINAL"),
            "the lead's group round consumed its own group turn"
        );

        let lead = swarm.bots.by_name("lead").expect("lead");
        let dm = swarm
            .conversations
            .tail(&lead.direct_conversation_id, 200)
            .expect("lead dm");
        assert!(
            dm.iter()
                .any(|entry| entry.kind.text() == Some("SCOUT-EARLY")),
            "scout's message_bot reached the lead's DM"
        );
        assert!(
            dm.iter()
                .any(|entry| matches!(entry.author, Author::Bot { .. })
                    && entry.kind.text() == Some("LEAD-DM-ACK")),
            "the lead's DM round consumed its own DM turn"
        );

        // The scout's own queue served the scout's task: its finding is the
        // handoff task's result.
        let tasks = swarm.tasks.in_conversation(&group.id).expect("tasks");
        let handoff = tasks
            .iter()
            .find(|task| matches!(task.kind, TaskKind::Handoff { .. }))
            .expect("the handoff task");
        assert_eq!(
            handoff.result.as_deref(),
            Some("SCOUT-RESULT"),
            "the scout's second turn became its task result"
        );
        embedded.shutdown().await;
    }

    /// Synthesising per-bot fakes must never replace a real model: a team on
    /// `open_ai_compat` stays on it through the config-resolution split (two
    /// scripted bots sharing one backend is exactly the shape that splits a
    /// `fake`), and the script is still refused with the existing wording.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_script_on_a_real_backend_is_refused_and_never_split() {
        let dir = TempDir::new("script-real-no-split").expect("temp");
        let script_path = dir.path().join("s.toml");
        std::fs::write(
            &script_path,
            "[[turns]]\nbot = \"lead\"\ntext = \"hi\"\n\n[[turns]]\nbot = \"scout\"\ntext = \"ho\"\n",
        )
        .expect("writes");
        // Building the spec dials nothing; no message is ever sent.
        let mut config = BrainConfig {
            default_backend: "inferhub".into(),
            backends: vec![BackendSpec::openai_compat(
                "inferhub",
                "https://api.example.invalid/v1",
                "SWARMBOT_CLI_TEST_UNSET_KEY",
                "m",
            )],
            team: Some(BrainConfig::demo_team("inferhub")),
            ..BrainConfig::default()
        };
        crate::config::per_bot_fakes_for_script(&mut config, &script_path);
        // Nothing synthesised: the backend list is exactly the real one, and
        // every bot still names it.
        assert_eq!(config.backends.len(), 1);
        assert_eq!(config.backends[0].id, "inferhub");
        let team = config.team.as_ref().expect("a team");
        assert!(
            team.bots
                .iter()
                .all(|bot| bot.backend.as_deref() == Some("inferhub")),
            "no bot was moved onto a synthesised fake"
        );

        let embedded = embedded_with(dir.path(), config);
        let swarm = Arc::clone(&embedded.swarm);
        let err = load(&swarm, &script_path).expect_err("a real model cannot be scripted");
        let text = format!("{err:#}");
        assert!(text.contains("2 problem(s)"), "{text}");
        assert!(text.contains("turn 0") && text.contains("lead"), "{text}");
        assert!(text.contains("turn 1") && text.contains("scout"), "{text}");
        assert!(text.contains("inferhub"), "names the backend: {text}");
        assert!(text.contains("open_ai_compat"), "names its kind: {text}");
        assert!(
            text.contains("never a real model"),
            "the existing refusal wording survives the split: {text}"
        );
        embedded.shutdown().await;
    }

    /// The hop limit, demonstrable from a script: two bots `message_bot` each
    /// other more times than `max_hops` allows, and the run must terminate with
    /// "too many hand-offs" in the transcript and nothing left queued to continue
    /// the loop. Per-bot fake backends (as `demo` uses) keep each role's queue
    /// independent, so the volley is deterministic. This is the part of the
    /// milestone `run` had never covered.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_hop_limit_is_demonstrable_from_a_script() {
        let dir = TempDir::new("script-hop-limit").expect("temp");
        // demo_config gives each bot its own fake (fake-lead, fake-scout, …).
        let embedded = embedded_with(dir.path(), demo_config());
        let swarm = Arc::clone(&embedded.swarm);

        // A small limit makes the bound itself the thing under test; the script
        // offers far more volleys than the limit allows.
        swarm
            .settings
            .update(|settings| settings.max_hops = 3)
            .expect("settings update");
        let limit = swarm.settings.get().max_hops;

        // Build the volley script: each bot messages the other, then ends the
        // round with a text-only turn (a tool-call turn alone would ask again).
        let volleys = (limit as usize) + 5;
        let mut script = String::new();
        for (bot, other) in [("lead", "scout"), ("scout", "lead")] {
            for _ in 0..volleys {
                script.push_str(&format!(
                    "[[turns]]\nbot = \"{bot}\"\n[[turns.tool_calls]]\nname = \"message_bot\"\n\
                     arguments = {{ to = \"{other}\", text = \"your turn\", urgency = \"background\" }}\n\n\
                     [[turns]]\nbot = \"{bot}\"\ntext = \"waiting\"\n\n"
                ));
            }
        }
        let pushed =
            load_str(&swarm, &script, Path::new("hop.toml")).expect("the volley script loads");
        assert_eq!(pushed, volleys * 4, "both bots' volleys were pushed");

        // Start the loop in the lead's DM; the owner's message wakes the lead.
        let lead = swarm.bots.by_name("lead").expect("lead");
        let scout = swarm.bots.by_name("scout").expect("scout");
        swarm
            .send(&lead.direct_conversation_id, "start the loop")
            .expect("sent");
        assert!(
            swarm.wait_for_idle(Duration::from_secs(30)).await,
            "the run terminates instead of oscillating forever"
        );

        // The transcript says the chain was stopped by the hop limit.
        let lead_dm = swarm
            .conversations
            .tail(&lead.direct_conversation_id, 200)
            .expect("lead dm");
        let scout_dm = swarm
            .conversations
            .tail(&scout.direct_conversation_id, 200)
            .expect("scout dm");
        let stopped = |entries: &[swarmbot_contracts::entry::Entry]| {
            entries.iter().any(|entry| {
                entry
                    .kind
                    .text()
                    .is_some_and(|text| text.contains("too many hand-offs"))
            })
        };
        assert!(
            stopped(&lead_dm) || stopped(&scout_dm),
            "the hop limit is written into the transcript"
        );

        // The chain really stopped: no delivery is left queued to continue it.
        let waiting: usize = [&lead.id, &scout.id]
            .iter()
            .map(|bot| {
                swarm
                    .store
                    .queued_deliveries(bot)
                    .map(|deliveries| deliveries.len())
                    .unwrap_or(0)
            })
            .sum();
        assert_eq!(waiting, 0, "nothing is queued to keep the loop going");
        embedded.shutdown().await;
    }
}
