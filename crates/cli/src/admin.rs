//! The owner's **write surface**: the commands that change a brain instead of
//! watching one.
//!
//! `run`, `chat` and the list commands read; these write — saved approval rules,
//! the cards nobody is looking at, the settings, a routine's schedule, a node's
//! pairing, a bot's definition, a transcript on its way out of the machine.
//! Every body here is written against [`Session`], so the embedded brain and a
//! `--brain` daemon answer them through the same code: there is one
//! `requests answer`, one `config set`, one prosaic rendering.
//!
//! Three honesty rules shaped the file:
//!
//! * **a write reports what happened, not what was asked for.** `requests
//!   answer` looks the card up among the *pending* ones first; an id that is
//!   not waiting is refused with what is waiting instead, never answered twice
//!   and never printed as success.
//! * **a refusal keeps the brain's own sentence.** A settings value the brain
//!   rejects (a budget of `0`) is reported with its status and message verbatim
//!   ([`Session::update_settings`]); an unknown key is refused *before* the call,
//!   listing the keys that exist — the same list the brain builds.
//! * **no secret is ever printed.** The rules here are about settings and cards;
//!   key material is `config key`, which prints names only.
//!
//! Every command's `--json` shape follows the existing two: one object per line
//! for a list (`rules ls`, `requests ls`, `routines ls`, `routines runs`), one
//! whole object for a single answer (`config set`, `bots edit`, `nodes revoke`),
//! so a script reads the same thing a human does.
//!
//! # Which mode answers what
//!
//! Every command here is written once against [`Session`] and works in both
//! modes, but a few answers are *about where the brain runs* and cannot be the
//! same in both. Those are stated here rather than discovered:
//!
//! | command | embedded (in-process) | `--brain` (daemon) |
//! | --- | --- | --- |
//! | `rules ls/rm` | the store in this process | `GET`/`DELETE /api/rules` |
//! | `requests ls/answer` | this store's pending rows; the answer wakes the bot here | `GET /api/requests`, `POST …/respond` |
//! | `config set` | `Settings::validate` + `put_settings`, live at once | `PATCH /api/settings`; the brain's own 400 is repeated verbatim |
//! | `routines *` | the scheduler in this process | `/api/routines…` |
//! | `bots edit` | `BotService::update` + `set_enabled` (starts/stops its runner) | `PATCH /api/bots/{id}` |
//! | `nodes revoke` | **refused**: there is no gateway here for a machine to pair with, and the local node cannot be revoked at all | `DELETE /api/nodes/{id}` |
//! | `status` | roster + this store's own counters | roster + `GET /api/metrics` |
//! | `config key *` | reads and writes `<data>/secrets.json` here (0600, atomic) | `GET/PUT/DELETE /api/secrets` on the daemon's own file |
//! | `transcript --before/--out` | pages this store; builds the NDJSON from its rows | `?before_seq=`, `GET …/export` byte for byte |
//!
//! Two of those refusals are the interesting ones. `nodes revoke` cannot work
//! in-process because a node pairs with a *listening* brain and an in-process one
//! has no listener — the same property `nodes pair` already states. And
//! `config key` is a file, so with `--brain` it must edit the daemon's file over
//! the daemon's own route, never a local one that nothing would read.

use std::io::Write;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::Value;
use swarmbot_contracts::bot::{Bot, BotStatus};
use swarmbot_contracts::config::Settings;
use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::id::ConversationId;
use swarmbot_contracts::repo::Persistence;
use swarmbot_contracts::request::{
    ApprovalRule, Decision, RequestKind, RequestStatus, RuleScope, RuntimeRequest,
};
use swarmbot_contracts::routine::{Routine, RoutineRun, RoutineSchedule, RunStatus};
use swarmbot_contracts::task::TaskStatus;

use crate::render::Printer;
use crate::session::Session;

/// How much of a transcript one export page asks for. Large enough that a long
/// conversation is a handful of reads, small enough that a runaway history is
/// never materialised twice.
const EXPORT_PAGE: u32 = 500;

/// What `requests answer` was told to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    /// `--allow-once`, `--always`, `--deny`: an approval decision.
    Decide(Decision),
    /// `--answer <text>`: free text for a question, a form or a hand-off.
    Text(String),
}

impl Answer {
    /// The words the command prints for what it sent.
    fn describe(&self) -> String {
        match self {
            Self::Decide(decision) => decision_label(*decision).to_string(),
            Self::Text(text) => format!("answer {text:?}"),
        }
    }
}

fn decision_label(decision: Decision) -> &'static str {
    match decision {
        Decision::AllowOnce => "allowed once",
        Decision::AllowAlways => "allowed always (a rule was saved)",
        Decision::Deny => "denied",
    }
}

/// The flag set of `requests answer`, turned into one answer — or refused.
///
/// clap keeps the four flags mutually exclusive, so "two of them" cannot reach
/// here; "none of them" can, and it is a refusal rather than an empty answer:
/// an approval settled with no decision reads as a denial downstream, and a
/// human who typed nothing would believe they left the card alone.
pub fn answer_from_flags(
    allow_once: bool,
    always: bool,
    deny: bool,
    answer: Option<&str>,
) -> Result<Answer> {
    if allow_once {
        return Ok(Answer::Decide(Decision::AllowOnce));
    }
    if always {
        return Ok(Answer::Decide(Decision::AllowAlways));
    }
    if deny {
        return Ok(Answer::Decide(Decision::Deny));
    }
    match answer {
        Some(text) if !text.trim().is_empty() => Ok(Answer::Text(text.trim().to_string())),
        Some(_) => Err(anyhow!(
            "`--answer` needs a non-empty answer: an empty one would settle the card with nothing"
        )),
        None => Err(anyhow!(
            "nothing to answer with: pass --allow-once, --always, --deny or --answer <text>"
        )),
    }
}

// --- id resolution -----------------------------------------------------------

/// Resolve a user-typed id against the rows that exist: the full id, or a
/// unique suffix of it.
///
/// Ids are long and share a prefix (`rul_0192…`), so the end is the part a
/// human copies. A partial match that is **not** unique is refused rather than
/// resolved to the first — answering the wrong card is worse than asking for
/// more characters.
pub fn resolve_id<'a, T>(
    raw: &str,
    items: &'a [T],
    id_of: for<'x> fn(&'x T) -> &'x str,
    what: &str,
    hint: &str,
) -> Result<&'a T> {
    let key = raw.trim();
    if key.is_empty() {
        return Err(anyhow!("{what} <id> is required: {hint}"));
    }
    if let Some(exact) = items.iter().find(|item| id_of(item) == key) {
        return Ok(exact);
    }
    let matches: Vec<&T> = items
        .iter()
        .filter(|item| id_of(item).ends_with(key))
        .collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(anyhow!(
            "there is no {what} `{key}` — {hint} (an id can also be shortened to its tail)"
        )),
        n => Err(anyhow!(
            "`{key}` matches {n} {what}s — type more characters"
        )),
    }
}

// --- rules -------------------------------------------------------------------

/// One row per saved rule: what it decides, where it applies, when it was saved.
pub fn rules_lines(rules: &[ApprovalRule]) -> Vec<String> {
    rules.iter().map(rule_line).collect()
}

fn rule_line(rule: &ApprovalRule) -> String {
    let scope = match &rule.scope {
        RuleScope::Global => "global".to_string(),
        RuleScope::Bot(id) => format!("bot {id}"),
    };
    let effect = match rule.effect {
        swarmbot_contracts::request::RuleEffect::Allow => "allow",
        swarmbot_contracts::request::RuleEffect::Ask => "ask",
        swarmbot_contracts::request::RuleEffect::Deny => "deny",
    };
    let mut matchers = Vec::new();
    if let Some(surface) = rule.surface {
        matchers.push(format!("surface={surface}"));
    }
    if let Some(tool) = &rule.tool {
        matchers.push(format!("tool={tool}"));
    }
    if let Some(prefix) = &rule.command_prefix {
        matchers.push(format!("prefix={prefix:?}"));
    }
    let matcher = if matchers.is_empty() {
        "every request in scope".to_string()
    } else {
        matchers.join(" ")
    };
    format!(
        "{} {effect:<5} {scope:<12} {matcher} · saved {}",
        rule.id,
        rule.created_at.to_rfc3339()
    )
}

/// `swarmbot rules ls`
pub fn rules_list<S: Session, W: Write>(
    session: &S,
    json: bool,
    printer: &mut Printer<W>,
) -> Result<i32> {
    let rules = session.rules()?;
    if json {
        for rule in &rules {
            json_line(printer, rule)?;
        }
        return Ok(0);
    }
    if rules.is_empty() {
        printer.note(
            "no approval rules saved — answering a card with `requests answer <id> --always` writes one",
        )?;
    }
    for line in rules_lines(&rules) {
        printer.line(&line)?;
    }
    Ok(0)
}

/// `swarmbot rules rm <id>`
pub fn rules_remove<S: Session, W: Write>(
    session: &S,
    raw: &str,
    json: bool,
    printer: &mut Printer<W>,
) -> Result<i32> {
    let rules = session.rules()?;
    let rule = resolve_id(
        raw,
        &rules,
        |rule| rule.id.as_str(),
        "approval rule",
        "`swarmbot rules ls` lists the saved ones",
    )?
    .clone();
    session.delete_rule(&rule.id)?;
    if json {
        printer.line(&serde_json::to_string(&serde_json::json!({
            "id": rule.id.as_str(),
            "deleted": true,
        }))?)?;
        return Ok(0);
    }
    printer.line(&format!(
        "deleted rule {} ({}) — {}",
        rule.id,
        rule_line(&rule)
            .split_once(' ')
            .map(|(_, rest)| rest)
            .unwrap_or("")
            .trim(),
        // Deleting a rule does not undo a decision already made; saying so
        // stops "I removed the rule" being read as "the command ran again".
        "the next identical ask reaches you again"
    ))?;
    Ok(0)
}

// --- requests ----------------------------------------------------------------

/// The word a card's kind prints as.
fn kind_label(kind: RequestKind) -> &'static str {
    match kind {
        RequestKind::Approval => "approval",
        RequestKind::Question => "question",
        RequestKind::Form => "form",
        RequestKind::Secret => "secret",
        RequestKind::Handoff => "handoff",
    }
}

/// One row per waiting card: id, kind, the bot and the conversation, what it
/// asks, and how long it has left before the expiry ticker settles it.
pub fn requests_lines(
    requests: &[RuntimeRequest],
    bots: &[Bot],
    conversations: &[Conversation],
) -> Vec<String> {
    let now = chrono::Utc::now();
    requests
        .iter()
        .map(|request| {
            let bot = bots
                .iter()
                .find(|bot| bot.id == request.bot_id)
                .map(|bot| format!("@{}", bot.name))
                .unwrap_or_else(|| "@unknown".to_string());
            let where_ = conversations
                .iter()
                .find(|conversation| conversation.id == request.conversation_id)
                .and_then(|conversation| conversation.title.clone())
                .unwrap_or_else(|| short_id(request.conversation_id.as_str()));
            let expiry = match request.expires_at {
                Some(at) => {
                    let seconds = at.signed_duration_since(now).num_seconds().max(0);
                    if seconds >= 60 {
                        format!("expires in {}m", seconds / 60)
                    } else {
                        format!("expires in {seconds}s")
                    }
                }
                None => "no expiry".to_string(),
            };
            format!(
                "{} {:<9} {bot} in \"{where_}\" · {} · {expiry}",
                request.id,
                kind_label(request.kind),
                request.title()
            )
        })
        .collect()
}

/// `swarmbot requests ls`
pub fn requests_list<S: Session, W: Write>(
    session: &S,
    json: bool,
    printer: &mut Printer<W>,
) -> Result<i32> {
    let requests = session.pending()?;
    if json {
        for request in &requests {
            json_line(printer, request)?;
        }
        return Ok(0);
    }
    if requests.is_empty() {
        printer.note("no request is waiting for an answer")?;
        return Ok(0);
    }
    for line in requests_lines(&requests, &session.bots()?, &session.conversations()?) {
        printer.line(&line)?;
    }
    Ok(0)
}

