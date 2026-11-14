//! [`BrainClient`]: one plainly-named method per brain endpoint.
//!
//! The wire types *are* the contract types ([`Bot`], [`Entry`], [`Settings`],
//! …): this module adds no parallel DTOs, only the few composite envelopes an
//! endpoint returns ([`Health`], [`SentMessage`], [`TaskDetail`], …).
//!
//! Identifier conventions (they mirror what the server accepts):
//!
//! * **bots** take `&str` — the brain resolves an id *or* an `@handle`;
//! * everything else takes the typed id (`&ConversationId`, `&TaskId`, …),
//!   so a mix-up is a compile error, not a 404.
//!
//! Blocking on purpose: the GPUI app calls these from a background thread and
//! the CLI from `main`; neither wants a tokio runtime in its process.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use swarmbot_contracts::turn::UsageReport;
use swarmbot_contracts::{
    ApprovalRule, BackendKind, Bot, BotId, Conversation, ConversationId, Decision, Entry, EntryId,
    NodeId, NodeInfo, RequestId, Routine, RoutineId, RoutineRun, RoutineSchedule, RuleId,
    RuntimeRequest, Settings, Task, TaskId, TaskStep, Toolset,
};

use crate::error::ClientError;
use crate::http::{Http, PROBE_TIMEOUT, SharedToken};

/// The most one `export_transcript` call will read into memory (64 MiB).
///
/// The endpoint itself streams, so the brain's side has no limit; this is the
/// **blocking client's** cost, because it hands back a `String`. A transcript
/// larger than this fails with a transport error instead of coming back
/// truncated — a short file that looks complete is the one outcome an archive
/// tool must never be given.
pub const EXPORT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// `GET /api/health` — also the cheapest "is anything listening?" check.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Health {
    pub ok: bool,
    pub version: String,
    /// The brain's live-log epoch; it changes on restart, which tells clients
    /// to take a fresh snapshot instead of resuming a cursor.
    pub epoch: u64,
    pub uptime_secs: u64,
}

/// `GET /api/metrics` — what the brain is doing right now, counted.
///
/// Mirrors `swarmbot_brain::routes::metrics::Metrics` field for field without
/// depending on that crate (this crate speaks the wire, not the brain). Every
/// number is a count of something the brain's store or runner map already knows:
/// nothing here is an estimate, a rate or a projection, and a field the brain
/// cannot fill is absent rather than invented.
///
/// One relation worth knowing when displaying these: `turns_running` comes from
/// the **store**, while `bots_working` comes from the roster row. They can
/// disagree for the moment between a turn ending and its row being written, and
/// `turns_running` is the one to trust.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metrics {
    /// The brain's version, so a client can say which binary it is talking to.
    pub version: String,
    /// Seconds since the brain's process started (monotonic, not wall clock).
    pub uptime_secs: u64,
    /// The live log's epoch: a different value means the brain restarted, and a
    /// stored cursor must resync instead of resuming.
    pub epoch: u64,

    /// Bots in the roster, disabled ones included.
    pub bots_total: u64,
    pub bots_idle: u64,
    /// Bots mid-turn as the roster row records it.
    pub bots_working: u64,
    /// Bots waiting for their runner.
    pub bots_queued: u64,
    /// Bots that failed `FAILURE_LIMIT` turns in a row.
    pub bots_error: u64,
    /// Bots turned off by the owner; they get no runner.
    pub bots_disabled: u64,
    /// Live runner tasks — smaller than `bots_total` when bots are disabled.
    pub runners: u64,

    /// Turns the store still calls running: the signal that decides idleness.
    pub turns_running: u64,
    /// Deliveries waiting for a bot that is already running, summed over the
    /// roster.
    pub deliveries_queued: u64,
    /// Deliveries waiting for a runner that does not exist yet — the number that
    /// can keep the swarm from ever going idle.
    pub deliveries_parked: u64,
    /// Tasks in the queue, across every bot.
    pub tasks_queued: u64,
    /// Tasks currently running, across every bot.
    pub tasks_running: u64,
    /// Cards (approvals, questions, forms) still waiting for the owner.
    pub requests_pending: u64,

    /// Every roster row, revoked ones included.
    pub nodes_total: u64,
    /// Nodes that are online and not revoked — able to run a tool call now.
    pub nodes_online: u64,
}

/// One credential **name** from `GET /api/secrets`.
///
/// A name, never a value: no endpoint in this client can read a secret back, by
/// design. `used_by` lists the backend ids whose `api_key_env` is this name —
/// what breaks if it is deleted. Empty means "no configured backend asks for it",
/// which is a real answer for a key left behind by a removed backend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretName {
    pub name: String,
    #[serde(default)]
    pub used_by: Vec<String>,
}

/// What a secret write did (`PUT`/`DELETE /api/secrets`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretWrite {
    pub name: String,
    /// `"restart"` or `"hot"`. The brain answers `"restart"` today: the backend
    /// registry resolves each key once, while the swarm is composed, so a changed
    /// value reaches the next process and not the running one. A client should
    /// print `applies`, not assume it.
    pub applies: String,
}

/// What `POST /api/conversations/{id}/messages` answers: the owner's entry as
/// committed, and how it was routed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SentMessage {
    /// The committed owner entry (has its real `id` and `seq`).
    pub entry: Entry,
    /// The bots the message wakes immediately.
    pub targets: Vec<BotId>,
    /// Bots that were busy: the message waits in their queue instead.
    pub queued: Vec<BotId>,
    /// Why it was routed this way (`"lead"`, `"mention"`, …) — display text
    /// for the transcript, so it stays a plain string.
    pub reason: String,
}

/// `GET /api/tasks/{id}` — the task plus its own progress steps.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskDetail {
    pub task: Task,
    pub steps: Vec<TaskStep>,
}

/// The redacted backend view (`BackendSpec::public_view()`): never a key,
/// only the environment variable the key comes from. `options` is the
/// redacted connection detail (an ACP agent's command line, a harness's
/// binary); secret-bearing values inside it arrive already masked as
/// `"[hidden]"` by the brain. `#[serde(default)]` keeps a client built
/// against this shape talking to an older brain that omits the field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackendView {
    pub id: String,
    pub kind: BackendKind,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub options: serde_json::Map<String, serde_json::Value>,
}

/// One model a backend advertises (`GET /api/backends/{id}/models`). Mirrors
/// `swarmbot_llm::ModelOption` without this crate depending on `llm`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelOption {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// The model list of one backend (`GET /api/backends/{id}/models`).
/// `authoritative` says whether picking from it actually reaches the agent;
/// a list read from the agent's own CLI is informational only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackendModels {
    pub models: Vec<ModelOption>,
    pub authoritative: bool,
}

/// One row of `GET /api/backends/probe` (or `POST /api/backends/{id}/connect`):
/// did the brain reach this backend, and what would it take to log in?
///
/// The three auth fields are mirrored from `swarmbot_llm::BackendStatus`
/// without depending on that crate (this crate speaks the wire, not the
/// adapter). Every one of them is `#[serde(default)]`: an older brain that does
/// not send them means "not asked", which is what the defaults say.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackendProbe {
    pub backend: String,
    pub ok: bool,
    pub detail: Option<String>,
    pub model: Option<String>,
    pub version: Option<String>,
    /// Logins the backend itself advertises. Empty = it named none (or the
    /// handshake never got that far).
    #[serde(default)]
    pub auth_methods: Vec<BackendAuthMethod>,
    /// The backend refused because a login is needed. `false` is not "logged
    /// in" — it is "not refused for that reason".
    #[serde(default)]
    pub auth_required: bool,
    /// The command the backend's config names for logging in, when it names
    /// one. Shown and copied, never executed by anything in this crate.
    #[serde(default)]
    pub login_command: Option<String>,
}

/// One login a backend advertises (`authMethods` over ACP, verbatim).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendAuthMethod {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The exact command an operator can run, when the agent or the config
    /// named one. `None` means "nobody named a command" — the UI shows the
    /// description then, and never guesses a command of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// `POST /api/nodes/pairing-codes`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairingCode {
    /// The `ABCD-EFGH` code the node operator types.
    pub code: String,
    /// RFC 3339 expiry, kept as text: this crate has no direct `chrono`
    /// dependency, and the CLI only displays it.
    pub expires_at: String,
    /// Always `null` at creation; the brain fills it when a node redeems it.
    pub node_id: Option<NodeId>,
}

