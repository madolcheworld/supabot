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
//! * a collection whose emptiness is *news* is a [`Fetched`], not a `Vec`:
//!   "unknown", "the fetch failed" and "there is genuinely nothing" are three
//!   different facts, and only the third may be rendered as an empty state.
//! * `entries[conv]` is merged, never replaced, when a page or a snapshot
//!   arrives (see [`AppState::merge_older_entries`]); [`History`] records how far
//!   back the owner has paged so the transcript's top is never a dead button.
//! * a toast is **time-bounded**: `toast_at` records when it was set, and
//!   [`AppState::expire_toast`] drops it once [`TOAST_TTL`] has passed. A
//!   status line that outlives its cause is a lie about the present.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use swarmbot_client::{BackendProbe, BackendView, Health, PairingCode};
use swarmbot_contracts::{
    ApprovalRule, Bot, BotId, BotStatus, Conversation, ConversationId, ConversationKind, Entry,
    EntryId, LiveCursor, NodeId, NodeInfo, RequestId, Routine, RoutineId, RoutineRun,
    RoutineSchedule, RuleId, RuntimeRequest, Settings, Task, TaskId, TaskStatus, TaskStep, Turn,
    TurnId, TurnStatus, UsageReport,
};

/// What the UI knows about one collection the brain reports.
///
/// Three states, and keeping them apart is the whole point (see
/// [`../../../docs/ui.md`] §3): `Unknown` means *nobody (or nothing) has told us
/// yet*, `Failed` means the fetch broke, and only `Ready` with an empty vector
/// may be rendered as "there is nothing here". A screen that prints "no nodes
/// paired yet" while its fetch is in flight — or after it failed — is telling the
/// owner something it does not know, and the owner then goes looking for a
/// problem that may not exist.
///
/// The Usage screen established the pattern with `Option<Result<..>>`; this enum
/// is the same three states with names, so a view cannot match on two of them
/// and forget the third.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Fetched<T> {
    /// Never fetched (cold start, the socket is not up, or nothing asked).
    #[default]
    Unknown,
    /// The last fetch failed, in the brain's or the client's own words.
    Failed(String),
    /// An answer. It may legitimately be empty — that is the *only* empty that
    /// may be shown as "nothing here".
    Ready(T),
}

impl<T> Fetched<T> {
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }

    /// The failure reason, if this collection's last fetch failed.
    pub fn failure(&self) -> Option<&str> {
        match self {
            Self::Failed(reason) => Some(reason),
            _ => None,
        }
    }
}

impl<T> Fetched<Vec<T>> {
    /// The rows, or an empty slice when there is no answer.
    ///
    /// For **questions about content**, not for deciding what to print: an empty
    /// slice here means "nothing known", which is not "nothing exists". A view
    /// that shows an empty state must match on [`Fetched`] itself.
    pub fn as_slice(&self) -> &[T] {
        match self {
            Self::Ready(rows) => rows,
            Self::Unknown | Self::Failed(_) => &[],
        }
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.as_slice().iter()
    }

    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }

    pub fn first(&self) -> Option<&T> {
        self.as_slice().first()
    }

    pub fn find(&self, matches: impl Fn(&T) -> bool) -> Option<&T> {
        self.as_slice().iter().find(|row| matches(row))
    }

    pub fn find_mut(&mut self, matches: impl Fn(&T) -> bool) -> Option<&mut T> {
        match self {
            Self::Ready(rows) => rows.iter_mut().find(|row| matches(row)),
            Self::Unknown | Self::Failed(_) => None,
        }
    }
}

/// Iterating a collection iterates what is known. `for bot in &state.bots` reads
/// the same as it did when the field was a `Vec`, which keeps render code honest
/// by default: an unknown list renders no rows. What it must **not** do is
/// decide an empty-state sentence — that is a match on [`Fetched`] itself, and
/// the views that print one do exactly that.
impl<'a, T> IntoIterator for &'a Fetched<Vec<T>> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

/// Fold one row into a collection, keyed by whatever `same` compares.
///
/// A live row is *knowledge about the list*, so it promotes `Unknown` to a
/// one-row `Ready` — an event naming a bot proves the roster exists. It
/// deliberately does **not** clear `Failed`: one live row does not make a failed
/// list fetch complete, and a screen that says "could not load the list" must
/// keep saying so (with its retry) rather than be quietly replaced by a one-row
/// list that merely looks whole.
fn upsert_row<T>(slot: &mut Fetched<Vec<T>>, row: T, same: impl Fn(&T, &T) -> bool) {
    match slot {
        Fetched::Ready(rows) => match rows.iter_mut().find(|existing| same(existing, &row)) {
            Some(found) => *found = row,
            None => rows.push(row),
        },
        Fetched::Unknown => *slot = Fetched::Ready(vec![row]),
        Fetched::Failed(_) => {}
    }
}