/// What one answer means for the card it answers, or why it cannot be sent.
///
/// A decision answers an approval and free text answers everything else; mixing
/// them up would settle a card with a value its waiter cannot read — an
/// approval answered `--answer "staging"` has no `decision`, which a tool gate
/// treats as not-allowed, and the human who typed it believes they allowed it.
pub fn answer_payload(
    request: &RuntimeRequest,
    answer: &Answer,
) -> Result<(Option<Decision>, Option<Value>)> {
    match (request.kind, answer) {
        (RequestKind::Approval, Answer::Decide(decision)) => Ok((Some(*decision), None)),
        (RequestKind::Approval, Answer::Text(text)) => Err(anyhow!(
            "request {} is an approval: answer it with --allow-once, --always or --deny \
             (the text {text:?} would settle the card with no decision)",
            request.id
        )),
        (kind, Answer::Decide(_)) => Err(anyhow!(
            "request {} is a {} and takes free text: answer it with --answer <text>",
            request.id,
            kind_label(kind)
        )),
        (_, Answer::Text(text)) => Ok((None, Some(Value::String(text.clone())))),
    }
}

/// `swarmbot requests answer <id> --allow-once|--always|--deny|--answer <text>`
///
/// The card is looked up among the **pending** ones before anything is sent, so
/// an id that is already answered/expired is refused with that fact instead of a
/// second answer the brain would reject (and that a careless caller would print
/// as success). The brain's own refusal still wins a race — a card answered
/// between the read and the write is reported by its sentence, never swallowed.
pub fn requests_answer<S: Session, W: Write>(
    session: &S,
    raw: &str,
    answer: &Answer,
    json: bool,
    printer: &mut Printer<W>,
) -> Result<i32> {
    let pending = session.pending()?;
    let request = resolve_id(
        raw,
        &pending,
        |request| request.id.as_str(),
        "pending request",
        "`swarmbot requests ls` shows the cards waiting for an answer",
    )?
    .clone();
    let (decision, payload) = answer_payload(&request, answer)?;
    let settled: RuntimeRequest = session.respond(&request.id, decision, payload)?;
    if settled.status != RequestStatus::Answered {
        // A brain that answers 200 with a still-pending card is not something
        // this CLI will dress up: the card is only settled when it says so.
        return Err(anyhow!(
            "the brain still reports request {} as {} after the answer — nothing was settled",
            settled.id,
            status_label(settled.status)
        ));
    }
    if json {
        json_line(printer, &settled)?;
        return Ok(0);
    }
    printer.line(&format!(
        "request {}: {} ({})",
        settled.id,
        status_label(settled.status),
        answer.describe()
    ))?;
    if settled.decision == Some(Decision::AllowAlways) {
        printer.note("a rule was saved — `swarmbot rules ls` shows it, `rules rm` unsaves it")?;
    }
    Ok(0)
}

fn status_label(status: RequestStatus) -> &'static str {
    match status {
        RequestStatus::Pending => "pending",
        RequestStatus::Answered => "answered",
        RequestStatus::Expired => "expired",
        RequestStatus::Cancelled => "cancelled",
    }
}

// --- settings ----------------------------------------------------------------

/// The JSON value a `config set <key> <text>` names, typed by what the setting
/// already holds.
///
/// The key list and the *shape* of each value come from the type itself
/// (serializing [`Settings::default`]), so this cannot drift from the fields —
/// and a key that does not exist is refused here, before any brain is asked,
/// with the same list the brain's `PATCH /api/settings` prints. Value *rules*
/// (a budget of zero, a fraction out of range) are the brain's to decide: they
/// are validated against the merged settings, in both modes, by
/// `Settings::validate`.
pub fn settings_value(key: &str, raw: &str, current: &Settings) -> Result<Value> {
    let defaults =
        serde_json::to_value(current).context("the settings did not serialize to a JSON value")?;
    let Some(object) = defaults.as_object() else {
        return Err(anyhow!("the settings did not serialize to a JSON object"));
    };
    let Some(current_value) = object.get(key) else {
        return Err(anyhow!(
            "unknown setting `{key}`; known settings are {}",
            crate::config::settings_keys().join(", ")
        ));
    };
    let text = raw.trim();
    let null_word = matches!(text.to_ascii_lowercase().as_str(), "null" | "none" | "off");
    match current_value {
        Value::Bool(_) => match text.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(Value::Bool(true)),
            "false" | "no" | "off" | "0" => Ok(Value::Bool(false)),
            _ => Err(anyhow!("`{key}` is true or false, and {raw:?} is neither")),
        },
        Value::Number(_) => {
            if null_word {
                // `off`/`none`/`null` is the one spelling of "no cap" for the
                // budget fields; for a plain counter the brain's validation
                // refuses null, and its sentence says why.
                return Ok(Value::Null);
            }
            if let Ok(int) = text.parse::<i64>() {
                return Ok(Value::Number(int.into()));
            }
            if let Ok(float) = text.parse::<f64>()
                && float.is_finite()
            {
                return Ok(serde_json::Number::from_f64(float)
                    .map(Value::Number)
                    .unwrap_or(Value::Null));
            }
            Err(anyhow!("`{key}` is a number, and {raw:?} is not one"))
        }
        // A null default is an *optional* value: either kind may be intended.
        Value::Null => {
            if null_word {
                return Ok(Value::Null);
            }
            if let Ok(int) = text.parse::<i64>() {
                return Ok(Value::Number(int.into()));
            }
            if let Ok(float) = text.parse::<f64>()
                && float.is_finite()
            {
                return Ok(serde_json::Number::from_f64(float)
                    .map(Value::Number)
                    .unwrap_or(Value::Null));
            }
            Ok(Value::String(text.to_string()))
        }
        Value::String(_) => {
            if null_word {
                return Ok(Value::Null);
            }
            Ok(Value::String(text.to_string()))
        }
        other => Err(anyhow!(
            "`{key}` is a structured setting ({}); `config set` takes one scalar — edit the \
             config file, or send the object with PATCH /api/settings",
            json_kind(other)
        )),
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "text",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// How a settings value prints. `off` rather than `0` for a cleared budget: the
/// brain refuses `0`, so printing it would describe a value nobody can set.
pub fn setting_display(value: &Value) -> String {
    match value {
        Value::Null => "off".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) if text.is_empty() => "(empty)".to_string(),
        Value::String(text) => format!("{text:?}"),
        other => other.to_string(),
    }
}

/// `swarmbot config set <key> <value>`
pub fn config_set<S: Session, W: Write>(
    session: &S,
    key: &str,
    raw: &str,
    json: bool,
    printer: &mut Printer<W>,
) -> Result<i32> {
    let current = session.settings()?;
    let value = settings_value(key, raw, &current)?;
    let patch = serde_json::json!({ key: value });
    // One `PATCH`, both modes: embedded merges and validates with the same rules
    // the brain applies, remote forwards the object and repeats the brain's
    // answer — a 400 arrives here as its own status and message.
    let saved = session
        .update_settings(&patch)
        .with_context(|| format!("cannot set `{key}`"))?;
    let now = serde_json::to_value(&saved).context("cannot re-read the settings")?;
    if json {
        json_line(printer, &saved)?;
        return Ok(0);
    }
    let shown = setting_display(now.get(key).unwrap_or(&Value::Null));
    printer.line(&format!("{key} = {shown} (on {})", session.describe()))?;
    if key.starts_with("max_tokens") {
        printer.note(&crate::config::budget_line(&saved))?;
    }
    Ok(0)
}

// --- routines ----------------------------------------------------------------

/// A schedule in the words a human reads: `every 60s`, `at <ts>`, `cron "<expr>"`.
pub fn schedule_label(schedule: &RoutineSchedule) -> String {
    match schedule {
        RoutineSchedule::Interval { every_secs } => format!("every {every_secs}s"),
        RoutineSchedule::Once { run_at } => format!("once at {}", run_at.to_rfc3339()),
        RoutineSchedule::Cron { expr } => format!("cron {expr:?}"),
    }
}

fn routine_status_label(routine: &Routine) -> &'static str {
    match routine.status {
        swarmbot_contracts::routine::RoutineStatus::Active => "active",
        swarmbot_contracts::routine::RoutineStatus::Paused => "paused",
        swarmbot_contracts::routine::RoutineStatus::Completed => "completed",
    }
}

/// One row per routine: id, status, owner, name, schedule, next run.
pub fn routines_lines(routines: &[Routine], bots: &[Bot]) -> Vec<String> {
    routines
        .iter()
        .map(|routine| {
            let bot = bots
                .iter()
                .find(|bot| bot.id == routine.bot_id)
                .map(|bot| format!("@{}", bot.name))
                .unwrap_or_else(|| "@unknown".to_string());
            let next = match routine.next_run_at {
                Some(at)
                    if routine.status == swarmbot_contracts::routine::RoutineStatus::Active =>
                {
                    at.to_rfc3339()
                }
                _ => "-".to_string(),
            };
            format!(
                "{} {:<9} {bot:<12} {:<20} {} · next {next}",
                routine.id,
                routine_status_label(routine),
                format!("{:?}", routine.name),
                schedule_label(&routine.schedule),
            )
        })
        .collect()
}

/// `swarmbot routines ls`
pub fn routines_list<S: Session, W: Write>(
    session: &S,
    json: bool,
    printer: &mut Printer<W>,
) -> Result<i32> {
    let routines = session.routines()?;
    if json {
        for routine in &routines {
            json_line(printer, routine)?;
        }
        return Ok(0);
    }
    if routines.is_empty() {
        printer.note("no routines")?;
        return Ok(0);
    }
    for line in routines_lines(&routines, &session.bots()?) {
        printer.line(&line)?;
    }
    Ok(0)
}

/// One routine by id-or-tail, with the lookup's own error when it is missing.
fn routine_by_id<S: Session>(session: &S, raw: &str) -> Result<Routine> {
    let routines = session.routines()?;
    let routine = resolve_id(
        raw,
        &routines,
        |routine| routine.id.as_str(),
        "routine",
        "`swarmbot routines ls` lists them",
    )?;
    Ok(routine.clone())
}

/// `swarmbot routines pause|resume|rm <id>`
pub fn routines_change<S: Session, W: Write>(
    session: &S,
    action: RoutineAction,
    raw: &str,
    json: bool,
    printer: &mut Printer<W>,
) -> Result<i32> {
    let routine = routine_by_id(session, raw)?;
    match action {
        RoutineAction::Pause => {
            let routine = session.pause_routine(&routine.id)?;
            if json {
                json_line(printer, &routine)?;
                return Ok(0);
            }
            printer.line(&format!(
                "paused routine {} ({}) — nothing fires until `routines resume`",
                routine.id, routine.name
            ))?;
        }
        RoutineAction::Resume => {
            let routine = session.resume_routine(&routine.id)?;
            if json {
                json_line(printer, &routine)?;
                return Ok(0);
            }
            match routine.next_run_at {
                Some(at)
                    if routine.status == swarmbot_contracts::routine::RoutineStatus::Active =>
                {
                    printer.line(&format!(
                        "resumed routine {} ({}) — next run {}",
                        routine.id,
                        routine.name,
                        at.to_rfc3339()
                    ))?;
                }
                _ => printer.line(&format!(
                    "resumed routine {} ({}) — it has no next run; a one-shot whose time has \
                     passed stays completed",
                    routine.id, routine.name
                ))?,
            }
        }
        RoutineAction::Delete => {
            session.delete_routine(&routine.id)?;
            if json {
                printer.line(&serde_json::to_string(&serde_json::json!({
                    "id": routine.id.as_str(),
                    "deleted": true,
                }))?)?;
                return Ok(0);
            }
            printer.line(&format!(
                "deleted routine {} ({})",
                routine.id, routine.name
            ))?;
        }
    }
    Ok(0)
}

/// Which change `routines_change` makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutineAction {
    Pause,
    Resume,
    Delete,
}

fn run_status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Running => "running",
        RunStatus::Success => "success",
        RunStatus::Failed => "failed",
    }
}

/// One row per past run: when it started, how it ended, and why it failed.
pub fn runs_lines(runs: &[RoutineRun]) -> Vec<String> {
    runs.iter()
        .map(|run| {
            let ended = run
                .ended_at
                .map(|at| {
                    let seconds = at
                        .signed_duration_since(run.started_at)
                        .num_seconds()
                        .max(0);
                    format!("{seconds}s")
                })
                .unwrap_or_else(|| "-".to_string());
            let reason = run
                .error
                .as_deref()
                .map(|error| format!(" · {error}"))
                .unwrap_or_default();
            format!(
                "{} {} started {} · took {ended}{reason}",
                run.id,
                run_status_label(run.status),
                run.started_at.to_rfc3339()
            )
        })
        .collect()
}