/// The body of `POST /api/bots`. Optional fields are skipped when unset so
/// the brain applies its own defaults instead of seeing explicit nulls.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewBotRequest {
    pub name: String,
    pub label: String,
    pub instructions: String,
    /// Backend id, as configured in the brain.
    pub backend: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Node label or id; `None` = the brain's local node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolsets: Option<Vec<Toolset>>,
    /// Skill names the bot has. Each names a file under `<data>/skills/`
    /// (`<name>/SKILL.md` or `<name>.md`); the brain renders it into the bot's
    /// instructions every turn. `#[serde(default)]` so a client built against an
    /// older brain, or a brain asked by an older client, still round-trips.
    ///
    /// Empty is skipped on the wire, because "no skills" is the brain's own
    /// default and sending an explicit empty list would say nothing new.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
}

impl NewBotRequest {
    pub fn new(
        name: impl Into<String>,
        label: impl Into<String>,
        instructions: impl Into<String>,
        backend: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            instructions: instructions.into(),
            backend: backend.into(),
            model: None,
            color: None,
            node: None,
            toolsets: None,
            skills: Vec::new(),
        }
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn color(mut self, color: impl Into<String>) -> Self {
        self.color = Some(color.into());
        self
    }

    pub fn node(mut self, node: impl Into<String>) -> Self {
        self.node = Some(node.into());
        self
    }

    pub fn toolsets(mut self, toolsets: Vec<Toolset>) -> Self {
        self.toolsets = Some(toolsets);
        self
    }

    pub fn skills(mut self, skills: Vec<String>) -> Self {
        self.skills = skills;
        self
    }
}

/// A `PATCH /api/bots/{id}` body, typed.
///
/// [`BrainClient::update_bot`] takes a raw [`Value`], which is the right shape for
/// a caller that already has a JSON object. This struct is for the ones that do
/// not: the two fields a front end most often forgets (`skills`, `enabled`) are
/// here with their wire names, so "mute this bot" and "give it this skill" are
/// expressible without hand-building JSON and without a typo becoming a silently
/// ignored field (`PATCH /api/bots/{id}` refuses unknown fields with a 400).
///
/// Every field is skipped when `None`, because the brain merges a patch: sending
/// an explicit `null` would not mean "leave it alone" to this API — it would be
/// read as a value.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BotPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Empty string clears the model (back to the backend's default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Empty string clears the colour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// A node id; empty string returns the bot to the local node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolsets: Option<Vec<Toolset>>,
    /// Replaces the bot's skill list (an empty list removes every skill).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// `false` disables the bot: nothing wakes it and its runner is shut down.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl BotPatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    pub fn backend(mut self, backend: impl Into<String>) -> Self {
        self.backend = Some(backend.into());
        self
    }

    pub fn skills(mut self, skills: Vec<String>) -> Self {
        self.skills = Some(skills);
        self
    }

    pub fn toolsets(mut self, toolsets: Vec<Toolset>) -> Self {
        self.toolsets = Some(toolsets);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self
    }

    /// The wire body, for a caller that wants to log it or store it.
    pub fn to_value(&self) -> Result<Value, ClientError> {
        serde_json::to_value(self).map_err(|err| ClientError::Decode {
            reason: format!("a bot patch cannot be serialized: {err}"),
        })
    }
}

/// What `POST /api/bots/{id}/session` can do to a bot's one mind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAction {
    /// Drop the history and start over (read cursors survive).
    New,
    /// Fold the history into a summary.
    Compact,
}

/// Parameters for `POST /api/routines`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewRoutineRequest {
    pub bot: String,
    pub name: String,
    pub prompt: String,
    pub schedule: RoutineSchedule,
}

impl NewRoutineRequest {
    pub fn new(
        bot: impl Into<String>,
        name: impl Into<String>,
        prompt: impl Into<String>,
        schedule: RoutineSchedule,
    ) -> Self {
        Self {
            bot: bot.into(),
            name: name.into(),
            prompt: prompt.into(),
            schedule,
        }
    }
}

/// A blocking client for one brain's REST API.
///
/// Cheap to clone: clones share the connection pool, the session cookie jar
/// and the token, so they are handles to the *same* session.
#[derive(Clone, Debug)]
pub struct BrainClient {
    /// Parsed, normalized origin used to build every endpoint URL.
    base: Url,
    /// The same origin as a display string, without a trailing `/`.
    base_url: String,
    http: Http,
}

/// Small bodies the endpoints exchange; kept private because they are wire
/// details, not API surface.
#[derive(Serialize)]
struct SessionBody<'a> {
    token: &'a str,
}

#[derive(Serialize)]
struct BotSessionBody {
    action: SessionAction,
}

#[derive(Serialize)]
struct CreateGroupBody<'a> {
    title: &'a str,
    members: &'a [BotId],
    #[serde(skip_serializing_if = "Option::is_none")]
    lead: Option<&'a BotId>,
}

#[derive(Serialize)]
struct UpdateGroupBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    members: Option<&'a [BotId]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lead: Option<&'a BotId>,
}

#[derive(Serialize)]
struct SendMessageBody<'a> {
    text: &'a str,
    addressed: &'a [BotId],
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<&'a EntryId>,
}

#[derive(Serialize)]
struct RespondBody {
    decision: Option<Decision>,
    answer: Option<Value>,
}

#[derive(Deserialize)]
struct OkBody {
    ok: bool,
}

#[derive(Deserialize)]
struct FoldedBody {
    folded: usize,
}

#[derive(Deserialize)]
struct StoppedBody {
    stopped: bool,
}

#[derive(Deserialize)]
struct RevokedBody {
    revoked: usize,
}

impl BrainClient {
    /// Validate and normalize the brain's origin: it must parse, must be
    /// `http`/`https`, and any trailing `/` is stripped so `base_url()` and
    /// the endpoint URLs read consistently.
    pub fn new(base_url: &str) -> Result<Self, ClientError> {
        let invalid = |reason: String| ClientError::Transport {
            url: base_url.to_string(),
            reason,
        };
        let mut base =
            Url::parse(base_url).map_err(|err| invalid(format!("not a parseable URL: {err}")))?;
        if base.cannot_be_a_base() || !matches!(base.scheme(), "http" | "https") {
            return Err(invalid(format!(
                "expected an http:// or https:// origin, got `{}`",
                base.scheme()
            )));
        }
        let trimmed = base.path().trim_end_matches('/').to_string();
        base.set_path(if trimmed.is_empty() { "/" } else { &trimmed });
        let base_url = base.to_string();
        let base_url = base_url.trim_end_matches('/').to_string();
        let token: SharedToken = Arc::new(Mutex::new(None));
        Ok(Self {
            base,
            base_url,
            http: Http::new(token),
        })
    }

    /// Authenticate every request with `Authorization: Bearer <token>`. The
    /// same token is used for the WebSocket upgrade, which cannot carry the
    /// session cookie.
    pub fn with_token(self, token: &str) -> Self {
        self.http.set_token(token);
        self
    }

