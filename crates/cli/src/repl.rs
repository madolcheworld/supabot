//! `swarmbot chat`: the REPL.
//!
//! The design constraint that shapes this file: **reading stdin must never
//! stall the event stream**. A blocking `read_line` on the async runtime
//! would freeze rendering while the human thinks, so stdin lives on a
//! dedicated `spawn_blocking` thread that hands lines over a channel and then
//! waits for an acknowledgement carrying the next prompt. The select loop stays
//! free to render events, track approval cards and answer them inline.
//!
//! The card flow: a pending `RequestUpsert` for this conversation becomes the
//! *input mode* — the next line is interpreted as the answer (`y`/`a`/`n`,
//! empty leaves it pending) instead of a message, and the prompt says so.
//!
//! Everything here is written against [`Session`], so one loop serves both an
//! in-process brain and a remote one: the card keys, the countdown a card
//! renders with, the `/stop` semantics and the "queued behind other work" note
//! are the same code, not two implementations that agree today. The input side
//! is behind [`Lines`] for the same reason in the small: production reads the
//! terminal, a test scripts the keystrokes.

use std::io::{IsTerminal, Write};

use anyhow::{Context, Result};
use serde_json::Value;
use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::id::ConversationId;
use swarmbot_contracts::live::LiveEvent;
use swarmbot_contracts::request::{Decision, RequestKind, RuntimeRequest};
use swarmbot_contracts::task::Task;

use crate::embedded::emit_event;
use crate::render::{Printer, Roster};
use crate::resolve::Target;
use crate::session::{Session, drain_synced_honest};

/// A local slash command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Bots,
    Groups,
    Tasks,
    Stop,
    New,
    Quit,
    Help,
    Unknown(String),
}

/// What one input line is: a command or a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Command(Command),
    Message(String),
}

/// Split a line into a command or a message. An empty line is a `Message("")`,
/// which the loop ignores (outside a card, where it means "leave it pending").
pub fn classify(line: &str) -> Input {
    let trimmed = line.trim();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return Input::Message(line.trim_end().to_string());
    };
    let name = rest.split_whitespace().next().unwrap_or("");
    let command = match name {
        "bots" => Command::Bots,
        "groups" => Command::Groups,
        "tasks" => Command::Tasks,
        "stop" => Command::Stop,
        "new" => Command::New,
        "quit" | "exit" => Command::Quit,
        "help" | "?" => Command::Help,
        other => Command::Unknown(other.to_string()),
    };
    Input::Command(command)
}

/// What a line means while an approval card is waiting.
#[derive(Clone, Debug, PartialEq)]
pub enum CardAction {
    /// Answer the request (an approval decision, or free text for a question).
    Answer {
        decision: Option<Decision>,
        answer: Option<Value>,
    },
    /// Empty input: the card stays pending and the bot keeps waiting.
    LeavePending,
    /// Something that is not an answer; the hint is printed and the card
    /// stays pending.
    Hint(&'static str),
}

/// `y` = allow once, `a` = always allow, `n` = deny, empty = leave pending —
/// for approvals. Any other request kind takes free text as its answer.
pub fn classify_card_input(request: &RuntimeRequest, line: &str) -> CardAction {
    let answer = line.trim();
    if request.kind == RequestKind::Approval {
        return match answer.to_ascii_lowercase().as_str() {
            "" => CardAction::LeavePending,
            "y" | "yes" => CardAction::Answer {
                decision: Some(Decision::AllowOnce),
                answer: None,
            },
            "a" | "always" => CardAction::Answer {
                decision: Some(Decision::AllowAlways),
                answer: None,
            },
            "n" | "no" | "deny" => CardAction::Answer {
                decision: Some(Decision::Deny),
                answer: None,
            },
            _ => CardAction::Hint(
                "answer y (allow once), a (always allow), n (deny) — or empty to leave it pending",
            ),
        };
    }
    if answer.is_empty() {
        CardAction::LeavePending
    } else {
        CardAction::Answer {
            decision: None,
            answer: Some(Value::String(answer.to_string())),
        }
    }
}

/// Send a card's answer through the session's `respond` — one call, the same
/// sentence in both modes.
pub fn answer_card<S: Session>(
    session: &S,
    request: &RuntimeRequest,
    action: &CardAction,
) -> Result<RuntimeRequest> {
    match action {
        CardAction::Answer { decision, answer } => {
            session.respond(&request.id, *decision, answer.clone())
        }
        CardAction::LeavePending | CardAction::Hint(_) => Ok(request.clone()),
    }
}

/// Where the REPL's input lines come from: a channel of lines, and an
/// acknowledgement channel that carries the next prompt back to the reader.
///
/// Production builds one with [`Lines::terminal`], which starts a dedicated
/// blocking thread on stdin. The fields are crate-visible so a test can drive
/// the *same* loop with its own sender — a test has no terminal to read, and
/// reading the real one would steal the developer's keyboard.
pub struct Lines {
    /// `None` on the channel means end of input (Ctrl-D, or a dead reader).
    rx: tokio::sync::mpsc::UnboundedReceiver<Option<String>>,
    ack: std::sync::mpsc::Sender<String>,
}

impl Lines {
    /// Start the terminal reader, prompting with `initial_prompt`.
    pub fn terminal(initial_prompt: String) -> Self {
        let (line_tx, rx) = tokio::sync::mpsc::unbounded_channel::<Option<String>>();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel::<String>();
        // Prompts are for humans at a keyboard; a piped stdin (a script driving
        // the chat) gets clean output with no prompt bytes interleaved.
        let show_prompt = std::io::stdin().is_terminal();
        tokio::task::spawn_blocking(move || {
            stdin_reader(initial_prompt, show_prompt, line_tx, ack_rx);
        });
        Self { rx, ack: ack_tx }
    }

