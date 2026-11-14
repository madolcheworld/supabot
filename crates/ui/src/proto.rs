//! The worker protocol: the plain values that cross the thread boundary.
//!
//! The main (GPUI) thread never does I/O and the worker threads never hold a
//! GPUI handle ([`gpui::AsyncApp`] is `!Send`, so it *cannot* cross into a
//! `std::thread`). They meet only through these `Send` types over two
//! [`std::sync::mpsc`] channels: [`Command`] main→worker, [`WorkerMsg`]
//! worker→main. This is the seam D7.2 demands — one executor per thread, no
//! tokio in the UI process.

use serde_json::Value;
use swarmbot_client::{
    BackendProbe, BackendView, ClientError, Health, NewBotRequest, NewRoutineRequest, PairingCode,
    SentMessage, SessionAction, TaskDetail,
};
use swarmbot_contracts::{
    ApprovalRule, Bot, BotId, Conversation, ConversationId, Decision, Entry, EntryId, LiveEvent,
    NodeId, NodeInfo, RequestId, Routine, RoutineId, RoutineRun, RuleId, RuntimeRequest, Settings,
    Task, TaskId, UsageReport,
};

/// An owner credential in transit: the value behind `--token`,
/// `SWARMBOT_TOKEN`, `<data-dir>/owner-token`, or a token typed into the login
/// prompt.
///
/// `Debug` is hand-written to redact the value, because these tokens ride
/// inside [`Command`] — a type that gets printed in panic messages and test
/// assertions — and a derived `Debug` would put a live credential wherever
/// nobody reviews it. Same pattern as `swarmbot-client`'s `Http` and the
/// brain's `OwnerAuth`: the *fact* of a token may be logged, never the value.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretToken(String);

impl SecretToken {
    /// Wrap a raw token value.
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The raw value, for the single moment it is put on the wire
    /// (`Authorization: Bearer`, `POST /api/session`) — never for display.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretToken {
    /// Manual, because the derived one would print the credential.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretToken")
            .field("value", &"[redacted]")
            .finish()
    }
}

impl std::str::FromStr for SecretToken {
    type Err = std::convert::Infallible;

    /// Any string is a candidate token; whether it authenticates is the
    /// brain's answer, not the parser's. Needed so clap can read `--token`
    /// straight into the redacting type.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_string()))
    }
}

/// A request from the UI to the worker's RPC thread. Each maps to exactly one
/// blocking [`swarmbot_client::BrainClient`] call, answered by a
/// [`CommandResult`].
#[derive(Debug)]
pub enum Command {
    /// Exchange a one-time token for a session (and remember it for `/ws/live`).
    Login(SecretToken),
    /// Post the owner's message.
    Send {
        conversation: ConversationId,
        text: String,
        addressed: Vec<BotId>,
        reply_to: Option<EntryId>,
    },
    /// Stop the running turn of a conversation.
    Interrupt(ConversationId),
    /// Answer a runtime request (a card).
    Respond {
        id: RequestId,
        decision: Option<Decision>,
        answer: Option<Value>,
    },
    CancelTask(TaskId),
    TaskDetail(TaskId),
    CreateGroup {
        title: String,
        members: Vec<BotId>,
        lead: Option<BotId>,
    },
    UpdateGroup {
        id: ConversationId,
        title: Option<String>,
        members: Option<Vec<BotId>>,
        lead: Option<BotId>,
    },
    CreateBot(NewBotRequest),
    DeleteBot(BotId),
    DeleteGroup(ConversationId),
    BotSession {
        id: String,
        action: SessionAction,
    },
    /// Mint a node pairing code.
    Pairing,
    RevokeNode(NodeId),
    FetchSettings,
    UpdateSettings(Value),
    FetchBackends,
    ProbeBackends,
    /// The Settings screen's **Connect**: the brain's handshake for one backend,
    /// plus `method` when the owner picked one of the logins that backend
    /// advertised. Costs no model call; may spawn the agent for a moment.
    ConnectBackend {
        backend: String,
        method: Option<String>,
    },
    /// Switch the configured backend (provider) one bot runs on. The brain
    /// validates the id; the next turn picks it up.
    SetBotBackend {
        bot: BotId,
        backend: String,
    },
    /// Set (`Some`) or clear (`None`) one bot's model override; clearing lets
    /// the backend's configured model apply.
    SetBotModel {
        bot: BotId,
        model: Option<String>,
    },
    /// Replace one bot's skill list (`PATCH /api/bots/{id}` with `skills`). An
    /// empty list removes every skill, which is a real answer and not a mistake
    /// to guard against.
    SetBotSkills {
        bot: BotId,
        skills: Vec<String>,
    },
    /// The model list one backend advertises, for the provider menu's right
    /// column. Cached in state; an empty list is an honest "no menu".
    FetchModels(String),
    FetchNodes,
    FetchRoutines,
    CreateRoutine(NewRoutineRequest),
    PauseRoutine(RoutineId),
    ResumeRoutine(RoutineId),
    DeleteRoutine(RoutineId),
    FetchRoutineRuns(RoutineId),
    /// `GET /api/rules` — the saved approval rules, i.e. the answers "Always
    /// allow" wrote. Read via a screen rather than only through SQLite.
    FetchRules,
    /// `DELETE /api/rules/{id}` — unsave one rule.
    DeleteRule(RuleId),
    /// `GET /api/usage` — what the backends reported the team spent.
    ///
    /// `since` is an RFC 3339 instant computed by the caller from its own
    /// clock; `None` means the brain's default window. It is sent as text
    /// rather than as a duration because the route takes an instant, and
    /// turning "7 days" into one is a decision the window buttons make
    /// explicitly (see `view::usage::since_for`), not something to hide in the
    /// worker.
    FetchUsage {
        since: Option<String>,
    },
    /// One page of an older transcript: the entries with `seq < before_seq`
    /// (the bound is **exclusive**, so the caller sends the smallest `seq` it
    /// already shows and cannot get that row back), at most `limit`.
    ///
    /// `limit` is carried through to the answer as well: a page shorter than
    /// what was asked proves the transcript has no older entries, and reading
    /// that off the sent limit is the only way to tell "the beginning" from "a
    /// full page that may have more behind it".
    EntriesBefore {
        conversation: ConversationId,
        before_seq: u64,
        limit: u32,
    },
    Shutdown,
    /// Ask the worker threads to exit.
    Stop,
}

