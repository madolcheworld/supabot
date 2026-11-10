//! [`AppState`]: the single source of truth the reducer mutates and the views
//! read.
//!
//! It is deliberately **plain data** — no GPUI entities, no handles, no clock
//! reads inside the mutation helpers — so [`crate::reducer::apply`] is a pure
//! function a `#[gpui::test]` can drive with fabricated [`LiveEvent`]s and
//! assert on. Per-screen GPUI entities (the transcript's scroll handle, the
//! composer's text buffer) live in the view layer and *read* this state; they
//! never own domain data.
//!
//! Invariants worth naming:
//!
//! * `entries[conv]` is always sorted by `seq` and de-duplicated by `EntryId`
//!   — a streamed reply is the *same* id at a rising `revision`, replaced in
//!   place, never appended (see [`AppState::upsert_entry`]).
//! * `cursor` is the newest [`LiveCursor`] seen; a reconnect resumes from it.
//! * collections are upserted by id, so an event for an unknown id inserts
//!   rather than being dropped on the floor.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use swarmbot_client::{BackendProbe, BackendView, Health, PairingCode};
use swarmbot_contracts::{
    Bot, BotId, Conversation, ConversationId, Entry, EntryId, LiveCursor, NodeId, NodeInfo,
    RequestId, Routine, RoutineId, RoutineRun, RoutineSchedule, RuntimeRequest, Settings, Task,
    TaskId, TaskStep, Turn, TurnStatus,
};

/// Which pane the main area shows. The delegation graph is an overlay panel
/// toggled independently of this.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    #[default]
    Conversation,
    Nodes,
    Routines,
    Settings,
}

/// The live connection's state, as the header reports it. Named for the four
/// honest labels the UI shows: `connected`, `reconnecting (attempt n)`,
/// `resynced`, `offline: reason`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// No socket yet (startup, or between a drop and the first retry).
    Connecting,
    /// Socket open and delivering events.
    Connected,
    /// The socket dropped; the worker is retrying with backoff.
    Reconnecting { attempt: u32 },
    /// Not connected and not retrying (a hard failure, or `--selftest`).
    Offline { reason: String },
}

impl ConnectionState {
    /// The one-line label for the header. `resynced` is layered on by the view
    /// from [`AppState::resynced_at`] because it is time-bounded, not a socket
    /// state.
    pub fn label(&self) -> String {
        match self {
            Self::Connecting => "connecting…".to_string(),
            Self::Connected => "connected".to_string(),
            Self::Reconnecting { attempt } => format!("reconnecting (attempt {attempt})"),
            Self::Offline { reason } => format!("offline: {reason}"),
        }
    }