    /// The next line, or `None` for "no more input".
    async fn next(&mut self) -> Option<String> {
        match self.rx.recv().await {
            Some(Some(line)) => Some(line),
            _ => None,
        }
    }

    /// Tell the reader which prompt to print next. `false` means the reader is
    /// gone and the loop should stop.
    fn acknowledge(&self, prompt: String) -> bool {
        self.ack.send(prompt).is_ok()
    }
}

/// The chat loop. Returns the process exit code.
pub async fn chat<W: Write, S: Session>(
    session: &S,
    target: &Target,
    printer: &mut Printer<W>,
) -> Result<i32> {
    chat_with(session, target, printer, Lines::terminal).await
}

/// The chat, with the input source supplied by the caller: the terminal in
/// production, a channel a test drives itself. Everything else — the snapshot,
/// the card tracking, the commands — is the same code either way.
async fn chat_with<W: Write, S: Session, F: FnOnce(String) -> Lines>(
    session: &S,
    target: &Target,
    printer: &mut Printer<W>,
    source: F,
) -> Result<i32> {
    let conversation = target.conversation_id().clone();
    printer.heading(&format!("── chat: {} ──", target.describe()))?;
    printer.note("commands: /bots /groups /tasks /stop /new /quit — anything else is a message")?;

    let mut rx = session.subscribe(Some(conversation.clone()))?;
    let mut roster = Roster::new(
        session.bots()?,
        session.conversations()?,
        Some(conversation.clone()),
    );
    // A chat shows where it lands: the snapshot's tail is the history.
    drain_synced_honest(session, &mut rx, printer, &mut roster, false, true).await?;

    // A card raised before this chat started is still pending — pick it up so
    // the input mode is right from the first line.
    let mut pending_card = session
        .pending_in(&conversation)?
        .into_iter()
        .find(|request| request.is_pending());

    let lines = source(prompt_for(pending_card.is_some()));
    chat_loop(
        session,
        &conversation,
        target,
        printer,
        &mut roster,
        &mut pending_card,
        rx,
        lines,
    )
    .await
}

/// The loop itself, split out from the terminal plumbing so a test can drive it
/// with [`Lines::scripted`].
#[allow(clippy::too_many_arguments)]
async fn chat_loop<W: Write, S: Session>(
    session: &S,
    conversation: &ConversationId,
    target: &Target,
    printer: &mut Printer<W>,
    roster: &mut Roster,
    pending_card: &mut Option<RuntimeRequest>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<LiveEvent>,
    mut lines: Lines,
) -> Result<i32> {
    let mut quit = false;
    // A stream that ended is a fact the user hears once, and then the chat keeps
    // going: a remote brain that closed the socket (it is stopping, or a proxy
    // gave up) should not silently kill the prompt somebody is typing into.
    let mut stream_open = true;
    let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());
    while !quit {
        tokio::select! {
            maybe_line = lines.next() => {
                match maybe_line {
                    Some(line) => {
                        quit = handle_input(
                            session, conversation, target, printer, pending_card, &line,
                        ).await?;
                        // The prompt is the acknowledgement: the reader prints
                        // it only after this line was fully handled, so output
                        // and prompts cannot interleave. On quit there is no
                        // ack — dropping the channel is what tells the reader
                        // no further prompt is wanted.
                        if !quit && !lines.acknowledge(prompt_for(pending_card.is_some())) {
                            break;
                        }
                    }
                    // EOF (Ctrl-D), an exhausted script, or the reader died: an
                    // explicit, clean end.
                    None => break,
                }
            }
            event = rx.recv(), if stream_open => {
                match event {
                    Some(event) => {
                        track_card(&event, conversation, pending_card);
                        emit_event(printer, roster, &event, false, false)?;
                    }
                    None => {
                        stream_open = false;
                        printer.note(
                            "the event stream ended — no further updates will arrive here",
                        )?;
                    }
                }
            }
            _ = &mut ctrl_c => {
                printer.note("interrupted — ending the chat")?;
                break;
            }
        }
    }