/// Drop rows from a collection that has an answer; a collection with no answer
/// stays without one.
fn retain_rows<T>(slot: &mut Fetched<Vec<T>>, keep: impl Fn(&T) -> bool) {
    if let Fetched::Ready(rows) = slot {
        rows.retain(|row| keep(row));
    }
}

impl History {
    /// Whether there is still an older page worth asking for.
    ///
    /// The view uses this to decide between a live button and the "beginning of
    /// the conversation" line: a control that has to be clicked to discover it
    /// does nothing is exactly the failure this bar exists to prevent. A
    /// conversation with no entries has no top of the list to hang it on.
    pub fn can_load(&self, entries: &[Entry]) -> bool {
        !self.exhausted && !self.loading && !entries.is_empty()
    }
}

/// The paging state of a conversation nobody has paged. A `static` (not a
/// `const`) so [`AppState::history`] can hand out a reference to it: the value
/// holds a `String` and so cannot be const-promoted from a borrow of a const.
/// It is never written to — every mutation goes through `&mut self`.
static EMPTY_HISTORY: History = History {
    loading: false,
    failed: None,
    exhausted: false,
};

/// How far back one conversation's transcript has been paged.
///
/// A transcript arrives as the newest [`swarmbot_contracts::SNAPSHOT_ENTRIES`]
/// entries, so a long conversation is missing its beginning. "Load earlier" asks
/// [`swarmbot_client::BrainClient::entries_before`] for the page before the
/// oldest row held. This struct is what the button needs to be honest about:
/// whether a page is in flight, whether the last one failed, and whether a short
/// page already proved there is nothing older — so the button can turn into
/// "beginning of the conversation" instead of staying a control that does
/// nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    /// A page request is in flight; a second click must not send another.
    pub loading: bool,
    /// The last page request failed, in the client's own words.
    pub failed: Option<String>,
    /// A page came back shorter than asked for, or the transcript is already at
    /// seq 1: there is nothing older left to fetch.
    pub exhausted: bool,
}

/// Which pane the main area shows. The delegation graph is an overlay panel
/// toggled independently of this.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    #[default]
    Conversation,
    Nodes,
    Routines,
    /// The saved approval rules (`GET /api/rules`) — the answers "Always allow"
    /// writes, which used to be visible nowhere in the app and deletable nowhere
    /// at all.
    Rules,
    /// What the backends reported the team spent (`GET /api/usage`). Read-only:
    /// nothing here can change the spend, so nothing here writes to the brain.
    Usage,
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

