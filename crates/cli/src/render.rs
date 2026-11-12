//! Terminal rendering: one function per entry kind, `crossterm` colours, no
//! layout library.
//!
//! Two rules shape this module:
//!
//! * **every renderer is a pure function into a `String`** — colour is applied
//!   through [`Theme`], so `NO_COLOR`/`--no-color`/a pipe strip the escapes by
//!   construction and a test can assert on the exact bytes;
//! * **a streamed entry is redrawn, not repeated** — the hub publishes every
//!   revision of a growing message or a finishing tool call, and printing one
//!   line per delta would bury the transcript. On a terminal the current entry
//!   is repainted in place; on a pipe the revisions coalesce and each entry is
//!   printed once, complete.

use std::io::Write;

use anyhow::Result;
use chrono::Utc;
use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::queue;
use crossterm::style::{Attribute, Color, Stylize};
use crossterm::terminal::{Clear, ClearType};
use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::entry::{
    Author, CardEntry, Entry, EntryKind, EventEntry, MessageEntry, NoticeEntry, NoticeLevel,
    PostState, ReasoningEntry, ToolCallEntry, ToolCallStatus,
};
use swarmbot_contracts::id::{BotId, ConversationId, EntryId, NodeId, TaskId, TurnId};
use swarmbot_contracts::live::{LiveEvent, Snapshot};
use swarmbot_contracts::request::{RequestKind, RequestStatus, RuntimeRequest};
use swarmbot_contracts::task::Task;
use swarmbot_contracts::turn::{Turn, TurnInitiator};

/// How many lines of a tool result are shown before the "more lines" marker.
const TOOL_RESULT_LINES: usize = 6;
/// Reasoning is collapsed to one line of at most this many characters unless
/// `--verbose` asks for the whole thought.
const REASONING_WIDTH: usize = 96;
/// Tool-call arguments are compacted to about one terminal line.
const ARGS_WIDTH: usize = 80;
/// Card boxes are capped so they never wrap on an 80-column terminal.
const CARD_WIDTH: usize = 68;

/// Colour, verbosity and quietness, decided once at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub color: bool,
    pub verbose: bool,
    pub quiet: bool,
}

impl Theme {
    /// No colour, no decoration changes — what tests render with.
    pub fn plain() -> Self {
        Self {
            color: false,
            verbose: false,
            quiet: false,
        }
    }

    /// The startup decision: `--no-color` and a non-empty `NO_COLOR` win over
    /// everything, and a pipe is never colourised (escaped bytes in a log file
    /// are vandalism, not styling).
    pub fn auto(no_color: bool, verbose: bool, quiet: bool) -> Self {
        let env_no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        Self {
            color: !no_color
                && !env_no_color
                && std::io::IsTerminal::is_terminal(&std::io::stdout()),
            verbose,
            quiet,
        }
    }

    /// Colour a span. Public because command code paints whole status lines
    /// (the demo's notice, a run's timeout) with the same theme decision.
    pub fn paint(&self, text: &str, color: Color) -> String {
        if self.color && !text.is_empty() {
            text.with(color).to_string()
        } else {
            text.to_string()
        }
    }

    fn attribute(&self, text: &str, attribute: Attribute) -> String {
        if self.color && !text.is_empty() {
            text.attribute(attribute).to_string()
        } else {
            text.to_string()
        }
    }

    fn dim(&self, text: &str) -> String {
        self.paint(text, Color::DarkGrey)
    }
}

/// `#rrggbb` (what `Bot::effective_color` always returns) into a terminal
/// colour. Anything else is `None` and renders uncoloured rather than wrong.
pub fn hex_color(hex: &str) -> Option<Color> {
    let digits = hex.strip_prefix('#').unwrap_or(hex);
    if digits.len() != 6 || !digits.is_ascii() {
        return None;
    }
    let r = u8::from_str_radix(&digits[0..2], 16).ok()?;
    let g = u8::from_str_radix(&digits[2..4], 16).ok()?;
    let b = u8::from_str_radix(&digits[4..6], 16).ok()?;
    Some(Color::Rgb { r, g, b })
}

/// Maximum number of transcript entries kept in [`Roster`] for reply-target
/// lookup without letting long sessions grow without bound.
const ROSTER_ENTRY_CAP: usize = 512;

fn upsert_by_id<T>(items: &mut Vec<T>, item: T, same: impl Fn(&T, &T) -> bool) {
    match items.iter_mut().find(|existing| same(existing, &item)) {
        Some(slot) => *slot = item,
        None => items.push(item),
    }
}

/// Who is who, and where: the roster an entry's author id is looked up in,
/// plus the conversation titles a multi-conversation stream labels lines with.
#[derive(Clone, Debug, Default)]
pub struct Roster {
    bots: Vec<Bot>,
    conversations: Vec<Conversation>,
    turns: Vec<Turn>,
    tasks: Vec<Task>,
    requests: Vec<RuntimeRequest>,
    entries: Vec<Entry>,
    /// The conversation in focus; entries from anywhere else get a label.
    /// `None` (the demo) labels everything but the chosen stage.
    focus: Option<ConversationId>,
}

impl Roster {
    pub fn new(
        bots: Vec<Bot>,
        conversations: Vec<Conversation>,
        focus: Option<ConversationId>,
    ) -> Self {
        Self {
            bots,
            conversations,
            turns: Vec::new(),
            tasks: Vec::new(),
            requests: Vec::new(),
            entries: Vec::new(),
            focus,
        }
    }

    pub fn adopt_snapshot(&mut self, snapshot: &Snapshot) {
        self.bots = snapshot.bots.clone();
        self.conversations = snapshot.conversations.clone();
        self.turns = snapshot.turns.clone();
        self.tasks = snapshot.tasks.clone();
        self.requests = snapshot.requests.clone();
        self.entries.clear();
        self.adopt_entries(snapshot.entries.clone());
    }

    pub fn adopt_tasks(&mut self, tasks: Vec<Task>) {
        for task in tasks {
            self.upsert_task(task);
        }
    }

    pub fn adopt_requests(&mut self, requests: Vec<RuntimeRequest>) {
        for request in requests {
            self.upsert_request(request);
        }
    }

    pub fn adopt_entries(&mut self, entries: Vec<Entry>) {
        for entry in entries {
            self.upsert_entry(entry);
        }
    }

    pub fn set_focus(&mut self, focus: Option<ConversationId>) {
        self.focus = focus;
    }

    pub fn focus(&self) -> Option<&ConversationId> {
        self.focus.as_ref()
    }

    pub fn bots(&self) -> &[Bot] {
        &self.bots
    }