    printer.finish()?;
    // A stream that died for a reason the session knows (a token that stopped
    // working, a frame this build cannot read) is that command's failure, not a
    // clean exit: the transcript on screen is incomplete and must say so.
    if let Some(reason) = session.stream_error() {
        return Err(anyhow::anyhow!("{reason}"));
    }
    // The brain's lifecycle belongs to the caller: `cmd_chat` shuts an embedded
    // one down after this returns, and leaves a remote daemon running, because
    // a chat that ends is not a request to stop somebody else's brain.
    Ok(0)
}

/// Keep the input mode in sync with the request stream: a pending card in
/// this conversation takes over the next line; settling it hands the line
/// back to messages.
fn track_card(
    event: &LiveEvent,
    conversation: &ConversationId,
    pending_card: &mut Option<RuntimeRequest>,
) {
    let LiveEvent::RequestUpsert { request, .. } = event else {
        return;
    };
    if request.conversation_id != *conversation {
        return;
    }
    if request.is_pending() {
        *pending_card = Some(request.clone());
    } else if pending_card
        .as_ref()
        .is_some_and(|card| card.id == request.id)
    {
        *pending_card = None;
    }
}

fn prompt_for(card_pending: bool) -> String {
    if card_pending {
        "answer › ".to_string()
    } else {
        "you › ".to_string()
    }
}

/// The blocking half: prompt, read, hand over, wait for the ack that carries
/// the next prompt. Runs on the blocking pool, so a thinking human never
/// stalls the renderer.
fn stdin_reader(
    initial_prompt: String,
    show_prompt: bool,
    line_tx: tokio::sync::mpsc::UnboundedSender<Option<String>>,
    ack_rx: std::sync::mpsc::Receiver<String>,
) {
    let stdin = std::io::stdin();
    let mut prompt = initial_prompt;
    loop {
        if show_prompt {
            print!("{prompt}");
            // A print! that never flushes is a prompt nobody sees.
            let _ = std::io::stdout().flush();
        }
        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                // EOF: tell the loop and stop.
                let _ = line_tx.send(None);
                return;
            }
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
                if line_tx.send(Some(trimmed)).is_err() {
                    return;
                }
                match ack_rx.recv() {
                    Ok(next) => prompt = next,
                    // The chat loop is gone; nothing left to read for.
                    Err(_) => return,
                }
            }
            Err(_) => {
                let _ = line_tx.send(None);
                return;
            }
        }
    }
}