    /// Override the per-request budget (default 30 s). Safe to call at any
    /// time: timeouts are applied per request, never by rebuilding the agent.
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.http.set_timeout(Duration::from_secs(secs));
        self
    }

    /// The normalized origin, e.g. `http://127.0.0.1:8787`.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The same origin with the WebSocket scheme (`ws`/`wss`), e.g.
    /// `ws://127.0.0.1:8787`.
    pub fn ws_url(&self) -> String {
        if let Some(rest) = self.base_url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.base_url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            // Unreachable through `new()`, which only accepts http(s).
            self.base_url.clone()
        }
    }

    /// The bearer token for the WebSocket upgrade, which cannot use the
    /// cookie jar (`with_token` or `login` set it).
    pub(crate) fn live_token(&self) -> Option<String> {
        self.http.token()
    }

    /// Exchange a one-time login token for the brain's session cookie. The
    /// cookie lands in the agent's jar and rides along on every later HTTP
    /// request; the token is remembered because the WebSocket upgrade cannot
    /// read the jar and authenticates with the bearer header instead.
    pub fn login(&self, one_time_token: &str) -> Result<(), ClientError> {
        let url = self.endpoint(&["api", "session"], &[])?;
        let body: OkBody = self.http.post(
            &url,
            &SessionBody {
                token: one_time_token,
            },
        )?;
        if !body.ok {
            return Err(ClientError::Decode {
                reason: "`/api/session` answered `ok: false`".to_string(),
            });
        }
        self.http.set_token(one_time_token);
        Ok(())
    }

    // --- health & lifecycle ------------------------------------------------

    pub fn health(&self) -> Result<Health, ClientError> {
        let url = self.endpoint(&["api", "health"], &[])?;
        self.http.get(&url)
    }

    /// Ask the brain to shut down. Returns once it acknowledged; the process
    /// may take a moment longer to exit.
    pub fn shutdown(&self) -> Result<(), ClientError> {
        let url = self.endpoint(&["api", "shutdown"], &[])?;
        let body: OkBody = self.http.post_empty(&url)?;
        if !body.ok {
            return Err(ClientError::Decode {
                reason: "`/api/shutdown` answered `ok: false`".to_string(),
            });
        }
        Ok(())
    }

    // --- bots ---------------------------------------------------------------

    pub fn bots(&self) -> Result<Vec<Bot>, ClientError> {
        let url = self.endpoint(&["api", "bots"], &[])?;
        self.http.get(&url)
    }

    pub fn create_bot(&self, request: &NewBotRequest) -> Result<Bot, ClientError> {
        let url = self.endpoint(&["api", "bots"], &[])?;
        self.http.post(&url, request)
    }

    /// `id_or_name` is a bot id (`bot_…`) or its `@handle`; the brain
    /// resolves both.
    pub fn bot(&self, id_or_name: &str) -> Result<Bot, ClientError> {
        let url = self.endpoint(&["api", "bots", id_or_name], &[])?;
        self.http.get(&url)
    }

    /// PATCH with only the fields to change (the brain merges them).
    pub fn update_bot(&self, id_or_name: &str, patch: &Value) -> Result<Bot, ClientError> {
        let url = self.endpoint(&["api", "bots", id_or_name], &[])?;
        self.http.patch(&url, patch)
    }

    /// [`Self::update_bot`], with a typed body — see [`BotPatch`].
    pub fn patch_bot(&self, id_or_name: &str, patch: &BotPatch) -> Result<Bot, ClientError> {
        let url = self.endpoint(&["api", "bots", id_or_name], &[])?;
        self.http.patch(&url, patch)
    }

    pub fn delete_bot(&self, id_or_name: &str) -> Result<(), ClientError> {
        let url = self.endpoint(&["api", "bots", id_or_name], &[])?;
        self.http.delete(&url)
    }

    /// Reset (`New`) or fold (`Compact`) the bot's one mind. Returns how many
    /// history messages were folded (0 for `New`).
    pub fn bot_session(
        &self,
        id_or_name: &str,
        action: SessionAction,
    ) -> Result<usize, ClientError> {
        let url = self.endpoint(&["api", "bots", id_or_name, "session"], &[])?;
        let body: FoldedBody = self.http.post(&url, &BotSessionBody { action })?;
        Ok(body.folded)
    }

    // --- conversations -------------------------------------------------------

    pub fn conversations(&self) -> Result<Vec<Conversation>, ClientError> {
        let url = self.endpoint(&["api", "conversations"], &[])?;
        self.http.get(&url)
    }

    /// Create a group of 2–6 bots. `lead` defaults to the first member when
    /// omitted; the brain validates membership.
    pub fn create_group(
        &self,
        title: &str,
        members: &[BotId],
        lead: Option<&BotId>,
    ) -> Result<Conversation, ClientError> {
        let url = self.endpoint(&["api", "conversations"], &[])?;
        self.http.post(
            &url,
            &CreateGroupBody {
                title,
                members,
                lead,
            },
        )
    }

    pub fn conversation(&self, id: &ConversationId) -> Result<Conversation, ClientError> {
        let url = self.endpoint(&["api", "conversations", id.as_str()], &[])?;
        self.http.get(&url)
    }

    /// Update a group's title, member list, and/or lead (`PATCH /api/conversations/{id}`).
    ///
    /// Only `Some(...)` fields are sent on the wire so the brain leaves
    /// omitted fields untouched.
    pub fn update_group(
        &self,
        id: &ConversationId,
        title: Option<&str>,
        members: Option<&[BotId]>,
        lead: Option<&BotId>,
    ) -> Result<Conversation, ClientError> {
        let url = self.endpoint(&["api", "conversations", id.as_str()], &[])?;
        self.http.patch(
            &url,
            &UpdateGroupBody {
                title,
                members,
                lead,
            },
        )
    }

    pub fn rename_conversation(
        &self,
        id: &ConversationId,
        title: &str,
    ) -> Result<Conversation, ClientError> {
        self.update_group(id, Some(title), None, None)
    }

    /// Groups only; a DM cannot be deleted (the brain answers 409).
    pub fn delete_conversation(&self, id: &ConversationId) -> Result<(), ClientError> {
        let url = self.endpoint(&["api", "conversations", id.as_str()], &[])?;
        self.http.delete(&url)
    }

    /// Entries with `seq > after_seq`, ascending, at most `limit`.
    pub fn entries(
        &self,
        id: &ConversationId,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<Entry>, ClientError> {
        let url = self.endpoint(
            &["api", "conversations", id.as_str(), "entries"],
            &[
                ("after_seq", after_seq.to_string()),
                ("limit", limit.to_string()),
            ],
        )?;
        self.http.get(&url)
    }

    /// Entries with `seq < before_seq`, ascending, at most `limit` — paging
    /// **backwards** from a cursor the caller already holds.
    ///
    /// Exclusive on purpose: sending the smallest `seq` you already display is
    /// safe, and the page is ascending so it can be prepended without re-sorting.
    /// `limit` of `0` asks the brain for its default page (100); the brain clamps
    /// anything above 1000. A page shorter than `limit` means the transcript has
    /// no older entries, and an empty page means the same thing said twice.
    pub fn entries_before(
        &self,
        id: &ConversationId,
        before_seq: u64,
        limit: u32,
    ) -> Result<Vec<Entry>, ClientError> {
        let url = self.endpoint(
            &["api", "conversations", id.as_str(), "entries"],
            &[
                ("before_seq", before_seq.to_string()),
                ("limit", limit.to_string()),
            ],
        )?;
        self.http.get(&url)
    }

    /// The newest `limit` entries, ascending — what a transcript shows first.
    pub fn tail(&self, id: &ConversationId, limit: u32) -> Result<Vec<Entry>, ClientError> {
        let url = self.endpoint(
            &["api", "conversations", id.as_str(), "tail"],
            &[("limit", limit.to_string())],
        )?;
        self.http.get(&url)
    }

    /// The whole transcript as NDJSON: `GET /api/conversations/{id}/export`.
    ///
    /// One [`Entry`] per line, each exactly as `/entries` serializes it, so a
    /// caller can write the bytes through unchanged (`swarmbot transcript --out
    /// FILE`) and a reader can parse a line at a time. The value is the raw body;
    /// this method does not reformat, re-order or re-encode it.
    ///
    /// The brain streams the export, but a `String` has to be held whole, so the
    /// blocking client caps one call at [`EXPORT_MAX_BYTES`]. Past that the call
    /// **fails** — it never returns a short file, because an archive tool cannot
    /// tell a truncated export from a complete one.
    pub fn export_transcript(&self, id: &ConversationId) -> Result<String, ClientError> {
        let url = self.endpoint(&["api", "conversations", id.as_str(), "export"], &[])?;
        self.http.get_text(&url, EXPORT_MAX_BYTES)
    }

    /// Post the owner's message. `addressed` are the bots mentioned with
    /// `@handle`; in a group with no mention the lead answers.
    pub fn send_message(
        &self,
        id: &ConversationId,
        text: &str,
        addressed: &[BotId],
        reply_to: Option<&EntryId>,
    ) -> Result<SentMessage, ClientError> {
        let url = self.endpoint(&["api", "conversations", id.as_str(), "messages"], &[])?;
        self.http.post(
            &url,
            &SendMessageBody {
                text,
                addressed,
                reply_to,
            },
        )
    }

    /// Stop the running turn of this conversation; `true` when one was
    /// actually running.
    pub fn interrupt(&self, id: &ConversationId) -> Result<bool, ClientError> {
        let url = self.endpoint(&["api", "conversations", id.as_str(), "interrupt"], &[])?;
        let body: StoppedBody = self.http.post_empty(&url)?;
        Ok(body.stopped)
    }

    // --- runtime requests ----------------------------------------------------

    /// Every request still waiting for an answer.
    pub fn pending_requests(&self) -> Result<Vec<RuntimeRequest>, ClientError> {
        let url = self.endpoint(&["api", "requests"], &[])?;
        self.http.get(&url)
    }

    /// Answer a request: approvals carry a [`Decision`], questions and forms
    /// carry an `answer` JSON value. Exactly one of the two is meaningful per
    /// request kind; the brain ignores what does not apply.
    pub fn respond_request(
        &self,
        id: &RequestId,
        decision: Option<Decision>,
        answer: Option<Value>,
    ) -> Result<RuntimeRequest, ClientError> {
        let url = self.endpoint(&["api", "requests", id.as_str(), "respond"], &[])?;
        self.http.post(&url, &RespondBody { decision, answer })
    }

    // --- tasks ----------------------------------------------------------------

    /// All known tasks, or only `bot`'s when given.
    pub fn tasks(&self, bot: Option<&BotId>) -> Result<Vec<Task>, ClientError> {
        let query: Vec<(&str, String)> = match bot {
            Some(bot) => vec![("bot", bot.as_str().to_string())],
            None => Vec::new(),
        };
        let url = self.endpoint(&["api", "tasks"], &query)?;
        self.http.get(&url)
    }

    pub fn task(&self, id: &TaskId) -> Result<TaskDetail, ClientError> {
        let url = self.endpoint(&["api", "tasks", id.as_str()], &[])?;
        self.http.get(&url)
    }

    pub fn cancel_task(&self, id: &TaskId) -> Result<Task, ClientError> {
        let url = self.endpoint(&["api", "tasks", id.as_str(), "cancel"], &[])?;
        self.http.post_empty(&url)
    }

    // --- routines --------------------------------------------------------------

    /// All known routines, or only `bot`'s when given.
    pub fn routines(&self, bot: Option<&BotId>) -> Result<Vec<Routine>, ClientError> {
        let query: Vec<(&str, String)> = match bot {
            Some(bot) => vec![("bot", bot.as_str().to_string())],
            None => Vec::new(),
        };
        let url = self.endpoint(&["api", "routines"], &query)?;
        self.http.get(&url)
    }

    pub fn create_routine(&self, request: &NewRoutineRequest) -> Result<Routine, ClientError> {
        let url = self.endpoint(&["api", "routines"], &[])?;
        self.http.post(&url, request)
    }

    pub fn routine(&self, id: &RoutineId) -> Result<Routine, ClientError> {
        let url = self.endpoint(&["api", "routines", id.as_str()], &[])?;
        self.http.get(&url)
    }

    pub fn pause_routine(&self, id: &RoutineId) -> Result<Routine, ClientError> {
        let url = self.endpoint(&["api", "routines", id.as_str(), "pause"], &[])?;
        self.http.post_empty(&url)
    }

    pub fn resume_routine(&self, id: &RoutineId) -> Result<Routine, ClientError> {
        let url = self.endpoint(&["api", "routines", id.as_str(), "resume"], &[])?;
        self.http.post_empty(&url)
    }

    pub fn delete_routine(&self, id: &RoutineId) -> Result<(), ClientError> {
        let url = self.endpoint(&["api", "routines", id.as_str()], &[])?;
        self.http.delete(&url)
    }

    pub fn routine_runs(
        &self,
        id: &RoutineId,
        limit: Option<u32>,
    ) -> Result<Vec<RoutineRun>, ClientError> {
        let query: Vec<(&str, String)> = match limit {
            Some(limit) => vec![("limit", limit.to_string())],
            None => Vec::new(),
        };
        let url = self.endpoint(&["api", "routines", id.as_str(), "runs"], &query)?;
        self.http.get(&url)
    }

    // --- nodes -----------------------------------------------------------------

    pub fn nodes(&self) -> Result<Vec<NodeInfo>, ClientError> {
        let url = self.endpoint(&["api", "nodes"], &[])?;
        self.http.get(&url)
    }

    /// Mint a one-time `ABCD-EFGH` pairing code for a node to redeem.
    pub fn create_pairing_code(&self) -> Result<PairingCode, ClientError> {
        let url = self.endpoint(&["api", "nodes", "pairing-codes"], &[])?;
        self.http.post_empty(&url)
    }

    /// Revoke a node's pairing; it drops offline and cannot re-pair.
    pub fn revoke_node(&self, id: &NodeId) -> Result<(), ClientError> {
        let url = self.endpoint(&["api", "nodes", id.as_str()], &[])?;
        self.http.delete(&url)
    }

    /// Edit one node's roster row: `PATCH /api/nodes/{id}`.
    ///
    /// `patch` is a JSON object with either or both of:
    ///
    /// * `label` — the display name the roster shows (an empty one is refused);
    /// * `approved` — the capability names the owner allows. It **replaces** the
    ///   stored list, must be a subset of what the node declared, and an empty
    ///   list approves nothing. An undeclared name is a 400 naming it.
    ///
    /// The write goes through the brain's guarded writer, so an owner's edit and a
    /// node's heartbeat cannot lose each other, and the answer is the row as
    /// stored (with its new revision).
    pub fn update_node(&self, id: &NodeId, patch: &Value) -> Result<NodeInfo, ClientError> {
        let url = self.endpoint(&["api", "nodes", id.as_str()], &[])?;
        self.http.patch(&url, patch)
    }

    // --- approval rules ----------------------------------------------------------

    pub fn rules(&self) -> Result<Vec<ApprovalRule>, ClientError> {
        let url = self.endpoint(&["api", "rules"], &[])?;
        self.http.get(&url)
    }

    pub fn create_rule(&self, rule: &ApprovalRule) -> Result<ApprovalRule, ClientError> {
        let url = self.endpoint(&["api", "rules"], &[])?;
        self.http.post(&url, rule)
    }

    pub fn delete_rule(&self, id: &RuleId) -> Result<(), ClientError> {
        let url = self.endpoint(&["api", "rules", id.as_str()], &[])?;
        self.http.delete(&url)
    }

    // --- settings & backends ------------------------------------------------------

    pub fn settings(&self) -> Result<Settings, ClientError> {
        let url = self.endpoint(&["api", "settings"], &[])?;
        self.http.get(&url)
    }

    /// A *partial* settings object: the brain JSON-merges it, so only the
    /// keys present change.
    pub fn update_settings(&self, patch: &Value) -> Result<Settings, ClientError> {
        let url = self.endpoint(&["api", "settings"], &[])?;
        self.http.patch(&url, patch)
    }

    /// The configured backends, redacted (never a key, only its env var).
    pub fn backends(&self) -> Result<Vec<BackendView>, ClientError> {
        let url = self.endpoint(&["api", "backends"], &[])?;
        self.http.get(&url)
    }

    /// Ask the brain to ping every backend. Uses a longer budget than normal
    /// calls (at least two minutes) because the brain dials out to real LLM
    /// providers here.
    pub fn probe_backends(&self) -> Result<Vec<BackendProbe>, ClientError> {
        let url = self.endpoint(&["api", "backends", "probe"], &[])?;
        let timeout = self.http.timeout().max(PROBE_TIMEOUT);
        self.http.get_with_timeout(&url, timeout)
    }

    /// Run the Settings screen's **Connect** for one backend: the handshake,
    /// plus `method` when the owner picked one of the backend's advertised
    /// logins. Probe-style budget (it spawns an agent on the brain's side), and
    /// it never spends a model call.
    pub fn connect_backend(
        &self,
        backend_id: &str,
        method: Option<&str>,
    ) -> Result<BackendProbe, ClientError> {
        let url = self.endpoint(&["api", "backends", backend_id, "connect"], &[])?;
        let timeout = self.http.timeout().max(PROBE_TIMEOUT);
        self.http
            .post_with_timeout(&url, &serde_json::json!({"method": method}), timeout)
    }

    /// The models one backend advertises. Probe-style budget: an ACP backend
    /// answers this from a live agent session.
    pub fn backend_models(&self, backend_id: &str) -> Result<BackendModels, ClientError> {
        let url = self.endpoint(&["api", "backends", backend_id, "models"], &[])?;
        let timeout = self.http.timeout().max(PROBE_TIMEOUT);
        self.http.get_with_timeout(&url, timeout)
    }

    // --- usage ------------------------------------------------------------------

    /// `GET /api/usage` — what the backends reported the team spent in a window.
    ///
    /// `since` is an **RFC 3339 timestamp** (`"2026-01-31T00:00:00Z"`), the same
    /// text [`UsageReport::since`] carries; `None` asks the brain for its
    /// default window (the last 24 hours). It is text and not a `DateTime` for
    /// the same reason [`PairingCode::expires_at`] is: this crate has no
    /// `chrono` dependency, on purpose, and the caller (CLI, GPUI) already has
    /// the clock it wants to ask about.
    ///
    /// The numbers are what the backends reported, not an estimate — see
    /// [`UsageReport`]. A zero here means "nothing was reported", so a caller
    /// showing it should also show `report.turns`.
    pub fn usage(&self, since: Option<&str>) -> Result<UsageReport, ClientError> {
        let query: Vec<(&str, String)> = match since {
            Some(since) => vec![("since", since.to_string())],
            None => Vec::new(),
        };
        let url = self.endpoint(&["api", "usage"], &query)?;
        self.http.get(&url)
    }

    // --- operational -------------------------------------------------------------

    /// `GET /api/metrics` — what the brain is doing right now, as counts.
    ///
    /// Everything is a count of something the store or the runner map already
    /// knows, so a zero is a fact ("nothing is running") rather than a missing
    /// measurement. See [`Metrics`] for what each field means.
    pub fn metrics(&self) -> Result<Metrics, ClientError> {
        let url = self.endpoint(&["api", "metrics"], &[])?;
        self.http.get(&url)
    }

    /// `GET /api/secrets` — the credential **names** the brain holds, with the
    /// backends that ask for each one. There is no way to read a value back, by
    /// design.
    pub fn secrets(&self) -> Result<Vec<SecretName>, ClientError> {
        let url = self.endpoint(&["api", "secrets"], &[])?;
        self.http.get(&url)
    }

    /// `PUT /api/secrets` — write or replace one credential.
    ///
    /// The brain writes `<data>/secrets.json` atomically with mode 0600 and
    /// registers the value with its log scrubber immediately. Read [`SecretWrite`]
    /// for what `applies` will say: today it is `"restart"`, because the backend
    /// registry resolves its keys once, when the swarm is composed.
    pub fn set_secret(&self, name: &str, value: &str) -> Result<SecretWrite, ClientError> {
        let url = self.endpoint(&["api", "secrets"], &[])?;
        self.http
            .put(&url, &serde_json::json!({"name": name, "value": value}))
    }

    /// `DELETE /api/secrets/{name}` — remove one credential from the file.
    ///
    /// A name the brain also receives from its own environment is refused with a
    /// 409: a process cannot unset its environment, so answering "deleted" would
    /// be a lie about a key that is still in use.
    pub fn delete_secret(&self, name: &str) -> Result<SecretWrite, ClientError> {
        let url = self.endpoint(&["api", "secrets", name], &[])?;
        self.http.delete_json(&url)
    }

    /// `POST /api/auth/revoke-sessions` — log every browser session out, keeping
    /// the owner token. Returns how many sessions died (idempotent: zero the
    /// second time). A caller authenticating with a cookie is revoking itself too.
    pub fn revoke_sessions(&self) -> Result<usize, ClientError> {
        let url = self.endpoint(&["api", "auth", "revoke-sessions"], &[])?;
        let body: RevokedBody = self.http.post_empty(&url)?;
        Ok(body.revoked)
    }

    /// Build one endpoint URL: `segments` are appended as percent-encoded
    /// path segments (so an id or handle can never break the path), and
    /// `query` values are form-percent-encoded (`3:42` survives as `3%3A42`).
    fn endpoint(&self, segments: &[&str], query: &[(&str, String)]) -> Result<String, ClientError> {
        let mut url = self.base.clone();
        {
            let mut paths = url
                .path_segments_mut()
                .map_err(|()| ClientError::Transport {
                    url: self.base_url.clone(),
                    reason: "the base URL cannot take a path".to_string(),
                })?;
            for segment in segments {
                paths.push(segment);
            }
        }
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }
        Ok(url.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::StubBrain;
    use serde_json::json;
    use swarmbot_contracts::EntryKind;
    use swarmbot_contracts::routine::RoutineStatus;

    fn stub_client(stub: &StubBrain) -> BrainClient {
        BrainClient::new(&stub.url()).expect("a loopback stub URL is valid")
    }

    fn entry_text(entry: &Entry) -> &str {
        match &entry.kind {
            EntryKind::Message(message) => &message.text,
            other => panic!("expected a message entry, got {other:?}"),
        }
    }

    fn body_json(request: &crate::testing::RecordedRequest) -> Value {
        serde_json::from_str(&request.body).expect("the stub records JSON bodies")
    }

    // --- URL building -------------------------------------------------------

    #[test]
    fn trailing_slashes_are_stripped_and_the_origin_is_kept() {
        let client = BrainClient::new("http://127.0.0.1:8787/").expect("valid");
        assert_eq!(client.base_url(), "http://127.0.0.1:8787");
        assert_eq!(client.ws_url(), "ws://127.0.0.1:8787");

        let secure = BrainClient::new("https://brain.example///").expect("valid");
        assert_eq!(secure.base_url(), "https://brain.example");
        assert_eq!(secure.ws_url(), "wss://brain.example");

        let prefixed = BrainClient::new("http://127.0.0.1:9/brain/").expect("valid");
        assert_eq!(prefixed.base_url(), "http://127.0.0.1:9/brain");
        assert_eq!(
            prefixed.endpoint(&["api", "bots"], &[]).expect("endpoint"),
            "http://127.0.0.1:9/brain/api/bots"
        );
    }

    #[test]
    fn only_http_origins_are_accepted() {
        assert!(BrainClient::new("ftp://127.0.0.1:9").is_err());
        assert!(BrainClient::new("not a url").is_err());
        assert!(BrainClient::new("").is_err());
    }

    #[test]
    fn endpoint_segments_and_queries_are_percent_encoded() {
        let client = BrainClient::new("http://127.0.0.1:9").expect("valid");
        // A hostile handle can never break out of its path segment.
        let url = client
            .endpoint(&["api", "bots", "na ughty/.."], &[])
            .expect("endpoint");
        assert!(url.contains("na%20ughty%2F.."), "{url}");
        // Form encoding: `3:42` survives the round trip as `3%3A42`.
        let url = client
            .endpoint(
                &["ws-check"],
                &[
                    ("cursor", "3:42".to_string()),
                    ("conversation", "cnv_a b".to_string()),
                ],
            )
            .expect("endpoint");
        assert!(url.contains("cursor=3%3A42"), "{url}");
        assert!(url.contains("conversation=cnv_a+b"), "{url}");
    }

    // --- auth ----------------------------------------------------------------

    #[test]
    fn the_bearer_header_is_sent_only_with_a_token() {
        let stub = StubBrain::start();
        let client = stub_client(&stub).with_token("sekrit");
        client.bots().expect("stub serves bots");
        let request = stub.last_request("GET", "/api/bots").expect("recorded");
        assert_eq!(request.auth.as_deref(), Some("Bearer sekrit"));

        let anonymous = stub_client(&stub);
        anonymous.health().expect("stub serves health");
        let request = stub.last_request("GET", "/api/health").expect("recorded");
        assert_eq!(request.auth, None);
    }

    #[test]
    fn login_stores_the_cookie_and_remembers_the_token() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);
        client.login("one-time-token").expect("login succeeds");

        let session = stub.last_request("POST", "/api/session").expect("recorded");
        // ureq writes request JSON pretty-printed, so compare parsed bodies.
        assert_eq!(body_json(&session)["token"], "one-time-token");

        // The agent's cookie jar rides along on later HTTP requests …
        client.health().expect("health");
        let health = stub.last_request("GET", "/api/health").expect("recorded");
        assert_eq!(
            health.cookie.as_deref(),
            Some("swarmbot_session=stub-session")
        );

        // … and the remembered token is what `live()` will authenticate with,
        // because a WebSocket upgrade cannot read the cookie jar.
        assert_eq!(client.live_token().as_deref(), Some("one-time-token"));
    }

    // --- happy paths -----------------------------------------------------------

    #[test]
    fn health_reports_the_stubbed_brain() {
        let stub = StubBrain::start();
        let health = stub_client(&stub).health().expect("health");
        assert!(health.ok);
        assert_eq!(health.version, "0.1.0");
        assert_eq!(health.epoch, 7);
        assert_eq!(health.uptime_secs, 42);
    }

    #[test]
    fn bots_round_trip_and_create_sends_the_request_body() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);

        let bots = client.bots().expect("bots");
        assert_eq!(bots.len(), 1);
        assert_eq!(bots[0].name, "stub");
        assert_eq!(bots[0].id.as_str(), "bot_000000000000000000000000000000b1");

        let request = NewBotRequest::new("scout", "Scout", "reads things", "fake")
            .model("m1")
            .color("#123456");
        let created = client.create_bot(&request).expect("create");
        assert_eq!(created.name, "scout");
        let recorded = stub.last_request("POST", "/api/bots").expect("recorded");
        let body = body_json(&recorded);
        assert_eq!(body["name"], "scout");
        assert_eq!(body["backend"], "fake");
        assert_eq!(body["model"], "m1");
        assert_eq!(body["color"], "#123456");
        // Unset options are skipped, not sent as nulls.
        assert!(body.get("node").is_none(), "{body}");

        // A handle works where the server takes id-or-handle.
        let by_handle = client.bot("stub").expect("bot by handle");
        assert_eq!(by_handle.label, "Stub Bot");
    }

    #[test]
    fn a_missing_bot_is_a_not_found_error() {
        let stub = StubBrain::start();
        let err = stub_client(&stub).bot("missing").expect_err("missing bot");
        assert!(err.is_not_found());
        assert_eq!(err.code(), Some("not_found"));
        match err {
            ClientError::Http {
                status, message, ..
            } => {
                assert_eq!(status, 404);
                assert_eq!(message, "bot not found: missing");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn bot_updates_deletes_and_sessions() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);

        let updated = client
            .update_bot("stub", &json!({"label": "Renamed"}))
            .expect("update");
        assert_eq!(updated.name, "stub");
        let recorded = stub
            .last_request("PATCH", "/api/bots/stub")
            .expect("recorded");
        assert_eq!(body_json(&recorded)["label"], "Renamed");

        let folded = client
            .bot_session("stub", SessionAction::Compact)
            .expect("session");
        assert_eq!(folded, 3);
        let recorded = stub
            .last_request("POST", "/api/bots/stub/session")
            .expect("recorded");
        assert_eq!(body_json(&recorded)["action"], "compact");

        client.delete_bot("stub").expect("delete");
        let recorded = stub
            .last_request("DELETE", "/api/bots/stub")
            .expect("recorded");
        assert_eq!(recorded.body, "");
    }

    #[test]
    fn a_typed_bot_patch_sends_only_what_was_set() {
        // The two fields a front end forgets most: `skills` and `enabled`.
        let stub = StubBrain::start();
        let client = stub_client(&stub);

        let patch = BotPatch::new()
            .skills(vec!["release-notes".to_string()])
            .enabled(false);
        client.patch_bot("stub", &patch).expect("patched");
        let recorded = stub
            .last_request("PATCH", "/api/bots/stub")
            .expect("recorded");
        let body = body_json(&recorded);
        assert_eq!(body["skills"], json!(["release-notes"]));
        assert_eq!(body["enabled"], json!(false));
        // Everything unset is skipped, not sent as a null the brain would read as
        // a value: a merge patch only touches what is present.
        assert!(body.get("label").is_none(), "{body}");
        assert!(body.get("instructions").is_none(), "{body}");

        // An empty list is a value, and it means "remove every skill".
        let clear = BotPatch::new().skills(Vec::new());
        client.patch_bot("stub", &clear).expect("patched");
        let recorded = stub
            .last_request("PATCH", "/api/bots/stub")
            .expect("recorded");
        assert_eq!(body_json(&recorded)["skills"], json!([]));

        // An empty patch serializes to `{}`, which the brain answers with the
        // current row rather than a 400.
        assert_eq!(BotPatch::new().to_value().expect("serializes"), json!({}));
    }

    #[test]
    fn the_new_read_routes_name_their_own_urls() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);

        // Backward paging: both parameters, in the documented spelling.
        let id = ConversationId::from_suffix("000000000000000000000000000000c1");
        let entries = client.entries_before(&id, 42, 50).expect("a backward page");
        assert_eq!(entries.len(), 1, "the stub answers a canned entry");
        let recorded = stub
            .last_request("GET", &format!("/api/conversations/{id}/entries"))
            .expect("recorded");
        assert!(recorded.query.contains("before_seq=42"), "{recorded:?}");
        assert!(recorded.query.contains("limit=50"), "{recorded:?}");

        // The export is handed back verbatim: one JSON object per line, not a
        // decoded array.
        let text = client.export_transcript(&id).expect("the export");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text:?}");
        for line in &lines {
            let value: Value = serde_json::from_str(line).expect("one Entry per line");
            assert_eq!(value["conversation_id"], json!(id.to_string()));
        }
        let recorded = stub
            .last_request("GET", &format!("/api/conversations/{id}/export"))
            .expect("recorded");
        assert_eq!(recorded.query, "", "the export takes no query");

        // Metrics decode into the typed struct field for field.
        let metrics = client.metrics().expect("metrics");
        assert_eq!(metrics.version, "0.1.0");
        assert_eq!(metrics.epoch, 7);
        assert_eq!(metrics.bots_total, 1);
        assert_eq!(metrics.runners, 1);
        stub.last_request("GET", "/api/metrics").expect("recorded");

        // Secrets: names in, the write's `applies` out, and no value anywhere.
        let secrets = client.secrets().expect("secrets");
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0].name, "INFERHUB_API_KEY");
        assert_eq!(secrets[0].used_by, vec!["inferhub".to_string()]);
        let written = client
            .set_secret("INFERHUB_API_KEY", "sk-stub-value")
            .expect("written");
        assert_eq!(written.name, "INFERHUB_API_KEY");
        assert_eq!(written.applies, "restart");
        let recorded = stub.last_request("PUT", "/api/secrets").expect("recorded");
        let body = body_json(&recorded);
        assert_eq!(body["name"], "INFERHUB_API_KEY");
        assert_eq!(
            body["value"], "sk-stub-value",
            "the value goes *to* the brain"
        );
        let removed = client.delete_secret("INFERHUB_API_KEY").expect("removed");
        assert_eq!(removed.applies, "restart");
        stub.last_request("DELETE", "/api/secrets/INFERHUB_API_KEY")
            .expect("recorded");
    }

    #[test]
    fn routines_round_trip_against_the_stub() {
        // This route set exists for the CLI's remote mode: without it, every
        // routine command in `--brain` mode could only be tested against a 404.
        let stub = StubBrain::start();
        let client = stub_client(&stub);

        let listed = client.routines(None).expect("routines");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Nightly build");
        assert_eq!(listed[0].status, RoutineStatus::Active);
        assert!(
            listed[0].next_run_at.is_some(),
            "an active routine has a next run"
        );

        let paused = client.pause_routine(&listed[0].id).expect("paused");
        assert_eq!(paused.status, RoutineStatus::Paused);
        stub.last_request("POST", &format!("/api/routines/{}/pause", listed[0].id))
            .expect("recorded");

        let resumed = client.resume_routine(&listed[0].id).expect("resumed");
        assert_eq!(resumed.status, RoutineStatus::Active);

        let runs = client.routine_runs(&listed[0].id, Some(10)).expect("runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].status,
            swarmbot_contracts::routine::RunStatus::Success
        );
        assert!(runs[0].ended_at.is_some(), "a finished run has an end");
    }

    #[test]
    fn conversations_create_rename_and_delete() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);

        let all = client.conversations().expect("conversations");
        assert_eq!(all.len(), 1);
        let conversation_id = all[0].id.clone();

        let members = vec![
            BotId::from_suffix("000000000000000000000000000000b1"),
            BotId::new(),
        ];
        let group = client
            .create_group("Team", &members, Some(&members[0]))
            .expect("create group");
        assert!(group.id.as_str().starts_with("cnv_"));
        let recorded = stub
            .last_request("POST", "/api/conversations")
            .expect("recorded");
        let body = body_json(&recorded);
        assert_eq!(body["title"], "Team");
        assert_eq!(body["members"][0], members[0].as_str());
        assert_eq!(body["lead"], members[0].as_str());

        let renamed = client
            .rename_conversation(&conversation_id, "New name")
            .expect("rename");
        assert_eq!(renamed.title.as_deref(), Some("New name"));

        client
            .delete_conversation(&conversation_id)
            .expect("delete");
    }

    #[test]
    fn update_group_updates_title_members_and_lead() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);

        let group_id = ConversationId::new();
        let b1 = BotId::from_suffix("000000000000000000000000000000b1");
        let b2 = BotId::from_suffix("000000000000000000000000000000b2");
        let b3 = BotId::from_suffix("000000000000000000000000000000b3");
        let members = vec![b1.clone(), b2.clone(), b3.clone()];

        let updated = client
            .update_group(&group_id, Some("Core Team"), Some(&members), Some(&b2))
            .expect("update group");
        assert_eq!(updated.id, group_id);
        assert_eq!(updated.title.as_deref(), Some("Core Team"));
        assert_eq!(
            updated.kind,
            swarmbot_contracts::ConversationKind::Group {
                members: members.clone(),
                lead: b2.clone(),
            }
        );

        let path = format!("/api/conversations/{group_id}");
        let recorded = stub.last_request("PATCH", &path).expect("recorded");
        let body = body_json(&recorded);
        assert_eq!(body["title"], "Core Team");
        assert_eq!(
            body["members"],
            json!([b1.as_str(), b2.as_str(), b3.as_str()])
        );
        assert_eq!(body["lead"], b2.as_str());
    }

    #[test]
    fn update_group_partial_updates_omit_none_fields() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);

        let group_id = ConversationId::new();
        let b2 = BotId::from_suffix("000000000000000000000000000000b2");
        let b3 = BotId::from_suffix("000000000000000000000000000000b3");
        let path = format!("/api/conversations/{group_id}");

        // Title only.
        let updated = client
            .update_group(&group_id, Some("New Title"), None, None)
            .expect("title patch");
        assert_eq!(updated.title.as_deref(), Some("New Title"));
        let body = body_json(&stub.last_request("PATCH", &path).expect("recorded"));
        assert_eq!(body, json!({"title": "New Title"}));

        // Members only.
        let subset = vec![b2.clone(), b3.clone()];
        let updated = client
            .update_group(&group_id, None, Some(&subset), None)
            .expect("members patch");
        assert_eq!(updated.kind.participants(), subset);
        let body = body_json(&stub.last_request("PATCH", &path).expect("recorded"));
        assert_eq!(body, json!({"members": [b2.as_str(), b3.as_str()]}));

        // Lead only.
        let updated = client
            .update_group(&group_id, None, None, Some(&b3))
            .expect("lead patch");
        match updated.kind {
            swarmbot_contracts::ConversationKind::Group { lead, .. } => assert_eq!(lead, b3),
            other => panic!("expected group, got {other:?}"),
        }
        let body = body_json(&stub.last_request("PATCH", &path).expect("recorded"));
        assert_eq!(body, json!({"lead": b3.as_str()}));

        // `rename_conversation` delegates cleanly with only `title` set.
        let renamed = client
            .rename_conversation(&group_id, "Via Rename")
            .expect("rename");
        assert_eq!(renamed.title.as_deref(), Some("Via Rename"));
        let body = body_json(&stub.last_request("PATCH", &path).expect("recorded"));
        assert_eq!(body, json!({"title": "Via Rename"}));
    }

    #[test]
    fn entries_and_tail_send_paging_queries() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);
        let conversation = ConversationId::from_suffix("000000000000000000000000000000c1");

        let entries = client.entries(&conversation, 5, 10).expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entry_text(&entries[0]), "canned entry");
        let path = format!("/api/conversations/{conversation}/entries");
        let recorded = stub.last_request("GET", &path).expect("recorded");
        assert_eq!(recorded.query, "after_seq=5&limit=10");

        let tail = client.tail(&conversation, 25).expect("tail");
        assert_eq!(tail.len(), 1);
        let path = format!("/api/conversations/{conversation}/tail");
        let recorded = stub.last_request("GET", &path).expect("recorded");
        assert_eq!(recorded.query, "limit=25");
    }

    #[test]
    fn send_message_carries_text_addressed_and_reply_to() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);
        let conversation = ConversationId::from_suffix("000000000000000000000000000000c1");
        let addressed = vec![BotId::from_suffix("000000000000000000000000000000b1")];
        let reply_to = EntryId::from_suffix("000000000000000000000000000000f1");

        let sent = client
            .send_message(&conversation, "hello team", &addressed, Some(&reply_to))
            .expect("send");
        assert_eq!(entry_text(&sent.entry), "hello team");
        assert_eq!(sent.targets, addressed);
        assert!(sent.queued.is_empty());
        assert_eq!(sent.reason, "lead");

        let path = format!("/api/conversations/{conversation}/messages");
        let recorded = stub.last_request("POST", &path).expect("recorded");
        let body = body_json(&recorded);
        assert_eq!(body["text"], "hello team");
        assert_eq!(body["addressed"][0], addressed[0].as_str());
        assert_eq!(body["reply_to"], reply_to.as_str());

        // Without a reply target the key is skipped entirely.
        client
            .send_message(&conversation, "again", &[], None)
            .expect("send");
        let recorded = stub.last_request("POST", &path).expect("recorded");
        let body = body_json(&recorded);
        assert_eq!(body["addressed"].as_array().expect("array").len(), 0);
        assert!(body.get("reply_to").is_none(), "{body}");
    }

    #[test]
    fn interrupt_reports_whether_something_stopped() {
        let stub = StubBrain::start();
        let conversation = ConversationId::from_suffix("000000000000000000000000000000c1");
        let stopped = stub_client(&stub)
            .interrupt(&conversation)
            .expect("interrupt");
        assert!(stopped);
    }

    #[test]
    fn requests_answer_with_a_decision_or_an_answer() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);
        let request_id = RequestId::from_suffix("000000000000000000000000000000e1");

        let pending = client.pending_requests().expect("pending");
        assert_eq!(pending.len(), 1);
        assert!(pending[0].is_pending());

        let decided = client
            .respond_request(&request_id, Some(Decision::AllowOnce), None)
            .expect("respond");
        assert_eq!(decided.status, swarmbot_contracts::RequestStatus::Answered);
        assert_eq!(decided.decision, Some(Decision::AllowOnce));
        let path = format!("/api/requests/{request_id}/respond");
        let recorded = stub.last_request("POST", &path).expect("recorded");
        let body = body_json(&recorded);
        assert_eq!(body["decision"], "allow_once");
        assert!(body["answer"].is_null(), "{body}");

        let answered = client
            .respond_request(&request_id, None, Some(json!({"port": 8080})))
            .expect("respond");
        assert_eq!(answered.answer, Some(json!({"port": 8080})));
        let recorded = stub.last_request("POST", &path).expect("recorded");
        let body = body_json(&recorded);
        assert!(body["decision"].is_null(), "{body}");
        assert_eq!(body["answer"]["port"], 8080);
    }

    #[test]
    fn tasks_list_filter_and_detail() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);
        let bot = BotId::from_suffix("000000000000000000000000000000b1");
        let task_id = TaskId::from_suffix("000000000000000000000000000000d1");

        let all = client.tasks(None).expect("tasks");
        assert_eq!(all.len(), 1);
        assert_eq!(
            stub.last_request("GET", "/api/tasks")
                .expect("recorded")
                .query,
            ""
        );

        let filtered = client.tasks(Some(&bot)).expect("tasks");
        assert_eq!(filtered[0].id, task_id);
        let recorded = stub.last_request("GET", "/api/tasks").expect("recorded");
        assert_eq!(recorded.query, format!("bot={bot}"));

        let detail = client.task(&task_id).expect("task");
        assert_eq!(detail.task.id, task_id);
        assert_eq!(detail.steps.len(), 1);
        assert_eq!(detail.steps[0].tool.as_deref(), Some("shell"));

        let cancelled = client.cancel_task(&task_id).expect("cancel");
        assert_eq!(cancelled.status, swarmbot_contracts::TaskStatus::Cancelled);
    }

    #[test]
    fn nodes_pairing_and_revocation() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);

        let nodes = client.nodes().expect("nodes");
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].id.is_local());

        let pairing = client.create_pairing_code().expect("pairing code");
        assert_eq!(pairing.code, "ABCD-EFGH");
        assert_eq!(pairing.expires_at, "2026-01-01T00:00:00Z");
        assert_eq!(pairing.node_id, None);

        client.revoke_node(&nodes[0].id).expect("revoke");
        let path = format!("/api/nodes/{}", nodes[0].id);
        assert!(stub.last_request("DELETE", &path).is_some());
    }

    #[test]
    fn rules_list_create_and_delete() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);

        let rules = client.rules().expect("rules");
        assert_eq!(rules.len(), 1);

        let rule = ApprovalRule::new(
            swarmbot_contracts::RuleScope::Global,
            swarmbot_contracts::RuleEffect::Deny,
        );
        let created = client.create_rule(&rule).expect("create rule");
        assert_eq!(created.effect, swarmbot_contracts::RuleEffect::Deny);
        let recorded = stub.last_request("POST", "/api/rules").expect("recorded");
        assert_eq!(body_json(&recorded)["effect"], "deny");

        client.delete_rule(&rule.id).expect("delete rule");
        let path = format!("/api/rules/{}", rule.id);
        assert!(stub.last_request("DELETE", &path).is_some());
    }

    #[test]
    fn settings_read_and_partial_update() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);

        let settings = client.settings().expect("settings");
        assert_eq!(settings, Settings::default());

        let updated = client
            .update_settings(&json!({"bot_messaging": false, "max_hops": 3}))
            .expect("update");
        assert!(!updated.bot_messaging);
        assert_eq!(updated.max_hops, 3);
        // Untouched keys keep the brain's value.
        assert_eq!(updated.max_tool_rounds, Settings::default().max_tool_rounds);

        // The body sent is exactly the partial patch — the merge is the
        // brain's job, not the client's.
        let recorded = stub
            .last_request("PATCH", "/api/settings")
            .expect("recorded");
        assert_eq!(
            body_json(&recorded),
            json!({"bot_messaging": false, "max_hops": 3})
        );
    }

    #[test]
    fn backends_are_redacted_and_probed() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);

        let backends = client.backends().expect("backends");
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].id, "fake");
        assert_eq!(backends[0].kind, BackendKind::Fake);
        assert_eq!(backends[0].model.as_deref(), Some("fake"));

        let probes = client.probe_backends().expect("probe");
        assert_eq!(probes.len(), 1);
        assert!(probes[0].ok);
        assert_eq!(probes[0].backend, "fake");
        // A reachable backend advertises no login: the fields exist and are
        // empty, so a UI can render them without a special case.
        assert!(probes[0].auth_methods.is_empty());
        assert!(!probes[0].auth_required);
    }

    #[test]
    fn connect_posts_the_chosen_method_and_decodes_the_login_it_needs() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);

        // The plain handshake: a `null` method, and the brain's answer decoded
        // in full — including the login the agent advertises and the command
        // the config names. The client never invents either.
        let status = client.connect_backend("claude", None).expect("connect");
        assert!(!status.ok);
        assert!(status.auth_required);
        assert_eq!(status.auth_methods.len(), 1);
        assert_eq!(status.auth_methods[0].id, "claude-login");
        assert_eq!(
            status.auth_methods[0].description.as_deref(),
            Some("Run `claude /login` in the terminal")
        );
        // The agent named no command, so the method carries none: the command
        // that is shown came from the config (`login_command`), not from a
        // guess.
        assert_eq!(status.auth_methods[0].command, None);
        assert_eq!(status.login_command.as_deref(), Some("claude /login"));

        // A chosen login is sent as `{"method": …}`, and the refusal it earns
        // is surfaced verbatim rather than turned into a client-side success.
        let refused = client
            .connect_backend("claude", Some("claude-login"))
            .expect("connect reports");
        assert!(!refused.ok, "{refused:?}");
        assert!(
            refused
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("Method not implemented")),
            "{refused:?}"
        );

        let posted = stub
            .last_request("POST", "/api/backends/claude/connect")
            .expect("the connect call reached the stub");
        assert!(
            posted.body.contains("\"claude-login\""),
            "the chosen method id goes on the wire: {}",
            posted.body
        );
    }

    #[test]
    fn shutdown_is_acknowledged() {
        let stub = StubBrain::start();
        stub_client(&stub).shutdown().expect("shutdown");
        assert!(stub.last_request("POST", "/api/shutdown").is_some());
    }

    // --- error mapping end to end ----------------------------------------------

    #[test]
    fn conflicts_and_non_json_failures_keep_their_status() {
        let stub = StubBrain::start();
        let client = stub_client(&stub);

        let conflict_url = format!("{}/api/boom/409", client.base_url());
        let err = client.http.get::<Value>(&conflict_url).expect_err("409");
        assert!(err.is_conflict());
        assert_eq!(err.code(), Some("conflict"));

        let broken_url = format!("{}/api/boom/502", client.base_url());
        let err = client.http.get::<Value>(&broken_url).expect_err("502");
        match err {
            ClientError::Http {
                status,
                code,
                message,
            } => {
                assert_eq!(status, 502, "the status is never lost");
                assert_eq!(code, "unknown");
                assert!(message.starts_with("upstream exploded: xxx"));
                assert_eq!(message.chars().count(), 500);
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn an_unreachable_brain_is_a_transport_error() {
        // Port 1 on loopback: nothing listens, nothing outside 127.0.0.1.
        let client = BrainClient::new("http://127.0.0.1:1").expect("valid url");
        let err = client.health().expect_err("unreachable");
        match err {
            ClientError::Transport { url, .. } => assert_eq!(url, "http://127.0.0.1:1/api/health"),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[test]
    fn the_client_and_subscriptions_cross_threads() {
        // The GPUI app runs every call on a background thread and the CLI
        // hands the client between commands: both need these bounds.
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<BrainClient>();
        assert_send_sync::<crate::LiveSubscription>();
        assert_send_sync::<ClientError>();
    }

    #[test]
    fn with_timeout_keeps_the_session_usable() {
        let stub = StubBrain::start();
        let client = stub_client(&stub).with_token("tok").with_timeout(5);
        client.login("one-time").expect("login");
        client.health().expect("health after timeout change");
        // The timeout change must not have dropped the login token or cookie.
        assert_eq!(client.live_token().as_deref(), Some("one-time"));
        let recorded = stub.last_request("GET", "/api/health").expect("recorded");
        assert_eq!(
            recorded.cookie.as_deref(),
            Some("swarmbot_session=stub-session")
        );
    }

    // --- usage --------------------------------------------------------------

    #[test]
    fn the_usage_window_is_sent_as_a_percent_encoded_rfc3339_value() {
        let client = BrainClient::new("http://127.0.0.1:9").expect("valid");
        let url = client
            .endpoint(
                &["api", "usage"],
                &[("since", "2026-01-31T00:00:00+00:00".to_string())],
            )
            .expect("endpoint");
        // The `:` and `+` of an RFC 3339 timestamp must survive the query
        // string; an unencoded `+` would arrive at the brain as a space.
        assert_eq!(
            url, "http://127.0.0.1:9/api/usage?since=2026-01-31T00%3A00%3A00%2B00%3A00",
            "{url}"
        );
        // No `since` means no query at all: the brain applies its own default.
        let url = client.endpoint(&["api", "usage"], &[]).expect("endpoint");
        assert_eq!(url, "http://127.0.0.1:9/api/usage", "{url}");
    }

    #[test]
    fn usage_asks_the_documented_path_and_keeps_a_404_typed() {
        // The stub serves no usage route on purpose: what this pins is the
        // request the client *sent*, and that a missing route is still a typed
        // `not_found` rather than a panic or a silent default.
        let stub = StubBrain::start();
        let client = stub_client(&stub);
        let err = client
            .usage(Some("2026-01-31T00:00:00Z"))
            .expect_err("the stub has no /api/usage");
        assert!(err.is_not_found(), "{err:?}");
        let request = stub
            .last_request("GET", "/api/usage")
            .expect("the client asked the documented path");
        assert_eq!(request.query, "since=2026-01-31T00%3A00%3A00Z");
    }

    #[test]
    fn a_usage_report_decodes_from_the_documented_body() {
        // The body the brain sends, byte for byte in shape: this is the test
        // that fails if the route's JSON and this client ever drift.
        let bot = BotId::new();
        let body = json!({
            "since": "2026-01-31T00:00:00Z",
            "until": "2026-02-01T00:00:00Z",
            "turns": 2,
            "totals": {"input_tokens": 120, "output_tokens": 30, "llm_calls": 2, "tool_calls": 1},
            "by_bot": [
                {
                    "bot_id": bot.as_str(),
                    "bot_name": "lead",
                    "usage": {"input_tokens": 120, "output_tokens": 30, "llm_calls": 2, "tool_calls": 1}
                }
            ]
        });
        let report: UsageReport =
            serde_json::from_value(body).expect("the documented body decodes");
        assert_eq!(report.totals.total_tokens(), 150);
        assert_eq!(report.turns, 2);
        assert_eq!(report.by_bot.len(), 1);
        assert_eq!(report.by_bot[0].bot_id, bot);
        assert_eq!(report.by_bot[0].bot_name.as_deref(), Some("lead"));
        assert_eq!(report.by_bot[0].usage.input_tokens, 120);
        // A deleted bot arrives without a name; the row (and its spend) stays.
        let nameless = json!({
            "since": "2026-01-31T00:00:00Z",
            "until": "2026-02-01T00:00:00Z",
            "turns": 1,
            "totals": {"input_tokens": 0, "output_tokens": 0, "llm_calls": 1, "tool_calls": 0},
            "by_bot": [{"bot_id": BotId::new().as_str(), "bot_name": null,
                        "usage": {"input_tokens": 0, "output_tokens": 0, "llm_calls": 1, "tool_calls": 0}}]
        });
        let report: UsageReport = serde_json::from_value(nameless).expect("a nameless bot decodes");
        assert_eq!(report.by_bot[0].bot_name, None);
        assert_eq!(report.totals.total_tokens(), 0);
        assert_eq!(report.turns, 1, "zeros with a turn count is not 'no work'");
    }
}