/// How long a toast stays on screen once it is set.
///
/// Long enough to read a failed send or a refused settings save, short enough
/// that a stale success line cannot be mistaken for the current state. The
/// ticker in `worker.rs` is what actually clears it; this constant is the one
/// place the window is defined, so the reducer, the view and their tests agree.
pub const TOAST_TTL: Duration = Duration::from_secs(6);

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
    //
    // Four of these are [`Fetched`], not plain vectors, because they are the
    // collections whose emptiness is *news*: "no nodes paired yet" is an
    // instruction to go and pair one, and it must not be printed while the fetch
    // is in flight, after it failed, or before anyone asked. `tasks`,
    // `requests` and `turns` stay plain vectors: they ride the snapshot, which
    // is itself the answer, and no screen invites the owner to fix their
    // absence.
    pub bots: Fetched<Vec<Bot>>,
    pub conversations: Fetched<Vec<Conversation>>,
    pub nodes: Fetched<Vec<NodeInfo>>,
    /// The saved approval rules. Fetched on demand (the Rules screen and the
    /// palette), upserted from create/delete answers.
    pub approval_rules: Fetched<Vec<ApprovalRule>>,
    pub tasks: Vec<Task>,
    pub requests: Vec<RuntimeRequest>,
    pub turns: Vec<Turn>,
    /// Entries per conversation, sorted by `seq`, unique by id.
    pub entries: HashMap<ConversationId, Vec<Entry>>,
    /// How far back the owner has paged each transcript, so "Load earlier" can
    /// say where it is and can refuse to ask again once a short page proved
    /// there is nothing older.
    pub history: HashMap<ConversationId, History>,
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
    /// Backend ids whose **Connect** handshake is in flight. A set, not a flag,
    /// because two rows can be connected at once and each row's button must
    /// report only its own progress; a click on a backend already in this set
    /// is ignored (one handshake per backend, never a pile of agent processes).
    pub connecting: HashSet<String>,
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
    /// Scheduled triggers, fetched on demand by the Routines screen.
    pub routines: Fetched<Vec<Routine>>,
    /// Absent key = not fetched (the three-state rule, keyed by routine).
    pub routine_runs: HashMap<RoutineId, Vec<RoutineRun>>,
    /// The usage screen's last answer. Three states, kept distinct on purpose:
    /// `None` = never fetched, `Ok` = a report, `Err` = the fetch failed and the
    /// screen must say so (with a Retry) rather than render zeroes.
    pub usage: Option<Result<UsageReport, String>>,
    /// The window the usage screen asks for, in hours. The buttons set it; the
    /// `since` instant sent to the brain is derived from it at click time
    /// (`view::usage::since_for`), so a long-lived window does not quietly
    /// become a different one while the app sits open.
    pub usage_hours: i64,
    pub selected_routine: Option<RoutineId>,
    pub routine_creator_open: bool,
    pub routine_draft_bot: Option<BotId>,
    pub routine_draft_schedule: RoutineSchedule,
    /// Popup menu opened by the `+` button in the left sidebar.
    pub plus_menu_open: bool,
    /// The bot whose skills are being edited on the Settings screen.
    ///
    /// Only the *selection* lives here; the text being typed lives in the one
    /// [`crate::widgets::text_input::TextInput`] entity the Settings screen
    /// shares, because that is where every other draft in this app lives (a text
    /// buffer is view state, not domain data — see this module's header). Keeping
    /// a second copy here would be a string that nothing renders and something
    /// has to remember to clear.
    pub skills_editor: Option<BotId>,
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
    /// When `toast` was set, so the ticker can expire it ([`TOAST_TTL`]).
    ///
    /// `None` while no toast is showing. A view that writes `toast` directly
    /// (the composer's optimistic refusal) leaves this `None`; the first tick
    /// stamps it, so nothing can live forever (see [`AppState::expire_toast`]).
    pub toast_at: Option<Instant>,

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
            bots: Fetched::Unknown,
            conversations: Fetched::Unknown,
            nodes: Fetched::Unknown,
            approval_rules: Fetched::Unknown,
            tasks: Vec::new(),
            requests: Vec::new(),
            turns: Vec::new(),
            entries: HashMap::new(),
            history: HashMap::new(),
            unread: HashMap::new(),
            steps: HashMap::new(),
            settings: None,
            settings_draft: None,
            backends: Vec::new(),
            probes: None,
            connecting: HashSet::new(),
            bot_menu_open: false,
            menu_bot: None,
            menu_backend: None,
            menu_model: None,
            models: HashMap::new(),
            pairing: None,
            routines: Fetched::Unknown,
            routine_runs: HashMap::new(),
            usage: None,
            // The brain's own default (`DEFAULT_WINDOW_HOURS`), so the first
            // fetch asks the same question the CLI's bare `swarmbot usage` does.
            usage_hours: swarmbot_contracts::turn::DEFAULT_WINDOW_HOURS,
            selected_routine: None,
            routine_creator_open: false,
            skills_editor: None,
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
            toast_at: None,
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
        self.bots.find(|b| &b.id == id)
    }

    pub fn conversation(&self, id: &ConversationId) -> Option<&Conversation> {
        self.conversations.find(|c| &c.id == id)
    }

    pub fn node(&self, id: &NodeId) -> Option<&NodeInfo> {
        self.nodes.find(|n| &n.id == id)
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
        upsert_row(&mut self.bots, bot, |a, b| a.id == b.id);
    }

    /// Record the whole roster as an answer. Only a snapshot or a fetch does
    /// this; an `Unknown` or `Failed` list is never replaced by guesswork.
    pub fn set_bots(&mut self, bots: Vec<Bot>) {
        self.bots = Fetched::Ready(bots);
    }

    pub fn set_conversations(&mut self, conversations: Vec<Conversation>) {
        self.conversations = Fetched::Ready(conversations);
    }

    pub fn set_nodes(&mut self, nodes: Vec<NodeInfo>) {
        self.nodes = Fetched::Ready(nodes);
    }

    pub fn set_routines(&mut self, routines: Vec<Routine>) {
        let mut routines = routines;
        routines.sort_by(|a, b| a.name.cmp(&b.name));
        self.routines = Fetched::Ready(routines);
    }

    pub fn set_rules(&mut self, rules: Vec<ApprovalRule>) {
        self.approval_rules = Fetched::Ready(rules);
    }

    pub fn remove_rule(&mut self, id: &RuleId) {
        retain_rows(&mut self.approval_rules, |rule| &rule.id != id);
    }

    // --- backend connection state ----------------------------------------

    /// Mark one backend's Connect as in flight. Returns `false` when it already
    /// was — the caller then does **not** send the command, so a double click
    /// cannot spawn two agents.
    pub fn begin_connect(&mut self, backend: &str) -> bool {
        self.connecting.insert(backend.to_string())
    }

    pub fn end_connect(&mut self, backend: &str) {
        self.connecting.remove(backend);
    }

    pub fn is_connecting(&self, backend: &str) -> bool {
        self.connecting.contains(backend)
    }

    /// The last report for one backend, if it has been checked at all.
    pub fn probe_for(&self, backend: &str) -> Option<&BackendProbe> {
        self.probes
            .as_ref()
            .and_then(|probes| probes.iter().find(|probe| probe.backend == backend))
    }

    /// Replace one backend's row with a fresher report, keeping the other rows.
    ///
    /// An upsert by backend id, like every other collection here: a Connect
    /// answer for one backend must never blank what is known about the others.
    /// When nothing has been probed yet this creates the list with the single
    /// row, because "one backend checked" is a real state.
    pub fn upsert_probe(&mut self, probe: BackendProbe) {
        let probes = self.probes.get_or_insert_with(Vec::new);
        match probes
            .iter_mut()
            .find(|existing| existing.backend == probe.backend)
        {
            Some(slot) => *slot = probe,
            None => probes.push(probe),
        }
    }

    /// Update only a bot's live status (the roster-only `BotStatus` event).
    pub fn set_bot_status(&mut self, id: &BotId, status: swarmbot_contracts::BotStatus) {
        if let Some(bot) = self.bots.find_mut(|b| &b.id == id) {
            bot.status = status;
        }
    }

    pub fn remove_bot(&mut self, id: &BotId) {
        if let Some(bot) = self.bots.find(|b| &b.id == id) {
            let dm = bot.direct_conversation_id.clone();
            self.remove_conversation(&dm);
        }
        retain_rows(&mut self.bots, |b| &b.id != id);
    }

    pub fn upsert_conversation(&mut self, conversation: Conversation) {
        upsert_row(&mut self.conversations, conversation, |a, b| a.id == b.id);
    }

    pub fn remove_conversation(&mut self, id: &ConversationId) {
        retain_rows(&mut self.conversations, |c| &c.id != id);
        self.entries.remove(id);
        self.history.remove(id);
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
        upsert_row(&mut self.nodes, node, |a, b| a.id == b.id);
    }

    pub fn set_node_presence(&mut self, id: &NodeId, online: bool) {
        if let Some(node) = self.nodes.find_mut(|n| &n.id == id) {
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
        upsert_row(&mut self.routines, routine, |a, b| a.id == b.id);
        if let Fetched::Ready(routines) = &mut self.routines {
            routines.sort_by(|a, b| a.name.cmp(&b.name));
        }
    }

    pub fn remove_routine(&mut self, id: &RoutineId) {
        retain_rows(&mut self.routines, |r| &r.id != id);
        self.routine_runs.remove(id);
        if self.selected_routine.as_ref() == Some(id) {
            self.selected_routine = None;
        }
    }

    /// Replace a conversation's entries wholesale (used when a snapshot or a
    /// per-conversation `tail` arrives).
    ///
    /// Paging state is kept, not reset: a snapshot brings the newest window, and
    /// the response to that is "the oldest row moved forward", which can be a
    /// reason to re-enable "Load earlier" — but never a reason to *forget* that
    /// the owner already paged back to the beginning (a refresh that resurrected
    /// the button would produce a page it already has).
    pub fn set_entries(&mut self, id: ConversationId, mut entries: Vec<Entry>) {
        entries.retain(|e| !self.dismissed.contains(&e.id));
        entries.sort_by_key(|e| e.seq);
        entries.dedup_by(|a, b| a.id == b.id);
        self.entries.insert(id, entries);
    }

    // --- transcript paging ("Load earlier") --------------------------------

    /// The oldest `seq` held for a conversation, which is what the next page is
    /// asked to come before.
    pub fn oldest_seq(&self, id: &ConversationId) -> Option<u64> {
        self.entries(id).first().map(|entry| entry.seq)
    }

    /// Claim the right to send one page request for `id`.
    ///
    /// Returns `false` when a request is already in flight, when the transcript
    /// is already at its beginning, or when there is nothing to page — that is
    /// how a double click stops being two requests and how a button whose work
    /// is done stops being clickable.
    pub fn begin_older_page(&mut self, id: &ConversationId) -> bool {
        let entries_empty = self.entries(id).is_empty();
        let oldest = self.oldest_seq(id);
        let history = self.history.entry(id.clone()).or_default();
        if history.loading || history.exhausted || entries_empty {
            return false;
        }
        // A transcript that already starts at seq 1 has no earlier page, and
        // asking for one would be a request whose answer can only be "nothing".
        if oldest.is_some_and(|seq| seq <= 1) {
            history.exhausted = true;
            return false;
        }
        history.loading = true;
        history.failed = None;
        true
    }

    /// Fold one page of older entries into a transcript.
    ///
    /// Merge, not replace: the transcript already holds the newer rows, and a
    /// page that arrived while a live event was streaming must not drop the
    /// stream. Rows are keyed by `EntryId` (a page can overlap a row that was
    /// appended live), and the list stays sorted by `seq`.
    ///
    /// `requested` is the limit that was sent, and it is what makes the honest
    /// "there is nothing older" conclusion possible: a *short* page proves the
    /// beginning was reached, while a full page only proves that more might
    /// exist. Silence is never read as "the end".
    pub fn merge_older_entries(
        &mut self,
        id: &ConversationId,
        older: Vec<Entry>,
        requested: u32,
    ) -> usize {
        let returned = older.len();
        let list = self.entries.entry(id.clone()).or_default();
        let mut added = 0;
        for entry in older {
            if self.dismissed.contains(&entry.id) {
                continue;
            }
            match list.iter_mut().find(|existing| existing.id == entry.id) {
                Some(slot) => *slot = entry,
                None => {
                    list.push(entry);
                    added += 1;
                }
            }
        }
        list.sort_by_key(|e| e.seq);
        list.dedup_by(|a, b| a.id == b.id);

        // A transcript whose oldest row is seq 1 has nothing before it, whatever
        // the page's size: the page reached the actual first entry.
        let starts_at_beginning = list.first().is_some_and(|entry| entry.seq <= 1);
        let history = self.history.entry(id.clone()).or_default();
        history.loading = false;
        history.failed = None;
        // Three independent proofs that the older side is empty; any one is
        // enough, and the first is the rule the client documents:
        //
        // * a transcript that now starts at seq 1 has literally nothing older;
        // * an empty page is the end said plainly;
        // * a page shorter than `limit` is the end too — but only because the page
        //   size is one the server never clamps (50, far below the 1000 cap), so a
        //   short page cannot be a truncated full one. A limit above the cap would
        //   make this test wrong, which is the reason `HISTORY_PAGE` is small;
        // * a page that added no row the transcript did not already have (the
        //   request overlapped what was loaded) proves the same thing, and is what
        //   a reconnect can produce.
        //
        // A *full* page proves nothing — it may have more behind it — so it leaves
        // the button available.
        history.exhausted = starts_at_beginning
            || returned == 0
            || (returned as u64) < u64::from(requested)
            || added == 0;
        added
    }

    /// Record that a page request failed: the reason stays visible, and the
    /// button becomes a retry instead of a control that does nothing.
    pub fn fail_older_page(&mut self, id: &ConversationId, reason: String) {
        let history = self.history.entry(id.clone()).or_default();
        history.loading = false;
        history.failed = Some(reason);
    }

    /// The paging state for one conversation.
    pub fn history(&self, id: &ConversationId) -> &History {
        self.history.get(id).unwrap_or(&EMPTY_HISTORY)
    }

    // --- the transient status line ----------------------------------------

    /// Set the one-line status message and stamp when it was set.
    ///
    /// The stamp is what lets the message expire: a "settings saved" that is
    /// still on screen an hour later is a claim about the present that is no
    /// longer true. Setting a new toast **restarts** the window instead of
    /// inheriting the old one's clock, so a burst of failures stays readable.
    pub fn set_toast(&mut self, message: impl Into<String>, now: Instant) {
        self.toast = Some(message.into());
        self.toast_at = Some(now);
    }

    /// Clear the toast once it has outlived [`TOAST_TTL`] at `now`; report
    /// whether the state changed so the caller knows a redraw is owed.
    ///
    /// A toast with no stamp was written straight into the field by a view (the
    /// composer's "offline — the message was not sent"); the first tick that
    /// sees it stamps it rather than treating "no timestamp" as "never expires",
    /// which is the bug this exists to prevent.
    pub fn expire_toast(&mut self, now: Instant) -> bool {
        match (self.toast.is_some(), self.toast_at) {
            (false, _) => {
                self.toast_at = None;
                false
            }
            (true, None) => {
                self.toast_at = Some(now);
                false
            }
            (true, Some(at)) if now.saturating_duration_since(at) >= TOAST_TTL => {
                self.toast = None;
                self.toast_at = None;
                true
            }
            (true, Some(_)) => false,
        }
    }

    /// The toast text if it is still inside its window at `now`.
    ///
    /// An unstamped toast is visible: it was just written by a view and the
    /// next tick stamps it (see [`AppState::expire_toast`]). An expired one is
    /// hidden even before the ticker clears the field, so the view never shows
    /// a message the state has already outlived.
    pub fn visible_toast(&self, now: Instant) -> Option<&str> {
        let text = self.toast.as_deref()?;
        match self.toast_at {
            Some(at) if now.saturating_duration_since(at) >= TOAST_TTL => None,
            _ => Some(text),
        }
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

    #[test]
    fn a_toast_set_through_the_helper_is_stamped_and_expires_inside_its_window() {
        let mut state = AppState::new("http://brain.test");
        let set_at = Instant::now();
        state.set_toast("send failed: transport", set_at);
        assert_eq!(state.toast_at, Some(set_at));
        assert_eq!(state.visible_toast(set_at), Some("send failed: transport"));

        // Still inside the window: visible, and the ticker must not clear it.
        let almost = set_at + TOAST_TTL - Duration::from_millis(1);
        assert!(!state.expire_toast(almost));
        assert_eq!(state.visible_toast(almost), Some("send failed: transport"));

        // The window is over: the tick clears it and says so, so the caller
        // knows to redraw the bar away.
        let after = set_at + TOAST_TTL;
        assert!(state.expire_toast(after), "the tick that clears it says so");
        assert_eq!(state.toast, None);
        assert_eq!(state.toast_at, None);
        assert_eq!(state.visible_toast(after), None);
    }

    #[test]
    fn a_toast_written_without_a_stamp_is_stamped_on_the_first_tick_and_then_expires() {
        // A view may write the field directly (the composer's optimistic
        // refusal). "No timestamp" must not mean "never expires".
        let mut state = AppState::new("http://brain.test");
        state.toast = Some("offline — the message was not sent".to_string());
        let first_tick = Instant::now();
        assert!(
            state.visible_toast(first_tick).is_some(),
            "it shows immediately, before the ticker has seen it"
        );
        assert!(
            !state.expire_toast(first_tick),
            "the first tick only stamps it"
        );
        assert_eq!(state.toast_at, Some(first_tick));
        assert_eq!(
            state.visible_toast(first_tick + Duration::from_secs(1)),
            Some("offline — the message was not sent")
        );
        assert!(state.expire_toast(first_tick + TOAST_TTL));
        assert_eq!(state.toast, None);
    }

    #[test]
    fn a_second_toast_restarts_the_window_instead_of_inheriting_the_first() {
        let mut state = AppState::new("http://brain.test");
        let first = Instant::now();
        state.set_toast("stopped", first);
        // Two seconds later the owner hits something else; the new toast gets
        // its own full window, not the first one's remainder.
        let second = first + Duration::from_secs(2);
        state.set_toast("nothing was running", second);
        assert_eq!(state.toast.as_deref(), Some("nothing was running"));
        assert_eq!(state.toast_at, Some(second));
        // The first toast's deadline has passed; the second is still up.
        assert!(!state.expire_toast(first + TOAST_TTL));
        assert_eq!(state.toast.as_deref(), Some("nothing was running"));
        assert!(state.expire_toast(second + TOAST_TTL));
        assert_eq!(state.toast, None);
    }
}