/// Handle one input line. Returns `true` when the REPL should quit.
#[allow(clippy::too_many_arguments)]
async fn handle_input<W: Write, S: Session>(
    session: &S,
    conversation: &ConversationId,
    target: &Target,
    printer: &mut Printer<W>,
    pending_card: &mut Option<RuntimeRequest>,
    line: &str,
) -> Result<bool> {
    match classify(line) {
        Input::Command(command) => {
            handle_command(session, conversation, target, printer, &command).await
        }
        Input::Message(text) => {
            if let Some(request) = pending_card.clone() {
                match classify_card_input(&request, &text) {
                    CardAction::Answer { decision, answer } => {
                        let settled = answer_card(
                            session,
                            &request,
                            &CardAction::Answer { decision, answer },
                        )
                        .context("cannot answer the request")?;
                        printer.note(&format!(
                            "request {}: {}",
                            settled.status.status_label(),
                            decision
                                .map(|decision| format!(" ({})", decision_label(decision)))
                                .unwrap_or_default()
                        ))?;
                        *pending_card = None;
                    }
                    CardAction::LeavePending => {
                        printer.note("left pending — the bot keeps waiting")?;
                    }
                    CardAction::Hint(hint) => printer.note(hint)?,
                }
                return Ok(false);
            }
            if text.trim().is_empty() {
                return Ok(false);
            }
            match session.send(conversation, &text) {
                // A send that fails is a note, not an error that ends the chat:
                // the human is sitting there and can retry, and losing the
                // session because one message did not land is worse.
                Ok(sent) => {
                    if !sent.queued.is_empty() {
                        printer.note(&format!(
                            "queued behind other work: {}",
                            sent.queued.join(", ")
                        ))?;
                    }
                }
                Err(err) => printer.note(&format!("cannot send: {err}"))?,
            }
            Ok(false)
        }
    }
}

async fn handle_command<W: Write, S: Session>(
    session: &S,
    conversation: &ConversationId,
    target: &Target,
    printer: &mut Printer<W>,
    command: &Command,
) -> Result<bool> {
    match command {
        Command::Bots => {
            for line in bots_lines(&session.bots()?) {
                printer.line(&line)?;
            }
        }
        Command::Groups => {
            let conversations = session.conversations()?;
            let bots = session.bots()?;
            let lines = groups_lines(&conversations, &bots);
            if lines.is_empty() {
                printer.note("no groups")?;
            }
            for line in lines {
                printer.line(&line)?;
            }
        }
        Command::Tasks => {
            let lines = tasks_lines(&session.bots()?, &session.tasks()?, None)?;
            if lines.is_empty() {
                printer.note("no tasks")?;
            }
            for line in lines {
                printer.line(&line)?;
            }
        }
        Command::Stop => {
            let stopped = session.interrupt(conversation)?;
            printer.note(if stopped {
                "stopped the running turn"
            } else {
                "nothing was running here"
            })?;
        }
        Command::New => match target.bot() {
            Some(bot) => {
                session.new_session(bot)?;
                printer.note(&format!(
                    "@{} starts a fresh session (read cursors stay)",
                    bot.name
                ))?;
            }
            // A group has no single mind to reset; pretending otherwise would
            // be a lie about what /new did.
            None => printer.note("/new works in a bot's DM; a group has no single session")?,
        },
        Command::Quit => return Ok(true),
        Command::Help => printer.note(
            "/bots /groups /tasks /stop (interrupt the current turn) /new (fresh session) /quit",
        )?,
        Command::Unknown(name) => {
            printer.note(&format!("unknown command /{name} — /help lists them"))?
        }
    }
    Ok(false)
}

fn decision_label(decision: Decision) -> &'static str {
    match decision {
        Decision::AllowOnce => "allowed once",
        Decision::AllowAlways => "always allowed from now on",
        Decision::Deny => "denied",
    }
}

/// One row per bot for `/bots` and `swarmbot bots ls`.
pub fn bots_lines(bots: &[Bot]) -> Vec<String> {
    bots.iter()
        .map(|bot| {
            let status = match &bot.status {
                swarmbot_contracts::bot::BotStatus::Working { activity, .. } => {
                    format!("working ({activity})")
                }
                other => other.label().to_string(),
            };
            format!(
                "@{:<10} {:<10} backend={:<12} {} · dm {}",
                bot.name, bot.label, bot.backend, status, bot.direct_conversation_id
            )
        })
        .collect()
}

