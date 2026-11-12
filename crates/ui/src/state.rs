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
use std::path::PathBuf;
use std::time::Instant;

use swarmbot_client::{BackendProbe, BackendView, Health, PairingCode};
use swarmbot_contracts::{
    Bot, BotId, BotStatus, Conversation, ConversationId, ConversationKind, Entry, EntryId,
    LiveCursor, NodeId, NodeInfo, RequestId, Routine, RoutineId, RoutineRun, RoutineSchedule,
    RuntimeRequest, Settings, Task, TaskId, TaskStatus, TaskStep, Turn, TurnId, TurnStatus,
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

/// A target selected for deletion confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeleteTarget {
    Bot { id: BotId, label: String },
    Group { id: ConversationId, title: String },
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
    /// Popup menu opened by the `+` button in the left sidebar.
    pub plus_menu_open: bool,
    pub create_group_open: bool,
    pub create_group_title: String,
    pub create_group_members: Vec<BotId>,
    pub create_group_lead: Option<BotId>,
    pub edit_group_id: Option<ConversationId>,
    /// One-shot seed pushed into `edit_group_title_input` when opening the modal.
    pub edit_group_title_seed: Option<String>,
    pub edit_group_members: Vec<BotId>,
    pub edit_group_lead: Option<BotId>,
    pub create_bot_open: bool,
    pub create_bot_backend: Option<String>,
    pub confirm_delete: Option<DeleteTarget>,
    /// Entry in the open conversation currently targeted by "Reply".
    pub reply_to: Option<EntryId>,
    /// Brain data dir when locally spawned or explicitly supplied via `--data-dir`.
    pub data_dir: Option<PathBuf>,

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
    /// Entries (such as error notices) dismissed by the user.
    pub dismissed: HashSet<EntryId>,
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
            plus_menu_open: false,
            create_group_open: false,
            create_group_title: String::new(),
            create_group_members: Vec::new(),
            create_group_lead: None,
            edit_group_id: None,
            edit_group_title_seed: None,
            edit_group_members: Vec::new(),
            edit_group_lead: None,
            create_bot_open: false,
            create_bot_backend: None,
            confirm_delete: None,
            reply_to: None,
            data_dir: None,
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
            dismissed: HashSet::new(),
        }
    }

    pub fn bot_workspace(&self, bot_name: &str) -> Option<PathBuf> {
        self.data_dir
            .as_ref()
            .map(|dir| dir.join("workspaces").join(bot_name))
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

    pub fn is_dismissed(&self, id: &EntryId) -> bool {
        self.dismissed.contains(id)
    }

    pub fn dismiss_entry(&mut self, id: &EntryId) {
        self.dismissed.insert(id.clone());
        for entries in self.entries.values_mut() {
            entries.retain(|e| &e.id != id);
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

    pub fn turn(&self, id: &TurnId) -> Option<&Turn> {
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

    /// Pending requests relevant to `conv`: requests recorded directly in `conv`
    /// plus requests raised inside delegated `handoff_task` DM turns whose
    /// originating task comes from `conv`.
    pub fn group_pending_requests(&self, conv: &Conversation) -> Vec<RuntimeRequest> {
        let task_origin = |r: &RuntimeRequest| -> Option<&ConversationId> {
            let tid = r.task_id.as_ref().or_else(|| {
                r.turn_id
                    .as_ref()
                    .and_then(|turn_id| self.turn(turn_id))
                    .and_then(|t| t.task_id.as_ref())
            });
            tid.and_then(|id| self.task(id)).map(|t| &t.origin)
        };
        let members = conv.kind.participants();
        self.requests
            .iter()
            .filter(|r| {
                if !r.is_pending() {
                    return false;
                }
                if r.conversation_id == conv.id {
                    return true;
                }
                if task_origin(r) == Some(&conv.id) {
                    return true;
                }
                // If a group member raised a pending request inside its own DM
                // without a linked task origin from another conversation,
                // keep it visible while viewing the group.
                if conv.kind.is_group()
                    && members.contains(&r.bot_id)
                    && task_origin(r).is_none()
                    && self
                        .bot(&r.bot_id)
                        .is_some_and(|b| b.direct_conversation_id == r.conversation_id)
                {
                    return true;
                }
                false
            })
            .cloned()
            .collect()
    }

    /// The running turn in a conversation, if any — drives the Stop button's
    /// enabled state and DM activity line.
    pub fn running_turn(&self, id: &ConversationId) -> Option<&Turn> {
        self.turns
            .iter()
            .find(|t| &t.conversation_id == id && t.status == TurnStatus::Running)
    }

    /// All turns currently running in a conversation.
    pub fn running_turns(&self, id: &ConversationId) -> Vec<&Turn> {
        self.turns
            .iter()
            .filter(|t| &t.conversation_id == id && t.status == TurnStatus::Running)
            .collect()
    }

    /// The bot currently working in a conversation, for the "Handling…" line.
    pub fn handling_bot(&self, id: &ConversationId) -> Option<&Bot> {
        self.running_turn(id)
            .and_then(|turn| self.bot(&turn.bot_id))
    }

    /// True when any turn, member bot, or task originating from `group` is active.
    pub fn is_group_working(&self, group: &Conversation) -> bool {
        if self.running_turn(&group.id).is_some() {
            return true;
        }
        let members = group.kind.participants();
        if members
            .iter()
            .any(|id| self.bot(id).is_some_and(|b| b.status.is_busy()))
        {
            return true;
        }
        self.tasks.iter().any(|t| {
            t.origin == group.id && matches!(t.status, TaskStatus::Running | TaskStatus::Queued)
        })
    }

    /// All currently active/working bots for a group conversation, including
    /// bots running delegated DM tasks whose `origin` is `conv.id`.
    pub fn active_group_activities(&self, conv: &Conversation) -> Vec<(Bot, String)> {
        let mut out: Vec<(Bot, String)> = Vec::new();
        let mut push_unique = |bot: &Bot, activity: String| {
            if let Some((_, existing)) = out.iter_mut().find(|(b, _)| b.id == bot.id) {
                if existing == "Working…" && activity != "Working…" {
                    *existing = activity;
                }
                return;
            }
            out.push((bot.clone(), activity));
        };

        let bot_activity = |bot: &Bot, fallback: &str| -> String {
            match &bot.status {
                BotStatus::Working { activity, .. } if !activity.trim().is_empty() => {
                    activity.clone()
                }
                _ => fallback.to_string(),
            }
        };

        for turn in self.running_turns(&conv.id) {
            if let Some(bot) = self.bot(&turn.bot_id) {
                let act = bot_activity(bot, "Working…");
                push_unique(bot, act);
            }
        }

        // Delegated tasks whose origin is this group (running or queued).
        for task in self.tasks.iter().filter(|t| {
            t.origin == conv.id && matches!(t.status, TaskStatus::Running | TaskStatus::Queued)
        }) {
            if let Some(bot) = self.bot(&task.owner) {
                let fallback = if task.title.trim().is_empty() {
                    "Delegated task…".to_string()
                } else {
                    format!("task: {}", task.title)
                };
                let act = bot_activity(bot, &fallback);
                push_unique(bot, act);
            }
        }

        // Also include any group member whose bot status is busy (e.g.
        // running a group-triggered turn before task state settles).
        for member_id in conv.kind.participants() {
            if let Some(bot) = self.bot(&member_id)
                && bot.status.is_busy()
            {
                let act = bot_activity(bot, "Working…");
                push_unique(bot, act);
            }
        }

        out
    }

    /// True when any bot is working or any turn is running — the app animates
    /// spinners only while this holds, to keep an idle window cheap.
    pub fn any_busy(&self) -> bool {
        self.turns.iter().any(|t| t.status == TurnStatus::Running)
            || self.bots.iter().any(|b| b.status.is_busy())
            || self
                .tasks
                .iter()
                .any(|t| matches!(t.status, TaskStatus::Running | TaskStatus::Queued))
    }

    /// Total unread across all conversations (for a badge on the app title).
    pub fn total_unread(&self) -> u32 {
        self.unread.values().sum()
    }

    // --- mutations (used by the reducer and by UI actions) ----------------

    /// Open a conversation: set the selection, clear its unread, and switch the
    /// main pane back to the transcript.
    pub fn open(&mut self, id: ConversationId) {
        if self.open_conversation.as_ref() != Some(&id) {
            self.reply_to = None;
        }
        self.open_conversation = Some(id.clone());
        self.unread.insert(id, 0);
        self.plus_menu_open = false;
        self.screen = Screen::Conversation;
    }

    /// Open the Add Bot modal and close any competing creation/menu overlays.
    pub fn open_create_bot(&mut self) {
        self.plus_menu_open = false;
        self.create_bot_open = true;
        self.create_group_open = false;
        self.edit_group_id = None;
    }

    /// Open the Create Group modal with an empty member selection so bots are
    /// added only when explicitly chosen.
    pub fn open_create_group(&mut self) {
        self.plus_menu_open = false;
        self.create_group_open = true;
        self.create_bot_open = false;
        self.edit_group_id = None;
        self.create_group_title.clear();
        self.create_group_members.clear();
        self.create_group_lead = None;
    }

    /// Open the Edit Group modal seeded with an existing group's title,
    /// members, and lead bot.
    pub fn open_edit_group(&mut self, conv: &Conversation) {
        let ConversationKind::Group { members, lead } = &conv.kind else {
            return;
        };
        self.plus_menu_open = false;
        self.create_group_open = false;
        self.create_bot_open = false;
        self.bot_menu_open = false;
        self.confirm_delete = None;
        self.edit_group_id = Some(conv.id.clone());
        self.edit_group_title_seed = Some(self.conversation_title(conv));
        self.edit_group_members = members.clone();
        self.edit_group_lead = Some(lead.clone());
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

    pub fn remove_bot(&mut self, id: &BotId) {
        if let Some(bot) = self.bots.iter().find(|b| &b.id == id) {
            let dm = bot.direct_conversation_id.clone();
            self.remove_conversation(&dm);
        }
        self.bots.retain(|b| &b.id != id);
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
            self.reply_to = None;
        }
        if self.edit_group_id.as_ref() == Some(id) {
            self.edit_group_id = None;
            self.edit_group_title_seed = None;
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
        if self.dismissed.contains(&entry.id) {
            return false;
        }
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
        entries.retain(|e| !self.dismissed.contains(&e.id));
        entries.sort_by_key(|e| e.seq);
        entries.dedup_by(|a, b| a.id == b.id);
        self.entries.insert(id, entries);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_helpers_toggle_exclusive_overlays_and_clear_group_selection() {
        let mut state = AppState::new("http://brain.test");
        state.plus_menu_open = true;
        state.create_group_title = "Old".into();
        state.create_group_members = vec![BotId::new()];

        state.open_create_bot();
        assert!(!state.plus_menu_open);
        assert!(state.create_bot_open);
        assert!(!state.create_group_open);

        state.plus_menu_open = true;
        state.open_create_group();
        assert!(!state.plus_menu_open);
        assert!(!state.create_bot_open);
        assert!(state.create_group_open);
        assert!(state.create_group_title.is_empty());
        assert!(state.create_group_members.is_empty());

        state.plus_menu_open = true;
        state.open(ConversationId::new());
        assert!(!state.plus_menu_open);
    }

    #[test]
    fn group_working_and_active_bots_include_delegated_dm_tasks() {
        use swarmbot_contracts::{Task, TurnInitiator};

        let mut state = AppState::new("http://brain.test");
        let b1 = Bot::new("lead", "Lead", "", "fake", ConversationId::new());
        let dm2 = ConversationId::new();
        let mut b2 = Bot::new("scout", "Scout", "", "fake", dm2.clone());
        b2.status = BotStatus::Working {
            activity: "Scanning repo".into(),
            conversation_id: dm2,
        };
        let b1_id = b1.id.clone();
        let b2_id = b2.id.clone();
        state.upsert_bot(b1);
        state.upsert_bot(b2);

        let group =
            Conversation::group("Launch", vec![b1_id.clone(), b2_id.clone()], b1_id.clone())
                .expect("group");
        let gid = group.id.clone();
        state.upsert_conversation(group.clone());

        // Turn running directly in group + delegated task owned by b2 originating from group.
        state.upsert_turn(Turn::start(
            b1_id.clone(),
            gid.clone(),
            TurnInitiator::Owner,
            None,
            None,
        ));
        let task = Task::handoff(
            b1_id.clone(),
            b2_id.clone(),
            gid.clone(),
            "Inspect docs",
            "Inspect docs",
            None,
        );
        state.upsert_task(task);

        assert!(state.is_group_working(&group));
        let active = state.active_group_activities(&group);
        assert_eq!(active.len(), 2);
        assert!(active.iter().any(|(b, _)| b.id == b1_id));
        assert!(
            active
                .iter()
                .any(|(b, act)| b.id == b2_id && act == "Scanning repo")
        );
    }

    #[test]
    fn group_pending_requests_surface_delegated_dm_handoff_cards() {
        use swarmbot_contracts::{Surface, Task};

        let mut state = AppState::new("http://brain.test");
        let dm1 = ConversationId::new();
        let dm2 = ConversationId::new();
        let b1 = Bot::new("lead", "Lead", "", "fake", dm1.clone());
        let b2 = Bot::new("scout", "Scout", "", "fake", dm2.clone());
        let b1_id = b1.id.clone();
        let b2_id = b2.id.clone();
        state.upsert_bot(b1);
        state.upsert_bot(b2);

        let group = Conversation::group("Team", vec![b1_id.clone(), b2_id.clone()], b1_id.clone())
            .expect("group");
        let gid = group.id.clone();
        let other_group = ConversationId::new();
        state.upsert_conversation(group.clone());

        // Task originating from `group`, running in `dm2`.
        let task = Task::handoff(
            b1_id.clone(),
            b2_id.clone(),
            gid.clone(),
            "Check host",
            "Check host",
            None,
        );
        let mut req_from_group_task = RuntimeRequest::approval(
            b2_id.clone(),
            dm2.clone(),
            Surface::Shell,
            "shell",
            "ls",
            None,
        );
        req_from_group_task.task_id = Some(task.id.clone());
        state.upsert_task(task);
        state.upsert_request(req_from_group_task.clone());

        // Task originating from a DIFFERENT group — must NOT leak into `group`.
        let other_task = Task::handoff(
            b1_id,
            b2_id.clone(),
            other_group,
            "Unrelated",
            "Unrelated",
            None,
        );
        let mut req_other_group =
            RuntimeRequest::question(b2_id, dm2, "unrelated question", vec![]);
        req_other_group.task_id = Some(other_task.id.clone());
        state.upsert_task(other_task);
        state.upsert_request(req_other_group);

        let surfaced = state.group_pending_requests(&group);
        assert_eq!(surfaced.len(), 1);
        assert_eq!(surfaced[0].id, req_from_group_task.id);
    }

    #[test]
    fn open_edit_group_seeds_existing_title_members_and_lead() {
        let mut state = AppState::new("http://brain.test");
        let b1 = BotId::new();
        let b2 = BotId::new();
        let group =
            Conversation::group("Ops", vec![b1.clone(), b2.clone()], b2.clone()).expect("group");
        let gid = group.id.clone();
        state.open_edit_group(&group);
        assert_eq!(state.edit_group_id, Some(gid));
        assert_eq!(state.edit_group_title_seed.as_deref(), Some("Ops"));
        assert_eq!(state.edit_group_members, vec![b1, b2.clone()]);
        assert_eq!(state.edit_group_lead, Some(b2));
    }
}