/// `swarmbot routines runs <id>`
pub fn routines_runs<S: Session, W: Write>(
    session: &S,
    raw: &str,
    limit: u32,
    json: bool,
    printer: &mut Printer<W>,
) -> Result<i32> {
    let routine = routine_by_id(session, raw)?;
    let runs = session.routine_runs(&routine.id, limit)?;
    if json {
        for run in &runs {
            json_line(printer, run)?;
        }
        return Ok(0);
    }
    if runs.is_empty() {
        printer.note(&format!(
            "routine {} ({}) has not run yet",
            routine.id, routine.name
        ))?;
        return Ok(0);
    }
    printer.heading(&format!(
        "runs of routine {} ({})",
        routine.id, routine.name
    ))?;
    for line in runs_lines(&runs) {
        printer.line(&line)?;
    }
    Ok(0)
}

// --- nodes -------------------------------------------------------------------

/// `swarmbot nodes revoke <id>`
pub fn nodes_revoke<S: Session, W: Write>(
    session: &S,
    raw: &str,
    json: bool,
    printer: &mut Printer<W>,
) -> Result<i32> {
    let nodes = session.nodes()?;
    let node = resolve_id(
        raw,
        &nodes,
        |node| node.id.as_str(),
        "node",
        "`swarmbot nodes ls` lists the roster",
    )?
    .clone();
    session.revoke_node(&node.id)?;
    if json {
        printer.line(&serde_json::to_string(&serde_json::json!({
            "id": node.id.as_str(),
            "revoked": true,
        }))?)?;
        return Ok(0);
    }
    printer.line(&format!(
        "revoked node {} (\"{}\") — it drops offline and may not redial; the row stays so the \
         roster says the machine was here",
        node.id, node.label
    ))?;
    Ok(0)
}

// --- bots --------------------------------------------------------------------

/// `swarmbot bots edit <name> [--backend] [--model] [--enabled|--disabled] [--skills]`
pub fn bots_edit<S: Session, W: Write>(
    session: &S,
    name: &str,
    edit: &crate::session::BotEdit,
    json: bool,
    printer: &mut Printer<W>,
) -> Result<i32> {
    if edit.is_empty() {
        return Err(anyhow!(
            "nothing to change: pass at least one of --backend, --model, --enabled/--disabled, \
             --skills"
        ));
    }
    // An empty roster is a different fact from "no such handle": nothing has
    // booted the team yet, and an owner who reads "no bot called @lead" in a
    // fresh data dir would go looking for a bug instead of at `--team`.
    if session.bots()?.is_empty() {
        return Err(anyhow!(
            "there are no bots to edit — a team is provisioned when a brain boots (`swarmbot run`, \
             `swarmbot demo`, or the daemon's own --team FILE), and this data dir has none yet"
        ));
    }
    let bot = session.edit_bot(name, edit)?;
    if json {
        json_line(printer, &bot)?;
        return Ok(0);
    }
    let row = crate::repl::bots_lines(std::slice::from_ref(&bot));
    for line in row {
        printer.line(&line)?;
    }
    // Say which of the edits are only real from the next turn: a model change
    // mid-session does not rewrite history, and an owner who cannot see that
    // would file the wrong bug report.
    if edit.backend.is_some() || edit.model.is_some() {
        printer.note(
            "the change applies to this bot's next turn; a session already open keeps its history",
        )?;
    }
    Ok(0)
}

// --- transcripts -------------------------------------------------------------

/// Every entry of one conversation as NDJSON: one `Entry` object per line,
/// ascending — the shape the brain's export endpoint serves, built from the same
/// rows so an embedded export and a daemon export of the same transcript are
/// byte-identical.
pub fn export_ndjson(store: &dyn Persistence, conversation: &ConversationId) -> Result<String> {
    let mut out = String::new();
    let mut after = 0u64;
    loop {
        let page = store
            .entries_after(conversation, after, EXPORT_PAGE)
            .map_err(|err| anyhow!("cannot read the transcript: {err}"))?;
        if page.is_empty() {
            break;
        }
        for entry in &page {
            out.push_str(&serde_json::to_string(entry)?);
            out.push('\n');
        }
        match page.last() {
            Some(last) => after = last.seq,
            None => break,
        }
        if page.len() < EXPORT_PAGE as usize {
            break;
        }
    }
    Ok(out)
}

// --- status ------------------------------------------------------------------

/// What `swarmbot status` prints: the roster's state as the brain holds it, the
/// work still moving, and the counters only an in-process brain can read.
///
/// Every number is counted from something the *session* answered — never
/// estimated. A counter a mode cannot see is named in [`StatusReport::unknown`]
/// instead of being printed as `0`, because "no turn is running" and "this CLI
/// cannot tell" are different facts.
#[derive(Clone, Debug, Default, Serialize)]
pub struct StatusReport {
    /// Which brain answered.
    pub brain: String,
    pub bots_total: usize,
    pub bots_idle: usize,
    pub bots_working: usize,
    pub bots_queued: usize,
    pub bots_error: usize,
    pub bots_disabled: usize,
    pub tasks_queued: usize,
    pub tasks_running: usize,
    pub tasks_other: usize,
    pub requests_pending: usize,
    pub nodes_total: usize,
    pub nodes_online: usize,
    pub nodes_revoked: usize,
    /// Present only when the session can read its own process's counters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<crate::session::LiveCounts>,
    /// The lines this mode cannot answer. Empty when nothing is missing.
    pub unknown: Vec<String>,
}

impl StatusReport {
    /// Count every list the port can answer. `live` is `None` for a remote
    /// brain, and the three counter lines are then listed in `unknown`.
    pub fn collect<S: Session>(session: &S) -> Result<Self> {
        let bots = session.bots()?;
        let tasks = session.tasks()?;
        let requests = session.pending()?;
        let nodes = session.nodes()?;
        let mut report = Self {
            brain: session.describe(),
            bots_total: bots.len(),
            ..Self::default()
        };
        for bot in &bots {
            match &bot.status {
                BotStatus::Idle => report.bots_idle += 1,
                BotStatus::Working { .. } => report.bots_working += 1,
                BotStatus::Queued { .. } => report.bots_queued += 1,
                BotStatus::Error { .. } => report.bots_error += 1,
                BotStatus::Disabled => report.bots_disabled += 1,
            }
        }
        for task in &tasks {
            match task.status {
                TaskStatus::Queued => report.tasks_queued += 1,
                TaskStatus::Running => report.tasks_running += 1,
                _ => report.tasks_other += 1,
            }
        }
        report.requests_pending = requests.len();
        report.nodes_total = nodes.len();
        report.nodes_online = nodes.iter().filter(|node| node.online).count();
        report.nodes_revoked = nodes.iter().filter(|node| node.revoked).count();
        report.live = session.live_counts();
        if report.live.is_none() {
            report.unknown = vec![
                "turns the store still calls running".to_string(),
                "deliveries waiting in a queue".to_string(),
                "deliveries parked for a runner that does not exist yet".to_string(),
                "live runners".to_string(),
            ];
        }
        Ok(report)
    }

    /// The prose block. The `unknown` lines come last and say why they are
    /// missing: a status with silently absent counters invites "so it is zero".
    pub fn to_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("brain: {}", self.brain)];
        lines.push(format!(
            "  bots {} total · {} idle · {} working · {} queued · {} error · {} disabled",
            self.bots_total,
            self.bots_idle,
            self.bots_working,
            self.bots_queued,
            self.bots_error,
            self.bots_disabled
        ));
        match &self.live {
            Some(live) if live.booted => {
                lines.push(format!(
                    "  turns still marked running: {} · live runners: {}",
                    live.turns_running, live.runners
                ));
                lines.push(format!(
                    "  deliveries: {} queued · {} parked",
                    live.deliveries_queued, live.deliveries_parked
                ));
            }
            // A read-only in-process brain opened the store but started nothing,
            // so `0 runners` would read as "the team has none". Say which it is.
            Some(live) => {
                lines.push(format!(
                    "  turns still marked running: {} — this CLI opened the store read-only and \
                     started nothing, so it has no live runners to report",
                    live.turns_running
                ));
            }
            None => lines.push(
                "  turns and deliveries: not visible from this CLI — they are counters inside \
                 the daemon's process"
                    .to_string(),
            ),
        }
        lines.push(format!(
            "  tasks: {} queued · {} running · {} finished",
            self.tasks_queued, self.tasks_running, self.tasks_other
        ));
        lines.push(format!(
            "  requests waiting for an answer: {}",
            self.requests_pending
        ));
        lines.push(format!(
            "  nodes: {} online of {} · {} revoked",
            self.nodes_online, self.nodes_total, self.nodes_revoked
        ));
        // The lines that are advice rather than counts, and why they are here: an
        // owner reading `status` is exactly who should hear that there is no team
        // yet, that a team is idle, or that a parked delivery is quietly keeping
        // it from ever going idle. A team of zero bots is never "idle".
        let moving = self.bots_working > 0
            || self.tasks_queued > 0
            || self.tasks_running > 0
            || self.requests_pending > 0
            || self.live.as_ref().is_some_and(|live| {
                live.turns_running > 0 || live.deliveries_queued > 0 || live.deliveries_parked > 0
            });
        if self.bots_total == 0 {
            lines.push(
                "no bots yet — a team is provisioned when a brain boots (`swarmbot run`, \
                 `swarmbot demo`, or the daemon's own --team FILE)"
                    .to_string(),
            );
        } else if !moving {
            lines.push("nothing is moving — the team is idle".to_string());
        }
        if self
            .live
            .as_ref()
            .is_some_and(|live| live.deliveries_parked > 0)
        {
            // Its own line because it never shows in the roster's counts, and it
            // is exactly the case that stops `run --wait` from ever reporting
            // idle: a disabled bot's routine keeps firing into a parked queue.
            lines.push(
                "note: a parked delivery waits for a runner that does not exist — a disabled \
                 bot's routine keeps firing into it"
                    .to_string(),
            );
        }
        lines
    }
}

/// `swarmbot status`
pub fn status<S: Session, W: Write>(
    session: &S,
    json: bool,
    printer: &mut Printer<W>,
) -> Result<i32> {
    let report = StatusReport::collect(session)?;
    if json {
        // One whole object, like `usage --json`: a snapshot is not a stream.
        printer.line(&serde_json::to_string(&report)?)?;
        return Ok(0);
    }
    printer.heading("── status ──")?;
    for line in report.to_lines() {
        printer.line(&line)?;
    }
    Ok(0)
}

// --- secrets -----------------------------------------------------------------

/// Why `applies` says `"restart"` and never `"hot"`.
///
/// The backend registry resolves each key **once**, while the brain is composed,
/// into an adapter's private field; nothing running re-reads the file afterwards.
/// Saying `"hot"` would claim a change nobody can observe until the next process
/// starts — the exact class of lie `AGENTS.md` forbids. The brain's own route
/// keeps the same word for the same reason.
pub const APPLIES_RESTART: &str = "restart";

/// One row per credential **name**. The value is never read into this function's
/// arguments, let alone printed: `swarmbot config key` exists so an owner can
/// rotate a key without opening the file, and a listing that leaked a value
/// would undo the 0600 it is stored under.
pub fn keys_lines(secrets: &[swarmbot_client::SecretName]) -> Vec<String> {
    secrets
        .iter()
        .map(|secret| {
            if secret.used_by.is_empty() {
                // A real answer, not a bug: a key written for a backend that has
                // since been removed from the config is still in the file, and a
                // list that hid it would leave the owner unable to delete it.
                format!("{} (no configured backend names it)", secret.name)
            } else {
                format!("{} — used by {}", secret.name, secret.used_by.join(", "))
            }
        })
        .collect()
}

/// Read `secrets.json` as a name→value map.
///
/// Only this function and [`write_secret`] ever hold a value; it is written
/// straight back and never returned to a caller that could print it.
pub fn read_secret_file(
    path: &std::path::Path,
) -> Result<std::collections::BTreeMap<String, String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not a JSON object of string keys", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(std::collections::BTreeMap::new())
        }
        Err(err) => Err(anyhow!("cannot read {}: {err}", path.display())),
    }
}