/// One row per group for `/groups`.
pub fn groups_lines(conversations: &[Conversation], bots: &[Bot]) -> Vec<String> {
    conversations
        .iter()
        .filter(|conversation| conversation.kind.is_group())
        .map(|conversation| {
            let members = conversation.kind.participants();
            let lead = match &conversation.kind {
                swarmbot_contracts::conversation::ConversationKind::Group { lead, .. } => {
                    Some(lead)
                }
                _ => None,
            };
            let names: Vec<String> = members
                .iter()
                .map(|id| {
                    bots.iter()
                        .find(|bot| &bot.id == id)
                        .map(|bot| {
                            if lead == Some(&bot.id) {
                                format!("@{} (lead)", bot.name)
                            } else {
                                format!("@{}", bot.name)
                            }
                        })
                        .unwrap_or_else(|| "@unknown".to_string())
                })
                .collect();
            format!(
                "\"{}\" — {} · id {}",
                conversation.title.as_deref().unwrap_or("untitled"),
                names.join(", "),
                conversation.id
            )
        })
        .collect()
}

/// One row per task for `/tasks` and `swarmbot tasks ls`.
///
/// Takes the roster and the tasks as lists rather than as a swarm, so a remote
/// session (which has no swarm, only `GET /api/tasks`) prints the identical row.
/// Ordering follows the roster, then the brain's own task order.
pub fn tasks_lines(bots: &[Bot], tasks: &[Task], only_bot: Option<&str>) -> Result<Vec<String>> {
    let filter = match only_bot {
        Some(name) => {
            let handle = swarmbot_contracts::bot::normalize_name(name);
            Some(
                bots.iter()
                    .find(|bot| bot.name == handle)
                    .ok_or_else(|| anyhow::anyhow!("there is no bot called @{handle}"))?
                    .id
                    .clone(),
            )
        }
        None => None,
    };
    let mut lines = Vec::new();
    for bot in bots {
        if filter.as_ref().is_some_and(|id| id != &bot.id) {
            continue;
        }
        for task in tasks.iter().filter(|task| task.owner == bot.id) {
            let from = task
                .requester
                .as_ref()
                .and_then(|id| bots.iter().find(|b| &b.id == id))
                .map(|b| format!("from @{}", b.name))
                .unwrap_or_else(|| "own task".to_string());
            lines.push(format!(
                "{} \"{:<40}\" @{} · {from} · {}",
                task.id,
                task.title,
                bot.name,
                task.status.label()
            ));
        }
    }
    Ok(lines)
}

/// `RequestStatus` has no `label()` in contracts; the REPL needs one word.
trait StatusLabel {
    fn status_label(&self) -> &'static str;
}