/// A message from a worker thread to the main thread.
#[derive(Debug)]
pub enum WorkerMsg {
    /// Live-socket lifecycle, from the live thread.
    Conn(ConnEvent),
    /// One frame off `/ws/live`, from the live thread.
    Live(LiveEvent),
    /// The outcome of a [`Command`], from the RPC thread.
    Result(CommandResult),
}

/// Live-socket connection lifecycle. The reducer turns these into the header's
/// honest connection label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnEvent {
    /// Dialling `/ws/live`.
    Connecting,
    /// The socket opened; a snapshot/synchronized will follow.
    Established,
    /// The socket dropped; retrying after `delay` (attempt is 1-based).
    Reconnecting { attempt: u32 },
    /// Giving up, or a hard failure.
    Offline { reason: String },
    /// The brain answered 401: a token is required.
    NeedsLogin { reason: String },
}

/// The typed outcome of one [`Command`]. Errors are values
/// ([`swarmbot_client::ClientError`]); the reducer renders them, it never
/// panics on them.
#[derive(Debug)]
pub enum CommandResult {
    Login(Result<(), ClientError>),
    Health(Result<Health, ClientError>),
    Send(Result<SentMessage, ClientError>),
    Interrupt(Result<bool, ClientError>),
    Respond(Result<RuntimeRequest, ClientError>),
    CancelTask(Result<Task, ClientError>),
    TaskDetail(Result<TaskDetail, ClientError>),
    CreateGroup(Result<Conversation, ClientError>),
    GroupUpdated(Result<Conversation, ClientError>),
    CreateBot(Result<Bot, ClientError>),
    DeleteBot {
        bot: BotId,
        result: Result<(), ClientError>,
    },
    DeleteGroup {
        conversation: ConversationId,
        result: Result<(), ClientError>,
    },
    BotSession(Result<usize, ClientError>),
    Pairing(Result<PairingCode, ClientError>),
    RevokeNode(Result<(), ClientError>),
    Settings(Result<Settings, ClientError>),
    UpdateSettings(Result<Settings, ClientError>),
    Backends(Result<Vec<BackendView>, ClientError>),
    Probes(Result<Vec<BackendProbe>, ClientError>),
    /// One backend's Connect answer, in the same row shape as a probe — plus
    /// the model list the worker fetched right after a *successful* handshake.
    ///
    /// The follow-up fetch is folded into this result on purpose: the useful
    /// thing about connecting is that the provider's model menu fills in, and
    /// a view cannot send commands from `render`. Doing it here keeps the
    /// reducer a pure function and the two calls in the order they matter.
    ConnectBackend {
        backend: String,
        result: Result<BackendProbe, ClientError>,
        models: Option<Result<swarmbot_client::BackendModels, String>>,
    },
    /// A bot field was patched; the new state arrives as a live `BotUpsert`,
    /// so this only carries success or the refusal to show.
    BotUpdated(Result<(), ClientError>),
    Models(String, Result<swarmbot_client::BackendModels, ClientError>),
    Nodes(Result<Vec<NodeInfo>, ClientError>),
    Routines(Result<Vec<Routine>, ClientError>),
    RoutineCreated(Result<Routine, ClientError>),
    RoutineUpdated(Result<Routine, ClientError>),
    RoutineDeleted(RoutineId, Result<(), ClientError>),
    RoutineRuns(RoutineId, Result<Vec<RoutineRun>, ClientError>),
    /// The saved approval rules, or why they could not be read.
    Rules(Result<Vec<ApprovalRule>, ClientError>),
    /// A page of older entries. The conversation and the limit travel with the
    /// result so the reducer can merge into the right transcript and decide
    /// whether the beginning was reached.
    EntriesBefore {
        conversation: ConversationId,
        limit: u32,
        result: Result<Vec<Entry>, ClientError>,
    },
    RuleDeleted(RuleId, Result<(), ClientError>),
    /// A usage report, or the reason there is none. `Err` is kept as a failure
    /// and never folded into an empty report: "the fetch broke" and "nothing
    /// ran in this window" are different truths, and the screen says so.
    Usage(Result<UsageReport, ClientError>),
    Shutdown(Result<(), ClientError>),
}
