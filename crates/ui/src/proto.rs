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
    Bot, BotId, Conversation, ConversationId, Decision, EntryId, LiveEvent, NodeId, NodeInfo,
    RequestId, Routine, RoutineId, RoutineRun, RuntimeRequest, Settings, Task, TaskId,
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
    Shutdown(Result<(), ClientError>),
}