    pub fn is_online(&self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// Everything the app knows, plus the owner's navigation state.
#[derive(Debug)]
pub struct AppState {
    // --- connection -------------------------------------------------------
    pub brain_url: String,
    pub conn: ConnectionState,
    pub cursor: Option<LiveCursor>,
    /// Set when a resync (fresh snapshot after a gap) completes; the header
    /// shows "resynced" for a moment, then falls back to `conn.label()`.
    pub resynced_at: Option<Instant>,
    pub health: Option<Health>,

    // --- domain data (from the snapshot + upserts) ------------------------
    pub bots: Vec<Bot>,
    pub conversations: Vec<Conversation>,
    pub nodes: Vec<NodeInfo>,
    pub tasks: Vec<Task>,
    pub requests: Vec<RuntimeRequest>,
    pub turns: Vec<Turn>,
    /// Entries per conversation, sorted by `seq`, unique by id.
    pub entries: HashMap<ConversationId, Vec<Entry>>,
    /// Unread non-owner entries per conversation, reset when it is opened.
    pub unread: HashMap<ConversationId, u32>,
    /// Task steps fetched on demand (clicking a graph node).
    pub steps: HashMap<TaskId, Vec<TaskStep>>,

    // --- settings / backends (REST, not in the snapshot) ------------------
    pub settings: Option<Settings>,
    /// The owner's in-progress edits; `Save` diffs this against `settings` and
    /// PATCHes only the keys that changed.
    pub settings_draft: Option<Settings>,
    pub backends: Vec<BackendView>,
    pub probes: Option<Vec<BackendProbe>>,
    /// The provider menu (per-bot backend + model override) is open.
    pub bot_menu_open: bool,
    /// The bot being configured inside the open menu (`None` = infer from DM).
    pub menu_bot: Option<BotId>,
    /// The provider highlighted inside the open menu.
    pub menu_backend: Option<String>,
    /// The model picked inside the open menu (`None` = backend default).
    pub menu_model: Option<String>,
    /// Cached per-backend model listings. Absent key = not fetched yet;
    /// `Ok([])` = fetched, the backend advertises no list; `Err` = the fetch
    /// failed and the menu must say so (with a Retry), not pretend the
    /// backend has nothing to offer.
    pub models: HashMap<String, Result<swarmbot_client::BackendModels, String>>,
    pub pairing: Option<PairingCode>,
    pub routines: Vec<Routine>,
    pub routine_runs: HashMap<RoutineId, Vec<RoutineRun>>,
    pub selected_routine: Option<RoutineId>,
    pub routine_creator_open: bool,
    pub routine_draft_bot: Option<BotId>,
    pub routine_draft_schedule: RoutineSchedule,

    // --- navigation / view state -----------------------------------------
    pub screen: Screen,
    pub open_conversation: Option<ConversationId>,
    pub open_task: Option<TaskId>,
    pub graph_open: bool,
    pub palette_open: bool,

    // --- auth -------------------------------------------------------------
    /// The brain answered 401: the window shows a token field.
    pub needs_login: bool,
    pub login_error: Option<String>,

    // --- transient --------------------------------------------------------
    /// Advances while anything is busy, to drive spinners.
    pub spinner_phase: usize,
    /// A short status line for the last action ("sent", "stopped", an error).
    pub toast: Option<String>,

    // --- per-entry view state (collapsibles) ------------------------------
    /// Reasoning blocks and tool-call cards the owner has expanded.
    pub expanded: HashSet<EntryId>,
    /// Tool-call results shown in full rather than truncated.
    pub expanded_full: HashSet<EntryId>,
}

impl AppState {
    /// A disconnected, empty state pointed at `brain_url`.
    pub fn new(brain_url: impl Into<String>) -> Self {
        Self {
            brain_url: brain_url.into(),
            conn: ConnectionState::Connecting,
            cursor: None,
            resynced_at: None,
            health: None,
            bots: Vec::new(),
            conversations: Vec::new(),
            nodes: Vec::new(),
            tasks: Vec::new(),
            requests: Vec::new(),
            turns: Vec::new(),
            entries: HashMap::new(),
            unread: HashMap::new(),
            steps: HashMap::new(),
            settings: None,
            settings_draft: None,
            backends: Vec::new(),
            probes: None,
            bot_menu_open: false,
            menu_bot: None,
            menu_backend: None,
            menu_model: None,
            models: HashMap::new(),
            pairing: None,
            routines: Vec::new(),
            routine_runs: HashMap::new(),
            selected_routine: None,
            routine_creator_open: false,
            routine_draft_bot: None,
            routine_draft_schedule: RoutineSchedule::Interval { every_secs: 300 },
            screen: Screen::default(),
            open_conversation: None,
            open_task: None,
            graph_open: true,
            palette_open: false,
            needs_login: false,
            login_error: None,
            spinner_phase: 0,
            toast: None,
            expanded: HashSet::new(),
            expanded_full: HashSet::new(),
        }
    }

    // --- collapsible view state -------------------------------------------

    pub fn is_expanded(&self, id: &EntryId) -> bool {
        self.expanded.contains(id)
    }

    pub fn toggle_expanded(&mut self, id: &EntryId) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.clone());
        }
    }

    pub fn is_full(&self, id: &EntryId) -> bool {
        self.expanded_full.contains(id)
    }

    pub fn toggle_full(&mut self, id: &EntryId) {
        if !self.expanded_full.remove(id) {
            self.expanded_full.insert(id.clone());
        }
    }

    // --- lookups ----------------------------------------------------------

    pub fn bot(&self, id: &BotId) -> Option<&Bot> {
        self.bots.iter().find(|b| &b.id == id)
    }

    pub fn conversation(&self, id: &ConversationId) -> Option<&Conversation> {
        self.conversations.iter().find(|c| &c.id == id)
    }

    pub fn node(&self, id: &NodeId) -> Option<&NodeInfo> {
        self.nodes.iter().find(|n| &n.id == id)
    }

    pub fn task(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| &t.id == id)
    }

    pub fn request(&self, id: &RequestId) -> Option<&RuntimeRequest> {
        self.requests.iter().find(|r| &r.id == id)
    }

    pub fn turn(&self, id: &swarmbot_contracts::TurnId) -> Option<&Turn> {
        self.turns.iter().find(|t| &t.id == id)
    }

    /// The conversation the main pane is showing.
    pub fn open_conversation(&self) -> Option<&Conversation> {
        self.open_conversation
            .as_ref()
            .and_then(|id| self.conversation(id))
    }

    /// Entries of one conversation, sorted by `seq`.
    pub fn entries(&self, id: &ConversationId) -> &[Entry] {
        self.entries.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Entries of the open conversation.
    pub fn open_entries(&self) -> &[Entry] {
        match &self.open_conversation {
            Some(id) => self.entries(id),
            None => &[],
        }
    }

    pub fn unread(&self, id: &ConversationId) -> u32 {
        self.unread.get(id).copied().unwrap_or(0)
    }

    /// A display title: a group's title, or a DM's bot label / handle.
    pub fn conversation_title(&self, conversation: &Conversation) -> String {
        if let Some(title) = &conversation.title
            && !title.trim().is_empty()
        {
            return title.clone();
        }
        match conversation.direct_bot() {
            Some(bot_id) => self
                .bot(bot_id)
                .map(|b| b.label.clone())
                .unwrap_or_else(|| bot_id.as_str().to_string()),
            None => conversation.id.as_str().to_string(),
        }
    }

    /// The bots taking part in a conversation, resolved to roster entries where
    /// possible (a DM's bot, or a group's members).
    pub fn members(&self, id: &ConversationId) -> Vec<BotId> {
        match self.conversation(id) {
            Some(c) => c.kind.participants(),
            None => Vec::new(),
        }
    }

    /// One-line preview for the sidebar: the newest entry's text, if any.
    pub fn preview(&self, id: &ConversationId) -> Option<String> {
        self.entries(id).last().and_then(|e| {
            e.kind.text().map(|t| {
                let first_line = t.lines().next().unwrap_or("").trim();
                if first_line.chars().count() > 60 {
                    let truncated: String = first_line.chars().take(60).collect();
                    format!("{truncated}…")
                } else {
                    first_line.to_string()
                }
            })
        })
    }

    /// Tasks whose result comes back to this conversation, newest first.
    pub fn tasks_for(&self, id: &ConversationId) -> Vec<Task> {
        let mut tasks: Vec<Task> = self
            .tasks
            .iter()
            .filter(|t| &t.origin == id)
            .cloned()
            .collect();
        tasks.sort_by_key(|task| std::cmp::Reverse(task.created_at));
        tasks
    }

    /// Pending requests belonging to a conversation.
    pub fn pending_requests_for(&self, id: &ConversationId) -> Vec<RuntimeRequest> {
        self.requests
            .iter()
            .filter(|r| &r.conversation_id == id && r.is_pending())
            .cloned()
            .collect()
    }

    /// The running turn in a conversation, if any — drives "Handling…" and the
    /// Stop button's enabled state.
    pub fn running_turn(&self, id: &ConversationId) -> Option<&Turn> {
        self.turns
            .iter()
            .find(|t| &t.conversation_id == id && t.status == TurnStatus::Running)
    }

    /// The bot currently working in a conversation, for the "Handling…" line.
    pub fn handling_bot(&self, id: &ConversationId) -> Option<&Bot> {
        self.running_turn(id)
            .and_then(|turn| self.bot(&turn.bot_id))
    }

    /// True when any bot is working or any turn is running — the app animates
    /// spinners only while this holds, to keep an idle window cheap.
    pub fn any_busy(&self) -> bool {
        self.turns.iter().any(|t| t.status == TurnStatus::Running)
            || self.bots.iter().any(|b| b.status.is_busy())
            || self.tasks.iter().any(|t| {
                use swarmbot_contracts::TaskStatus;
                matches!(t.status, TaskStatus::Running | TaskStatus::Queued)
            })
    }

    /// Total unread across all conversations (for a badge on the app title).
    pub fn total_unread(&self) -> u32 {
        self.unread.values().sum()
    }

    // --- mutations (used by the reducer and by UI actions) ----------------

    /// Open a conversation: set the selection, clear its unread, and switch the
    /// main pane back to the transcript.
    pub fn open(&mut self, id: ConversationId) {
        self.open_conversation = Some(id.clone());
        self.unread.insert(id, 0);
        self.screen = Screen::Conversation;
    }

    pub fn upsert_bot(&mut self, bot: Bot) {
        match self.bots.iter_mut().find(|b| b.id == bot.id) {
            Some(slot) => *slot = bot,
            None => self.bots.push(bot),
        }
    }

    /// Update only a bot's live status (the roster-only `BotStatus` event).
    pub fn set_bot_status(&mut self, id: &BotId, status: swarmbot_contracts::BotStatus) {
        if let Some(bot) = self.bots.iter_mut().find(|b| &b.id == id) {
            bot.status = status;
        }
    }

    pub fn upsert_conversation(&mut self, conversation: Conversation) {
        match self
            .conversations
            .iter_mut()
            .find(|c| c.id == conversation.id)
        {
            Some(slot) => *slot = conversation,
            None => self.conversations.push(conversation),
        }
    }

    pub fn remove_conversation(&mut self, id: &ConversationId) {
        self.conversations.retain(|c| &c.id != id);
        self.entries.remove(id);
        self.unread.remove(id);
        if self.open_conversation.as_ref() == Some(id) {
            self.open_conversation = None;
        }
    }

    pub fn upsert_node(&mut self, node: NodeInfo) {
        match self.nodes.iter_mut().find(|n| n.id == node.id) {
            Some(slot) => *slot = node,
            None => self.nodes.push(node),
        }
    }

    pub fn set_node_presence(&mut self, id: &NodeId, online: bool) {
        if let Some(node) = self.nodes.iter_mut().find(|n| &n.id == id) {
            node.online = online;
        }
    }

    pub fn upsert_task(&mut self, task: Task) {
        match self.tasks.iter_mut().find(|t| t.id == task.id) {
            Some(slot) => *slot = task,
            None => self.tasks.push(task),
        }
    }

    pub fn upsert_request(&mut self, request: RuntimeRequest) {
        match self.requests.iter_mut().find(|r| r.id == request.id) {
            Some(slot) => *slot = request,
            None => self.requests.push(request),
        }
    }

    pub fn upsert_turn(&mut self, turn: Turn) {
        match self.turns.iter_mut().find(|t| t.id == turn.id) {
            Some(slot) => *slot = turn,
            None => self.turns.push(turn),
        }
    }

    /// Insert-or-replace one entry, keyed by id, keeping the list sorted by
    /// `seq`.
    ///
    /// A streamed reply and a tool call both arrive repeatedly under one id at a
    /// rising `revision`; this replaces the row instead of appending, which is
    /// the difference between a transcript and a bot "saying" its reply eleven
    /// times. A lower `revision` than what is already stored is ignored as a
    /// stale/reordered write rather than clobbering newer text.
    pub fn upsert_entry(&mut self, entry: Entry) -> bool {
        let list = self
            .entries
            .entry(entry.conversation_id.clone())
            .or_default();
        if let Some(slot) = list.iter_mut().find(|e| e.id == entry.id) {
            if entry.revision < slot.revision {
                return false;
            }
            *slot = entry;
            return true;
        }
        list.push(entry);
        list.sort_by_key(|e| e.seq);
        true
    }

    pub fn upsert_routine(&mut self, routine: Routine) {
        match self.routines.iter_mut().find(|r| r.id == routine.id) {
            Some(slot) => *slot = routine,
            None => self.routines.push(routine),
        }
        self.routines.sort_by(|a, b| a.name.cmp(&b.name));
    }

    pub fn remove_routine(&mut self, id: &RoutineId) {
        self.routines.retain(|r| &r.id != id);
        self.routine_runs.remove(id);
        if self.selected_routine.as_ref() == Some(id) {
            self.selected_routine = None;
        }
    }

    /// Replace a conversation's entries wholesale (used when a snapshot or a
    /// per-conversation `tail` arrives).
    pub fn set_entries(&mut self, id: ConversationId, mut entries: Vec<Entry>) {
        entries.sort_by_key(|e| e.seq);
        entries.dedup_by(|a, b| a.id == b.id);
        self.entries.insert(id, entries);
    }
}