impl StatusLabel for swarmbot_contracts::request::RequestStatus {
    fn status_label(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Answered => "answered",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::demo_config;
    use crate::embedded::{BrainParams, Embedded};
    use crate::remote::Remote;
    use crate::render::{Theme, render_entry};
    use crate::resolve::Directory;
    use crate::tmp::TempDir;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;
    use swarmbot_client::testing::StubBrain;
    use swarmbot_contracts::id::BotId;
    use swarmbot_contracts::node::Surface;
    use swarmbot_contracts::request::RequestStatus;

    fn approval_request() -> RuntimeRequest {
        RuntimeRequest::approval(
            BotId::new(),
            ConversationId::new(),
            Surface::Shell,
            "shell",
            "cargo test",
            Some("run the tests".into()),
        )
    }

    #[test]
    fn lines_classify_into_commands_and_messages() {
        assert_eq!(classify("/bots"), Input::Command(Command::Bots));
        assert_eq!(classify("  /quit  "), Input::Command(Command::Quit));
        assert_eq!(classify("/exit"), Input::Command(Command::Quit));
        assert_eq!(classify("/help"), Input::Command(Command::Help));
        assert_eq!(classify("/stop"), Input::Command(Command::Stop));
        assert_eq!(classify("/new"), Input::Command(Command::New));
        assert_eq!(classify("/tasks"), Input::Command(Command::Tasks));
        assert_eq!(classify("/groups"), Input::Command(Command::Groups));
        assert_eq!(
            classify("/frobnicate now"),
            Input::Command(Command::Unknown("frobnicate".into()))
        );
        assert_eq!(
            classify("hello @lead"),
            Input::Message("hello @lead".into())
        );
        assert_eq!(
            classify("/not-a-command is this"),
            Input::Command(Command::Unknown("not-a-command".into()))
        );
    }

    #[test]
    fn approval_answers_follow_the_documented_keys() {
        let request = approval_request();
        for (line, expected) in [
            ("y", Some(Decision::AllowOnce)),
            ("Y", Some(Decision::AllowOnce)),
            ("yes", Some(Decision::AllowOnce)),
            ("a", Some(Decision::AllowAlways)),
            ("always", Some(Decision::AllowAlways)),
            ("n", Some(Decision::Deny)),
            ("no", Some(Decision::Deny)),
        ] {
            match classify_card_input(&request, line) {
                CardAction::Answer { decision, answer } => {
                    assert_eq!(decision, expected, "{line}");
                    assert!(answer.is_none());
                }
                other => panic!("{line} classified as {other:?}"),
            }
        }
        assert_eq!(classify_card_input(&request, ""), CardAction::LeavePending);
        assert!(matches!(
            classify_card_input(&request, "maybe?"),
            CardAction::Hint(_)
        ));
    }

    #[test]
    fn a_question_takes_free_text_and_empty_leaves_it_pending() {
        let mut request = approval_request();
        request.kind = RequestKind::Question;
        match classify_card_input(&request, "staging") {
            CardAction::Answer { decision, answer } => {
                assert!(decision.is_none());
                assert_eq!(answer, Some(Value::String("staging".into())));
            }
            other => panic!("classified as {other:?}"),
        }
        assert_eq!(
            classify_card_input(&request, "  "),
            CardAction::LeavePending
        );
    }

    /// The approval flow through the REPL's input path: raise a request via
    /// the real `ApprovalService`, answer it the way a chat line would, and
    /// watch the card's status change in the store.
    fn booted(dir: &std::path::Path) -> Embedded {
        Embedded::build(
            BrainParams {
                config: demo_config(),
                data_dir: dir.to_path_buf(),
                roots: Vec::new(),
                secrets: BTreeMap::new(),
                offline: true,
            },
            true,
        )
        .expect("a brain builds")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_approval_answered_from_the_input_path_settles_the_card() {
        let dir = TempDir::new("repl").expect("temp");
        let embedded = booted(dir.path());
        let swarm = Arc::clone(&embedded.swarm);
        let lead = swarm.bots.by_name("lead").expect("the lead");
        // The answer delivery wakes the lead; give its runner one scripted
        // turn so the reply is words, not the fake's empty-queue placeholder.
        swarm
            .backends
            .registry()
            .fake("fake-lead")
            .expect("the demo config registers fakes")
            .push_turn(swarmbot_llm::ScriptedTurn::text(
                "Proceeding with the allowed command.",
            ));

        let mut request = RuntimeRequest::approval(
            lead.id.clone(),
            lead.direct_conversation_id.clone(),
            Surface::Shell,
            "shell",
            "cargo test",
            Some("run the tests".into()),
        );
        swarm.approval.raise_async(&mut request).expect("raised");
        assert_eq!(request.status, RequestStatus::Pending);
        let card_id = request.card_entry_id.clone().expect("a card entry");

        // The card entry renders, and shows as pending.
        let card_entry = swarm
            .store
            .get_entry(&card_id)
            .expect("read")
            .expect("the card");
        let rendered = render_entry(
            &card_entry,
            &Roster::new(Vec::new(), Vec::new(), None),
            &Theme::plain(),
        )
        .expect("the card renders");
        assert!(rendered.contains("Allow shell"), "{rendered}");
        assert!(rendered.contains("[y] allow once"), "{rendered}");

        // Empty input leaves it pending (the loop must not respond).
        assert_eq!(classify_card_input(&request, ""), CardAction::LeavePending);
        assert_eq!(
            swarm
                .store
                .get_request(&request.id)
                .expect("read")
                .expect("the request")
                .status,
            RequestStatus::Pending
        );

        // "y" through the REPL's answer path settles it.
        let action = classify_card_input(&request, "y");
        let settled = answer_card(&embedded, &request, &action).expect("answered");
        assert_eq!(settled.status, RequestStatus::Answered);
        assert_eq!(settled.decision, Some(Decision::AllowOnce));

        // The card's status changed in the store — that is what a UI reads.
        let card_entry = swarm
            .store
            .get_entry(&card_id)
            .expect("read")
            .expect("the card");
        assert_eq!(
            card_entry.as_card().expect("a card").status,
            RequestStatus::Answered
        );

        // The answer woke the bot; let it finish before shutting down.
        assert!(swarm.wait_for_idle(Duration::from_secs(20)).await);
        swarm.shutdown().await;
    }

    /// "Always allow" through the same path writes the rule, so the next
    /// identical ask never reaches a human.
    #[tokio::test(flavor = "multi_thread")]
    async fn always_allow_writes_a_rule_from_the_input_path() {
        let dir = TempDir::new("repl-always").expect("temp");
        let embedded = booted(dir.path());
        let swarm = Arc::clone(&embedded.swarm);
        let lead = swarm.bots.by_name("lead").expect("the lead");
        swarm
            .backends
            .registry()
            .fake("fake-lead")
            .expect("fakes")
            .push_turn(swarmbot_llm::ScriptedTurn::text("ok"));

        let mut request = RuntimeRequest::approval(
            lead.id.clone(),
            lead.direct_conversation_id.clone(),
            Surface::Shell,
            "shell",
            "cargo test --workspace",
            None,
        );
        swarm.approval.raise_async(&mut request).expect("raised");
        let action = classify_card_input(&request, "a");
        let settled = answer_card(&embedded, &request, &action).expect("answered");
        assert_eq!(settled.decision, Some(Decision::AllowAlways));
        let rules = swarm.approval.rules().expect("rules");
        assert_eq!(rules.len(), 1, "always-allow wrote exactly one rule");
        assert_eq!(rules[0].command_prefix.as_deref(), Some("cargo"));

        assert!(swarm.wait_for_idle(Duration::from_secs(20)).await);
        swarm.shutdown().await;
    }

    #[test]
    fn list_formatters_render_rows() {
        let dm = ConversationId::new();
        let lead = Bot::new("lead", "Lead", "", "fake", dm.clone());
        let scout = Bot::new("scout", "Scout", "", "fake", ConversationId::new());
        let lines = bots_lines(&[lead.clone(), scout.clone()]);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].contains("@lead") && lines[0].contains("idle"),
            "{}",
            lines[0]
        );

        let group = Conversation::group(
            "Team",
            vec![lead.id.clone(), scout.id.clone()],
            lead.id.clone(),
        )
        .expect("a group");
        let group_lines = groups_lines(&[group], &[lead.clone(), scout.clone()]);
        assert_eq!(group_lines.len(), 1);
        assert!(group_lines[0].contains("\"Team\""), "{}", group_lines[0]);
        assert!(
            group_lines[0].contains("@lead (lead)"),
            "{}",
            group_lines[0]
        );

        // A handed-off task names both ends; an own task says so. The rows come
        // from lists, which is what lets a remote session print the same ones.
        let mut handed = Task::own(scout.id.clone(), dm.clone(), "find what changed", "brief");
        handed.requester = Some(lead.id.clone());
        let own = Task::own(lead.id.clone(), dm, "tidy up", "brief");
        // Rows follow the roster, not the order the tasks arrived in: lead
        // first, then scout, whichever way round the brain lists the tasks.
        let task_rows = tasks_lines(&[lead, scout], &[handed, own], None).expect("rows");
        assert_eq!(task_rows.len(), 2);
        // The title column is padded to a fixed width, so the assertions look
        // for the words, not for a quoted exact match.
        assert!(task_rows[0].contains("tidy up"), "{task_rows:?}");
        assert!(task_rows[0].contains("@lead · own task"), "{task_rows:?}");
        assert!(task_rows[1].contains("find what changed"), "{task_rows:?}");
        assert!(
            task_rows[1].contains("@scout · from @lead"),
            "{task_rows:?}"
        );
        // An unknown bot name is refused rather than silently listing nothing.
        let err = tasks_lines(&[], &[], Some("ghost")).expect_err("no such bot");
        assert!(err.to_string().contains("no bot called @ghost"), "{err}");
    }