/// Which backend ids name each key (`api_key_env`), so a key nobody reads says
/// so instead of looking load-bearing.
fn secret_users(
    config: &swarmbot_contracts::config::BrainConfig,
) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut users: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for backend in &config.backends {
        if let Some(name) = &backend.api_key_env {
            users
                .entry(name.clone())
                .or_default()
                .push(backend.id.clone());
        }
    }
    users
}

/// The names an in-process brain would load: the file **and** the
/// environment-provided ones its backends name.
///
/// The union is the brain's own rule, and it matters: a key that is in use but
/// invisible would leave an owner believing a rotation reached a name that is
/// still coming from the environment. A value is never read to build this.
pub fn list_secrets(
    data_dir: &std::path::Path,
    config: &swarmbot_contracts::config::BrainConfig,
) -> Result<Vec<swarmbot_client::SecretName>> {
    let users = secret_users(config);
    let mut names: std::collections::BTreeSet<String> = read_secret_file(&secret_file(data_dir))?
        .keys()
        .cloned()
        .collect();
    for backend in &config.backends {
        if let Some(name) = &backend.api_key_env
            && std::env::var(name).is_ok_and(|value| !value.trim().is_empty())
        {
            names.insert(name.clone());
        }
    }
    Ok(names
        .into_iter()
        .map(|name| swarmbot_client::SecretName {
            used_by: users.get(&name).cloned().unwrap_or_default(),
            name,
        })
        .collect())
}

/// Where a data dir keeps its keys.
pub fn secret_file(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("secrets.json")
}

/// Write (or remove, with `value = None`) one key in `secrets.json`.
///
/// The file is rewritten whole — a partial write would leave a half-key behind —
/// through [`write_secret_file`], which writes a temp file and renames it, so a
/// crash leaves the old file intact and the new one is 0600 from its first byte.
/// **The value is never logged, echoed into an error, or put in a message.**
pub fn write_secret(
    data_dir: &std::path::Path,
    name: &str,
    value: Option<&str>,
) -> Result<swarmbot_client::SecretWrite> {
    let path = secret_file(data_dir);
    let name = name.trim();
    if name.is_empty() {
        return Err(anyhow!("a key needs a name"));
    }
    let mut secrets = read_secret_file(&path)?;
    match value {
        Some(value) => {
            if value.is_empty() {
                return Err(anyhow!(
                    "a key cannot be empty: `config key rm {name}` removes one"
                ));
            }
            secrets.insert(name.to_string(), value.to_string());
        }
        None => {
            if !secrets.contains_key(name) {
                // Removing an environment-provided name cannot work: a process
                // cannot unset its own environment, so a cheerful "removed"
                // would be a lie about a key still in use.
                if std::env::var(name).is_ok() {
                    return Err(anyhow!(
                        "{name} is set in this process's environment, and a file cannot unset it \
                         — unset the variable in the environment that starts the brain instead"
                    ));
                }
                return Err(anyhow!(
                    "there is no key called {name} in {} — `config key ls` lists the names",
                    path.display()
                ));
            }
            secrets.remove(name);
        }
    }
    let body = serde_json::to_string_pretty(&secrets).context("cannot encode the key file")?;
    write_secret_file(&path, &body)?;
    Ok(swarmbot_client::SecretWrite {
        name: name.to_string(),
        applies: APPLIES_RESTART.to_string(),
    })
}

/// Write `<data>/secrets.json` **atomically**, 0600 from the first byte.
///
/// Written to a sibling temp file and renamed, so a crash mid-write leaves the
/// old file intact and there is no moment when the key is half-written or
/// world-readable. A secret file left group- or world-readable is a leaked
/// credential waiting for somebody to run `cat`, which is why the mode is set at
/// creation *and* on the finished file (an existing file keeps its old mode
/// otherwise).
pub fn write_secret_file(path: &std::path::Path, body: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("secrets.json"),
        std::process::id()
    ));
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let write = || -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temp)
                .with_context(|| format!("cannot write {}", temp.display()))?;
            file.write_all(body.as_bytes())
                .with_context(|| format!("cannot write {}", temp.display()))?;
            file.sync_all()
                .with_context(|| format!("cannot flush {}", temp.display()))?;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("cannot restrict permissions on {}", temp.display()))?;
            std::fs::rename(&temp, path).with_context(|| {
                format!("cannot replace {} with the new key file", path.display())
            })?;
            Ok(())
        };
        if let Err(err) = write() {
            // Best effort: a leftover temp file is untidy, not wrong, and the
            // error that matters is the one being returned.
            let _ = std::fs::remove_file(&temp);
            return Err(err);
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&temp, body).with_context(|| format!("cannot write {}", temp.display()))?;
        std::fs::rename(&temp, path)
            .with_context(|| format!("cannot replace {}", path.display()))?;
        Ok(())
    }
}

/// The line every secret write prints about *when* it takes effect.
///
/// `restart`, not `hot`, and stated once here so no command can promise
/// otherwise: an in-process brain (or a daemon) builds its backend registry from
/// the secrets it loaded at boot, so a file written now is read by the next
/// process. Saying "applied" would be a lie about a live registry.
pub fn applies_hint() -> String {
    "takes effect when the brain restarts: its backend registry was built from the keys loaded \
     at boot, so a file written now is read by the next process"
        .to_string()
}

// --- transcripts (the write half) --------------------------------------------

/// Refuse a `--out` that was narrowed by a window selector.
///
/// `--out FILE` writes the **whole** transcript, and clap cannot catch the pair
/// (`--limit` has no default now, but "both were typed" is not "both make
/// sense"). A partial file under the name of a complete export is an archive
/// nobody can trust, so the contradiction is refused in words rather than one of
/// the flags being silently ignored.
pub fn check_export_flags(out: bool, limit: Option<u32>) -> Result<()> {
    if out && limit.is_some() {
        return Err(anyhow!(
            "`transcript --out FILE` writes the whole transcript, so `--limit` has nothing to \
             narrow: drop `--limit` for a complete export, or use `transcript --limit N` to \
             print a window"
        ));
    }
    Ok(())
}

/// Where an export goes: stdout, or a file the caller named.
pub fn write_export<W: Write>(
    body: &str,
    out: Option<&std::path::Path>,
    printer: &mut Printer<W>,
) -> Result<()> {
    match out {
        Some(path) => {
            std::fs::write(path, body)
                .with_context(|| format!("cannot write {}", path.display()))?;
            printer.note(&format!(
                "wrote {} line(s) to {}",
                body.lines().count(),
                path.display()
            ))?;
        }
        None => {
            // To stdout, verbatim: an export piped into `jq` or compared with
            // `diff` must be exactly the NDJSON, not a rendering of it.
            print!("{body}");
            std::io::stdout().flush()?;
        }
    }
    Ok(())
}

// --- shared ------------------------------------------------------------------

/// One JSON object, one line, through the same sink as the prose: the contract
/// `run --json` and `transcript --json` already keep.
fn json_line<W: Write, T: Serialize>(printer: &mut Printer<W>, value: &T) -> Result<()> {
    printer.line(&serde_json::to_string(value)?)
}