    pub fn conversations(&self) -> &[Conversation] {
        &self.conversations
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn entry(&self, id: &EntryId) -> Option<&Entry> {
        self.entries.iter().find(|e| &e.id == id)
    }

    pub fn upsert_bot(&mut self, bot: Bot) {
        upsert_by_id(&mut self.bots, bot, |a, b| a.id == b.id);
    }

    pub fn upsert_conversation(&mut self, conversation: Conversation) {
        upsert_by_id(&mut self.conversations, conversation, |a, b| a.id == b.id);
    }

    pub fn upsert_turn(&mut self, turn: Turn) {
        upsert_by_id(&mut self.turns, turn, |a, b| a.id == b.id);
    }

    pub fn upsert_task(&mut self, task: Task) {
        upsert_by_id(&mut self.tasks, task, |a, b| a.id == b.id);
    }

    pub fn upsert_request(&mut self, request: RuntimeRequest) {
        upsert_by_id(&mut self.requests, request, |a, b| a.id == b.id);
    }

    pub fn upsert_entry(&mut self, entry: Entry) {
        upsert_by_id(&mut self.entries, entry, |a, b| a.id == b.id);
        if self.entries.len() > ROSTER_ENTRY_CAP {
            let excess = self.entries.len() - ROSTER_ENTRY_CAP;
            self.entries.drain(..excess);
        }
    }

    /// Update roster state from a live event without printing anything.
    pub fn observe_event(&mut self, event: &LiveEvent) {
        match event {
            LiveEvent::Snapshot(snapshot) => self.adopt_snapshot(snapshot),
            LiveEvent::EntryUpsert { entry, .. } => self.upsert_entry(entry.clone()),
            LiveEvent::BotUpsert { bot, .. } => self.upsert_bot(bot.clone()),
            LiveEvent::BotStatus { bot_id, status, .. } => {
                if let Some(bot) = self.bots.iter_mut().find(|b| &b.id == bot_id) {
                    bot.status = status.clone();
                }
            }
            LiveEvent::ConversationUpsert { conversation, .. } => {
                self.upsert_conversation(conversation.clone());
            }
            LiveEvent::ConversationDeleted {
                conversation_id, ..
            } => {
                self.conversations.retain(|c| &c.id != conversation_id);
            }
            LiveEvent::TurnUpsert { turn, .. } => self.upsert_turn(turn.clone()),
            LiveEvent::TaskUpsert { task, .. } => self.upsert_task(task.clone()),
            LiveEvent::RequestUpsert { request, .. } => self.upsert_request(request.clone()),
            _ => {}
        }
    }

    pub fn bot(&self, id: &BotId) -> Option<&Bot> {
        self.bots.iter().find(|b| &b.id == id)
    }

    pub fn conversation(&self, id: &ConversationId) -> Option<&Conversation> {
        self.conversations.iter().find(|c| &c.id == id)
    }

    pub fn is_group(&self, id: &ConversationId) -> bool {
        self.conversation(id).is_some_and(|c| c.kind.is_group())
    }

    pub fn is_group_focus(&self) -> bool {
        self.focus.as_ref().is_some_and(|id| self.is_group(id))
    }

    pub(crate) fn is_request_relevant(
        &self,
        request: &RuntimeRequest,
        focus: &ConversationId,
    ) -> bool {
        crate::session::request_relevant_to(
            request,
            focus,
            &self.conversations,
            &self.bots,
            &self.tasks,
            &self.turns,
        )
    }

    /// Whether a streamed/snapshot entry belongs in the focused view.
    ///
    /// - Focused conversation entries always show.
    /// - Entries belonging to a delegated [`Task`] show iff `task.origin == focus`.
    /// - When focused on a group, off-focus bot-to-bot DMs between members of
    ///   that group (`message_bot`) show, whereas private owner DMs, routines,
    ///   non-member DMs, and other groups stay hidden.
    /// - When focused on a DM, unrelated groups and other DMs stay hidden.
    pub fn should_show(&self, entry: &Entry) -> bool {
        let Some(focused) = &self.focus else {
            return true;
        };
        if &entry.conversation_id == focused {
            return true;
        }
        if let Some(task_id) = self.entry_task_id(entry)
            && let Some(task) = self.tasks.iter().find(|t| t.id == task_id)
        {
            return &task.origin == focused;
        }
        let Some(conv) = self.conversation(focused) else {
            return false;
        };
        if !conv.kind.is_group() {
            return false;
        }
        let participants = conv.kind.participants();
        let in_member_dm = participants.iter().any(|bot_id| {
            self.bot(bot_id)
                .is_some_and(|bot| bot.direct_conversation_id == entry.conversation_id)
        });
        if !in_member_dm {
            return false;
        }
        if let Some(turn_id) = entry_turn(entry)
            && let Some(turn) = self.turns.iter().find(|t| t.id == turn_id)
        {
            return match &turn.initiator {
                TurnInitiator::Bot { bot_id } => participants.contains(bot_id),
                TurnInitiator::Task { task_id } => self
                    .tasks
                    .iter()
                    .find(|t| &t.id == task_id)
                    .is_some_and(|t| &t.origin == focused),
                TurnInitiator::Request => participants.contains(&turn.bot_id),
                TurnInitiator::Owner | TurnInitiator::Routine { .. } | TurnInitiator::System => {
                    false
                }
            };
        }
        match &entry.author {
            Author::Owner => false,
            Author::Bot { id } => participants.contains(id),
            Author::System => true,
        }
    }

    fn entry_task_id(&self, entry: &Entry) -> Option<TaskId> {
        match &entry.kind {
            EntryKind::ToolCall(call) => call.task_id.clone().or_else(|| {
                call.turn_id.as_ref().and_then(|tid| {
                    self.turns
                        .iter()
                        .find(|t| &t.id == tid)
                        .and_then(|t| t.task_id.clone())
                })
            }),
            EntryKind::Card(card) => self
                .requests
                .iter()
                .find(|r| r.id == card.request_id)
                .and_then(|r| {
                    r.task_id.clone().or_else(|| {
                        r.turn_id.as_ref().and_then(|tid| {
                            self.turns
                                .iter()
                                .find(|t| &t.id == tid)
                                .and_then(|t| t.task_id.clone())
                        })
                    })
                }),
            _ => entry_turn(entry).and_then(|tid| {
                self.turns
                    .iter()
                    .find(|t| t.id == tid)
                    .and_then(|t| t.task_id.clone())
            }),
        }
    }

    fn resolve_entry_bot(&self, entry: &Entry) -> Option<BotId> {
        if let Some(id) = entry.author.bot_id() {
            return Some(id.clone());
        }
        match &entry.kind {
            EntryKind::ToolCall(call) => {
                if let Some(turn_id) = &call.turn_id
                    && let Some(turn) = self.turns.iter().find(|t| &t.id == turn_id)
                {
                    return Some(turn.bot_id.clone());
                }
                if let Some(task_id) = &call.task_id
                    && let Some(task) = self.tasks.iter().find(|t| &t.id == task_id)
                {
                    return Some(task.owner.clone());
                }
                self.conversation(&entry.conversation_id)
                    .and_then(|c| c.direct_bot().cloned())
            }
            EntryKind::Card(card) => {
                if let Some(req) = self.requests.iter().find(|r| r.id == card.request_id) {
                    return Some(req.bot_id.clone());
                }
                if let Some(bot_id) = self
                    .conversation(&entry.conversation_id)
                    .and_then(|c| c.direct_bot())
                {
                    return Some(bot_id.clone());
                }
                // `ApprovalService::raise` emits the Card entry right before
                // `RequestUpsert`; if exactly one bot turn is currently active
                // in this conversation, attribute cleanly without guessing
                // across concurrent speakers.
                let active: Vec<&Turn> = self
                    .turns
                    .iter()
                    .filter(|t| {
                        t.conversation_id == entry.conversation_id && !t.status.is_terminal()
                    })
                    .collect();
                if active.len() == 1 {
                    Some(active[0].bot_id.clone())
                } else {
                    None
                }
            }
            _ => entry_turn(entry).and_then(|tid| {
                self.turns
                    .iter()
                    .find(|t| t.id == tid)
                    .map(|t| t.bot_id.clone())
            }),
        }
    }

    fn entry_bot_label(&self, entry: &Entry) -> Option<(String, Option<Color>)> {
        let multi_bot =
            self.is_group(&entry.conversation_id) || self.is_group_focus() || self.focus.is_none();
        let bot_id = self.resolve_entry_bot(entry)?;
        if !multi_bot {
            let same_dm_bot = self
                .focus
                .as_ref()
                .and_then(|id| self.conversation(id))
                .and_then(|c| c.direct_bot())
                .is_some_and(|dm_bot| dm_bot == &bot_id);
            let unknown_focus = self
                .focus
                .as_ref()
                .is_some_and(|id| self.conversation(id).is_none());
            if same_dm_bot || unknown_focus {
                return None;
            }
        }
        Some((self.handle(&bot_id), self.bot_color(&bot_id)))
    }

    /// `@name`, or `@unknown` for a deleted bot — a transcript outlives its
    /// author and must still render.
    pub fn handle(&self, id: &BotId) -> String {
        self.bot(id)
            .map(|bot| format!("@{}", bot.name))
            .unwrap_or_else(|| "@unknown".to_string())
    }

    pub fn bot_color(&self, id: &BotId) -> Option<Color> {
        self.bot(id)
            .and_then(|bot| hex_color(&bot.effective_color()))
    }

    /// The label for an entry's conversation when it is not the one in focus
    /// (`"Team"`, `"dm @scout"`); `None` when no label is needed.
    pub fn conversation_label(&self, conversation: &ConversationId) -> Option<String> {
        if self.focus.as_ref() == Some(conversation) {
            return None;
        }
        let known = self.conversations.iter().find(|c| &c.id == conversation)?;
        Some(match known.direct_bot() {
            Some(bot_id) => format!("dm {}", self.handle(bot_id)),
            None => known.title.clone().unwrap_or_else(|| "group".to_string()),
        })
    }
}

/// Render one entry, or `None` when it must not appear (an empty streaming
/// placeholder, a kind suppressed by `--quiet`).
pub fn render_entry(entry: &Entry, roster: &Roster, theme: &Theme) -> Option<String> {
    let author = roster.entry_bot_label(entry);
    let author_ref = author
        .as_ref()
        .map(|(handle, color)| (handle.as_str(), *color));
    match &entry.kind {
        EntryKind::Message(message) => render_message(entry, message, roster, theme),
        EntryKind::Reasoning(reasoning) => Some(render_reasoning(reasoning, author_ref, theme)),
        EntryKind::ToolCall(call) => Some(render_tool_call(call, author_ref, theme)),
        EntryKind::Card(card) => Some(render_card(card, author_ref, theme)),
        EntryKind::Event(event) => Some(render_event(event, theme)),
        EntryKind::Notice(notice) => Some(render_notice(notice, theme)),
        // A newer schema than this build: say so instead of pretending.
        EntryKind::Unknown => Some(theme.dim("(an entry kind this build does not know)")),
    }
}

fn format_bot_handle(id: &BotId, roster: &Roster, theme: &Theme) -> String {
    let handle = roster.handle(id);
    paint_handle(&handle, roster.bot_color(id), theme)
}

fn paint_handle(handle: &str, color: Option<Color>, theme: &Theme) -> String {
    match color {
        Some(color) => theme.paint(handle, color),
        None => handle.to_string(),
    }
}

fn format_reply_tag(reply_id: &EntryId, roster: &Roster, theme: &Theme) -> String {
    let Some(parent) = roster.entry(reply_id) else {
        return theme.dim(&format!("(↩ {})", short_id(reply_id.as_str())));
    };
    let who = match &parent.author {
        Author::Owner => "you".to_string(),
        Author::Bot { id } => roster.handle(id),
        Author::System => "system".to_string(),
    };
    let preview = match &parent.kind {
        EntryKind::Message(m) if !m.text.trim().is_empty() => collapse(m.text.trim(), 32),
        EntryKind::Card(c) if !c.title.trim().is_empty() => collapse(c.title.trim(), 32),
        _ => String::new(),
    };
    if preview.is_empty() {
        theme.dim(&format!("(↩ {who})"))
    } else {
        theme.dim(&format!("(↩ {who}: \"{preview}\")"))
    }
}

/// Owner: `you › text`. Bot: `@name › text` in the bot's own colour.
fn render_message(
    entry: &Entry,
    message: &MessageEntry,
    roster: &Roster,
    theme: &Theme,
) -> Option<String> {
    // The first streamed revision is empty; printing a bare prefix that grows
    // into a sentence is what the redraw path is for, but an *empty* draw is
    // noise, so it is suppressed.
    if message.text.is_empty() {
        return None;
    }
    let sender = match &entry.author {
        Author::Owner => theme.attribute("you", Attribute::Bold),
        Author::Bot { id } => format_bot_handle(id, roster, theme),
        Author::System => theme.dim("system"),
    };
    let target = message
        .addressed_to
        .as_ref()
        .map(|id| {
            format!(
                " {} {}",
                theme.dim("→"),
                format_bot_handle(id, roster, theme)
            )
        })
        .unwrap_or_default();
    let reply = message
        .reply_to
        .as_ref()
        .map(|id| format!(" {}", format_reply_tag(id, roster, theme)))
        .unwrap_or_default();
    let prefix = format!("{sender}{target}{reply} › ");
    let suffix = match message.post {
        PostState::Posted => String::new(),
        // The bot is busy elsewhere: the owner should see the message waiting.
        PostState::Queued => theme.dim(" (queued)"),
        PostState::Steered => theme.dim(" (steered into the running turn)"),
    };
    Some(format!("{prefix}{}{suffix}", message.text))
}

/// Reasoning is decoration around an answer: dim, one line, unless `--verbose`
/// asks to read the bot's mind in full.
fn render_reasoning(
    reasoning: &ReasoningEntry,
    author: Option<(&str, Option<Color>)>,
    theme: &Theme,
) -> String {
    let body = if theme.verbose {
        reasoning.text.clone()
    } else {
        collapse(&reasoning.text, REASONING_WIDTH)
    };
    match author {
        Some((handle, color)) => format!(
            "{} {}",
            paint_handle(handle, color, theme),
            theme.dim(&format!("· thinking › {body}"))
        ),
        None => theme.dim(&format!("thinking › {body}")),
    }
}

/// `⚙ shell {"command":"cargo test"} → exit=0 (1.2s, node local)` plus at most
/// [`TOOL_RESULT_LINES`] lines of the result, errors in red.
fn render_tool_call(
    call: &ToolCallEntry,
    author: Option<(&str, Option<Color>)>,
    theme: &Theme,
) -> String {
    let args = collapse(&compact_json(&call.args), ARGS_WIDTH);
    let actor = author
        .map(|(handle, color)| format!("{} ", paint_handle(handle, color, theme)))
        .unwrap_or_default();
    let mut head = format!(
        "{}{} {} {}",
        actor,
        theme.paint("⚙", Color::Cyan),
        theme.attribute(&call.tool, Attribute::Bold),
        theme.dim(&args),
    );

    let exit_line = call
        .result_text
        .as_ref()
        .and_then(|text| text.lines().next())
        .filter(|line| line.starts_with("exit="))
        .map(str::to_string);
    let outcome = match call.status {
        ToolCallStatus::Pending | ToolCallStatus::Running => theme.dim("→ running…"),
        ToolCallStatus::Completed => match &exit_line {
            // The shell handler's first line is `exit=<code>`; showing it
            // verbatim beats inventing our own "ok".
            Some(line) => theme.paint(&format!("→ {line}"), Color::Green),
            None => theme.paint("→ ok", Color::Green),
        },
        ToolCallStatus::Failed => theme.paint("→ failed", Color::Red),
        ToolCallStatus::Denied => theme.paint("→ denied", Color::Red),
        ToolCallStatus::Cancelled => theme.paint("→ cancelled", Color::Yellow),
    };
    head.push_str(&format!(" {outcome}"));

    let mut facts = Vec::new();
    if let Some(ms) = call.duration_ms {
        facts.push(format_duration(ms));
    }
    if let Some(node) = &call.node_id {
        facts.push(if node == &NodeId::local() {
            "node local".to_string()
        } else {
            format!("node {}", short_id(node.as_str()))
        });
    }
    if !facts.is_empty() {
        head.push_str(&theme.dim(&format!(" ({})", facts.join(", "))));
    }

    let Some(result) = call.result_text.as_deref() else {
        return head;
    };
    // The exit line already appeared in the head; the body starts behind it.
    let body: Vec<&str> = result
        .lines()
        .skip(if exit_line.is_some() { 1 } else { 0 })
        .filter(|line| !line.trim().is_empty())
        .collect();
    if body.is_empty() {
        return head;
    }
    let failed = matches!(call.status, ToolCallStatus::Failed | ToolCallStatus::Denied);
    let mut out = head;
    for line in body.iter().take(TOOL_RESULT_LINES) {
        let rendered = format!("  {line}");
        let styled = if failed {
            theme.paint(&rendered, Color::Red)
        } else {
            theme.dim(&rendered)
        };
        out.push('\n');
        out.push_str(&styled);
    }
    if body.len() > TOOL_RESULT_LINES {
        out.push('\n');
        out.push_str(&theme.dim(&format!(
            "  … {} more lines",
            body.len() - TOOL_RESULT_LINES
        )));
    }
    out
}

/// A boxed prompt: title, body, what to type, and how long is left.
fn render_card(card: &CardEntry, author: Option<(&str, Option<Color>)>, theme: &Theme) -> String {
    let kind = match card.card {
        RequestKind::Approval => "approval",
        RequestKind::Question => "question",
        RequestKind::Form => "form",
        RequestKind::Secret => "secret needed",
        RequestKind::Handoff => "handoff",
    };
    let label = match author {
        Some((handle, _)) => format!("{kind} · {handle}"),
        None => kind.to_string(),
    };
    let mut rows: Vec<String> = Vec::new();
    rows.push(card.title.clone());
    if !card.body.trim().is_empty() && card.body != card.title {
        rows.extend(card.body.lines().map(str::to_string));
    }
    if card.status == RequestStatus::Pending {
        let hint = match card.card {
            RequestKind::Approval => {
                "[y] allow once  [a] always allow  [n] deny  [enter] leave pending"
            }
            RequestKind::Secret => "type the value (it is never echoed into the transcript)",
            _ => "type your answer (enter leaves it pending)",
        };
        rows.push(hint.to_string());
        if let Some(expires) = card.expires_at {
            rows.push(countdown(expires, theme));
        }
    } else {
        rows.push(format!("status: {}", status_label(card.status)));
    }

    let pending = card.status == RequestStatus::Pending;
    let edge = if pending {
        Color::Yellow
    } else {
        Color::DarkGrey
    };
    box_lines(&label, &rows, theme, edge, pending)
}

fn status_label(status: RequestStatus) -> &'static str {
    match status {
        RequestStatus::Pending => "pending",
        RequestStatus::Answered => "answered",
        RequestStatus::Expired => "expired",
        RequestStatus::Cancelled => "cancelled",
    }
}