    /// The remote half of the REPL: the same loop, the same card keys, the same
    /// `render.rs` — with a brain in another process behind the port.
    ///
    /// The stub brain reports one pending approval in this conversation, so the
    /// first line is read as the card's answer (`y` = allow once) exactly as it
    /// is in embedded mode, and the answer goes out over HTTP.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_remote_chat_renders_the_snapshot_and_answers_a_card_inline() {
        let stub = StubBrain::start();
        let remote = Remote::connect(&stub.url(), Some("tok"), Path::new("."), false)
            .expect("the stub answers health");
        let directory = Directory::new(
            Session::bots(&remote).expect("bots"),
            Session::conversations(&remote).expect("conversations"),
        );
        let target = directory.resolve("stub").expect("the stub bot resolves");

        // One scripted line, then end of input: the reader half a test drives
        // instead of a terminal.
        let (line_tx, line_rx) = tokio::sync::mpsc::unbounded_channel::<Option<String>>();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel::<String>();
        line_tx.send(Some("y".to_string())).expect("queued");
        drop(line_tx);

        let mut buffer = Vec::new();
        let code = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            chat_with(&remote, &target, &mut printer, move |_prompt| Lines {
                rx: line_rx,
                ack: ack_tx,
            })
            .await
            .expect("the chat runs")
        };
        // The acknowledgement must have been offered, or the reader contract
        // (prompt only after a line was handled) would be broken.
        assert!(
            ack_rx.try_recv().is_ok(),
            "the loop acknowledged the handled line with the next prompt"
        );
        assert_eq!(code, 0, "a clean stream end is not a failure");

        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("── chat: bot @stub"), "{out}");
        // The snapshot's tail is the history, rendered by the shared path.
        assert!(out.contains("you › canned entry"), "{out}");
        assert!(out.contains("@stub › stub reply"), "{out}");
        // The card was answered inline, with the same keys as embedded mode.
        assert!(out.contains("request answered:"), "{out}");
        assert!(out.contains("(allowed once)"), "{out}");

        // The answer really went over the wire, with the token in a header.
        assert!(
            stub.requests()
                .iter()
                .any(|recorded| recorded.method == "POST"
                    && recorded.path.ends_with("/respond")
                    && recorded.body.contains("allow_once")
                    && recorded.auth.as_deref() == Some("Bearer tok")),
            "the respond call carried the decision and the token: {:?}",
            stub.requests()
        );
    }

    /// The `cmd_chat` sequence on an unscripted fake: the decision comes from
    /// the resolved backend (the demo config's `fake-lead` has an empty script
    /// queue), the notice is printed once to the stderr sink before the first
    /// message, and the chat's stdout never carries it — not even per message.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_chat_on_an_unscripted_fake_warns_once_on_stderr() {
        let dir = TempDir::new("repl-notice").expect("temp");
        let embedded = booted(dir.path());
        let swarm = Arc::clone(&embedded.swarm);
        let directory = Directory::load(&swarm).expect("the roster reads");
        let target = directory.resolve("lead").expect("the demo lead resolves");

        assert!(
            crate::notice::embedded_unscripted_fake(&swarm, target.conversation_id()),
            "nothing scripted fake-lead, so the notice is due"
        );
        let mut stderr = Vec::new();
        assert!(crate::notice::warn(&mut stderr, false, true));

        // One scripted line, then end of input.
        let (line_tx, line_rx) = tokio::sync::mpsc::unbounded_channel::<Option<String>>();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel::<String>();
        line_tx.send(Some("hello".to_string())).expect("queued");
        drop(line_tx);

        let mut buffer = Vec::new();
        let code = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            chat_with(&embedded, &target, &mut printer, move |_prompt| Lines {
                rx: line_rx,
                ack: ack_tx,
            })
            .await
            .expect("the chat runs")
        };
        assert_eq!(code, 0);
        assert!(ack_rx.try_recv().is_ok(), "the line was handled");
        assert!(swarm.wait_for_idle(Duration::from_secs(20)).await);

        // The notice went to the stderr sink exactly once ...
        let err = String::from_utf8(stderr).expect("utf8");
        assert_eq!(
            err.matches("only answers what a script tells it to")
                .count(),
            1,
            "one chat, one notice: {err}"
        );
        // ... and never reached the chat's stdout.
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("── chat: bot @lead"), "{out}");
        assert!(
            !out.contains("only answers what a script tells it to"),
            "stdout stays clean: {out}"
        );
        swarm.shutdown().await;
    }
}