/// The distinguishing tail of an id, for a line that has no room for all 36
/// characters. Shares the rule with `embedded::short_conversation`, so the two
/// spellings of an id in one screen agree.
pub fn short_id(raw: &str) -> String {
    let tail: String = raw
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

/// Turns a `--json` printer's output into the objects it wrote, for the tests.
#[cfg(test)]
pub(crate) fn json_objects(text: &str) -> Vec<Value> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("`{line}` is one JSON object ({err})"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Theme;
    use swarmbot_contracts::id::{BotId, NodeId, RequestId, RuleId};
    use swarmbot_contracts::node::Surface;
    use swarmbot_contracts::repo::{ConversationRepo, EntryRepo};
    use swarmbot_contracts::request::RuleEffect;

    fn approval(id: &str) -> RuntimeRequest {
        let mut request = RuntimeRequest::approval(
            BotId::from_suffix("000000000000000000000000000000b1"),
            swarmbot_contracts::id::ConversationId::from_suffix("000000000000000000000000000000c1"),
            Surface::Shell,
            "shell",
            "cargo test",
            None,
        );
        request.id = RequestId::from_suffix(id);
        request
    }

    fn rule(suffix: &str) -> ApprovalRule {
        let mut rule = ApprovalRule::new(RuleScope::Global, RuleEffect::Allow);
        rule.id = RuleId::from_suffix(suffix);
        rule.surface = Some(Surface::Shell);
        rule.command_prefix = Some("cargo".to_string());
        rule
    }

    #[test]
    fn an_id_resolves_exactly_then_by_a_unique_tail() {
        let rules = vec![rule("00000000000000000000000000000001")];
        let found = resolve_id(
            "rul_00000000000000000000000000000001",
            &rules,
            |rule| rule.id.as_str(),
            "approval rule",
            "hint",
        )
        .expect("the whole id");
        assert_eq!(found.id, rules[0].id);
        // The tail a human copies out of `rules ls` works too.
        assert!(
            resolve_id(
                "0001",
                &rules,
                |rule| rule.id.as_str(),
                "approval rule",
                "hint"
            )
            .is_ok()
        );

        // Ambiguous tails are refused instead of resolved to the first match:
        // deleting the wrong rule is not recoverable.
        let two = vec![
            rule("aaaaaaaaaaaaaaaaaaaaaaaaaaaa0001"),
            rule("bbbbbbbbbbbbbbbbbbbbbbbbbbbb0001"),
        ];
        // Both ids end the same way, so the tail a human copies is ambiguous and
        // must be refused rather than resolved to whichever came first.
        let err = resolve_id(
            "0001",
            &two,
            |rule| rule.id.as_str(),
            "approval rule",
            "hint",
        )
        .expect_err("ambiguous");
        let text = err.to_string();
        assert!(text.contains("matches 2"), "{text}");
        assert!(text.contains("type more characters"), "{text}");

        let err = resolve_id(
            "nothing",
            &two,
            |rule| rule.id.as_str(),
            "approval rule",
            "run `rules ls`",
        )
        .expect_err("missing");
        assert!(err.to_string().contains("no approval rule"), "{err}");
        assert!(err.to_string().contains("run `rules ls`"), "{err}");
    }

    #[test]
    fn a_decision_answers_an_approval_and_free_text_answers_the_rest() {
        let approval = approval("000000000000000000000000000000a1");
        assert_eq!(
            answer_payload(&approval, &Answer::Decide(Decision::AllowOnce)).expect("decision"),
            (Some(Decision::AllowOnce), None)
        );

        // Free text on an approval is refused: the card would settle with no
        // decision, which a tool gate reads as "not allowed".
        let err = answer_payload(&approval, &Answer::Text("yes please".into()))
            .expect_err("not an approval answer");
        assert!(err.to_string().contains("--allow-once"), "{err}");

        let mut question = approval.clone();
        question.kind = RequestKind::Question;
        assert_eq!(
            answer_payload(&question, &Answer::Text("staging".into())).expect("text"),
            (None, Some(Value::String("staging".into())))
        );
        let err = answer_payload(&question, &Answer::Decide(Decision::Deny))
            .expect_err("a question takes text");
        assert!(err.to_string().contains("--answer <text>"), "{err}");
    }

    #[test]
    fn a_settings_key_is_typed_by_the_value_it_already_holds() {
        let current = Settings::default();
        // A boolean, in every spelling a human might type.
        for (raw, expected) in [("false", false), ("off", false), ("no", false), ("1", true)] {
            assert_eq!(
                settings_value("bot_messaging", raw, &current).expect(raw),
                Value::Bool(expected),
                "{raw}"
            );
        }
        // A number.
        assert_eq!(
            settings_value("max_hops", "16", &current).expect("number"),
            Value::from(16)
        );
        // A cleared budget is `off`, not `0`: the brain refuses zero.
        assert_eq!(
            settings_value("max_tokens_per_turn", "off", &current).expect("clear"),
            Value::Null
        );
        // A string.
        assert_eq!(
            settings_value("agent_cwd", "/srv/work", &current).expect("string"),
            Value::String("/srv/work".into())
        );
        // An unknown key names the keys that exist, before any brain is asked.
        let err = settings_value("max_hp", "3", &current).expect_err("unknown");
        let text = err.to_string();
        assert!(text.contains("unknown setting `max_hp`"), "{text}");
        assert!(text.contains("max_hops"), "{text}");
        assert!(text.contains("bot_messaging"), "{text}");
        // A structured one is refused with what it is.
        let err = settings_value("safety", "{}", &current).expect_err("structured");
        assert!(err.to_string().contains("structured setting"), "{err}");
        // A non-boolean for a boolean key.
        let err = settings_value("bot_messaging", "maybe", &current).expect_err("not a bool");
        assert!(err.to_string().contains("true or false"), "{err}");
    }

    /// The brain's own rule, kept here because the embedded half applies it: a
    /// budget of zero is a 400, not "unlimited" and not "spend nothing".
    #[test]
    fn a_budget_of_zero_is_refused_by_the_same_validation_the_brain_uses() {
        let patch = serde_json::json!({"max_tokens_per_turn": 0});
        let err = crate::config::merge_settings(&Settings::default(), &patch)
            .expect_err("zero is not a budget");
        let text = err.to_string();
        assert!(text.contains("max_tokens_per_turn"), "{text}");
        assert!(text.contains("null"), "the one spelling of no cap: {text}");

        // And the legal merge still lands.
        let merged = crate::config::merge_settings(
            &Settings::default(),
            &serde_json::json!({"max_tokens_per_turn": 250_000, "max_hops": 3}),
        )
        .expect("merges");
        assert_eq!(merged.max_tokens_per_turn, Some(250_000));
        assert_eq!(merged.max_hops, 3);
    }

    #[test]
    fn the_settings_key_list_comes_from_the_type() {
        let keys = crate::config::settings_keys();
        for key in [
            "bot_messaging",
            "max_hops",
            "max_tool_rounds",
            "approval_expiry_secs",
            "max_tokens_per_turn",
            "max_tokens_per_day",
            "agent_cwd",
        ] {
            assert!(keys.iter().any(|known| known == key), "missing {key}");
        }
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "the refusal's list is stable");
    }

    #[test]
    fn a_cleared_budget_prints_off_not_zero() {
        assert_eq!(setting_display(&Value::Null), "off");
        assert_eq!(setting_display(&Value::from(5_000_000)), "5000000");
        assert_eq!(setting_display(&Value::Bool(false)), "false");
        assert_eq!(setting_display(&Value::String("/srv".into())), "\"/srv\"");
    }

    #[test]
    fn rules_and_requests_render_one_readable_row_each() {
        let lines = rules_lines(&[rule("00000000000000000000000000000001")]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("allow"), "{}", lines[0]);
        assert!(lines[0].contains("global"), "{}", lines[0]);
        assert!(lines[0].contains("prefix=\"cargo\""), "{}", lines[0]);

        let bots = vec![Bot::new(
            "lead",
            "Lead",
            "",
            "fake",
            swarmbot_contracts::id::ConversationId::from_suffix("000000000000000000000000000000c1"),
        )];
        let conversations = vec![Conversation::direct(bots[0].id.clone())];
        let rows = requests_lines(
            &[approval("000000000000000000000000000000a1")],
            &bots,
            &conversations,
        );
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains("approval"), "{}", rows[0]);
        assert!(rows[0].contains("Allow shell `cargo test`?"), "{}", rows[0]);
        assert!(rows[0].contains("expires in"), "{}", rows[0]);
    }

    #[test]
    fn routine_rows_name_their_schedule_and_next_run() {
        let bot = Bot::new(
            "lead",
            "Lead",
            "",
            "fake",
            swarmbot_contracts::id::ConversationId::new(),
        );
        let routine = Routine::new(
            bot.id.clone(),
            "sync",
            "summarize",
            RoutineSchedule::Interval { every_secs: 60 },
            chrono::Utc::now(),
        )
        .expect("a routine");
        let rows = routines_lines(std::slice::from_ref(&routine), std::slice::from_ref(&bot));
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains("@lead"), "{}", rows[0]);
        assert!(rows[0].contains("every 60s"), "{}", rows[0]);
        assert!(rows[0].contains("active"), "{}", rows[0]);
        assert!(rows[0].contains("next 20"), "{}", rows[0]);
    }

    #[test]
    fn the_export_is_one_entry_per_line_and_ascending() {
        // A scratch in-memory store, seeded through the contract the CLI is
        // handed in embedded mode.
        let store = swarmbot_store::MemoryStore::new();
        let conversation = Conversation::direct(BotId::new());
        store
            .insert_conversation(&conversation)
            .expect("conversation");
        let first =
            swarmbot_contracts::entry::Entry::owner_message(conversation.id.clone(), 1, "hello");
        store.upsert_entry(&first).expect("first");
        let second = swarmbot_contracts::entry::Entry::bot_message(
            conversation.id.clone(),
            2,
            BotId::new(),
            "hi",
            None,
        );
        store.upsert_entry(&second).expect("second");

        let text = export_ndjson(&store, &conversation.id).expect("exports");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        let objects = json_objects(&text);
        assert_eq!(objects[0]["seq"], 1, "{text}");
        assert_eq!(objects[1]["seq"], 2, "{text}");

        // An empty conversation is an empty export, not an error.
        let empty = Conversation::direct(BotId::new());
        store.insert_conversation(&empty).expect("conversation");
        assert_eq!(
            export_ndjson(&store, &empty.id).expect("exports"),
            "",
            "an empty transcript is zero lines"
        );
    }

    /// A session whose answers the test decides, so a command body's own rules
    /// (the pending-card lookup, the unknown-key refusal, the empty edit) are
    /// testable without a brain or a socket.
    /// Every request/decision pair a `Recorder` answered, so a test can assert
    /// *what* was sent rather than only that something was.
    type Answered = std::sync::Arc<std::sync::Mutex<Vec<(RequestId, Option<Decision>)>>>;
    type Patches = std::sync::Arc<std::sync::Mutex<Vec<Value>>>;
    type DeletedRules = std::sync::Arc<std::sync::Mutex<Vec<RuleId>>>;

    #[derive(Clone, Default)]
    struct Recorder {
        pending: Vec<RuntimeRequest>,
        rules: Vec<ApprovalRule>,
        routines: Vec<Routine>,
        nodes: Vec<swarmbot_contracts::node::NodeInfo>,
        bots: Vec<Bot>,
        answered: Answered,
        deleted_rules: DeletedRules,
        settings: Settings,
        patches: Patches,
        fail_settings: Option<String>,
    }

    impl Session for Recorder {
        fn describe(&self) -> String {
            "the recording brain".to_string()
        }
        fn poll_interval(&self) -> std::time::Duration {
            std::time::Duration::from_millis(1)
        }
        fn bots(&self) -> Result<Vec<Bot>> {
            Ok(self.bots.clone())
        }
        fn conversations(&self) -> Result<Vec<Conversation>> {
            Ok(Vec::new())
        }
        fn tasks(&self) -> Result<Vec<swarmbot_contracts::task::Task>> {
            Ok(Vec::new())
        }
        fn nodes(&self) -> Result<Vec<swarmbot_contracts::node::NodeInfo>> {
            Ok(self.nodes.clone())
        }
        fn usage(&self, _: Option<&str>) -> Result<swarmbot_contracts::turn::UsageReport> {
            let now = chrono::Utc::now();
            Ok(swarmbot_contracts::turn::UsageReport::from_turns(
                now,
                now,
                &[],
                |_| None,
            ))
        }
        fn tail(
            &self,
            _: &swarmbot_contracts::id::ConversationId,
            _: u32,
        ) -> Result<Vec<swarmbot_contracts::entry::Entry>> {
            Ok(Vec::new())
        }
        fn send(
            &self,
            _: &swarmbot_contracts::id::ConversationId,
            _: &str,
        ) -> Result<crate::session::SendReport> {
            Ok(crate::session::SendReport::default())
        }
        fn subscribe(
            &self,
            _: Option<swarmbot_contracts::id::ConversationId>,
        ) -> Result<tokio::sync::mpsc::UnboundedReceiver<swarmbot_contracts::live::LiveEvent>>
        {
            let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
            Ok(rx)
        }
        fn respond(
            &self,
            request: &RequestId,
            decision: Option<Decision>,
            answer: Option<Value>,
        ) -> Result<RuntimeRequest> {
            let mut settled = self
                .pending
                .iter()
                .find(|candidate| &candidate.id == request)
                .cloned()
                .ok_or_else(|| anyhow!("request {request} is not pending"))?;
            settled.status = RequestStatus::Answered;
            settled.decision = decision;
            settled.answer = answer;
            self.answered
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .push((request.clone(), decision));
            Ok(settled)
        }
        fn interrupt(&self, _: &swarmbot_contracts::id::ConversationId) -> Result<bool> {
            Ok(false)
        }
        fn pending_in(
            &self,
            _: &swarmbot_contracts::id::ConversationId,
        ) -> Result<Vec<RuntimeRequest>> {
            Ok(self.pending.clone())
        }
        fn new_session(&self, _: &Bot) -> Result<()> {
            Ok(())
        }
        fn busy(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn delegation_lines(
            &self,
            _: &swarmbot_contracts::id::ConversationId,
        ) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn rules(&self) -> Result<Vec<ApprovalRule>> {
            Ok(self.rules.clone())
        }
        fn delete_rule(&self, id: &RuleId) -> Result<()> {
            self.deleted_rules
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .push(id.clone());
            Ok(())
        }
        fn pending(&self) -> Result<Vec<RuntimeRequest>> {
            Ok(self.pending.clone())
        }
        fn settings(&self) -> Result<Settings> {
            Ok(self.settings.clone())
        }
        fn update_settings(&self, patch: &Value) -> Result<Settings> {
            if let Some(reason) = &self.fail_settings {
                return Err(anyhow!("{reason}"));
            }
            self.patches
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .push(patch.clone());
            // Apply it the way a real brain does — a merge, then the answer as
            // saved — so a command that prints the saved value is tested against
            // the value, not against its own guess.
            let mut merged = serde_json::to_value(&self.settings)?;
            if let (Some(target), Some(source)) = (merged.as_object_mut(), patch.as_object()) {
                for (key, value) in source {
                    target.insert(key.clone(), value.clone());
                }
            }
            Ok(serde_json::from_value(merged)?)
        }
        fn routines(&self) -> Result<Vec<Routine>> {
            Ok(self.routines.clone())
        }
        fn revoke_node(&self, _: &NodeId) -> Result<()> {
            Ok(())
        }
    }

    fn printer_for(buffer: &mut Vec<u8>) -> Printer<&mut Vec<u8>> {
        Printer::new(buffer, Theme::plain(), false)
    }

    #[test]
    fn answering_an_id_that_is_not_waiting_refuses_and_names_what_is() {
        let session = Recorder {
            pending: vec![approval("000000000000000000000000000000a1")],
            ..Recorder::default()
        };
        let mut buffer = Vec::new();
        let err = {
            let mut printer = printer_for(&mut buffer);
            requests_answer(
                &session,
                "000000000000000000000000000000ff",
                &Answer::Decide(Decision::AllowOnce),
                false,
                &mut printer,
            )
            .expect_err("nothing was answered")
        };
        let text = err.to_string();
        assert!(text.contains("no pending request"), "{text}");
        assert!(text.contains("requests ls"), "{text}");
        assert!(
            session
                .answered
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .is_empty(),
            "a wrong id must not settle anything"
        );
        assert!(
            String::from_utf8(buffer).expect("utf8").is_empty(),
            "and it must not print a success line"
        );
    }

    #[test]
    fn answering_a_pending_card_reports_the_status_the_brain_answered_with() {
        let session = Recorder {
            pending: vec![approval("000000000000000000000000000000a1")],
            ..Recorder::default()
        };
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            let code = requests_answer(
                &session,
                "000000000000000000000000000000a1",
                &Answer::Decide(Decision::AllowAlways),
                false,
                &mut printer,
            )
            .expect("answers");
            assert_eq!(code, 0);
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("answered"), "{out}");
        assert!(out.contains("allowed always"), "{out}");
        assert!(
            out.contains("rules ls"),
            "it names where the rule is: {out}"
        );
        assert_eq!(
            session
                .answered
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .len(),
            1
        );
    }

    #[test]
    fn a_json_answer_is_one_object_that_says_answered() {
        let session = Recorder {
            pending: vec![approval("000000000000000000000000000000a1")],
            ..Recorder::default()
        };
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            requests_answer(
                &session,
                "000000000000000000000000000000a1",
                &Answer::Decide(Decision::Deny),
                true,
                &mut printer,
            )
            .expect("answers");
        }
        let objects = json_objects(&String::from_utf8(buffer).expect("utf8"));
        assert_eq!(objects.len(), 1, "{objects:?}");
        assert_eq!(objects[0]["status"], "answered", "{objects:?}");
        assert_eq!(objects[0]["decision"], "deny", "{objects:?}");
    }

    #[test]
    fn config_set_sends_one_patch_and_prints_the_saved_value() {
        let session = Recorder::default();
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            config_set(&session, "max_hops", "16", false, &mut printer).expect("sets");
        }
        let patches = session
            .patches
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        assert_eq!(patches.len(), 1, "{patches:?}");
        assert_eq!(patches[0], serde_json::json!({"max_hops": 16}));
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("max_hops = 16"), "{out}");
        assert!(out.contains("the recording brain"), "{out}");
    }

    #[test]
    fn config_set_repeats_the_brains_own_refusal_verbatim() {
        let session = Recorder {
            fail_settings: Some(
                "the brain at http://127.0.0.1:1 refused the request: 400 invalid_input: \
             max_tokens_per_turn must be at least 1, or null for no cap"
                    .to_string(),
            ),
            ..Recorder::default()
        };
        let mut buffer = Vec::new();
        let mut printer = printer_for(&mut buffer);
        let err = config_set(&session, "max_tokens_per_turn", "0", false, &mut printer)
            .expect_err("refused");
        let text = format!("{err:#}");
        assert!(
            text.contains("max_tokens_per_turn must be at least 1"),
            "the brain's sentence survives: {text}"
        );
        assert!(text.contains("400"), "{text}");
    }

    #[test]
    fn config_set_refuses_an_unknown_key_without_patching_anything() {
        let session = Recorder::default();
        let mut buffer = Vec::new();
        let mut printer = printer_for(&mut buffer);
        let err = config_set(&session, "max_hp", "3", false, &mut printer).expect_err("unknown");
        let text = err.to_string();
        assert!(text.contains("unknown setting `max_hp`"), "{text}");
        assert!(text.contains("max_hops"), "{text}");
        assert!(
            session
                .patches
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .is_empty(),
            "an unknown key never reaches the brain"
        );
    }

    #[test]
    fn an_empty_edit_is_refused_before_the_brain_is_asked() {
        let session = Recorder::default();
        let mut buffer = Vec::new();
        let mut printer = printer_for(&mut buffer);
        let err = bots_edit(
            &session,
            "lead",
            &crate::session::BotEdit::default(),
            false,
            &mut printer,
        )
        .expect_err("nothing to change");
        assert!(err.to_string().contains("nothing to change"), "{err}");
    }

    #[test]
    fn deleting_a_missing_rule_names_the_listing_that_would_have_it() {
        let session = Recorder {
            rules: vec![rule("00000000000000000000000000000001")],
            ..Recorder::default()
        };
        let mut buffer = Vec::new();
        let mut printer = printer_for(&mut buffer);
        let err = rules_remove(
            &session,
            "rul_00000000000000000000000000000009",
            false,
            &mut printer,
        )
        .expect_err("missing");
        assert!(err.to_string().contains("no approval rule"), "{err}");
    }

    #[test]
    fn deleting_a_rule_reports_what_it_was() {
        let session = Recorder {
            rules: vec![rule("00000000000000000000000000000001")],
            ..Recorder::default()
        };
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            rules_remove(&session, "0001", false, &mut printer).expect("deletes");
        }
        assert_eq!(
            session
                .deleted_rules
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .len(),
            1
        );
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("deleted rule"), "{out}");
        assert!(
            out.contains("the next identical ask reaches you again"),
            "{out}"
        );
    }

    // --- both modes, on real code paths --------------------------------------
    //
    // The `Recorder` above pins the command bodies' own rules (the pending-card
    // lookup, the unknown key, the empty edit). These run the *same* bodies
    // through the two real halves: an in-process `Embedded` over a memory store,
    // and a `Remote` against the stub brain — no socket but the stub's loopback,
    // which is the crate's documented way to test `--brain`.

    use crate::embedded::{BrainParams, Embedded};
    use crate::remote::Remote;
    use crate::tmp::TempDir;
    use std::collections::BTreeMap;
    use std::path::Path;
    use swarmbot_client::testing::StubBrain;
    use swarmbot_contracts::config::BrainConfig;
    use swarmbot_contracts::id::ConversationId;

    fn embedded_brain(dir: &Path) -> Embedded {
        let config = BrainConfig {
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        };
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

    /// One routine, saved the way the API would save it.
    fn seed_routine(embedded: &Embedded, bot_name: &str) -> swarmbot_contracts::routine::Routine {
        let bot = embedded.swarm.bots.by_name(bot_name).expect("the bot");
        embedded
            .swarm
            .routines
            .create_routine(
                bot.id,
                "sync",
                "summarize the day",
                RoutineSchedule::Interval { every_secs: 60 },
                chrono::Utc::now(),
            )
            .expect("a routine")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn embedded_rules_can_be_listed_and_removed_and_not_removed_twice() {
        let dir = TempDir::new("admin-rules-embedded").expect("temp");
        let embedded = embedded_brain(dir.path());
        let lead = embedded.swarm.bots.by_name("lead").expect("lead");

        // Nothing saved yet: an honest note, not a blank screen.
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            rules_list(&embedded, false, &mut printer).expect("lists");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("no approval rules saved"), "{out}");

        // "Always allow" is the path that writes one.
        let mut request = RuntimeRequest::approval(
            lead.id.clone(),
            lead.direct_conversation_id.clone(),
            Surface::Shell,
            "shell",
            "cargo test",
            None,
        );
        embedded
            .swarm
            .approval
            .raise_async(&mut request)
            .expect("raised");
        let rule = embedded
            .swarm
            .approval
            .allow_always(&lead.id, &request)
            .expect("a rule from the answer");
        assert_eq!(embedded.rules().expect("rules").len(), 1);

        // The listing names it, and the removal is real.
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            let code =
                rules_remove(&embedded, rule.id.as_str(), false, &mut printer).expect("deletes");
            assert_eq!(code, 0);
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("deleted rule"), "{out}");
        assert!(
            embedded.rules().expect("rules").is_empty(),
            "the rule really went"
        );

        // A second removal is refused: "deleted" twice is exactly the kind of
        // unverifiable success a rule id must not produce.
        let mut buffer = Vec::new();
        let mut printer = printer_for(&mut buffer);
        let err = rules_remove(&embedded, rule.id.as_str(), false, &mut printer)
            .expect_err("nothing left to delete");
        assert!(err.to_string().contains("no approval rule"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn embedded_requests_lists_and_answers_a_card_raised_outside_the_repl() {
        let dir = TempDir::new("admin-requests-embedded").expect("temp");
        let embedded = embedded_brain(dir.path());
        let scout = embedded.swarm.bots.by_name("scout").expect("scout");

        // A card in scout's DM — a conversation no `chat` is watching.
        let mut request = RuntimeRequest::approval(
            scout.id.clone(),
            scout.direct_conversation_id.clone(),
            Surface::Shell,
            "shell",
            "cargo check",
            Some("check the build".into()),
        );
        embedded
            .swarm
            .approval
            .raise_async(&mut request)
            .expect("raised");

        // `requests ls` shows it from outside every conversation.
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            requests_list(&embedded, false, &mut printer).expect("lists");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("approval"), "{out}");
        assert!(out.contains("@scout"), "{out}");
        assert!(out.contains("Allow shell `cargo check`?"), "{out}");

        // The answer goes through the one `respond` both modes share.
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            let code = requests_answer(
                &embedded,
                request.id.as_str(),
                &Answer::Decide(Decision::AllowOnce),
                false,
                &mut printer,
            )
            .expect("answers");
            assert_eq!(code, 0);
        }
        let stored = embedded
            .swarm
            .store
            .get_request(&request.id)
            .expect("read")
            .expect("the request");
        assert_eq!(stored.status, RequestStatus::Answered);
        assert_eq!(stored.decision, Some(Decision::AllowOnce));

        // Answering it again is refused, with the pending rule, not a crash.
        let mut buffer = Vec::new();
        let mut printer = printer_for(&mut buffer);
        let err = requests_answer(
            &embedded,
            request.id.as_str(),
            &Answer::Decide(Decision::Deny),
            false,
            &mut printer,
        )
        .expect_err("already answered");
        assert!(err.to_string().contains("no pending request"), "{err}");
    }

    /// `--always` is the one answer with a side effect: it writes a rule. A
    /// command that printed "allowed always" without one would leave the owner
    /// re-answering the same card forever.
    #[tokio::test(flavor = "multi_thread")]
    async fn embedded_answering_always_saves_the_rule_it_claims() {
        let dir = TempDir::new("admin-answer-always").expect("temp");
        let embedded = embedded_brain(dir.path());
        let lead = embedded.swarm.bots.by_name("lead").expect("lead");
        let mut request = RuntimeRequest::approval(
            lead.id.clone(),
            lead.direct_conversation_id.clone(),
            Surface::Shell,
            "shell",
            "cargo test",
            None,
        );
        embedded
            .swarm
            .approval
            .raise_async(&mut request)
            .expect("raised");

        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            requests_answer(
                &embedded,
                request.id.as_str(),
                &Answer::Decide(Decision::AllowAlways),
                false,
                &mut printer,
            )
            .expect("answers");
        }
        let rules = embedded.rules().expect("rules");
        assert_eq!(rules.len(), 1, "the rule the answer promises exists");
        assert_eq!(rules[0].command_prefix.as_deref(), Some("cargo"));
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("allowed always"), "{out}");
        assert!(
            out.contains("rules ls"),
            "it points at where to undo it: {out}"
        );

        // And the json answer carries the decision the brain stored.
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            requests_list(&embedded, true, &mut printer).expect("lists");
        }
        assert!(
            String::from_utf8(buffer).expect("utf8").is_empty(),
            "an answered card is not pending any more"
        );
    }

    /// A question is answered with text and an approval with a decision; the
    /// wrong one for the card is refused *before* the brain is asked.
    #[tokio::test(flavor = "multi_thread")]
    async fn embedded_refuses_the_wrong_kind_of_answer_for_a_card() {
        let dir = TempDir::new("admin-wrong-answer").expect("temp");
        let embedded = embedded_brain(dir.path());
        let lead = embedded.swarm.bots.by_name("lead").expect("lead");
        let mut request = RuntimeRequest::question(
            lead.id.clone(),
            lead.direct_conversation_id.clone(),
            "which environment?",
            Vec::new(),
        );
        embedded
            .swarm
            .approval
            .raise_async(&mut request)
            .expect("raised");

        // A decision on a question settles it with no answer the bot can read.
        let mut buffer = Vec::new();
        let mut printer = printer_for(&mut buffer);
        let err = requests_answer(
            &embedded,
            request.id.as_str(),
            &Answer::Decide(Decision::Deny),
            false,
            &mut printer,
        )
        .expect_err("a question takes text");
        assert!(err.to_string().contains("--answer <text>"), "{err}");
        let stored = embedded
            .swarm
            .store
            .get_request(&request.id)
            .expect("read")
            .expect("the request");
        assert_eq!(
            stored.status,
            RequestStatus::Pending,
            "a refused answer leaves the card pending"
        );

        // The text answer works, and the bot receives it as a string.
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            requests_answer(
                &embedded,
                request.id.as_str(),
                &Answer::Text("staging".to_string()),
                false,
                &mut printer,
            )
            .expect("answers");
        }
        let stored = embedded
            .swarm
            .store
            .get_request(&request.id)
            .expect("read")
            .expect("the request");
        assert_eq!(stored.answer, Some(Value::String("staging".to_string())));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn embedded_config_set_writes_through_and_refuses_what_the_brain_refuses() {
        let dir = TempDir::new("admin-config-embedded").expect("temp");
        let embedded = embedded_brain(dir.path());

        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            config_set(&embedded, "max_hops", "3", false, &mut printer).expect("sets");
        }
        // The live handle sees it: `config show` and every routing decision read
        // this same value, so "set" is not a write to a file nobody re-reads.
        assert_eq!(embedded.swarm.settings.get().max_hops, 3);
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("max_hops = 3"), "{out}");

        // A budget of zero is the brain's own 400-class refusal, in its words.
        let mut buffer = Vec::new();
        let mut printer = printer_for(&mut buffer);
        let err = config_set(&embedded, "max_tokens_per_turn", "0", false, &mut printer)
            .expect_err("zero is not a budget");
        let text = format!("{err:#}");
        assert!(text.contains("max_tokens_per_turn"), "{text}");
        assert!(text.contains("null"), "{text}");
        assert_eq!(
            embedded.swarm.settings.get().max_tokens_per_turn,
            None,
            "a refused value changed nothing"
        );

        // An unknown key is refused before the settings are touched at all.
        let mut buffer = Vec::new();
        let mut printer = printer_for(&mut buffer);
        let err =
            config_set(&embedded, "max_hp", "3", false, &mut printer).expect_err("unknown key");
        assert!(
            err.to_string().contains("unknown setting `max_hp`"),
            "{err}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn embedded_routines_pause_resume_run_and_delete() {
        let dir = TempDir::new("admin-routines-embedded").expect("temp");
        let embedded = embedded_brain(dir.path());
        let routine = seed_routine(&embedded, "lead");

        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            routines_list(&embedded, false, &mut printer).expect("lists");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("sync"), "{out}");
        assert!(out.contains("every 60s"), "{out}");
        assert!(out.contains("active"), "{out}");

        // Pause, then resume: the next run comes back.
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            routines_change(
                &embedded,
                RoutineAction::Pause,
                routine.id.as_ref(),
                false,
                &mut printer,
            )
            .expect("pauses");
        }
        let paused = embedded
            .swarm
            .routines
            .get_routine(&routine.id)
            .expect("read")
            .expect("the routine");
        assert_eq!(
            paused.status,
            swarmbot_contracts::routine::RoutineStatus::Paused
        );
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("paused routine"), "{out}");

        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            routines_change(
                &embedded,
                RoutineAction::Resume,
                routine.id.as_ref(),
                false,
                &mut printer,
            )
            .expect("resumes");
        }
        let resumed = embedded
            .swarm
            .routines
            .get_routine(&routine.id)
            .expect("read")
            .expect("the routine");
        assert_eq!(
            resumed.status,
            swarmbot_contracts::routine::RoutineStatus::Active
        );
        assert!(
            resumed.next_run_at.is_some(),
            "a resumed interval has a next run"
        );
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("resumed routine"), "{out}");

        // Runs: none yet, and the note says so rather than printing an empty list.
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            routines_runs(&embedded, routine.id.as_ref(), 20, false, &mut printer)
                .expect("lists runs");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("has not run yet"), "{out}");

        // Delete, and then the id is gone for good.
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            routines_change(
                &embedded,
                RoutineAction::Delete,
                routine.id.as_ref(),
                false,
                &mut printer,
            )
            .expect("deletes");
        }
        assert!(
            embedded
                .swarm
                .routines
                .get_routine(&routine.id)
                .expect("read")
                .is_none()
        );
        let mut buffer = Vec::new();
        let mut printer = printer_for(&mut buffer);
        let err = routines_change(
            &embedded,
            RoutineAction::Delete,
            routine.id.as_ref(),
            false,
            &mut printer,
        )
        .expect_err("already deleted");
        assert!(err.to_string().contains("no routine"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn embedded_nodes_revoke_refuses_the_local_node_and_names_the_way_out() {
        let dir = TempDir::new("admin-nodes-embedded").expect("temp");
        let embedded = embedded_brain(dir.path());
        let local = embedded
            .nodes()
            .expect("nodes")
            .into_iter()
            .next()
            .expect("the local node");

        let mut buffer = Vec::new();
        let mut printer = printer_for(&mut buffer);
        let err = nodes_revoke(&embedded, local.id.as_str(), false, &mut printer)
            .expect_err("the local node cannot be revoked");
        let text = err.to_string();
        assert!(text.contains("local node cannot be revoked"), "{text}");
        assert!(
            text.contains("--brain"),
            "it names the mode that can: {text}"
        );
        assert!(
            String::from_utf8(buffer).expect("utf8").is_empty(),
            "a refusal prints no success line"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn embedded_bots_edit_changes_the_fields_and_says_what_it_changed() {
        let dir = TempDir::new("admin-bots-embedded").expect("temp");
        let embedded = embedded_brain(dir.path());

        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            bots_edit(
                &embedded,
                "scout",
                &crate::session::BotEdit {
                    model: Some("m-2".to_string()),
                    skills: Some(vec!["rust".to_string(), "git".to_string()]),
                    ..crate::session::BotEdit::default()
                },
                false,
                &mut printer,
            )
            .expect("edits");
        }
        let scout = embedded.swarm.bots.by_name("scout").expect("scout");
        assert_eq!(scout.model.as_deref(), Some("m-2"));
        assert_eq!(scout.skills, vec!["rust".to_string(), "git".to_string()]);
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("@scout"), "{out}");
        assert!(out.contains("next turn"), "it says when it applies: {out}");

        // `--disabled` really stops it being woken, and `--enabled` brings it back.
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            bots_edit(
                &embedded,
                "scout",
                &crate::session::BotEdit {
                    enabled: Some(false),
                    ..crate::session::BotEdit::default()
                },
                false,
                &mut printer,
            )
            .expect("disables");
        }
        assert_eq!(
            embedded.swarm.bots.by_name("scout").expect("scout").status,
            swarmbot_contracts::bot::BotStatus::Disabled
        );
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            bots_edit(
                &embedded,
                "scout",
                &crate::session::BotEdit {
                    enabled: Some(true),
                    ..crate::session::BotEdit::default()
                },
                false,
                &mut printer,
            )
            .expect("enables");
        }
        assert_eq!(
            embedded.swarm.bots.by_name("scout").expect("scout").status,
            swarmbot_contracts::bot::BotStatus::Idle
        );

        // An empty model string goes back to the backend's default, the same rule
        // the brain's PATCH documents.
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            bots_edit(
                &embedded,
                "scout",
                &crate::session::BotEdit {
                    model: Some(String::new()),
                    ..crate::session::BotEdit::default()
                },
                false,
                &mut printer,
            )
            .expect("clears the model");
        }
        assert_eq!(
            embedded.swarm.bots.by_name("scout").expect("scout").model,
            None
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn embedded_status_counts_what_the_store_knows_and_never_invents_a_zero() {
        let dir = TempDir::new("admin-status-embedded").expect("temp");
        let embedded = embedded_brain(dir.path());

        let report = StatusReport::collect(&embedded).expect("collects");
        assert_eq!(
            report.bots_total,
            embedded.swarm.bots.list().expect("bots").len()
        );
        assert!(report.bots_total >= 2, "the demo team: {report:?}");
        assert!(
            report.live.is_some(),
            "an in-process brain reads its own counters"
        );
        assert!(
            report.unknown.is_empty(),
            "nothing is unknown in-process: {report:?}"
        );
        let lines = report.to_lines();
        assert!(lines[0].starts_with("brain: "), "{lines:?}");
        assert!(
            lines.iter().any(|line| line.contains("the team is idle")),
            "a quiet brain says so: {lines:?}"
        );

        // The JSON shape is one whole object, and its live block is there.
        let value = serde_json::to_value(&report).expect("serializes");
        assert!(value["bots_total"].is_number(), "{value}");
        assert!(value["live"].is_object(), "{value}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn embedded_transcript_pages_backwards_and_exports_every_line() {
        let dir = TempDir::new("admin-transcript-embedded").expect("temp");
        let embedded = embedded_brain(dir.path());
        let lead = embedded.swarm.bots.by_name("lead").expect("lead");
        let conversation = lead.direct_conversation_id.clone();
        for seq in 1..=5u64 {
            let entry = swarmbot_contracts::entry::Entry::owner_message(
                conversation.clone(),
                seq,
                format!("line {seq}"),
            );
            embedded.swarm.store.upsert_entry(&entry).expect("seeded");
        }

        // Backwards from 6: the last three before it, ascending.
        let page = embedded.entries_before(&conversation, 6, 3).expect("pages");
        let seqs: Vec<u64> = page.iter().map(|entry| entry.seq).collect();
        assert_eq!(seqs, vec![3, 4, 5], "{seqs:?}");

        // From 3: nothing at or after it.
        let page = embedded
            .entries_before(&conversation, 3, 10)
            .expect("pages");
        let seqs: Vec<u64> = page.iter().map(|entry| entry.seq).collect();
        assert_eq!(seqs, vec![1, 2], "{seqs:?}");

        // The export is every line, ascending, one object each.
        let text = embedded.export_transcript(&conversation).expect("exports");
        let objects = json_objects(&text);
        let seqs: Vec<u64> = objects
            .iter()
            .map(|object| object["seq"].as_u64().expect("a seq"))
            .collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5], "{seqs:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn embedded_secrets_are_names_only_and_the_file_is_private() {
        let dir = TempDir::new("admin-secrets-embedded").expect("temp");
        let embedded = embedded_brain(dir.path());

        // The demo config's fakes name no `api_key_env`, so there is nothing yet.
        assert!(
            embedded.secrets().expect("lists").is_empty(),
            "a fresh data dir holds no keys"
        );

        // Write one, through the same call the command makes.
        let written = embedded
            .set_secret("DEMO_API_KEY", "sk-not-a-real-key")
            .expect("writes");
        assert_eq!(written.name, "DEMO_API_KEY");
        assert_eq!(
            written.applies, APPLIES_RESTART,
            "a file write cannot reach a registry built at boot"
        );

        let listed = embedded.secrets().expect("lists");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "DEMO_API_KEY");
        // The listing is a name, never a value.
        let rendered = keys_lines(&listed).join("\n");
        assert!(!rendered.contains("sk-not-a-real-key"), "{rendered}");
        assert!(rendered.contains("DEMO_API_KEY"), "{rendered}");

        // The file is 0600, and its bytes are the value (which is why nothing
        // prints it).
        let path = secret_file(dir.path());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a key file anybody can read is a leak");
        }
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("sk-not-a-real-key"), "the value is stored");

        // Removing it is real, and a second removal says there is nothing there.
        embedded.delete_secret("DEMO_API_KEY").expect("removes");
        assert!(embedded.secrets().expect("lists").is_empty());
        let err = embedded
            .delete_secret("DEMO_API_KEY")
            .expect_err("nothing left");
        assert!(err.to_string().contains("no key called"), "{err}");
    }

    /// A read-only in-process brain opened the store but started nothing, so a
    /// bare `0 live runners` would read as "the team has no runners". The report
    /// says which it is — that is the whole difference between `None`, `booted:
    /// false` and a true zero.
    #[test]
    fn a_read_only_brain_reports_that_it_started_nothing_rather_than_zero_runners() {
        let dir = TempDir::new("admin-status-readonly").expect("temp");
        let config = BrainConfig {
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        };
        // Composed but **not** booted — what `bots ls` and `status` build.
        let embedded = Embedded::build(
            BrainParams {
                config,
                data_dir: dir.path().to_path_buf(),
                roots: Vec::new(),
                secrets: BTreeMap::new(),
                offline: true,
            },
            false,
        )
        .expect("composes");
        let live = embedded.live_counts().expect("the store is readable");
        assert_eq!(live.runners, 0);
        assert!(
            !live.booted,
            "nothing was booted, so the zero is about this process"
        );
        let lines = StatusReport::collect(&embedded)
            .expect("collects")
            .to_lines();
        assert!(
            lines.iter().any(|line| line.contains("started nothing")),
            "it says why there are no runners: {lines:?}"
        );
        assert!(
            lines.iter().all(|line| !line.contains("live runners: 0")),
            "it must not print a bare zero instead: {lines:?}"
        );
    }

    /// The bytes an embedded export writes and the bytes a daemon export serves
    /// are the same shape: one `Entry` object per line, ascending. That is what
    /// makes `transcript --out` comparable across modes.
    #[tokio::test(flavor = "multi_thread")]
    async fn both_modes_export_the_same_byte_shape() {
        let dir = TempDir::new("admin-export-both").expect("temp");
        let embedded = embedded_brain(dir.path());
        let lead = embedded.swarm.bots.by_name("lead").expect("lead");
        let conversation = lead.direct_conversation_id.clone();
        let entry =
            swarmbot_contracts::entry::Entry::owner_message(conversation.clone(), 1, "hello");
        embedded.swarm.store.upsert_entry(&entry).expect("seeded");
        let local = embedded.export_transcript(&conversation).expect("exports");
        assert_eq!(local.lines().count(), 1, "{local}");

        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), Path::new("."), false).expect("connects");
        let id = ConversationId::from_suffix("000000000000000000000000000000c1");
        let served = remote.export_transcript(&id).expect("exports");
        let lines: Vec<&str> = served.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "the stub serves two canned entries: {served}"
        );
        for line in lines {
            let value: Value = serde_json::from_str(line).expect("one object per line");
            assert!(value["id"].is_string(), "{value}");
            assert!(value["seq"].is_number(), "{value}");
        }
        // The remote half really called the brain's export route.
        assert!(
            stub.last_request("GET", &format!("/api/conversations/{id}/export"))
                .is_some()
        );
    }

    // --- the stub brain, which is how `--brain` is tested ---------------------

    #[test]
    fn remote_rules_list_and_remove_go_over_the_wire() {
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), Path::new("."), false).expect("connects");

        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            rules_list(&remote, false, &mut printer).expect("lists");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("allow"), "{out}");
        assert!(out.contains("global"), "{out}");
        assert!(stub.last_request("GET", "/api/rules").is_some());

        // A rule id the stub does not serve is refused before any DELETE: the
        // resolver only offers what the brain actually holds.
        let mut buffer = Vec::new();
        let mut printer = printer_for(&mut buffer);
        let err = rules_remove(
            &remote,
            "rul_000000000000000000000000000000ff",
            false,
            &mut printer,
        )
        .expect_err("not a rule the brain has");
        assert!(err.to_string().contains("no approval rule"), "{err}");
        assert!(
            stub.last_request(
                "DELETE",
                &format!("/api/rules/{}", "rul_000000000000000000000000000000ff")
            )
            .is_none(),
            "nothing was deleted over the wire"
        );
    }

    #[test]
    fn remote_requests_list_and_answer_reach_the_brains_route() {
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), Path::new("."), false).expect("connects");

        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            requests_list(&remote, false, &mut printer).expect("lists");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("approval"), "{out}");
        assert!(out.contains("@stub"), "{out}");

        // The stub's canned request is answered over the documented route, with
        // the decision in the body.
        let pending = remote.pending().expect("pending");
        let request = pending.first().expect("the stub serves one");
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            requests_answer(
                &remote,
                request.id.as_str(),
                &Answer::Decide(Decision::AllowOnce),
                false,
                &mut printer,
            )
            .expect("answers");
        }
        let recorded = stub
            .last_request("POST", &format!("/api/requests/{}/respond", request.id))
            .expect("recorded");
        assert!(recorded.body.contains("allow_once"), "{}", recorded.body);
        assert_eq!(recorded.auth.as_deref(), Some("Bearer tok"));
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("answered"), "{out}");
    }

    /// Every list command's `--json` is one object per line on stdout and nothing
    /// else — the same contract `run --json` and `transcript --json` keep, so a
    /// script can pipe any of them into `jq` without a special case.
    #[test]
    fn every_json_list_is_one_object_per_line() {
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), Path::new("."), false).expect("connects");

        // Each command is run with `json = true` and its stdout parsed as NDJSON.
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            rules_list(&remote, true, &mut printer).expect("rules");
        }
        let rules = json_objects(&String::from_utf8(buffer).expect("utf8"));
        assert_eq!(rules.len(), 1, "{rules:?}");
        assert!(rules[0]["id"].is_string(), "{rules:?}");
        assert_eq!(rules[0]["effect"], "allow", "{rules:?}");

        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            routines_list(&remote, true, &mut printer).expect("routines");
        }
        let routines = json_objects(&String::from_utf8(buffer).expect("utf8"));
        assert_eq!(routines.len(), 1, "{routines:?}");
        assert_eq!(routines[0]["name"], "Nightly build", "{routines:?}");
        assert_eq!(routines[0]["status"], "active", "{routines:?}");

        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            requests_list(&remote, true, &mut printer).expect("requests");
        }
        let requests = json_objects(&String::from_utf8(buffer).expect("utf8"));
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0]["kind"], "approval", "{requests:?}");
        assert_eq!(requests[0]["status"], "pending", "{requests:?}");

        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            let id = remote.routines().expect("routines")[0].id.clone();
            routines_runs(&remote, id.as_str(), 10, true, &mut printer).expect("runs");
        }
        let runs = json_objects(&String::from_utf8(buffer).expect("utf8"));
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0]["status"], "success", "{runs:?}");

        // No colour escapes and no prose on stdout: every line is one object.
        for text in [
            serde_json::to_string(&rules).expect("rules"),
            serde_json::to_string(&routines).expect("routines"),
            serde_json::to_string(&requests).expect("requests"),
        ] {
            assert!(!text.contains('\u{1b}'), "{text}");
        }
    }

    /// `config set`'s single-answer JSON is one whole `Settings` object — the
    /// same shape `GET /api/settings` returns, so a script can read a key out of
    /// it without a second call.
    #[test]
    fn config_set_json_is_one_whole_settings_object() {
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), Path::new("."), false).expect("connects");
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            config_set(&remote, "max_hops", "3", true, &mut printer).expect("sets");
        }
        let objects = json_objects(&String::from_utf8(buffer).expect("utf8"));
        assert_eq!(objects.len(), 1, "one object, not a stream: {objects:?}");
        assert_eq!(objects[0]["max_hops"], 3, "{objects:?}");
        assert!(objects[0]["bot_messaging"].is_boolean(), "{objects:?}");
    }

    #[test]
    fn remote_config_set_posts_one_patch_and_prints_the_answer() {
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), Path::new("."), false).expect("connects");

        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            config_set(&remote, "max_hops", "3", false, &mut printer).expect("sets");
        }
        let recorded = stub
            .last_request("PATCH", "/api/settings")
            .expect("the patch reached the brain");
        let body: Value = serde_json::from_str(&recorded.body).expect("a JSON body");
        assert_eq!(body["max_hops"], 3, "{body}");
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("max_hops = 3"), "{out}");
        assert!(out.contains(&stub.url()), "it names the brain: {out}");
    }

    #[test]
    fn remote_routines_read_and_change_through_the_routes() {
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), Path::new("."), false).expect("connects");

        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            routines_list(&remote, false, &mut printer).expect("lists");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("Nightly build"), "{out}");
        assert!(out.contains("every 3600s"), "{out}");
        assert!(out.contains("active"), "{out}");
        assert!(stub.last_request("GET", "/api/routines").is_some());

        let id = remote.routines().expect("routines")[0].id.clone();
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            routines_change(
                &remote,
                RoutineAction::Pause,
                id.as_str(),
                false,
                &mut printer,
            )
            .expect("pauses");
        }
        assert!(
            stub.last_request("POST", &format!("/api/routines/{id}/pause"))
                .is_some()
        );
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("paused routine"), "{out}");

        // Runs are one row each, from the brain's own list.
        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            routines_runs(&remote, id.as_str(), 10, false, &mut printer).expect("lists runs");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("success"), "{out}");
        assert!(
            stub.last_request("GET", &format!("/api/routines/{id}/runs"))
                .is_some()
        );
    }

    #[test]
    fn remote_nodes_revoke_and_edit_bot_take_the_documented_routes() {
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), Path::new("."), false).expect("connects");

        // An id that is not on the roster is refused before any DELETE is sent.
        // (The stub answers *every* DELETE with 204, so the happy path of a remote
        // revoke is not distinguishable here; the brain's own route tests cover
        // it, and this CLI calls the documented `DELETE /api/nodes/{id}`.)
        let mut buffer = Vec::new();
        let mut printer = printer_for(&mut buffer);
        let err = nodes_revoke(
            &remote,
            "nod_000000000000000000000000000000ff",
            false,
            &mut printer,
        )
        .expect_err("not a node the brain has");
        assert!(err.to_string().contains("no node"), "{err}");
        assert!(
            stub.last_request("DELETE", "/api/nodes/nod_000000000000000000000000000000ff")
                .is_none(),
            "nothing was sent for an id the roster does not hold"
        );

        // `bots edit` sends a PATCH with only the fields that were named.
        let bot = remote.edit_bot(
            "stub",
            &crate::session::BotEdit {
                model: Some("m-9".to_string()),
                ..crate::session::BotEdit::default()
            },
        );
        bot.expect("edits");
        let recorded = stub
            .last_request("PATCH", "/api/bots/stub")
            .expect("the patch reached the brain");
        let body: Value = serde_json::from_str(&recorded.body).expect("a JSON body");
        assert_eq!(body["model"], "m-9", "{body}");
        assert!(
            body.get("enabled").is_none(),
            "an unnamed field is not sent: {body}"
        );
    }

    #[test]
    fn remote_status_reads_the_daemons_metrics_and_says_so() {
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), Path::new("."), false).expect("connects");

        let report = StatusReport::collect(&remote).expect("collects");
        // The counters come from `GET /api/metrics`, which the stub really serves.
        let live = report.live.expect("the daemon can answer its own counters");
        assert_eq!(live.runners, 1, "{live:?}");
        assert_eq!(live.turns_running, 0, "{live:?}");
        assert!(
            report.unknown.is_empty(),
            "a daemon with a metrics route has nothing unknown: {report:?}"
        );

        let mut buffer = Vec::new();
        {
            let mut printer = printer_for(&mut buffer);
            status(&remote, false, &mut printer).expect("prints");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("── status ──"), "{out}");
        assert!(out.contains("bots 1 total"), "{out}");
        assert!(out.contains("turns still marked running: 0"), "{out}");
    }

    #[test]
    fn remote_secrets_list_names_and_never_a_value() {
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), Path::new("."), false).expect("connects");

        let secrets = remote.secrets().expect("lists");
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0].name, "INFERHUB_API_KEY");
        assert_eq!(secrets[0].used_by, vec!["inferhub".to_string()]);

        // A write answers the brain's own word about when it applies.
        let written = remote
            .set_secret("INFERHUB_API_KEY", "sk-a-value")
            .expect("writes");
        assert_eq!(written.name, "INFERHUB_API_KEY");
        assert_eq!(written.applies, APPLIES_RESTART, "never a cheerful hot");
        let recorded = stub
            .last_request("PUT", "/api/secrets")
            .expect("the write reached the brain");
        assert!(
            recorded.body.contains("sk-a-value"),
            "the value is sent once"
        );
        // … and the *answer* carries no value at all.
        let answer: Value =
            serde_json::from_str(&serde_json::to_string(&written).expect("serializes"))
                .expect("an object");
        assert!(!answer.to_string().contains("sk-a-value"), "{answer}");

        let removed = remote.delete_secret("INFERHUB_API_KEY").expect("removes");
        assert_eq!(removed.applies, APPLIES_RESTART);
        assert!(
            stub.last_request("DELETE", "/api/secrets/INFERHUB_API_KEY")
                .is_some()
        );
    }

    /// The one true thing about the secret *file* writer: it is atomic and
    /// private. Tested on a temp dir, with the mode checked on unix.
    #[test]
    fn the_secret_file_is_rewritten_atomically_and_stays_private() {
        let dir = TempDir::new("admin-secret-file").expect("temp");
        let data_dir = dir.path().to_path_buf();
        write_secret(&data_dir, "A", Some("one")).expect("first");
        write_secret(&data_dir, "B", Some("two")).expect("second");
        let path = secret_file(&data_dir);
        let body = std::fs::read_to_string(&path).expect("read");
        let parsed: std::collections::BTreeMap<String, String> =
            serde_json::from_str(&body).expect("a map");
        assert_eq!(parsed.get("A").map(String::as_str), Some("one"));
        assert_eq!(parsed.get("B").map(String::as_str), Some("two"));

        // No temp file is left behind after a clean write.
        let leftovers: Vec<String> = std::fs::read_dir(&data_dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");

        // An empty value is refused rather than written: "" is not a key.
        let err = write_secret(&data_dir, "C", Some("")).expect_err("empty");
        assert!(err.to_string().contains("cannot be empty"), "{err}");
        // Removing a name this process has in its environment is refused with
        // the reason, not with a cheerful "removed".
        assert!(
            std::env::var("PATH").is_ok(),
            "the test needs one variable that exists"
        );
        let err = write_secret(&data_dir, "PATH", None).expect_err("environment");
        assert!(err.to_string().contains("environment"), "{err}");
    }
}