/// The expiry countdown, rendered at draw time: a card that died while nobody
/// looked must not claim time it does not have.
fn countdown(expires_at: chrono::DateTime<Utc>, theme: &Theme) -> String {
    let remaining = expires_at.signed_duration_since(Utc::now());
    if remaining.num_seconds() <= 0 {
        return theme.paint("expires now", Color::Red);
    }
    let seconds = remaining.num_seconds();
    let text = if seconds >= 3600 {
        format!(
            "expires in {}h {:02}m",
            seconds / 3600,
            (seconds % 3600) / 60
        )
    } else if seconds >= 60 {
        format!("expires in {}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("expires in {seconds}s")
    };
    theme.paint(&text, Color::Yellow)
}

/// Draw the `╭─ label ─╮ … ╰─╯` box by hand: fixed-width assumption (one char,
/// one column) is fine for the ASCII/box-drawing mix used here, and pulling in
/// a unicode-width crate for a prototype box is not a trade worth making.
fn box_lines(label: &str, rows: &[String], theme: &Theme, edge: Color, bold_title: bool) -> String {
    let width = rows
        .iter()
        .map(|row| row.chars().count())
        .chain([label.chars().count() + 2])
        .max()
        .unwrap_or(0)
        .min(CARD_WIDTH);
    let pad = |row: &str| -> String {
        let taken: String = row.chars().take(width).collect();
        let fill = width.saturating_sub(taken.chars().count());
        format!("{}{}{}", taken, " ".repeat(fill), "")
    };
    let bar = "─".repeat(width.saturating_add(2));
    let mut out = String::new();
    let top = format!("╭─ {label} ");
    let top = format!(
        "{top}{}╮",
        "─".repeat(
            width
                .saturating_add(2)
                .saturating_sub(label.chars().count() + 3)
        )
    );
    out.push_str(&theme.paint(&top, edge));
    out.push('\n');
    let _ = bar;
    for (index, row) in rows.iter().enumerate() {
        let body = pad(row);
        let styled = if index == 0 && bold_title {
            theme.attribute(&body, Attribute::Bold)
        } else {
            body
        };
        out.push_str(&theme.paint("│", edge));
        out.push(' ');
        out.push_str(&styled);
        out.push(' ');
        out.push_str(&theme.paint("│", edge));
        out.push('\n');
    }
    let bottom = format!("╰{}╯", "─".repeat(width.saturating_add(2)));
    out.push_str(&theme.paint(&bottom, edge));
    out.trim_end().to_string()
}

/// Events are the delegation story: `→ Messaged @scout`, `→ Handed off …`,
/// `→ Task … succeeded`, `⊘ Stopped: too many hand-offs`. The core writes the
/// sentence; the prefix and colour are the only thing rendering adds.
fn render_event(event: &EventEntry, theme: &Theme) -> String {
    match event.event.as_str() {
        "no-reply" => theme.dim(&format!("· {}", event.text)),
        "hop-limit" | "stopped" | "bot-disabled" | "task-cancelled" => {
            format!("{} {}", theme.paint("⊘", Color::Red), event.text)
        }
        _ => format!("{} {}", theme.paint("→", Color::Cyan), event.text),
    }
}

/// Notices by level: yellow warnings, red errors, dim info.
fn render_notice(notice: &NoticeEntry, theme: &Theme) -> String {
    match notice.level {
        NoticeLevel::Info => theme.dim(&format!("note: {}", notice.text)),
        NoticeLevel::Warning => theme.paint(&format!("warning: {}", notice.text), Color::Yellow),
        NoticeLevel::Error => theme.paint(&format!("error: {}", notice.text), Color::Red),
    }
}

/// One line of JSON-ish argument display: compact, whitespace-collapsed,
/// length-capped — an argument blob is context, not content.
fn compact_json(value: &serde_json::Value) -> String {
    let text = value.to_string();
    collapse(&text, usize::MAX)
}

/// Whitespace-collapse and cap at `width` characters, marking the cut.
fn collapse(text: &str, width: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= width {
        return flat;
    }
    let cut: String = flat.chars().take(width.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

fn short_id(id: &str) -> String {
    id.rsplit('_')
        .next()
        .unwrap_or(id)
        .chars()
        .take(6)
        .collect()
}

/// Writes the rendered stream to a terminal or a pipe.
///
/// Streamed entries (a bot message growing, reasoning, a tool call finishing)
/// are *held* in a block keyed by entry id instead of being printed per
/// revision: the runner alternates revisions of a round's reasoning and
/// message entries, so a printer that held only one entry would re-print both
/// on every change. The block commits — printed once, in arrival order — when
/// a one-shot entry needs to print where it happened, when the entries' turn
/// ends, or when the stream finishes.
///
/// On a terminal the block is additionally *painted* while open and repainted
/// in place on every revision (the cursor walks back up the rows the previous
/// paint used), so a human watches the sentence grow. On a pipe there is no
/// cursor to walk: the block stays invisible until it commits, which is why a
/// piped run shows complete entries instead of a per-delta stutter.
pub struct Printer<W: Write> {
    out: W,
    pub theme: Theme,
    interactive: bool,
    /// The held streamed entries, in first-seen order (which is `seq` order:
    /// an entry is created before its revisions arrive).
    block: Vec<BlockEntry>,
    /// Terminal rows the painted block occupies, for the in-place redraw.
    block_rows: usize,
}

struct BlockEntry {
    id: EntryId,
    turn: Option<TurnId>,
    text: String,
}

/// True for the entry kinds the runner upserts repeatedly while a turn runs.
/// The `turn_id` requirement is what separates a streamed reply from a
/// one-shot bot message written by a tool like `message_bot` (final on
/// arrival, so it prints where it happened).
fn is_streamed(entry: &Entry) -> bool {
    match &entry.kind {
        EntryKind::Message(message) => {
            message.turn_id.is_some() && matches!(entry.author, Author::Bot { .. })
        }
        EntryKind::Reasoning(reasoning) => reasoning.turn_id.is_some(),
        EntryKind::ToolCall(call) => call.turn_id.is_some(),
        _ => false,
    }
}

impl<W: Write> Printer<W> {
    pub fn new(out: W, theme: Theme, interactive: bool) -> Self {
        Self {
            out,
            theme,
            interactive,
            block: Vec::new(),
            block_rows: 0,
        }
    }

    /// Render one entry from the live stream.
    pub fn entry(&mut self, entry: &Entry, roster: &Roster) -> Result<()> {
        let rendered = if self.theme.quiet {
            quiet_text(entry, roster)
        } else {
            render_entry(entry, roster, &self.theme).map(|text| {
                match roster.conversation_label(&entry.conversation_id) {
                    Some(label) => format!("{} {text}", self.theme.dim(&format!("[{label}]"))),
                    None => text,
                }
            })
        };
        let Some(text) = rendered else {
            return Ok(());
        };
        if is_streamed(entry) {
            let turn = entry_turn(entry);
            match self.block.iter_mut().find(|slot| slot.id == entry.id) {
                Some(slot) => slot.text = text,
                None => self.block.push(BlockEntry {
                    id: entry.id.clone(),
                    turn,
                    text,
                }),
            }
            if self.interactive {
                self.paint_block()?;
            }
            return Ok(());
        }
        // A one-shot entry prints where it happened; the open block commits
        // first so the output keeps the real order.
        self.flush_block()?;
        writeln!(self.out, "{text}")?;
        self.out.flush()?;
        Ok(())
    }

    /// One JSON object per line, for `--json`: every event verbatim, flushed
    /// at once so `swarmbot run --json | jq` tails live.
    pub fn json_event(&mut self, event: &LiveEvent) -> Result<()> {
        let line = serde_json::to_string(event)?;
        writeln!(self.out, "{line}")?;
        self.out.flush()?;
        Ok(())
    }

    /// The same for a stored entry: `transcript --json` prints what the brain
    /// returned, one object per line, through the same sink as the stream.
    pub fn json_entry(&mut self, entry: &Entry) -> Result<()> {
        let line = serde_json::to_string(entry)?;
        writeln!(self.out, "{line}")?;
        self.out.flush()?;
        Ok(())
    }

    /// A prose line, committing anything currently open first.
    pub fn line(&mut self, text: &str) -> Result<()> {
        self.flush_block()?;
        writeln!(self.out, "{text}")?;
        self.out.flush()?;
        Ok(())
    }

    /// Decoration: a dim line that `--quiet` swallows.
    pub fn note(&mut self, text: &str) -> Result<()> {
        if self.theme.quiet {
            return Ok(());
        }
        self.line(&self.theme.dim(text))
    }

    /// A heading that `--quiet` also swallows.
    pub fn heading(&mut self, text: &str) -> Result<()> {
        if self.theme.quiet {
            return Ok(());
        }
        self.line(&self.theme.attribute(text, Attribute::Bold))
    }

    /// Text that survives `--quiet` (the bot's own words).
    pub fn plain(&mut self, text: &str) -> Result<()> {
        self.line(text)
    }

    /// End the stream: commit whatever the block still holds.
    pub fn finish(&mut self) -> Result<()> {
        self.flush_block()?;
        self.out.flush()?;
        Ok(())
    }

    /// Commit the whole block. On a terminal the latest paint is already on
    /// screen and only needs its newline; on a pipe this is where the text
    /// appears at all.
    pub fn flush_block(&mut self) -> Result<()> {
        if self.block.is_empty() {
            return Ok(());
        }
        let entries = std::mem::take(&mut self.block);
        if self.interactive && self.block_rows > 0 {
            writeln!(self.out)?;
        } else {
            for entry in &entries {
                writeln!(self.out, "{}", entry.text)?;
            }
        }
        self.block_rows = 0;
        self.out.flush()?;
        Ok(())
    }

    /// Commit one turn's entries: the runner's terminal `TurnUpsert` is the
    /// promise that they will not change again.
    ///
    /// On a pipe this is surgical — only that turn's lines print, so two bots
    /// streaming at once do not commit each other's half-written sentences.
    /// On a terminal there is no way to lift lines out of the middle of the
    /// painted block without a full-screen UI, so the whole block commits and
    /// anything still streaming repaints below; a rare duplicate beat is
    /// cheaper than a corrupted screen.
    pub fn flush_turn(&mut self, turn: &TurnId) -> Result<()> {
        if self.block.is_empty() {
            return Ok(());
        }
        if self.interactive {
            return self.flush_block();
        }
        let held = std::mem::take(&mut self.block);
        let (done, kept): (Vec<BlockEntry>, Vec<BlockEntry>) = held
            .into_iter()
            .partition(|entry| entry.turn.as_ref() == Some(turn));
        self.block = kept;
        for entry in done {
            writeln!(self.out, "{}", entry.text)?;
        }
        self.out.flush()?;
        Ok(())
    }

    /// Repaint the whole block in place: up to its first row, clear down,
    /// print the joined current texts.
    fn paint_block(&mut self) -> Result<()> {
        let joined = self
            .block
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if self.block_rows > 0 {
            queue!(
                self.out,
                MoveToColumn(0),
                MoveUp((self.block_rows - 1) as u16),
                Clear(ClearType::FromCursorDown)
            )?;
        }
        write!(self.out, "{joined}")?;
        self.out.flush()?;
        self.block_rows = count_rows(&joined);
        Ok(())
    }
}

/// The turn a streamed entry belongs to, for turn-scoped commits.
fn entry_turn(entry: &Entry) -> Option<TurnId> {
    match &entry.kind {
        EntryKind::Message(message) => message.turn_id.clone(),
        EntryKind::Reasoning(reasoning) => reasoning.turn_id.clone(),
        EntryKind::ToolCall(call) => call.turn_id.clone(),
        _ => None,
    }
}

/// `--quiet` means "only the bots' words": no events, tool calls or colour.
/// Inside a group (or off-conversation DM), keep the `@bot` / `[dm @bot]`
/// speaker prefix so multiple bots' outputs stay attributable.
fn quiet_text(entry: &Entry, roster: &Roster) -> Option<String> {
    let Author::Bot { id } = &entry.author else {
        return None;
    };
    let EntryKind::Message(message) = &entry.kind else {
        return None;
    };
    if message.text.is_empty() {
        return None;
    }
    let multi_bot = roster.is_group(&entry.conversation_id) || roster.is_group_focus();
    let label = roster.conversation_label(&entry.conversation_id);
    let target = message
        .addressed_to
        .as_ref()
        .map(|target_id| format!(" → {}", roster.handle(target_id)))
        .unwrap_or_default();
    if let Some(label) = label {
        return Some(format!(
            "[{label}] {}{target} › {}",
            roster.handle(id),
            message.text
        ));
    }
    if multi_bot {
        return Some(format!("{}{target} › {}", roster.handle(id), message.text));
    }
    if let Some(target_id) = &message.addressed_to {
        return Some(format!("{} {}", roster.handle(target_id), message.text));
    }
    Some(message.text.clone())
}

/// Feed one live event to the printer and the roster. `with_snapshot` decides
/// whether a subscription's initial snapshot replays its entries (a chat does;
/// a fresh `run`/`demo` must not double-print what it is about to cause).
pub fn print_event(
    printer: &mut Printer<impl Write>,
    roster: &mut Roster,
    event: &LiveEvent,
    with_snapshot: bool,
) -> Result<()> {
    roster.observe_event(event);
    match event {
        LiveEvent::Snapshot(snapshot) => {
            if with_snapshot {
                for entry in &snapshot.entries {
                    if roster.should_show(entry) {
                        printer.entry(entry, roster)?;
                    }
                }
            }
        }
        LiveEvent::EntryUpsert { entry, .. } => {
            if roster.should_show(entry) {
                printer.entry(entry, roster)?;
            }
        }
        LiveEvent::TurnUpsert { turn, .. } => {
            // The turn ended: its streamed entries are final. Committing here
            // (instead of only at stream end) keeps a piped tail live.
            if turn.status.is_terminal() {
                printer.flush_turn(&turn.id)?;
            }
        }
        LiveEvent::Resync { reason } => printer.note(&format!("resyncing the stream: {reason}"))?,
        LiveEvent::Failed { code, message } => printer.line(
            &printer
                .theme
                .paint(&format!("stream error [{code}]: {message}"), Color::Red),
        )?,
        // Status/turn/task/node events are already recorded into `roster` above;
        // `chat` reads RequestUpsert separately to drive its card prompt.
        _ => {}
    }
    Ok(())
}

fn count_rows(text: &str) -> usize {
    text.split('\n').count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::entry::Entry;
    use swarmbot_contracts::id::{RequestId, ToolCallId, TurnId};
    use swarmbot_contracts::live::LiveCursor;
    use swarmbot_contracts::node::Surface;
    use swarmbot_contracts::request::RuntimeRequest;
    use swarmbot_contracts::tool::ToolResult;
    use swarmbot_contracts::turn::Usage;
    use swarmbot_contracts::turn::{Turn, TurnInitiator};

    fn roster() -> Roster {
        let dm = ConversationId::new();
        let lead = Bot::new("lead", "Lead", "", "fake", dm.clone());
        let mut conversations = vec![Conversation {
            kind: swarmbot_contracts::conversation::ConversationKind::Direct {
                bot_id: lead.id.clone(),
            },
            ..Conversation::direct(BotId::new())
        }];
        conversations[0].id = dm.clone();
        Roster::new(vec![lead], conversations, Some(dm))
    }

    fn entry(kind: EntryKind) -> Entry {
        Entry::new(ConversationId::new(), 1, Author::Owner, kind)
    }

    fn message_entry(author: Author, text: &str) -> Entry {
        Entry::new(
            ConversationId::new(),
            1,
            author,
            EntryKind::Message(Box::new(MessageEntry {
                text: text.to_string(),
                attachments: Vec::new(),
                reply_to: None,
                post: PostState::Posted,
                addressed_to: None,
                turn_id: None,
            })),
        )
    }

    fn tool_entry(status: ToolCallStatus, result: Option<&str>) -> Entry {
        entry(EntryKind::ToolCall(Box::new(ToolCallEntry {
            call_id: ToolCallId::new(),
            tool: "shell".into(),
            args: serde_json::json!({"command": "cargo test"}),
            status,
            result_text: result.map(str::to_string),
            node_id: Some(NodeId::local()),
            task_id: None,
            turn_id: Some(TurnId::new()),
            spill_path: None,
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            duration_ms: Some(1234),
        })))
    }

    #[test]
    fn hex_colors_parse_and_garbage_does_not() {
        assert_eq!(
            hex_color("#599ce7"),
            Some(Color::Rgb {
                r: 0x59,
                g: 0x9c,
                b: 0xe7
            })
        );
        assert_eq!(
            hex_color("123456"),
            Some(Color::Rgb {
                r: 0x12,
                g: 0x34,
                b: 0x56
            })
        );
        assert_eq!(hex_color("#12345"), None);
        assert_eq!(hex_color("#gggggg"), None);
        assert_eq!(hex_color(""), None);
    }

    #[test]
    fn every_kind_renders_with_its_author_or_tool_named() {
        let roster = roster();
        let theme = Theme::plain();
        let lead_id = roster.bots[0].id.clone();

        let owner =
            render_entry(&message_entry(Author::Owner, "hello"), &roster, &theme).expect("renders");
        assert!(owner.contains("you › hello"), "{owner}");

        let bot = render_entry(
            &message_entry(Author::bot(lead_id.clone()), "on it"),
            &roster,
            &theme,
        )
        .expect("renders");
        assert!(bot.contains("@lead › on it"), "{bot}");

        let reasoning = render_entry(
            &entry(EntryKind::Reasoning(Box::new(ReasoningEntry {
                text: "let me think\nabout it".into(),
                turn_id: None,
            }))),
            &roster,
            &theme,
        )
        .expect("renders");
        assert!(reasoning.contains("thinking ›"), "{reasoning}");
        assert!(
            !reasoning.contains('\n'),
            "collapsed to one line: {reasoning:?}"
        );

        let tool = render_entry(
            &tool_entry(ToolCallStatus::Completed, Some("exit=0\nall tests passed")),
            &roster,
            &theme,
        )
        .expect("renders");
        assert!(tool.contains("shell"), "{tool}");
        assert!(tool.contains("exit=0"), "{tool}");
        assert!(tool.contains("1.2s"), "{tool}");
        assert!(tool.contains("node local"), "{tool}");

        let event = render_entry(
            &Entry::event(ConversationId::new(), 2, "bot-messaged", "Messaged @scout"),
            &roster,
            &theme,
        )
        .expect("renders");
        assert!(event.contains("→ Messaged @scout"), "{event}");

        let no_reply = render_entry(
            &Entry::event(
                ConversationId::new(),
                3,
                "no-reply",
                "@scout had nothing to add",
            ),
            &roster,
            &theme,
        )
        .expect("renders");
        assert!(
            no_reply.contains("· @scout had nothing to add"),
            "{no_reply}"
        );

        let hop = render_entry(
            &Entry::event(
                ConversationId::new(),
                4,
                "hop-limit",
                "Stopped: too many hand-offs (the limit is 8)",
            ),
            &roster,
            &theme,
        )
        .expect("renders");
        assert!(hop.starts_with('⊘'), "{hop}");

        let card = render_entry(
            &entry(EntryKind::Card(Box::new(CardEntry {
                request_id: RequestId::new(),
                card: RequestKind::Approval,
                status: RequestStatus::Pending,
                title: "Allow shell `cargo test`?".into(),
                body: "run the tests".into(),
                payload: serde_json::Value::Null,
                expires_at: Some(Utc::now() + chrono::Duration::seconds(599)),
            }))),
            &roster,
            &theme,
        )
        .expect("renders");
        assert!(card.contains("Allow shell"), "{card}");
        assert!(card.contains("expires in"), "{card}");
        assert!(card.contains('╭') && card.contains('╯'), "{card}");

        let notice = render_entry(
            &Entry::notice(
                ConversationId::new(),
                5,
                "backend refused",
                NoticeLevel::Error,
            ),
            &roster,
            &theme,
        )
        .expect("renders");
        assert!(notice.contains("backend refused"), "{notice}");

        let unknown = render_entry(&entry(EntryKind::Unknown), &roster, &theme).expect("renders");
        assert!(unknown.contains("does not know"), "{unknown}");
    }

    #[test]
    fn color_is_only_in_the_output_when_the_theme_says_so() {
        let roster = roster();
        let lead_id = roster.bots[0].id.clone();
        let message = message_entry(Author::bot(lead_id), "coloured");

        let plain = render_entry(&message, &roster, &Theme::plain()).expect("renders");
        assert!(!plain.contains('\u{1b}'), "{plain:?}");

        let colourful = Theme {
            color: true,
            ..Theme::plain()
        };
        let painted = render_entry(&message, &roster, &colourful).expect("renders");
        assert!(painted.contains('\u{1b}'), "escapes present: {painted:?}");
        assert!(painted.contains("@lead"), "{painted}");
    }

    #[test]
    fn no_color_strips_escapes_from_every_kind() {
        let roster = roster();
        let theme = Theme::plain();
        let entries = vec![
            message_entry(Author::Owner, "x"),
            entry(EntryKind::Reasoning(Box::new(ReasoningEntry {
                text: "hmm".into(),
                turn_id: None,
            }))),
            tool_entry(ToolCallStatus::Failed, Some("ERROR: boom")),
            Entry::event(
                ConversationId::new(),
                1,
                "handoff",
                "Handed off \"x\" to @scout",
            ),
            Entry::notice(ConversationId::new(), 2, "careful", NoticeLevel::Warning),
        ];
        for candidate in entries {
            let rendered = render_entry(&candidate, &roster, &theme).unwrap_or_default();
            assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        }
    }

    #[test]
    fn a_failed_tool_result_is_marked_and_an_error_result_renders_red() {
        let theme = Theme {
            color: true,
            ..Theme::plain()
        };
        let failed = render_tool_call(
            tool_entry(ToolCallStatus::Failed, Some("ERROR: cargo exploded"))
                .as_tool_call()
                .expect("a tool call"),
            None,
            &theme,
        );
        assert!(failed.contains("→ failed"), "{failed}");
        assert!(failed.contains("ERROR: cargo exploded"), "{failed}");
        assert!(failed.contains('\u{1b}'), "red is painted: {failed:?}");
    }

    #[test]
    fn a_long_tool_result_is_truncated_with_a_more_lines_marker() {
        let theme = Theme::plain();
        let long = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = render_tool_call(
            tool_entry(ToolCallStatus::Completed, Some(&long))
                .as_tool_call()
                .expect("a tool call"),
            None,
            &theme,
        );
        assert!(rendered.contains("… 14 more lines"), "{rendered}");
        assert!(rendered.contains("line 6"), "{rendered}");
        assert!(!rendered.contains("line 7"), "{rendered}");
    }

    #[test]
    fn a_shell_exit_line_is_not_repeated_in_the_body() {
        let theme = Theme::plain();
        let rendered = render_tool_call(
            tool_entry(ToolCallStatus::Completed, Some("exit=0\nhi"))
                .as_tool_call()
                .expect("a tool call"),
            None,
            &theme,
        );
        assert_eq!(rendered.matches("exit=0").count(), 1, "{rendered}");
        assert!(rendered.contains("hi"), "{rendered}");
    }

    #[test]
    fn reasoning_expands_under_verbose() {
        let theme = Theme {
            verbose: true,
            ..Theme::plain()
        };
        let reasoning = ReasoningEntry {
            text: "first thought\nsecond thought".into(),
            turn_id: None,
        };
        let rendered = render_reasoning(&reasoning, None, &theme);
        assert!(
            rendered.contains("first thought\nsecond thought"),
            "{rendered}"
        );
        let collapsed = render_reasoning(&reasoning, None, &Theme::plain());
        assert!(
            collapsed.contains("first thought second thought"),
            "{collapsed}"
        );
    }

    #[test]
    fn an_empty_streaming_placeholder_is_suppressed() {
        let roster = roster();
        let lead_id = roster.bots[0].id.clone();
        assert!(
            render_entry(
                &message_entry(Author::bot(lead_id), ""),
                &roster,
                &Theme::plain()
            )
            .is_none()
        );
    }

    #[test]
    fn a_deleted_author_renders_as_unknown() {
        let roster = roster();
        let ghost = message_entry(Author::bot(BotId::new()), "from the grave");
        let rendered = render_entry(&ghost, &roster, &Theme::plain()).expect("renders");
        assert!(rendered.contains("@unknown"), "{rendered}");
    }

    #[test]
    fn a_queued_owner_message_says_it_waits() {
        let roster = roster();
        let mut message = message_entry(Author::Owner, "later");
        if let Some(m) = message.as_message_mut() {
            m.post = PostState::Queued;
        }
        let rendered = render_entry(&message, &roster, &Theme::plain()).expect("renders");
        assert!(rendered.contains("(queued)"), "{rendered}");
    }

    #[test]
    fn labels_appear_only_outside_the_focus_conversation() {
        let mut roster = roster();
        let other = ConversationId::new();
        let dm = ConversationId::new();
        let scout = Bot::new("scout", "Scout", "", "fake", dm.clone());
        roster.upsert_bot(scout.clone());
        roster.upsert_conversation(Conversation {
            id: dm,
            kind: swarmbot_contracts::conversation::ConversationKind::Direct {
                bot_id: scout.id.clone(),
            },
            title: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_seq: 0,
            archived: false,
        });
        let focused = roster.focus.clone().expect("a focus");
        assert_eq!(roster.conversation_label(&focused), None);
        assert_eq!(
            roster.conversation_label(&other),
            None,
            "an unknown conversation gets no label rather than a wrong one"
        );
        assert_eq!(
            roster
                .conversation_label(&scout.direct_conversation_id)
                .as_deref(),
            Some("dm @scout")
        );
    }

    #[test]
    fn the_pipe_printer_coalesces_revisions_into_one_final_line() {
        let mut buffer = Vec::new();
        let roster = roster();
        let lead_id = roster.bots[0].id.clone();
        {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            let mut growing = message_entry(Author::bot(lead_id), "hel");
            // A turn id makes it a streamed entry: held, not printed per delta.
            growing.as_message_mut().expect("a message").turn_id = Some(TurnId::new());
            printer.entry(&growing, &roster).expect("holds");
            if let Some(m) = growing.as_message_mut() {
                m.text = "hello".into();
            }
            growing.revision += 1;
            printer.entry(&growing, &roster).expect("updates");
            // A one-shot owner line commits the held block first, so the
            // output keeps the real order.
            let next = message_entry(Author::Owner, "thanks");
            printer.entry(&next, &roster).expect("draws");
            printer.finish().expect("finishes");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert_eq!(out.lines().count(), 2, "{out:?}");
        assert!(out.contains("@lead › hello"), "{out}");
        assert!(
            !out.contains("hel\n"),
            "the intermediate revision is gone: {out:?}"
        );
        assert!(out.contains("you › thanks"), "{out}");
    }

    /// The regression the block model exists for: the runner's throttled
    /// flush alternates revisions of a round's reasoning and message entries,
    /// and each must still be printed exactly once — by the turn's end, in
    /// arrival order, with only the final text.
    #[test]
    fn interleaved_streamed_entries_commit_once_in_order() {
        let conversation = ConversationId::new();
        let dm = ConversationId::new();
        let lead = Bot::new("lead", "Lead", "", "fake", dm);
        let mut roster = Roster::new(vec![lead.clone()], Vec::new(), Some(conversation.clone()));
        let mut turn = Turn::start(
            lead.id.clone(),
            conversation.clone(),
            TurnInitiator::Owner,
            None,
            None,
        );
        let mut reasoning = Entry::new(
            conversation.clone(),
            1,
            Author::bot(lead.id.clone()),
            EntryKind::Reasoning(Box::new(ReasoningEntry {
                text: "thin".into(),
                turn_id: Some(turn.id.clone()),
            })),
        );
        let mut message = Entry::new(
            conversation.clone(),
            2,
            Author::bot(lead.id.clone()),
            EntryKind::Message(Box::new(MessageEntry {
                text: "hel".into(),
                attachments: Vec::new(),
                reply_to: None,
                post: PostState::Posted,
                addressed_to: None,
                turn_id: Some(turn.id.clone()),
            })),
        );

        let mut buffer = Vec::new();
        {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            for (thought, said) in [("thinking", "hello"), ("thinking hard", "hello world")] {
                if let EntryKind::Reasoning(r) = &mut reasoning.kind {
                    r.text = thought.into();
                }
                reasoning.revision += 1;
                printer.entry(&reasoning, &roster).expect("holds");
                if let EntryKind::Message(m) = &mut message.kind {
                    m.text = said.into();
                }
                message.revision += 1;
                printer.entry(&message, &roster).expect("holds");
            }
            turn.complete(Utc::now());
            print_event(
                &mut printer,
                &mut roster,
                &LiveEvent::TurnUpsert {
                    cursor: LiveCursor::new(1, 1),
                    turn: turn.clone(),
                },
                false,
            )
            .expect("the turn end commits");
            printer.finish().expect("finishes");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines,
            vec!["thinking › thinking hard", "@lead › hello world"],
            "{out:?}"
        );
    }

    #[test]
    fn quiet_prints_only_bot_words() {
        let mut buffer = Vec::new();
        let roster = roster();
        let lead_id = roster.bots[0].id.clone();
        {
            let mut printer = Printer::new(
                &mut buffer,
                Theme {
                    quiet: true,
                    ..Theme::plain()
                },
                false,
            );
            printer
                .entry(&message_entry(Author::Owner, "hi"), &roster)
                .expect("skipped");
            printer
                .entry(
                    &Entry::event(ConversationId::new(), 1, "handoff", "Handed off"),
                    &roster,
                )
                .expect("skipped");
            printer
                .entry(&message_entry(Author::bot(lead_id), "the answer"), &roster)
                .expect("printed");
            printer.note("decoration").expect("skipped");
            printer.finish().expect("finishes");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert_eq!(out, "the answer\n");
    }

    #[test]
    fn notes_and_lines_end_an_open_entry_first() {
        let mut buffer = Vec::new();
        let roster = roster();
        let lead_id = roster.bots[0].id.clone();
        {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), true);
            let mut message = message_entry(Author::bot(lead_id), "open line");
            message.as_message_mut().expect("a message").turn_id = Some(TurnId::new());
            // Interactive: the streamed entry is painted without a newline…
            printer.entry(&message, &roster).expect("paints");
            // …and any prose line must terminate it first.
            printer.line("a plain line").expect("prints");
            printer.finish().expect("finishes");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("@lead › open line\na plain line"), "{out:?}");
    }

    #[test]
    fn json_events_are_one_object_per_line() {
        let mut buffer = Vec::new();
        {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            printer
                .json_event(&LiveEvent::Synchronized {
                    cursor: swarmbot_contracts::live::LiveCursor::new(1, 2),
                })
                .expect("writes");
            let message = message_entry(Author::Owner, "multi\nline");
            printer
                .json_event(&LiveEvent::EntryUpsert {
                    cursor: swarmbot_contracts::live::LiveCursor::new(1, 3),
                    entry: message,
                })
                .expect("writes");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out:?}");
        for line in lines {
            let value: serde_json::Value = serde_json::from_str(line).expect("one object per line");
            assert!(value.get("type").is_some(), "{line}");
        }
    }

    /// `transcript --json` prints stored entries through the same sink as the
    /// stream, so a piped transcript is one parsable object per line too.
    #[test]
    fn json_entries_are_one_object_per_line() {
        let mut buffer = Vec::new();
        {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            printer
                .json_entry(&message_entry(Author::Owner, "first"))
                .expect("writes");
            printer
                .json_entry(&message_entry(Author::Owner, "second"))
                .expect("writes");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out:?}");
        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("one object per line");
        assert_eq!(first["author"]["kind"], "owner");
        assert_eq!(first["kind"]["text"], "first");
    }

    // Keep the imports used by helpers above referenced once, so an editing
    // mistake shows up as a compile error here rather than a silent drift.
    #[test]
    fn helper_shapes_compile() {
        let request = RuntimeRequest::approval(
            BotId::new(),
            ConversationId::new(),
            Surface::Shell,
            "shell",
            "cargo test",
            None,
        );
        assert_eq!(request.title(), "Allow shell `cargo test`?");
        let result = ToolResult::text("x");
        assert_eq!(result.text_only(), "x");
        let usage = Usage::default();
        assert_eq!(usage.total_tokens(), 0);
    }

    #[test]
    fn group_entries_attribute_reasoning_tool_calls_and_cards() {
        let mut group_roster = {
            let lead_dm = ConversationId::new();
            let scout_dm = ConversationId::new();
            let lead = Bot::new("lead", "Lead", "", "fake", lead_dm.clone());
            let scout = Bot::new("scout", "Scout", "", "fake", scout_dm.clone());
            let group = Conversation::group(
                "Team",
                vec![lead.id.clone(), scout.id.clone()],
                lead.id.clone(),
            )
            .expect("group");
            let mut dm_lead = Conversation::direct(lead.id.clone());
            dm_lead.id = lead_dm;
            let mut dm_scout = Conversation::direct(scout.id.clone());
            dm_scout.id = scout_dm;
            let group_id = group.id.clone();
            Roster::new(
                vec![lead, scout],
                vec![dm_lead, dm_scout, group],
                Some(group_id),
            )
        };
        let group_id = group_roster.focus().expect("focus").clone();
        let scout = group_roster.bots()[1].clone();
        let turn = Turn::start(
            scout.id.clone(),
            group_id.clone(),
            TurnInitiator::Owner,
            None,
            None,
        );
        group_roster.upsert_turn(turn.clone());

        let request = RuntimeRequest::approval(
            scout.id.clone(),
            group_id.clone(),
            Surface::Shell,
            "shell",
            "cargo check",
            None,
        );
        group_roster.upsert_request(request.clone());

        let reasoning = Entry::new(
            group_id.clone(),
            1,
            Author::bot(scout.id.clone()),
            EntryKind::Reasoning(Box::new(ReasoningEntry {
                text: "checking imports".into(),
                turn_id: Some(turn.id.clone()),
            })),
        );
        let tool = Entry::new(
            group_id.clone(),
            2,
            Author::System,
            EntryKind::ToolCall(Box::new(ToolCallEntry {
                call_id: ToolCallId::new(),
                tool: "shell".into(),
                args: serde_json::json!({"command": "cargo check"}),
                status: ToolCallStatus::Completed,
                result_text: Some("exit=0".into()),
                node_id: Some(NodeId::local()),
                task_id: None,
                spill_path: None,
                started_at: Utc::now(),
                ended_at: Some(Utc::now()),
                duration_ms: Some(120),
                turn_id: Some(turn.id.clone()),
            })),
        );
        let card = Entry::new(
            group_id,
            3,
            Author::System,
            EntryKind::Card(Box::new(CardEntry {
                request_id: request.id.clone(),
                card: RequestKind::Approval,
                status: RequestStatus::Pending,
                title: request.title(),
                body: String::new(),
                payload: serde_json::Value::Null,
                expires_at: None,
            })),
        );

        let theme = Theme::plain();
        let r_out = render_entry(&reasoning, &group_roster, &theme).expect("reasoning");
        let t_out = render_entry(&tool, &group_roster, &theme).expect("tool");
        let c_out = render_entry(&card, &group_roster, &theme).expect("card");
        assert!(
            r_out.contains("@scout · thinking › checking imports"),
            "{r_out}"
        );
        assert!(t_out.contains("@scout ⚙ shell"), "{t_out}");
        assert!(c_out.contains("approval · @scout"), "{c_out}");
    }

    #[test]
    fn messages_render_addressed_to_and_reply_to_attribution() {
        let mut group_roster = roster();
        let lead = group_roster.bots()[0].clone();
        let scout_dm = ConversationId::new();
        let scout = Bot::new("scout", "Scout", "", "fake", scout_dm);
        group_roster.upsert_bot(scout.clone());

        let mut dm_ask = message_entry(Author::bot(lead.id.clone()), "inspect build");
        dm_ask.as_message_mut().expect("message").addressed_to = Some(scout.id.clone());
        group_roster.upsert_entry(dm_ask.clone());

        let rendered_ask =
            render_entry(&dm_ask, &group_roster, &Theme::plain()).expect("renders ask");
        assert!(
            rendered_ask.contains("@lead → @scout › inspect build"),
            "{rendered_ask}"
        );

        let mut reply = message_entry(Author::Owner, "focus on Cargo.toml");
        reply.as_message_mut().expect("message").reply_to = Some(dm_ask.id.clone());
        let rendered_reply =
            render_entry(&reply, &group_roster, &Theme::plain()).expect("renders reply");
        assert!(
            rendered_reply.contains("you (↩ @lead: \"inspect build\") › focus on Cargo.toml"),
            "{rendered_reply}"
        );

        let missing = EntryId::new();
        let mut orphan_reply = message_entry(Author::Owner, "ping");
        orphan_reply.as_message_mut().expect("message").reply_to = Some(missing.clone());
        let rendered_orphan =
            render_entry(&orphan_reply, &group_roster, &Theme::plain()).expect("renders");
        assert!(rendered_orphan.contains("(↩ "), "{rendered_orphan}");
        assert!(rendered_orphan.contains("ping"), "{rendered_orphan}");
    }

    #[test]
    fn quiet_keeps_bot_attribution_in_groups_and_off_focus_dms() {
        let lead_dm = ConversationId::new();
        let scout_dm = ConversationId::new();
        let lead = Bot::new("lead", "Lead", "", "fake", lead_dm.clone());
        let scout = Bot::new("scout", "Scout", "", "fake", scout_dm.clone());
        let group = Conversation::group(
            "Team",
            vec![lead.id.clone(), scout.id.clone()],
            lead.id.clone(),
        )
        .expect("group");
        let mut dm_scout = Conversation::direct(scout.id.clone());
        dm_scout.id = scout_dm.clone();
        let mut dm_lead = Conversation::direct(lead.id.clone());
        dm_lead.id = lead_dm;
        let roster = Roster::new(
            vec![lead.clone(), scout.clone()],
            vec![dm_lead, dm_scout, group.clone()],
            Some(group.id.clone()),
        );

        let mut group_msg = Entry::new(
            group.id.clone(),
            1,
            Author::bot(lead.id.clone()),
            EntryKind::Message(Box::new(MessageEntry {
                text: "please check".into(),
                attachments: Vec::new(),
                reply_to: None,
                post: PostState::Posted,
                addressed_to: Some(scout.id.clone()),
                turn_id: None,
            })),
        );
        let dm_msg = Entry::new(
            scout_dm,
            1,
            Author::bot(scout.id),
            EntryKind::Message(Box::new(MessageEntry {
                text: "found two issues".into(),
                attachments: Vec::new(),
                reply_to: None,
                post: PostState::Posted,
                addressed_to: None,
                turn_id: None,
            })),
        );
        let mut buffer = Vec::new();
        {
            let mut printer = Printer::new(
                &mut buffer,
                Theme {
                    quiet: true,
                    ..Theme::plain()
                },
                false,
            );
            printer.entry(&group_msg, &roster).expect("group msg");
            printer.entry(&dm_msg, &roster).expect("dm msg");
            group_msg.as_message_mut().expect("message").addressed_to = None;
            printer.entry(&group_msg, &roster).expect("plain group msg");
            printer.finish().expect("finish");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(
            out.contains("@lead → @scout › please check"),
            "group addressee attribution preserved under --quiet: {out:?}"
        );
        assert!(
            out.contains("[dm @scout] @scout › found two issues"),
            "off-focus DM label & speaker preserved under --quiet: {out:?}"
        );
        assert!(
            out.contains("@lead › please check"),
            "group speaker preserved under --quiet: {out:?}"
        );
    }

    #[test]
    fn print_event_filters_unrelated_conversations_while_showing_group_dm_delegation() {
        let lead_dm = ConversationId::new();
        let scout_dm = ConversationId::new();
        let outsider_dm = ConversationId::new();
        let lead = Bot::new("lead", "Lead", "", "fake", lead_dm.clone());
        let scout = Bot::new("scout", "Scout", "", "fake", scout_dm.clone());
        let outsider = Bot::new("outsider", "Outsider", "", "fake", outsider_dm.clone());
        let team = Conversation::group(
            "Team",
            vec![lead.id.clone(), scout.id.clone()],
            lead.id.clone(),
        )
        .expect("team");
        let other_group = Conversation::group(
            "Other",
            vec![scout.id.clone(), outsider.id.clone()],
            outsider.id.clone(),
        )
        .expect("other");
        let mut conv_lead = Conversation::direct(lead.id.clone());
        conv_lead.id = lead_dm.clone();
        let mut conv_scout = Conversation::direct(scout.id.clone());
        conv_scout.id = scout_dm.clone();
        let mut conv_outsider = Conversation::direct(outsider.id.clone());
        conv_outsider.id = outsider_dm.clone();

        let mut team_task = Task::handoff(
            lead.id.clone(),
            scout.id.clone(),
            team.id.clone(),
            "inspect",
            "brief",
            None,
        );
        let mut other_task = Task::handoff(
            outsider.id.clone(),
            scout.id.clone(),
            other_group.id.clone(),
            "other",
            "brief",
            None,
        );
        let team_turn = Turn::start(
            scout.id.clone(),
            scout_dm.clone(),
            TurnInitiator::Task {
                task_id: team_task.id.clone(),
            },
            None,
            Some(team_task.id.clone()),
        );
        let other_turn = Turn::start(
            scout.id.clone(),
            scout_dm.clone(),
            TurnInitiator::Task {
                task_id: other_task.id.clone(),
            },
            None,
            Some(other_task.id.clone()),
        );
        let owner_dm_turn = Turn::start(
            scout.id.clone(),
            scout_dm.clone(),
            TurnInitiator::Owner,
            None,
            None,
        );
        let bot_dm_turn = Turn::start(
            scout.id.clone(),
            scout_dm.clone(),
            TurnInitiator::Bot {
                bot_id: lead.id.clone(),
            },
            None,
            None,
        );
        team_task.status = swarmbot_contracts::task::TaskStatus::Running;
        other_task.status = swarmbot_contracts::task::TaskStatus::Running;

        let mut roster = Roster::new(
            vec![lead.clone(), scout.clone(), outsider.clone()],
            vec![
                conv_lead,
                conv_scout,
                conv_outsider,
                team.clone(),
                other_group.clone(),
            ],
            Some(team.id.clone()),
        );
        roster.adopt_tasks(vec![team_task, other_task]);
        roster.upsert_turn(team_turn.clone());
        roster.upsert_turn(other_turn.clone());
        roster.upsert_turn(owner_dm_turn.clone());
        roster.upsert_turn(bot_dm_turn.clone());

        let msg = |conv: &ConversationId, author: Author, text: &str, turn: Option<&Turn>| {
            Entry::new(
                conv.clone(),
                1,
                author,
                EntryKind::Message(Box::new(MessageEntry {
                    text: text.into(),
                    attachments: Vec::new(),
                    reply_to: None,
                    post: PostState::Posted,
                    addressed_to: None,
                    turn_id: turn.map(|t| t.id.clone()),
                })),
            )
        };

        let mut buffer = Vec::new();
        {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            for entry in [
                msg(&team.id, Author::bot(lead.id.clone()), "in-team", None),
                msg(
                    &scout_dm,
                    Author::bot(scout.id.clone()),
                    "delegated-for-team",
                    Some(&team_turn),
                ),
                msg(
                    &scout_dm,
                    Author::bot(scout.id.clone()),
                    "member-bot-dm",
                    Some(&bot_dm_turn),
                ),
                msg(
                    &scout_dm,
                    Author::bot(scout.id.clone()),
                    "delegated-for-other",
                    Some(&other_turn),
                ),
                msg(
                    &scout_dm,
                    Author::bot(scout.id.clone()),
                    "private-owner-dm",
                    Some(&owner_dm_turn),
                ),
                msg(
                    &other_group.id,
                    Author::bot(outsider.id.clone()),
                    "other-group",
                    None,
                ),
                msg(
                    &outsider_dm,
                    Author::bot(outsider.id.clone()),
                    "outsider-dm",
                    None,
                ),
            ] {
                print_event(
                    &mut printer,
                    &mut roster,
                    &LiveEvent::EntryUpsert {
                        cursor: LiveCursor::new(1, 1),
                        entry,
                    },
                    false,
                )
                .expect("prints");
            }
            printer.finish().expect("finish");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("in-team"), "{out}");
        assert!(
            out.contains("[dm @scout] @scout › delegated-for-team"),
            "{out}"
        );
        assert!(out.contains("[dm @scout] @scout › member-bot-dm"), "{out}");
        assert!(!out.contains("delegated-for-other"), "{out}");
        assert!(!out.contains("private-owner-dm"), "{out}");
        assert!(!out.contains("other-group"), "{out}");
        assert!(!out.contains("outsider-dm"), "{out}");

        // Switching focus to a single DM suppresses group chatter.
        roster.set_focus(Some(lead_dm.clone()));
        assert!(!roster.should_show(&msg(&team.id, Author::bot(lead.id), "team-noise", None)));
        assert!(roster.should_show(&msg(&lead_dm, Author::Owner, "dm-direct", None)));
    }
}
