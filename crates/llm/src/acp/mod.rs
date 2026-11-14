//! The **ACP harness adapter**: one [`LlmBackend`] for every agent that speaks
//! the [Agent Client Protocol](https://agentclientprotocol.com) — JSON-RPC 2.0
//! over the agent's stdio. One protocol, three configurations shipped in the
//! example config:
//!
//! * **opencode** — native: `command = "opencode"`, `args = ["acp"]`;
//! * **Claude Code** — through Zed's adapter: `command = "claude-code-acp"`
//!   (`npm i -g @zed-industries/claude-code-acp`, or `npx -y
//!   @zed-industries/claude-code-acp` as the command);
//! * **Codex** — through Zed's adapter: `command = "codex-acp"`
//!   (`npm i -g @zed-industries/codex-acp`).
//!
//! Nothing here is agent-specific: whatever answers `initialize` on stdio is a
//! backend. That is the point of ACP, and it is why this is one adapter and not
//! three.
//!
//! # How a turn maps onto ACP
//!
//! The runner hands `chat` the bot's **whole history** every turn and never
//! drives a tool loop against this backend (`tool_calls == false`,
//! `BackendKind::Acp.owns_tool_loop()`). So each turn:
//!
//! 1. reuses the agent *process* (spawned lazily, kept alive, respawned if it
//!    died) but creates a **fresh ACP session** — the history we render *is*
//!    the agent's context, exactly like the stateless HTTP backend, and a
//!    per-bot native session would double it. The turn releases that session
//!    when it ends (see "Sessions and processes" below);
//! 2. renders instructions + transcript into text content blocks
//!    ([`prompt_blocks`]);
//! 3. streams `session/update` notifications into [`LlmEvent`]s:
//!    `agent_message_chunk` → `TextDelta`, `agent_thought_chunk` →
//!    `ReasoningDelta`, and the agent's **own** `tool_call`s →
//!    [`LlmEvent::AgentToolCall`], which the runner transcribes as a tool-call
//!    row marked `origin = Agent` (D9.31). Deliberately **not**
//!    `ToolCallStart`: those events mean "the runner is about to run this
//!    through `ToolRegistry`", and an agent's call was decided by the agent, in
//!    the agent's sandbox, with no `PolicyEngine`, `AuditLog` or
//!    `NodeExecutor` of ours anywhere in the path. The event type is what keeps
//!    a consumer from executing what it was only told about;
//! 4. maps `stopReason` onto [`FinishReason`] and returns the accumulated text
//!    as the authoritative response.
//!
//! # One process, many turns: a timeout fails one turn, never the backend
//!
//! Every bot that names this backend shares one agent process per (pinned
//! model, bot workspace). That sharing is the reason for the invariant this
//! module holds everywhere: **a per-request timeout fails the turn that asked,
//! never the shared process.**
//!
//! Abandoning a request is safe by protocol, not by hope: JSON-RPC responses
//! are correlated by id and never ordered, so an unanswered `session/new`
//! desyncs nothing — the reader drops a late answer with a debug log
//! (`client.rs`). A setup call that runs out its budget is therefore dropped
//! and the connection keeps serving everybody else. One honest limit follows:
//! an abandoned `session/new` may leave a session inside the agent whose id we
//! never learned; it is released when the process exits (completed turns
//! release theirs with `session/close` — see below).
//!
//! A *run* of unanswered setup calls means the process is wedged, not the
//! turn: after [`MAX_CONSECUTIVE_SETUP_TIMEOUTS`] of them the next turn
//! replaces the connection lazily — the old process is shut down only once the
//! turns still holding it finish, so replacing it never kills somebody else's
//! turn. Connection *establishment* is serialised per connection key only and
//! is cancel-aware, so a bot's cold start never makes another bot's stopped
//! turn wait for it.
//!
//! # Sessions and processes: the stated limits
//!
//! A completed turn sends `session/close` — but only to an agent that
//! advertises `agentCapabilities.sessionCapabilities.close`. The method is
//! optional in ACP v1, and an unadvertised optional method is not ours to call:
//! for such an agent the per-turn session lives inside the process until the
//! process exits. There is **no idle reaper** in this build — a connection
//! lives until the backend is dropped, the agent dies, or a run of setup
//! timeouts replaces it. An agent that is merely idle keeps its process.
//!
//! # Tools, permissions, safety
//!
//! The client declares **no** fs/terminal capabilities, so the agent uses its
//! *own* tools under its *own* sandbox (Claude Code's permission system,
//! codex's sandbox, opencode's permission config). SwarmBot's policy engine
//! does not gate those actions — the intervention point is ACP's
//! `session/request_permission`, answered here by an [`AcpPermissions`]
//! implementation:
//!
//! * `options.permission_mode = "reject"` (**default**) refuses every
//!   permission ask: the agent can still read, think and answer, but changes
//!   nothing — a bot that says "I was not allowed to edit" is honest;
//! * `options.permission_mode = "allow"` selects the agent's *allow-once*
//!   option per action. This is an explicit operator opt-in in config, never a
//!   default and never silent; `*_always` options are never chosen, so no
//!   blanket grant outlives a turn (D5.2's rule, applied to a foreign agent).
//!
//! The brain can later surface owner cards by injecting its own
//! [`AcpPermissions`] (the seam mirrors `tools::ApprovalGate`; `llm` cannot
//! reach the approval machinery itself — dependency direction).
//!
//! Credentials: the child inherits the owner's environment (agents use the
//! owner's own login — nothing is read from claude.ai/ChatGPT stores and
//! nothing is forwarded anywhere), `options.env` adds explicit variables, and
//! when `api_key_env` names a key the registry resolved, it is injected into
//! the child's environment under that name — never into argv, never into a
//! log, never into `Debug`.
//!
//! # Model selection, honestly — and how a wish becomes a pin
//!
//! `model` is a **wish**: when the agent advertises the (unstable) session
//! model state we switch to it and fail the turn if the agent refuses; when it
//! does not, the agent uses its own configured model and a warning says so.
//! `probe` reports which of the two an agent is.
//!
//! For agents that stay silent about models (opencode), `options.model_env`
//! turns the wish into a **pin**: env templates such as
//! `OPENCODE_CONFIG_CONTENT = '{"model":"{model}"}'` are substituted with the
//! wished model and injected into the child at spawn. Environments are born,
//! not changed, so each pinned model gets its own connection (and process),
//! keyed by the model id — switching models in the menu respawns the agent
//! pinned to the new one, which is what makes "gonta-ganti model" actually
//! true for opencode. With `model_env` configured the menu's list is
//! authoritative again and the warning disappears.

mod client;
mod proto;

pub use client::{AcpTransport, AgentHandle, AgentLaunch, AgentProcess, ProcessTransport};
pub use proto::{AuthMethod, PROTOCOL_VERSION, PermissionOption, PermissionOptionKind};

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use swarmbot_contracts::{BackendKind, BackendSpec, Usage};

use crate::backend::{LlmBackend, LlmError, emit};
use crate::openai::truncate_utf8;
use crate::types::{
    BackendStatus, Capabilities, ChatMessage, ChatRequest, ChatResponse, FinishReason, LlmEvent,
    McpServerSpec, ModelList, ModelOption, Role,
};

use self::client::{AcpRequestError, Connection, SCRUBBED_ENV};
use self::proto::{McpCapabilities, NewSessionResult, PermissionRequest, PromptResult, StopReason};

/// How long the agent gets to answer a *setup* call (`initialize`,
/// `session/new`, `session/set_model`). Derived from `timeout_secs`, with a
/// floor: a cold `npx` wrapper can take a while to boot even when turns are
/// expected to be quick.
fn setup_timeout(spec: &BackendSpec) -> Duration {
    Duration::from_secs(spec.timeout_secs.clamp(30, 600))
}

/// After the owner stops a turn we tell the agent `session/cancel` and give it
/// this long to acknowledge before we walk away. Bounded, so a stuck agent
/// cannot hold the stop.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// How long a finished turn waits for `session/close`. Bounded because the
/// close is a courtesy the next turn must not pay for: an agent that does not
/// answer it keeps the session until it exits, and the turn's result is already
/// decided when we send it.
const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// A connection whose setup calls went unanswered this many times *in a row*
/// is replaced on the next turn. One unanswered call is a slow turn (the
/// timeout fails only that turn); a run of them is a wedged process, and
/// replacing it lazily — once, for future turns — is the only reaping this
/// build does.
const MAX_CONSECUTIVE_SETUP_TIMEOUTS: u32 = 3;

/// The probe's own setup budget. `probe_all` runs every backend in parallel
/// behind one HTTP call whose client gives it two minutes; a single agent that
/// hangs in `session/new` (opencode's watcher scan, observed on this very
/// machine) must not eat that whole budget and take the other backends'
/// results down with it. Healthy agents answer in seconds, so twenty is
/// generous — and a hung one fails here with an honest row instead of a
/// client-side "timeout: global" that names no backend.
const PROBE_BUDGET: Duration = Duration::from_secs(20);

/// How long a configured `models_command` lister may run. It is a plain CLI
/// call (`opencode models` answers in seconds); a lister that hangs must not
/// hang the menu that asked for it.
const MODELS_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// A model lister's stdout is capped so a runaway list cannot become a
/// runaway menu.
const MAX_LISTED_MODELS: usize = 500;

/// The setup budget a probe may spend: the spec's, capped by [`PROBE_BUDGET`].
pub(crate) fn probe_budget(spec: &BackendSpec) -> Duration {
    setup_timeout(spec).min(PROBE_BUDGET)
}

/// What one bounded setup call produced, before it is mapped to an
/// [`LlmError`]. Kept separate from `LlmError` because the callers disagree on
/// what each outcome means: a failed `session/new` is fatal, a missing optional
/// `session/set_model` is a warning, and a timeout is fatal *for the turn* but
/// must not touch the shared connection.
enum SetupReply {
    Answered(Result<Value, AcpRequestError>),
    TimedOut,
    Cancelled,
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

/// One permission question from the agent, lifted out of the ACP
/// `session/request_permission` payload.
#[derive(Clone, Debug)]
pub struct AcpPermissionAsk {
    pub session_id: String,
    pub tool_call_id: String,
    /// The agent's human-readable description of the action ("Edit src/main.rs").
    pub title: String,
    /// ACP tool kind (`read`, `edit`, `execute`, …), when the agent set one.
    pub kind: Option<String>,
    /// The tool's raw input, when the agent shared it. May be large; handlers
    /// should truncate before rendering.
    pub raw_input: Option<Value>,
    /// The answers the agent offers. A decision must name one of these ids
    /// (or cancel).
    pub options: Vec<PermissionOption>,
}

impl AcpPermissionAsk {
    pub(crate) fn from_protocol(request: PermissionRequest) -> Self {
        let tool_call = &request.tool_call;
        Self {
            session_id: request.session_id,
            tool_call_id: tool_call
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            title: tool_call
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("(untitled action)")
                .to_string(),
            kind: tool_call
                .get("kind")
                .and_then(Value::as_str)
                .map(str::to_string),
            raw_input: tool_call.get("rawInput").cloned(),
            options: request.options,
        }
    }
}

/// The answer to an [`AcpPermissionAsk`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcpPermissionDecision {
    /// Choose one of the offered options, by its id.
    Select(String),
    /// "The user dismissed this" — the agent's own cancelled path.
    Cancel,
}

/// Decides the agent's permission asks. The default implementation is
/// [`FixedPermissionPolicy`] from `options.permission_mode`; the brain can
/// inject its own (owner cards, rules) through [`AcpAgent::with_permissions`].
///
/// Contract: `decide` **must answer** — hanging here hangs the agent's turn.
#[async_trait::async_trait]
pub trait AcpPermissions: Send + Sync {
    async fn decide(&self, ask: &AcpPermissionAsk) -> AcpPermissionDecision;
}

/// The configured permission stance of one ACP backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionMode {
    /// Refuse every ask (default). The agent stays read-only from our side of
    /// the socket; whatever its own sandbox allows without asking still runs.
    Reject,
    /// Allow each ask once. Explicit operator opt-in; never `*_always`.
    Allow,
}

impl PermissionMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "reject" => Ok(Self::Reject),
            "allow" => Ok(Self::Allow),
            other => Err(format!(
                "unknown permission_mode `{other}` (expected \"reject\" or \"allow\")"
            )),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::Allow => "allow",
        }
    }
}

/// Answer every ask the same configured way, always picking a `*_once` option:
/// an `*_always` grant would outlive the turn and this build keeps no record
/// of it — a blanket approval nobody wrote down is exactly what the safety
/// rules forbid.
#[derive(Clone, Copy, Debug)]
pub struct FixedPermissionPolicy {
    pub mode: PermissionMode,
}

#[async_trait::async_trait]
impl AcpPermissions for FixedPermissionPolicy {
    async fn decide(&self, ask: &AcpPermissionAsk) -> AcpPermissionDecision {
        let wanted = match self.mode {
            PermissionMode::Reject => PermissionOptionKind::RejectOnce,
            PermissionMode::Allow => PermissionOptionKind::AllowOnce,
        };
        ask.options
            .iter()
            .find(|option| option.kind == wanted)
            .map_or(AcpPermissionDecision::Cancel, |option| {
                AcpPermissionDecision::Select(option.option_id.clone())
            })
    }
}

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

/// The ACP agent adapter. One instance per configured backend, shared by
/// every bot that names it: the agent *process* is kept alive and reused,
/// while each turn gets its own ACP *session* (see the module docs).
pub struct AcpAgent {
    spec: BackendSpec,
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    extra_env: BTreeMap<String, String>,
    inherit_env: bool,
    /// Optional fallback model lister: the agent's own CLI (e.g.
    /// `["opencode", "models"]`) for agents whose ACP server advertises no
    /// model state. `None` = no list, and the menu says so.
    models_command: Option<Vec<String>>,
    /// Env templates that pin the model inside the child itself (the only
    /// channel agents without ACP model selection understand), e.g.
    /// `OPENCODE_CONFIG_CONTENT = '{"model":"{model}"}'`. `{model}` is
    /// replaced per connection; entries without it pass through verbatim.
    /// Configuring this makes model picks authoritative: one connection per
    /// model, spawned with the substituted env.
    model_env: BTreeMap<String, String>,
    /// The env var name the key is injected under (from `api_key_env`).
    key_env: Option<String>,
    /// Resolved by the registry (secrets store, then process env). Lives only
    /// here and in the child's environment; no `Debug`, log or error renders it.
    key: Option<String>,
    permissions: Arc<dyn AcpPermissions>,
    /// The configured mode, while the policy is still the built-in one;
    /// `with_permissions` clears it — a custom decider has no single label.
    mode: Option<PermissionMode>,
    transport: Arc<dyn AcpTransport>,
    /// Live connections keyed by the model pinned into the child's env
    /// (empty key = the agent's own default). One process per model, because
    /// an env-pinned model cannot change inside a running child.
    conn: tokio::sync::Mutex<HashMap<String, Arc<Connection>>>,
    /// One establishment gate per connection key. Held across a spawn and
    /// handshake so two turns for the same key cannot start two agents — but
    /// **only** per key, and only by the turn doing the connecting: the map
    /// lock above is never held across a handshake, so a bot's cold `npx` boot
    /// cannot make another bot's turn wait.
    connect_gates: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl fmt::Debug for AcpAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Manual: `key` and `extra_env` values are secret-bearing. Names and
        // the command line are all a log line may see.
        f.debug_struct("AcpAgent")
            .field("id", &self.spec.id)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field("permission_mode", &self.policy_mode_label())
            .field("inherit_env", &self.inherit_env)
            .field("env_names", &self.extra_env.keys().collect::<Vec<_>>())
            .field("api_key_env", &self.key_env)
            .field("has_key", &self.key.is_some())
            .finish()
    }
}

impl AcpAgent {
    /// Build from a spec with the real process transport. `key` is the
    /// registry-resolved API key, when the spec named an `api_key_env`.
    pub fn from_spec(spec: BackendSpec, key: Option<String>) -> Result<Self, LlmError> {
        Self::with_transport(spec, key, Arc::new(ProcessTransport))
    }

    /// Build with an injected transport — tests run the whole protocol
    /// against in-memory agents, no process and no network.
    pub fn with_transport(
        spec: BackendSpec,
        key: Option<String>,
        transport: Arc<dyn AcpTransport>,
    ) -> Result<Self, LlmError> {
        let invalid = |reason: String| LlmError::Unavailable {
            backend: spec.id.clone(),
            reason,
        };
        if !matches!(spec.kind, BackendKind::Acp) {
            return Err(invalid(format!("spec kind {:?} is not acp", spec.kind)));
        }
        let command = spec
            .option_str("command")
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .ok_or_else(|| {
                invalid(
                    "acp needs options.command (e.g. command = \"opencode\", args = [\"acp\"])"
                        .to_string(),
                )
            })?
            .to_string();
        let args = match spec.options.get("args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                let mut args = Vec::with_capacity(items.len());
                for item in items {
                    match item.as_str() {
                        Some(arg) => args.push(arg.to_string()),
                        None => {
                            return Err(invalid(
                                "options.args must be an array of strings".to_string(),
                            ));
                        }
                    }
                }
                args
            }
            Some(_) => {
                return Err(invalid(
                    "options.args must be an array of strings".to_string(),
                ));
            }
        };
        let cwd = match spec.option_str("cwd") {
            Some(configured) => {
                let path = PathBuf::from(configured);
                if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir()
                        .map_err(|err| {
                            invalid(format!("cannot resolve a relative options.cwd: {err}"))
                        })?
                        .join(path)
                }
            }
            None => std::env::current_dir().map_err(|err| {
                invalid(format!(
                    "cannot determine the working directory ({err}); set options.cwd"
                ))
            })?,
        };
        let mode = match spec.options.get("permission_mode") {
            None | Some(Value::Null) => PermissionMode::Reject,
            Some(Value::String(value)) => PermissionMode::parse(value).map_err(invalid)?,
            Some(_) => {
                return Err(invalid(
                    "options.permission_mode must be a string".to_string(),
                ));
            }
        };
        let mut extra_env = BTreeMap::new();
        match spec.options.get("env") {
            None | Some(Value::Null) => {}
            Some(Value::Object(entries)) => {
                for (name, value) in entries {
                    match value.as_str() {
                        Some(text) => {
                            extra_env.insert(name.clone(), text.to_string());
                        }
                        None => {
                            return Err(invalid(format!(
                                "options.env.{name} must be a string (values are never logged)"
                            )));
                        }
                    }
                }
            }
            Some(_) => return Err(invalid("options.env must be an object".to_string())),
        }
        let inherit_env = match spec.options.get("inherit_env") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(flag)) => *flag,
            Some(_) => return Err(invalid("options.inherit_env must be a boolean".to_string())),
        };
        let model_env = match spec.options.get("model_env") {
            None | Some(Value::Null) => BTreeMap::new(),
            Some(Value::Object(entries)) => {
                let mut out = BTreeMap::new();
                for (name, value) in entries {
                    match value.as_str() {
                        Some(template) => {
                            out.insert(name.clone(), template.to_string());
                        }
                        None => {
                            return Err(invalid(format!(
                                "options.model_env.{name} must be a string template"
                            )));
                        }
                    }
                }
                out
            }
            Some(_) => {
                return Err(invalid(
                    "options.model_env must be an object of string templates".to_string(),
                ));
            }
        };
        let models_command = match spec.options.get("models_command") {
            None | Some(Value::Null) => None,
            Some(Value::String(command)) if !command.trim().is_empty() => {
                Some(vec![command.trim().to_string()])
            }
            Some(Value::Array(items)) if !items.is_empty() => {
                let mut argv = Vec::with_capacity(items.len());
                for item in items {
                    match item.as_str() {
                        Some(arg) => argv.push(arg.to_string()),
                        None => {
                            return Err(invalid(
                                "options.models_command must be an array of strings".to_string(),
                            ));
                        }
                    }
                }
                Some(argv)
            }
            Some(_) => {
                return Err(invalid(
                    "options.models_command must be a string or a non-empty array of strings"
                        .to_string(),
                ));
            }
        };

        Ok(Self {
            key_env: spec.api_key_env.clone(),
            spec,
            command,
            args,
            cwd,
            extra_env,
            inherit_env,
            models_command,
            model_env,
            key,
            permissions: Arc::new(FixedPermissionPolicy { mode }),
            mode: Some(mode),
            transport,
            conn: tokio::sync::Mutex::new(HashMap::new()),
            connect_gates: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Inject a custom permission decider (the brain's seam to owner cards).
    #[must_use]
    pub fn with_permissions(mut self, permissions: Arc<dyn AcpPermissions>) -> Self {
        self.permissions = permissions;
        self.mode = None;
        self
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    pub fn permission_mode(&self) -> Option<PermissionMode> {
        self.mode
    }

    /// The configured fallback model lister, when any.
    pub fn models_command(&self) -> Option<&[String]> {
        self.models_command.as_deref()
    }

    fn policy_mode_label(&self) -> String {
        self.permission_mode().map_or_else(
            || "custom handler".to_string(),
            |mode| mode.label().to_string(),
        )
    }

    /// The child environment additions: `options.env` plus the resolved API
    /// key under the name the spec chose — the same channel every agent CLI
    /// documents, and the only one that keeps the key out of argv and logs.
    /// Shared by the agent spawn and the `models_command` lister.
    fn child_env(&self) -> BTreeMap<String, String> {
        let mut env = self.extra_env.clone();
        if let (Some(name), Some(key)) = (&self.key_env, &self.key) {
            env.insert(name.clone(), key.clone());
        }
        env
    }

    /// The `model_env` templates with `{model}` substituted (entries that
    /// need a model but have none are dropped: an unpinned connection runs
    /// the agent's own default).
    fn substituted_model_env(&self, model: Option<&str>) -> BTreeMap<String, String> {
        self.model_env
            .iter()
            .filter_map(
                |(name, template)| match template.split("{model}").collect::<Vec<_>>() {
                    parts if parts.len() == 1 => Some((name.clone(), template.clone())),
                    parts => {
                        let model = model?;
                        let mut value = String::new();
                        for (i, part) in parts.iter().enumerate() {
                            if i > 0 {
                                value.push_str(model);
                            }
                            value.push_str(part);
                        }
                        Some((name.clone(), value))
                    }
                },
            )
            .collect()
    }

    fn session_cwd(&self, request: &ChatRequest) -> PathBuf {
        match &request.cwd {
            Some(dir) if dir.is_absolute() => dir.clone(),
            Some(dir) => self.cwd.join(dir),
            None => self.cwd.clone(),
        }
    }

    fn launch_with(&self, extra_env: BTreeMap<String, String>, cwd: PathBuf) -> AgentLaunch {
        let mut env = self.child_env();
        env.extend(extra_env);
        AgentLaunch {
            command: self.command.clone(),
            args: self.args.clone(),
            env,
            cwd: Some(cwd),
            inherit_env: self.inherit_env,
        }
    }

    fn launch(&self) -> AgentLaunch {
        self.launch_with(self.substituted_model_env(None), self.cwd.clone())
    }

    /// The live connection for one env-pin key, spawning (and handshaking) on
    /// first use or after the agent died or was replaced as wedged.
    ///
    /// Three rules, each of them a bug that used to exist:
    ///
    /// 1. the slot map's lock is held only to read or replace an entry, never
    ///    across a spawn or handshake;
    /// 2. establishment is serialised **per key** — two turns for the same
    ///    workspace still share one agent, but a different key never waits;
    /// 3. waiting for that gate and for the connect itself both select on
    ///    `cancel`, so a stopped turn leaves the queue immediately instead of
    ///    waiting out somebody else's `setup_timeout × (max_retries + 1)`.
    async fn connection_for(
        &self,
        key: &str,
        cwd: PathBuf,
        extra_env: BTreeMap<String, String>,
        cancel: &CancellationToken,
    ) -> Result<Arc<Connection>, LlmError> {
        // The common case: an established, healthy connection. It must not
        // queue behind anybody's cold start.
        if let Some(conn) = self.live_connection(key).await {
            return Ok(conn);
        }
        let gate = self.connect_gate(key).await;
        let _connecting = tokio::select! {
            guard = gate.lock_owned() => guard,
            _ = cancel.cancelled() => return Err(LlmError::Cancelled),
        };
        // Somebody may have finished connecting while this turn waited for the
        // gate; adopting their connection is the whole point of the gate.
        if let Some(conn) = self.live_connection(key).await {
            return Ok(conn);
        }
        let launch = self.launch_with(extra_env, cwd);
        let timeout = setup_timeout(&self.spec);
        let mut attempt = 0u32;
        loop {
            let connect = Connection::connect(
                self.spec.id.clone(),
                &self.transport,
                &launch,
                Arc::clone(&self.permissions),
                timeout,
            );
            let result = tokio::select! {
                result = connect => result,
                _ = cancel.cancelled() => return Err(LlmError::Cancelled),
            };
            match result {
                Ok(conn) => {
                    self.conn
                        .lock()
                        .await
                        .insert(key.to_string(), Arc::clone(&conn));
                    return Ok(conn);
                }
                Err(error) => {
                    // Only *establishment* is retried. A failure mid-turn is
                    // not: the agent may already have run tools, and rerunning
                    // a half-finished turn is not this layer's call.
                    if error.retryable() && attempt < self.spec.max_retries {
                        attempt += 1;
                        let backoff =
                            Duration::from_millis(250u64.saturating_mul(1 << attempt.min(4)));
                        tracing::warn!(
                            backend = %self.spec.id,
                            attempt,
                            error = %error,
                            "ACP agent connection failed; backing off and retrying"
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => {}
                            _ = cancel.cancelled() => return Err(LlmError::Cancelled),
                        }
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }

    /// The stored connection for `key`, when it is still usable. A dead one, or
    /// one that has run out its setup-timeout budget, is removed here: that is
    /// the lazy reap — the process itself is only shut down when the last turn
    /// still holding it finishes (`Connection::drop`), so replacing a wedged
    /// agent cannot kill a turn that is still running.
    async fn live_connection(&self, key: &str) -> Option<Arc<Connection>> {
        let mut slots = self.conn.lock().await;
        match slots.get(key) {
            Some(conn)
                if conn.alive() && conn.setup_timeouts() < MAX_CONSECUTIVE_SETUP_TIMEOUTS =>
            {
                Some(Arc::clone(conn))
            }
            Some(conn) => {
                if conn.alive() {
                    tracing::warn!(
                        backend = %self.spec.id,
                        setup_timeouts = conn.setup_timeouts(),
                        "ACP agent has not answered setup calls; replacing it for the next turn"
                    );
                }
                slots.remove(key);
                None
            }
            None => None,
        }
    }

    /// The per-key establishment gate, created on first use. The outer lock is
    /// taken only to fetch or insert the gate and is never held across the
    /// handshake.
    async fn connect_gate(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self.connect_gates.lock().await;
        Arc::clone(
            gates
                .entry(key.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    /// Fold one `session/update` notification into the running turn: stream
    /// the event, accumulate the authoritative text. `Err(Cancelled)` means
    /// the event consumer walked away (turn aborted, UI closed) — the agent
    /// has been told to stop and the caller ends the turn.
    async fn apply_update(
        &self,
        conn: &Connection,
        session_id: &str,
        params: Value,
        events: &mpsc::Sender<LlmEvent>,
        text: &mut String,
        reasoning: &mut String,
    ) -> Result<(), LlmError> {
        let update = params.get("update").cloned().unwrap_or(Value::Null);
        match proto::classify_update(&update) {
            proto::SessionUpdate::AgentMessage(chunk) => {
                text.push_str(&chunk);
                if emit(events, LlmEvent::TextDelta(chunk)).await.is_err() {
                    conn.notify("session/cancel", json!({"sessionId": session_id}));
                    return Err(LlmError::Cancelled);
                }
            }
            proto::SessionUpdate::AgentThought(chunk) => {
                reasoning.push_str(&chunk);
                if emit(events, LlmEvent::ReasoningDelta(chunk)).await.is_err() {
                    conn.notify("session/cancel", json!({"sessionId": session_id}));
                    return Err(LlmError::Cancelled);
                }
            }
            proto::SessionUpdate::ToolCall(update) => {
                let proto::ToolCallUpdate {
                    announced,
                    tool_call_id,
                    title,
                    name,
                    kind,
                    status,
                    raw_input,
                    content_text,
                    locations,
                } = *update;
                // The agent's own tool activity. Emitted as `AgentToolCall`
                // (never as `ToolCallStart`, which would mean "this runner is
                // about to run it") so the transcript can show it without
                // implying any policy, approval or audit of ours was involved.
                // The state is announced here and merged by the consumer: one
                // call arrives as several of these.
                let state = crate::types::AgentToolState::from_acp(status.as_deref());
                // The tool's identity, not its current display text: the
                // announcement's `name` (`shell`) wins, and an update's
                // `title` (which becomes the *command*) is only a fallback for
                // agents that never send a `name`. Renaming a row mid-turn
                // from `shell` to `ls crates | wc -l` is what this avoids.
                let tool = tool_identity(announced, name.as_deref(), &title, kind.as_deref());
                // Passed through as the agent sent them: no fallback here,
                // because only the consumer that merges several notifications
                // knows whether an earlier one already carried the real
                // arguments (see `AgentToolCall::args`).
                let args = raw_input;
                tracing::debug!(
                    backend = %self.spec.id,
                    tool_call_id,
                    tool = %tool,
                    state = ?state,
                    "ACP agent tool call"
                );
                if emit(
                    events,
                    LlmEvent::AgentToolCall(Box::new(crate::types::AgentToolCall {
                        id: tool_call_id,
                        tool,
                        args,
                        locations,
                        state,
                        result_text: content_text,
                    })),
                )
                .await
                .is_err()
                {
                    conn.notify("session/cancel", json!({"sessionId": session_id}));
                    return Err(LlmError::Cancelled);
                }
            }
            proto::SessionUpdate::Ignored(reason) => {
                tracing::debug!(backend = %self.spec.id, reason, "ACP session update ignored");
            }
        }
        Ok(())
    }

    /// One setup-style RPC under a budget, before it is mapped to an
    /// [`LlmError`]: callers differ on what a timeout or a refusal means for
    /// them (`session/new` is fatal, a missing `session/set_model` is not).
    ///
    /// A timeout here **abandons the request and leaves the connection
    /// alone** — see the module docs' invariant. Nothing on this wire depends
    /// on request order, so a late answer for an abandoned id is dropped by the
    /// reader; the next turn (or this one, after a refusal) can use the same
    /// process.
    async fn setup_reply(
        &self,
        conn: &Connection,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> SetupReply {
        let result = tokio::select! {
            result = conn.rpc(method, params) => result,
            _ = cancel.cancelled() => return SetupReply::Cancelled,
            _ = tokio::time::sleep(timeout) => {
                conn.note_setup_timeout();
                return SetupReply::TimedOut;
            }
        };
        if result.is_ok() {
            // The process answered: any run of timeouts is over, so it is not
            // replaced as wedged on a later turn.
            conn.note_setup_ok();
        }
        SetupReply::Answered(result)
    }

    /// One setup-style RPC whose failure is the turn's: bounded by `timeout` and
    /// the owner's stop. A timeout fails this turn only — it must not touch the
    /// shared connection.
    async fn setup_rpc(
        &self,
        conn: &Connection,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value, LlmError> {
        match self
            .setup_reply(conn, method, params, timeout, cancel)
            .await
        {
            SetupReply::Cancelled => Err(LlmError::Cancelled),
            SetupReply::TimedOut => Err(LlmError::Timeout {
                secs: timeout.as_secs(),
            }),
            SetupReply::Answered(Ok(value)) => Ok(value),
            SetupReply::Answered(Err(error)) => {
                let mut mapped = conn.map_error(method, error.clone());
                // A refused session/new is where a missing login surfaces; the
                // agent's own error text plus its advertised auth methods is the
                // most honest message we can build.
                if method == "session/new"
                    && matches!(error, AcpRequestError::Rpc(_))
                    && let LlmError::Unavailable { reason, .. } = &mut mapped
                    && let Some(hint) = conn.auth_hint()
                {
                    reason.push_str(" — ");
                    reason.push_str(&hint);
                }
                Err(mapped)
            }
        }
    }

    /// Release one turn's ACP session inside the long-lived agent process.
    ///
    /// The protocol's release is `session/close`, and it is **optional**: this
    /// build sends it only to an agent that advertised
    /// `agentCapabilities.sessionCapabilities.close`. For an agent that did
    /// not, the session stays in the process until the process exits — the
    /// stated limit in the module docs, not a silent omission.
    ///
    /// Never fails the turn: the turn's result is already decided, so a close
    /// that errors or hangs is logged and the next turn reuses the process
    /// either way.
    async fn close_session(&self, conn: &Connection, session_id: &str) {
        if !conn.supports_session_close() {
            tracing::debug!(
                backend = %self.spec.id,
                session = %session_id,
                "the agent does not advertise session/close; the session stays in the process until it exits"
            );
            return;
        }
        if !conn.alive() {
            // Nothing to close: the process is gone, which releases everything.
            return;
        }
        let close = conn.rpc("session/close", json!({"sessionId": session_id}));
        match tokio::time::timeout(CLOSE_GRACE, close).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(
                backend = %self.spec.id,
                session = %session_id,
                error = %conn.map_error("session/close", error),
                "session/close failed; the agent keeps this session until it exits"
            ),
            Err(_) => tracing::warn!(
                backend = %self.spec.id,
                session = %session_id,
                secs = CLOSE_GRACE.as_secs(),
                "session/close went unanswered; the agent keeps this session until it exits"
            ),
        }
    }
}

#[async_trait]
impl LlmBackend for AcpAgent {
    fn id(&self) -> &str {
        &self.spec.id
    }

    fn kind(&self) -> BackendKind {
        BackendKind::Acp
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // agent_message_chunk flows as it arrives.
            streaming: true,
            // The load-bearing one: the agent runs its own tool loop with its
            // own tools; the runner must not drive one.
            tool_calls: false,
            // agent_thought_chunk → ReasoningDelta.
            reasoning: true,
            // A turn can be cancelled but not fed mid-flight in this build.
            steer: false,
            // No native resume: each turn is a fresh ACP session, and the
            // bot's history in our store *is* the memory — like the HTTP
            // backend, there is nothing to survive.
            survives_restart: false,
        }
    }

    async fn chat(
        &self,
        request: ChatRequest,
        events: mpsc::Sender<LlmEvent>,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, LlmError> {
        if self.key.is_none() && self.key_env.is_some() {
            // Fail before spawning anything we know is unauthorized.
            return Err(LlmError::MissingKey {
                env: self.key_env.clone().unwrap_or_default(),
            });
        }
        // Resolve the wished model and per-bot workspace *before* connecting:
        // when `model_env` is configured or a bot supplies its own `cwd`, each
        // distinct `(model, cwd)` gets its own connection (and agent process)
        // started directly inside that workspace.
        let cwd = self.session_cwd(&request);
        let desired = desired_model(&request, self.spec.model.as_deref());
        let env_key = if self.model_env.is_empty() {
            String::new()
        } else {
            desired.clone().unwrap_or_default()
        };
        let conn_key = if request.cwd.is_some() {
            format!("{env_key}\n{}", cwd.display())
        } else {
            env_key
        };
        let conn = self
            .connection_for(
                &conn_key,
                cwd.clone(),
                self.substituted_model_env(desired.as_deref()),
                &cancel,
            )
            .await?;
        let idle = Duration::from_secs(self.spec.timeout_secs.max(1));
        let setup = setup_timeout(&self.spec);

        // 1. A fresh session per turn (module docs: the rendered history *is*
        //    the context; a reused session would double it).
        let mcp_servers =
            acp_mcp_servers(&self.spec.id, &request.mcp_servers, conn.mcp_capabilities());
        let new_params = json!({
            "cwd": cwd.to_string_lossy(),
            "mcpServers": mcp_servers,
        });
        let session_value = self
            .setup_rpc(&conn, "session/new", new_params, setup, &cancel)
            .await?;
        let session: NewSessionResult =
            serde_json::from_value(session_value).map_err(|err| LlmError::Malformed {
                backend: self.spec.id.clone(),
                reason: format!("unreadable `session/new` result: {err}"),
            })?;
        let session_id = session.session_id;

        // From here on the session exists inside the agent, so every exit path
        // — a refusal, a timeout, a stop, a full answer — must release it. The
        // block makes that structural: no `?` below can skip the close.
        let outcome: Result<ChatResponse, LlmError> = async {
        // 2. Model selection, honest channels in order of strength:
        //    (a) `model_env` pinned the model into this connection's child at
        //    spawn — nothing left to negotiate; (b) the agent advertises the
        //    unstable ACP model state — `session/set_model`, and a refusal is
        //    an error, never a silent swap; (c) the agent advertises
        //    `configOptions` with `id == "model"` (e.g. OpenCode) —
        //    `session/set_config_option`; (d) neither — the agent keeps its
        //    own configured model and the log says so.
        if let Some(desired) = desired {
            if !self.model_env.is_empty() {
                // Channel (a): the child was spawned pinned to `desired`.
            } else if let Some(models) = &session.models {
                if models.current != desired {
                    if models
                        .available
                        .iter()
                        .any(|model| model.model_id == desired)
                    {
                        let result = self
                            .setup_reply(
                                &conn,
                                "session/set_model",
                                json!({"sessionId": session_id, "modelId": desired}),
                                setup,
                                &cancel,
                            )
                            .await;
                        match result {
                            SetupReply::Answered(Ok(_)) => {}
                            SetupReply::Answered(Err(AcpRequestError::Rpc(rpc)))
                                if rpc.is_method_not_found() =>
                            {
                                tracing::warn!(
                                    backend = %self.spec.id,
                                    model = %desired,
                                    "the agent advertised models but has no session/set_model; it keeps `{}`",
                                    models.current
                                );
                            }
                            SetupReply::Answered(Err(error)) => {
                                return Err(conn.map_error("session/set_model", error));
                            }
                            // A timeout fails this turn only: the connection is
                            // shared, and abandoning the request desyncs nothing.
                            SetupReply::TimedOut => {
                                return Err(LlmError::Timeout {
                                    secs: setup.as_secs(),
                                });
                            }
                            SetupReply::Cancelled => return Err(LlmError::Cancelled),
                        }
                    } else {
                        let available: Vec<&str> = models
                            .available
                            .iter()
                            .map(|model| model.model_id.as_str())
                            .collect();
                        return Err(LlmError::Unavailable {
                            backend: self.spec.id.clone(),
                            reason: format!(
                                "agent `{}` does not offer model `{desired}` (available: {})",
                                conn.label(),
                                if available.is_empty() {
                                    "none advertised".to_string()
                                } else {
                                    available.join(", ")
                                }
                            ),
                        });
                    }
                }
            } else if let Some(model_opt) =
                session.config_options.iter().find(|opt| opt.id == "model")
            {
                let current = model_opt.current_value_str();
                if current != Some(&desired) {
                    let matches_available = model_opt.options.is_empty()
                        || model_opt
                            .options
                            .iter()
                            .any(|choice| choice.value == desired);
                    if matches_available {
                        let result = self
                            .setup_reply(
                                &conn,
                                "session/set_config_option",
                                json!({
                                    "sessionId": session_id,
                                    "configId": "model",
                                    "value": desired,
                                }),
                                setup,
                                &cancel,
                            )
                            .await;
                        match result {
                            SetupReply::Answered(Ok(_)) => {}
                            SetupReply::Answered(Err(AcpRequestError::Rpc(rpc)))
                                if rpc.is_method_not_found() =>
                            {
                                tracing::warn!(
                                    backend = %self.spec.id,
                                    model = %desired,
                                    "the agent advertised configOptions but has no session/set_config_option; it keeps `{}`",
                                    current.unwrap_or("(default)")
                                );
                            }
                            SetupReply::Answered(Err(error)) => {
                                return Err(conn.map_error("session/set_config_option", error));
                            }
                            // Same rule as session/set_model: one turn's timeout,
                            // never the shared process.
                            SetupReply::TimedOut => {
                                return Err(LlmError::Timeout {
                                    secs: setup.as_secs(),
                                });
                            }
                            SetupReply::Cancelled => return Err(LlmError::Cancelled),
                        }
                    } else {
                        let available: Vec<&str> = model_opt
                            .options
                            .iter()
                            .map(|choice| choice.value.as_str())
                            .collect();
                        return Err(LlmError::Unavailable {
                            backend: self.spec.id.clone(),
                            reason: format!(
                                "agent `{}` does not offer model `{desired}` (available: {})",
                                conn.label(),
                                if available.is_empty() {
                                    "none advertised".to_string()
                                } else {
                                    available.join(", ")
                                }
                            ),
                        });
                    }
                }
            } else {
                tracing::warn!(
                    backend = %self.spec.id,
                    model = %desired,
                    "the agent does not advertise model selection over ACP; it uses its own configured model"
                );
            }
        }

        // 3. The prompt turn: stream updates until the agent answers, the
        //    idle timeout fires, or the owner stops.
        let mut updates = conn.subscribe(&session_id);
        let prompt_future = conn.rpc(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": prompt_blocks(&request)}),
        );
        tokio::pin!(prompt_future);
        let sleeper = tokio::time::sleep(idle);
        tokio::pin!(sleeper);

        let mut text = String::new();
        let mut reasoning = String::new();
        let cancel_params = || json!({"sessionId": session_id});

        let stop: Result<StopReason, LlmError> = loop {
            tokio::select! {
                out = &mut prompt_future => {
                    // The final result can overtake updates still queued in
                    // the channel (the reader feeds both, and select! picks
                    // randomly between ready branches): drain the queue
                    // first, or the last chunks of the answer are dropped on
                    // the floor.
                    let mut drained = Ok(());
                    while let Ok(params) = updates.try_recv() {
                        if let Err(error) = self
                            .apply_update(&conn, &session_id, params, &events, &mut text, &mut reasoning)
                            .await
                        {
                            drained = Err(error);
                            break;
                        }
                    }
                    if let Err(error) = drained {
                        break Err(error);
                    }
                    break match out {
                        Ok(value) => serde_json::from_value::<PromptResult>(value)
                            .map(|result| result.stop_reason)
                            .map_err(|err| LlmError::Malformed {
                                backend: self.spec.id.clone(),
                                reason: format!("unreadable `session/prompt` result: {err}"),
                            }),
                        Err(AcpRequestError::Rpc(rpc)) => Err(conn.map_error("session/prompt", AcpRequestError::Rpc(rpc))),
                        Err(AcpRequestError::Exited(detail)) => Err(LlmError::Unavailable {
                            backend: self.spec.id.clone(),
                            reason: format!(
                                "agent `{}` died mid-turn ({detail}); its tools may have partly run — this turn is not retried automatically",
                                conn.label()
                            ),
                        }),
                    };
                }
                maybe_update = updates.recv() => match maybe_update {
                    Some(params) => {
                        if let Err(error) = self
                            .apply_update(&conn, &session_id, params, &events, &mut text, &mut reasoning)
                            .await
                        {
                            break Err(error);
                        }
                        // Any sign of life resets the idle clock: `timeout_secs`
                        // bounds silence, not the whole turn — an agent that
                        // streams for ten minutes is working, not hung.
                        sleeper.as_mut().reset(tokio::time::Instant::now() + idle);
                    }
                    // The reader clears watchers when the agent is gone; this
                    // branch and the Exited branch above are the same failure
                    // seen from two ends of the same socket, so they say the
                    // same honest thing (and select! may pick either).
                    None => break Err(LlmError::Unavailable {
                        backend: self.spec.id.clone(),
                        reason: format!(
                            "agent `{}` died mid-turn ({}); its tools may have partly run — this turn is not retried automatically",
                            conn.label(),
                            conn.exit_detail()
                        ),
                    }),
                },
                _ = &mut sleeper => {
                    // Idle timeout: tell the agent to stop, fail the turn.
                    conn.notify("session/cancel", cancel_params());
                    break Err(LlmError::Timeout { secs: idle.as_secs() });
                }
                _ = cancel.cancelled() => {
                    conn.notify("session/cancel", cancel_params());
                    // Give the agent a bounded moment to acknowledge, so it is
                    // not still working when the owner believes it stopped.
                    let _ = tokio::time::timeout(CANCEL_GRACE, &mut prompt_future).await;
                    break Err(LlmError::Cancelled);
                }
            }
        };
        conn.unsubscribe(&session_id);
        let stop = stop?;
        // `cancelled` has two meanings and they are not the same fact. Our own
        // stop token being set is the owner's stop, and the runner turns that
        // into "stopped by the owner" — an error, but the right one. The agent
        // cancelling **itself** is not an infrastructure failure: the common
        // cause is a denied permission, and after `permission_mode = "reject"`
        // it is the *expected* one. Reporting it as `Cancelled` made the CLI
        // print "backend `opencode` is unavailable: cancelled" right after the
        // transcript showed a denied tool call — a healthy backend blamed for
        // our own policy. So this closes the turn normally and says so in a
        // warning; the refused call is already in the transcript as `failed`.
        if stop == StopReason::Cancelled {
            if cancel.is_cancelled() {
                return Err(LlmError::Cancelled);
            }
            tracing::warn!(
                backend = %self.spec.id,
                "the agent cancelled its own turn (a denied permission does this); reporting the \
                 turn as ended rather than as a backend failure"
            );
        }

        let finish_reason = match stop {
            StopReason::EndTurn => FinishReason::Stop,
            StopReason::MaxTokens => FinishReason::Length,
            // The agent's own round limit — not our Length notice's "output
            // limit", so it maps to the honest unmapped bucket.
            StopReason::MaxTurnRequests => FinishReason::Error,
            StopReason::Refusal => FinishReason::ContentFilter,
            // Handled above: our cancel returned early, the agent's own cancel
            // is an end of turn.
            StopReason::Cancelled => FinishReason::Stop,
            StopReason::Unknown => {
                tracing::warn!(backend = %self.spec.id, "the agent reported an unknown stopReason");
                FinishReason::Error
            }
        };

        let response = ChatResponse {
            text,
            reasoning,
            // The agent executed its own tools; there is nothing for the
            // runner to call, and faking ToolCall values here would run them
            // twice.
            tool_calls: Vec::new(),
            finish_reason,
            usage: Usage {
                llm_calls: 1,
                ..Usage::default()
            },
        };
        emit(
            &events,
            LlmEvent::Finished {
                finish_reason: response.finish_reason,
                usage: response.usage,
                text: response.text.clone(),
                reasoning: response.reasoning.clone(),
                tool_calls: Vec::new(),
            },
        )
        .await?;
        Ok(response)
        }
        .await;

        // Release the session inside the agent before this turn's future
        // returns — including after a refusal, a timeout or a stop. The process
        // outlives the turn, so a session nobody closes is a leak inside it.
        self.close_session(&conn, &session_id).await;
        outcome
    }

    async fn probe(&self) -> Result<BackendStatus, LlmError> {
        // Like every probe in this crate: reports, never throws.
        let backend = self.spec.id.clone();
        let model = self.spec.model.clone();
        if self.key.is_none() && self.key_env.is_some() {
            return Ok(BackendStatus {
                backend,
                ok: false,
                detail: format!(
                    "no api key: set {}",
                    self.key_env.as_deref().unwrap_or_default()
                ),
                model,
                version: None,
            });
        }
        let launch = self.launch();
        let timeout = probe_budget(&self.spec);
        let conn = match Connection::connect(
            backend.clone(),
            &self.transport,
            &launch,
            Arc::clone(&self.permissions),
            timeout,
        )
        .await
        {
            Ok(conn) => conn,
            Err(error) => {
                return Ok(BackendStatus {
                    backend,
                    ok: false,
                    detail: error.to_string(),
                    model,
                    version: None,
                });
            }
        };
        // Proving `session/new` works is what surfaces a missing login — and
        // it costs no model call.
        let status = match tokio::time::timeout(
            timeout,
            conn.rpc(
                "session/new",
                json!({"cwd": self.cwd.to_string_lossy(), "mcpServers": []}),
            ),
        )
        .await
        {
            Err(_) => BackendStatus {
                backend: backend.clone(),
                ok: false,
                detail: format!(
                    "agent `{}` initialized but did not answer session/new within {}s",
                    conn.label(),
                    timeout.as_secs()
                ),
                model,
                version: None,
            },
            Ok(Err(error)) => {
                let mut detail = conn.map_error("session/new", error).to_string();
                if let Some(hint) = conn.auth_hint() {
                    detail.push_str(" — ");
                    detail.push_str(&hint);
                }
                BackendStatus {
                    backend: backend.clone(),
                    ok: false,
                    detail,
                    model,
                    version: None,
                }
            }
            Ok(Ok(value)) => match serde_json::from_value::<NewSessionResult>(value) {
                Err(err) => BackendStatus {
                    backend: backend.clone(),
                    ok: false,
                    detail: format!("agent `{}` sent an unreadable session: {err}", conn.label()),
                    model,
                    version: None,
                },
                Ok(session) => {
                    let mut detail = format!(
                        "ACP v{PROTOCOL_VERSION} agent `{}` ready; sessions start in `{}`; permissions: {}",
                        conn.label(),
                        self.cwd.display(),
                        self.policy_mode_label()
                    );
                    if session.models.is_some()
                        || session.config_options.iter().any(|opt| opt.id == "model")
                    {
                        detail.push_str("; the agent advertises model selection");
                    } else {
                        detail.push_str("; the agent picks its own model (no ACP model selection)");
                    }
                    if let Some(hint) = conn.auth_hint() {
                        detail.push_str(". ");
                        detail.push_str(&hint);
                    }
                    BackendStatus {
                        backend: backend.clone(),
                        ok: true,
                        detail,
                        model,
                        // ACP's initialize carries no agent version; the
                        // protocol version is the honest number we have.
                        version: Some(format!("acp v{PROTOCOL_VERSION}")),
                    }
                }
            },
        };
        conn.shutdown();
        Ok(status)
    }

    async fn available_models(&self) -> Result<ModelList, LlmError> {
        if self.key.is_none() && self.key_env.is_some() {
            return Err(LlmError::MissingKey {
                env: self.key_env.clone().unwrap_or_default(),
            });
        }
        // When an explicit models_command is configured (opencode's ACP server
        // stays silent about models, but its own CLI lists them), run it
        // directly. Spawning an ACP agent and dialing session/new is unnecessary
        // and would fail or hang on agents whose ACP session setup takes time.
        if self.models_command.is_some() {
            let models = self.run_models_command().await?;
            return Ok(if self.model_env.is_empty() {
                ModelList::informational(models)
            } else {
                ModelList::authoritative(models)
            });
        }
        // `session/new` is where an ACP agent advertises its model list, and
        // it costs no model call. Reuses the live connection when there is
        // one (a menu opening must not respawn the agent), and the probe
        // budget when it has to spawn one.
        let cancel = CancellationToken::new();
        let conn = self
            .connection_for(
                "",
                self.cwd.clone(),
                self.substituted_model_env(None),
                &cancel,
            )
            .await?;
        let budget = probe_budget(&self.spec);
        let result = tokio::time::timeout(
            budget,
            conn.rpc(
                "session/new",
                json!({"cwd": self.cwd.to_string_lossy(), "mcpServers": []}),
            ),
        )
        .await;
        let session = match result {
            Err(_) => Err(LlmError::Timeout {
                secs: budget.as_secs(),
            }),
            Ok(Err(error)) => Err(conn.map_error("session/new", error)),
            Ok(Ok(value)) => serde_json::from_value::<NewSessionResult>(value).map_err(|err| {
                LlmError::Malformed {
                    backend: self.spec.id.clone(),
                    reason: format!("unreadable `session/new` result: {err}"),
                }
            }),
        }?;
        // No advertised model state is not an error: fall back to the
        // agent's own CLI lister when the operator configured one (opencode's
        // ACP server stays silent about models, but `opencode models` is
        // not); without it, an empty list is the honest answer and the menu
        // says how to get one.
        //
        // This probe opened a session too, so it closes it on the same terms as
        // a turn: the block keeps every `?` below from skipping the close.
        let session_id = session.session_id.clone();
        let listed: Result<ModelList, LlmError> = async {
            match session.models {
                // Advertised by the agent itself: a pick reaches it via
                // session/set_model, so the list is authoritative.
                Some(state) => Ok(ModelList::authoritative(
                    state
                        .available
                        .into_iter()
                        .map(|model| ModelOption::new(model.model_id))
                        .collect(),
                )),
                // Read from the agent's own CLI: real models, but a pick cannot
                // reach the agent over ACP — informational only, and the UI says
                // so instead of offering a choice that silently does nothing.
                None => {
                    if let Some(opt) = session
                        .config_options
                        .into_iter()
                        .find(|opt| opt.id == "model")
                        && !opt.options.is_empty()
                    {
                        return Ok(ModelList::authoritative(
                            opt.options
                                .into_iter()
                                .map(|choice| {
                                    if let Some(name) = choice.name {
                                        ModelOption::with_label(choice.value, name)
                                    } else {
                                        ModelOption::new(choice.value)
                                    }
                                })
                                .collect(),
                        ));
                    }
                    let models = match &self.models_command {
                        None => Vec::new(),
                        Some(_) => self.run_models_command().await?,
                    };
                    // With `model_env` configured a pick really does reach the
                    // agent (its connection is respawned pinned to the pick), so
                    // the list is authoritative even though the wire was silent.
                    Ok(if self.model_env.is_empty() {
                        ModelList::informational(models)
                    } else {
                        ModelList::authoritative(models)
                    })
                }
            }
        }
        .await;
        self.close_session(&conn, &session_id).await;
        listed
    }
}

impl AcpAgent {
    /// Run the configured `models_command` (the agent's own CLI model lister)
    /// and parse its stdout. Spawned under the same environment rules as the
    /// agent itself, and group-killed if it outstays its timeout — a lister
    /// that hangs must not hang the menu that asked for it.
    async fn run_models_command(&self) -> Result<Vec<ModelOption>, LlmError> {
        let argv = self.models_command.clone().unwrap_or_default();
        if argv.is_empty() {
            return Ok(Vec::new());
        }
        let label = argv.join(" ");
        let mut command = tokio::process::Command::new(&argv[0]);
        command.args(&argv[1..]);
        if !self.inherit_env {
            command.env_clear();
            for name in SCRUBBED_ENV {
                if let Ok(value) = std::env::var(name) {
                    command.env(name, value);
                }
            }
        }
        for (name, value) in self.child_env() {
            command.env(name, value);
        }
        command
            .current_dir(&self.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().map_err(|err| LlmError::Unavailable {
            backend: self.spec.id.clone(),
            reason: format!("cannot run models_command `{label}`: {err}"),
        })?;
        let pid = child.id();
        match tokio::time::timeout(MODELS_COMMAND_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) if output.status.success() => {
                Ok(parse_model_list(&String::from_utf8_lossy(&output.stdout)))
            }
            Ok(Ok(output)) => Err(LlmError::Unavailable {
                backend: self.spec.id.clone(),
                reason: format!(
                    "models_command `{label}` exited {}: {}",
                    output.status,
                    truncate_utf8(&String::from_utf8_lossy(&output.stderr), 256)
                ),
            }),
            Ok(Err(err)) => Err(LlmError::Unavailable {
                backend: self.spec.id.clone(),
                reason: format!("models_command `{label}` failed: {err}"),
            }),
            Err(_) => {
                // The child is unreaped in this branch, so its pid is still
                // its own: group-kill is safe (same rule as the supervisor).
                #[cfg(unix)]
                if let Some(pid) = pid {
                    use nix::sys::signal::{Signal, killpg};
                    use nix::unistd::Pid;
                    let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
                }
                Err(LlmError::Timeout {
                    secs: MODELS_COMMAND_TIMEOUT.as_secs(),
                })
            }
        }
    }
}

/// Parse a lister's stdout into model ids: one per non-empty trimmed line,
/// de-duplicated in first-seen order, capped at [`MAX_LISTED_MODELS`].
/// Listers are agent CLIs printing plain ids (`opencode models`); anything
/// else in the output simply becomes an unusable menu row, never an error.
pub(crate) fn parse_model_list(stdout: &str) -> Vec<ModelOption> {
    let mut seen = std::collections::BTreeSet::new();
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| seen.insert((*line).to_string()))
        .take(MAX_LISTED_MODELS)
        .map(ModelOption::new)
        .collect()
}

/// Convert per-turn [`McpServerSpec`] values into ACP `session/new`
/// `mcpServers` entries.
///
/// Stdio servers are supported by all ACP agents. HTTP servers are included
/// unless the agent explicitly advertised `mcpCapabilities.http = false`, in
/// which case they are skipped with a warning log.
pub(crate) fn acp_mcp_servers(
    backend_id: &str,
    specs: &[McpServerSpec],
    caps: Option<McpCapabilities>,
) -> Vec<Value> {
    let http_explicitly_unsupported = caps.is_some_and(|c| c.http == Some(false));
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        match spec {
            McpServerSpec::Http { name, url, headers } => {
                if http_explicitly_unsupported {
                    tracing::warn!(
                        backend = %backend_id,
                        server = %name,
                        "ACP agent advertised mcpCapabilities.http = false; skipping HTTP MCP server"
                    );
                    continue;
                }
                let header_list: Vec<Value> = headers
                    .iter()
                    .map(|(k, v)| json!({"name": k, "value": v}))
                    .collect();
                out.push(json!({
                    "type": "http",
                    "name": name,
                    "url": url,
                    "headers": header_list,
                }));
            }
            McpServerSpec::Stdio {
                name,
                command,
                args,
                env,
            } => {
                let env_list: Vec<Value> = env
                    .iter()
                    .map(|(k, v)| json!({"name": k, "value": v}))
                    .collect();
                out.push(json!({
                    "name": name,
                    "command": command,
                    "args": args,
                    "env": env_list,
                }));
            }
        }
    }
    out
}

/// The model this turn wants: the runner resolves bot → spec already, and
/// `"default"`/empty means "whatever the backend chooses".
fn desired_model(request: &ChatRequest, spec_model: Option<&str>) -> Option<String> {
    let wanted = |value: &str| {
        let trimmed = value.trim();
        (!trimmed.is_empty() && trimmed != "default").then(|| trimmed.to_string())
    };
    wanted(&request.model).or_else(|| spec_model.and_then(wanted))
}

/// Render one turn's prompt: instructions, then the transcript, then (when the
/// history ends with a user message) a one-line "your turn" marker. Text
/// blocks only — ACP's baseline every agent must accept.
///
/// The transcript is the whole history because this backend keeps no native
/// session; compaction in the runner is what bounds it.
pub(crate) fn prompt_blocks(request: &ChatRequest) -> Vec<Value> {
    let mut blocks = Vec::new();
    if !request.instructions.trim().is_empty() {
        blocks.push(text_block(&request.instructions));
    }
    let transcript = render_transcript(&request.messages);
    if !transcript.is_empty() {
        blocks.push(text_block(&transcript));
    }
    if request
        .messages
        .last()
        .is_some_and(|message| message.role == Role::User)
    {
        blocks.push(text_block(
            "(Your turn: respond to the last message above, in your persona and following the instructions.)",
        ));
    }
    if blocks.is_empty() {
        blocks.push(text_block("(no message content)"));
    }
    blocks
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn render_transcript(messages: &[ChatMessage]) -> String {
    let mut lines = Vec::with_capacity(messages.len() + 1);
    lines.push("Conversation so far, oldest first:".to_string());
    let mut any = false;
    for message in messages {
        let speaker = match message.role {
            Role::System => "system".to_string(),
            Role::User => message.name.clone().unwrap_or_else(|| "user".to_string()),
            Role::Assistant => message
                .name
                .clone()
                .unwrap_or_else(|| "assistant".to_string()),
            Role::Tool => "tool result".to_string(),
        };
        let mut line = if message.content.is_empty() {
            String::new()
        } else {
            format!("{speaker}: {}", message.content)
        };
        // Legacy history from a tool-calling backend: name the calls instead
        // of silently dropping them.
        if !message.tool_calls.is_empty() {
            let names: Vec<&str> = message
                .tool_calls
                .iter()
                .map(|call| call.name.as_str())
                .collect();
            if line.is_empty() {
                line = format!("{speaker}:");
            }
            line.push_str(&format!(" [called tools: {}]", names.join(", ")));
        }
        if message.role == Role::Tool
            && let Some(call_id) = &message.tool_call_id
        {
            line = format!("tool result (for {call_id}): {}", message.content);
        }
        if !line.is_empty() {
            any = true;
            lines.push(line);
        }
    }
    if any { lines.join("\n") } else { String::new() }
}

/// The name a transcript row should carry for one `tool_call` notification.
///
/// Agents are not consistent about what they put where, and the difference is
/// not cosmetic. Captured from `opencode acp` on 2026-11-14:
///
/// ```text
/// tool_call        name:"shell" title:"shell"        kind:"execute"
/// tool_call_update name:—       title:"ls crates | wc -l"
/// ```
///
/// So the **announcement** names the tool and its later updates rename the
/// display text to the *command*. Taking "the first non-empty label" every time
/// would turn a row's `shell` into `ls crates | wc -l` mid-turn — exactly what
/// a live run of D9.31 showed. The rule: `name` always wins when present; a
/// `title`/`kind` is only consulted on the announcing notification, i.e. only
/// for an agent that sends no `name` at all; and an update with none of them
/// contributes nothing, so the runner keeps the name it already has.
fn tool_identity(announced: bool, name: Option<&str>, title: &str, kind: Option<&str>) -> String {
    let pick = |value: &str| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };
    name.and_then(pick)
        .or_else(|| announced.then(|| pick(title)).flatten())
        .or_else(|| announced.then(|| kind.and_then(pick)).flatten())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatMessage, ToolCall};

    fn spec_with_options(options: serde_json::Map<String, Value>) -> BackendSpec {
        BackendSpec {
            id: "acp-test".into(),
            kind: BackendKind::Acp,
            base_url: None,
            api_key_env: None,
            model: None,
            extra_body: Default::default(),
            extra_headers: Default::default(),
            timeout_secs: 60,
            max_retries: 0,
            options,
        }
    }

    fn agent_spec() -> BackendSpec {
        let mut options = serde_json::Map::new();
        options.insert("command".into(), json!("/bin/false"));
        options.insert("args".into(), json!(["acp"]));
        spec_with_options(options)
    }

    // -- construction -------------------------------------------------------

    #[tokio::test]
    async fn options_parse_with_defaults_and_fail_loudly() {
        let agent = AcpAgent::from_spec(agent_spec(), None).expect("valid spec");
        assert_eq!(agent.command, "/bin/false");
        assert_eq!(agent.args, vec!["acp".to_string()]);
        assert_eq!(agent.permission_mode(), Some(PermissionMode::Reject));
        assert!(agent.cwd.is_absolute());

        // A missing command is a build-time error, not a first-turn surprise.
        let error = AcpAgent::from_spec(spec_with_options(Default::default()), None)
            .expect_err("command is required");
        assert!(error.to_string().contains("options.command"), "{error}");

        // Unknown permission modes fail fast rather than defaulting.
        let mut options = serde_json::Map::new();
        options.insert("command".into(), json!("opencode"));
        options.insert("permission_mode".into(), json!("yolo"));
        let error = AcpAgent::from_spec(spec_with_options(options), None).expect_err("bad mode");
        assert!(error.to_string().contains("permission_mode"), "{error}");

        // Non-string args and env values are config mistakes, named as such.
        let mut options = serde_json::Map::new();
        options.insert("command".into(), json!("opencode"));
        options.insert("args".into(), json!([1, 2]));
        assert!(AcpAgent::from_spec(spec_with_options(options), None).is_err());
        let mut options = serde_json::Map::new();
        options.insert("command".into(), json!("opencode"));
        options.insert("env".into(), json!({"KEY": 42}));
        let error = AcpAgent::from_spec(spec_with_options(options), None).expect_err("bad env");
        assert!(error.to_string().contains("options.env.KEY"), "{error}");

        // A wrong kind is rejected.
        let mut wrong = agent_spec();
        wrong.kind = BackendKind::OpenAiCompat;
        assert!(AcpAgent::from_spec(wrong, None).is_err());
    }

    #[test]
    fn debug_renders_the_command_but_never_a_secret() {
        let mut spec = agent_spec();
        spec.api_key_env = Some("ANTHROPIC_API_KEY".into());
        let mut options = spec.options.clone();
        options.insert("env".into(), json!({"OPENCODE_SECRET": "hunter2"}));
        spec.options = options;
        let agent = AcpAgent::with_transport(
            spec,
            Some("sk-super-secret".into()),
            Arc::new(ProcessTransport),
        )
        .expect("valid");
        let debug = format!("{agent:?}");
        assert!(!debug.contains("sk-super-secret"), "{debug}");
        assert!(!debug.contains("hunter2"), "{debug}");
        // Names stay visible: debugging must not go blind.
        assert!(debug.contains("ANTHROPIC_API_KEY"), "{debug}");
        assert!(debug.contains("OPENCODE_SECRET"), "{debug}");
        assert!(debug.contains("has_key: true"), "{debug}");
    }

    #[test]
    fn the_key_travels_into_the_child_environment_only() {
        let mut spec = agent_spec();
        spec.api_key_env = Some("ANTHROPIC_API_KEY".into());
        let agent = AcpAgent::from_spec(spec, Some("sk-secret-value".into())).expect("valid");
        let launch = agent.launch();
        assert_eq!(
            launch.env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-secret-value")
        );
        // The label is what errors and logs render — command line only.
        assert_eq!(launch.label(), "/bin/false acp");
        assert!(!launch.label().contains("sk-secret-value"));
        assert!(!format!("{launch:?}").contains("sk-secret-value"));
    }

    // -- capabilities -------------------------------------------------------

    #[test]
    fn capabilities_say_the_agent_owns_the_tool_loop() {
        let agent = AcpAgent::from_spec(agent_spec(), None).expect("valid");
        let capabilities = agent.capabilities();
        assert!(capabilities.streaming);
        // The load-bearing one: the runner must not drive a tool loop.
        assert!(!capabilities.tool_calls);
        assert!(capabilities.reasoning);
        assert!(!capabilities.steer);
        assert!(!capabilities.survives_restart);
        assert_eq!(agent.kind(), BackendKind::Acp);
        assert!(BackendKind::Acp.owns_tool_loop());
    }

    // -- permissions --------------------------------------------------------

    fn ask_with(kinds: &[(&str, PermissionOptionKind)]) -> AcpPermissionAsk {
        AcpPermissionAsk {
            session_id: "s1".into(),
            tool_call_id: "t1".into(),
            title: "Run `rm -rf build`".into(),
            kind: Some("execute".into()),
            raw_input: None,
            options: kinds
                .iter()
                .map(|(id, kind)| PermissionOption {
                    option_id: (*id).to_string(),
                    name: format!("{id} label"),
                    kind: *kind,
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn the_fixed_policy_picks_once_options_and_never_always() {
        use PermissionOptionKind::*;
        let ask = ask_with(&[
            ("a", AllowAlways),
            ("o", AllowOnce),
            ("r", RejectOnce),
            ("n", RejectAlways),
        ]);

        let reject = FixedPermissionPolicy {
            mode: PermissionMode::Reject,
        };
        assert_eq!(
            reject.decide(&ask).await,
            AcpPermissionDecision::Select("r".into())
        );
        let allow = FixedPermissionPolicy {
            mode: PermissionMode::Allow,
        };
        assert_eq!(
            allow.decide(&ask).await,
            AcpPermissionDecision::Select("o".into())
        );

        // Only *_always options on offer → cancel, never a blanket grant.
        let only_always = ask_with(&[("a", AllowAlways), ("n", RejectAlways)]);
        assert_eq!(
            allow.decide(&only_always).await,
            AcpPermissionDecision::Cancel
        );
        assert_eq!(
            reject.decide(&only_always).await,
            AcpPermissionDecision::Cancel
        );
        // No options at all → cancel.
        assert_eq!(
            allow.decide(&ask_with(&[])).await,
            AcpPermissionDecision::Cancel
        );
    }

    #[test]
    fn permission_modes_parse_strictly() {
        assert_eq!(
            PermissionMode::parse("allow").expect("ok"),
            PermissionMode::Allow
        );
        assert_eq!(
            PermissionMode::parse(" Reject ").expect("ok"),
            PermissionMode::Reject
        );
        assert!(PermissionMode::parse("always").is_err());
    }

    // -- prompt rendering ---------------------------------------------------

    #[test]
    fn prompts_render_instructions_transcript_and_the_turn_marker() {
        let mut request = ChatRequest::new("model-x", "You are @lead, a careful bot.");
        request
            .messages
            .push(ChatMessage::user("please fix the build").with_name("owner"));
        request.messages.push(ChatMessage::assistant("on it"));
        request.messages.push(ChatMessage::user("thanks!"));

        let blocks = prompt_blocks(&request);
        assert_eq!(blocks.len(), 3, "{blocks:?}");
        assert_eq!(
            blocks[0]["text"].as_str(),
            Some("You are @lead, a careful bot.")
        );
        let transcript = blocks[1]["text"].as_str().expect("transcript");
        assert!(
            transcript.contains("owner: please fix the build"),
            "{transcript}"
        );
        assert!(transcript.contains("assistant: on it"), "{transcript}");
        assert!(transcript.contains("user: thanks!"), "{transcript}");
        assert!(
            blocks[2]["text"]
                .as_str()
                .is_some_and(|text| text.contains("Your turn")),
            "{blocks:?}"
        );
        assert!(blocks.iter().all(|block| block["type"] == "text"));
    }

    #[test]
    fn transcripts_keep_legacy_tool_activity_visible() {
        let mut request = ChatRequest::new("m", "");
        request.messages.push(ChatMessage::assistant_tool_calls(
            "let me look",
            vec![ToolCall::new("c1", "read_file", json!({"path": "x"}))],
        ));
        request
            .messages
            .push(ChatMessage::tool("c1", "file contents"));
        request.messages.push(ChatMessage::user("and now?"));
        let blocks = prompt_blocks(&request);
        let transcript = blocks[0]["text"].as_str().expect("transcript");
        assert!(
            transcript.contains("[called tools: read_file]"),
            "{transcript}"
        );
        assert!(
            transcript.contains("tool result (for c1): file contents"),
            "{transcript}"
        );
    }

    #[test]
    fn an_empty_request_still_produces_one_honest_block() {
        let blocks = prompt_blocks(&ChatRequest::new("", ""));
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0]["text"].as_str().is_some_and(|t| !t.is_empty()));
        // No trailing user message → no turn marker.
        let mut request = ChatRequest::new("", "");
        request
            .messages
            .push(ChatMessage::assistant("previous answer"));
        let blocks = prompt_blocks(&request);
        assert!(blocks.iter().all(|block| {
            !block["text"]
                .as_str()
                .unwrap_or_default()
                .contains("Your turn")
        }));
    }

    /// The names `opencode acp` really sends, captured 2026-11-14: the
    /// announcement names the tool, its updates carry the *command* as the
    /// title. A row that followed the title would be renamed mid-turn.
    #[test]
    fn a_tool_keeps_the_name_its_announcement_gave_it() {
        // The announcement: name and title agree.
        assert_eq!(
            tool_identity(true, Some("shell"), "shell", Some("execute")),
            "shell"
        );
        // The updates: no name, and a title that is now the command. The
        // adapter sends an empty tool, which the runner reads as "nothing new".
        assert_eq!(
            tool_identity(false, None, "ls crates | wc -l", None),
            "",
            "an update must not rename the call to its command line"
        );
        // An agent that never sends `name` still gets its title, from the
        // announcement only.
        assert_eq!(
            tool_identity(true, None, "Edit src/main.rs", Some("edit")),
            "Edit src/main.rs"
        );
        // And `kind` is the last resort, for an announcement with neither.
        assert_eq!(tool_identity(true, None, "", Some("execute")), "execute");
        // Nothing at all stays empty rather than inventing a label.
        assert_eq!(tool_identity(true, None, "  ", None), "");
    }

    #[test]
    fn desired_model_ignores_the_default_placeholder() {
        assert_eq!(
            desired_model(&ChatRequest::new("gpt-x", ""), None).as_deref(),
            Some("gpt-x")
        );
        assert_eq!(
            desired_model(&ChatRequest::new("default", ""), Some("spec-model")).as_deref(),
            Some("spec-model")
        );
        assert_eq!(
            desired_model(&ChatRequest::new("", ""), Some("default")),
            None
        );
        assert_eq!(desired_model(&ChatRequest::new("default", ""), None), None);
    }

    #[test]
    fn model_env_templates_substitute_and_drop_unpinned() {
        let mut options = serde_json::Map::new();
        options.insert("command".into(), json!("opencode"));
        options.insert(
            "model_env".into(),
            json!({
                "OPENCODE_CONFIG_CONTENT": "{\"model\":\"{model}\"}",
                "PLAIN": "keep-me",
            }),
        );
        let agent = AcpAgent::from_spec(spec_with_options(options), None).expect("valid");
        let pinned = agent.substituted_model_env(Some("inferhub/ali/qwen3.8-flash"));
        assert_eq!(
            pinned.get("OPENCODE_CONFIG_CONTENT").map(String::as_str),
            Some("{\"model\":\"inferhub/ali/qwen3.8-flash\"}")
        );
        assert_eq!(pinned.get("PLAIN").map(String::as_str), Some("keep-me"));
        // A template with no model to fill cannot be sent half-filled.
        let unpinned = agent.substituted_model_env(None);
        assert!(!unpinned.contains_key("OPENCODE_CONFIG_CONTENT"));
        assert_eq!(unpinned.get("PLAIN").map(String::as_str), Some("keep-me"));

        // Junk shapes fail fast at config load.
        let mut options = serde_json::Map::new();
        options.insert("command".into(), json!("opencode"));
        options.insert("model_env".into(), json!({"A": 1}));
        assert!(AcpAgent::from_spec(spec_with_options(options), None).is_err());
    }

    #[test]
    fn parse_model_list_reads_one_id_per_line_and_dedupes() {
        let models = parse_model_list("  m1 \nm2\n\nm1\nm3\n");
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["m1", "m2", "m3"],
            "trimmed, de-duplicated, first-seen order"
        );
        assert!(parse_model_list("  \n\n").is_empty());
    }

    #[test]
    fn models_command_parses_both_forms_and_rejects_junk() {
        let mut options = serde_json::Map::new();
        options.insert("command".into(), json!("opencode"));
        options.insert("models_command".into(), json!(["opencode", "models"]));
        let agent = AcpAgent::from_spec(spec_with_options(options), None).expect("array form");
        assert_eq!(
            agent.models_command(),
            Some(["opencode".to_string(), "models".to_string()].as_slice())
        );

        let mut options = serde_json::Map::new();
        options.insert("command".into(), json!("opencode"));
        options.insert("models_command".into(), json!("opencode"));
        let agent = AcpAgent::from_spec(spec_with_options(options), None).expect("string form");
        assert_eq!(
            agent.models_command(),
            Some(["opencode".to_string()].as_slice())
        );

        let mut options = serde_json::Map::new();
        options.insert("command".into(), json!("opencode"));
        options.insert("models_command".into(), json!([1, 2]));
        assert!(AcpAgent::from_spec(spec_with_options(options), None).is_err());
    }

    #[test]
    fn the_probe_budget_never_lets_one_agent_eat_the_probe_window() {
        // A patient operator config must not turn one hung agent into a
        // client-side timeout that hides every other backend's result.
        let mut spec = agent_spec();
        spec.timeout_secs = 600;
        assert_eq!(probe_budget(&spec), Duration::from_secs(20));
        spec.timeout_secs = 1;
        assert_eq!(
            probe_budget(&spec),
            Duration::from_secs(20),
            "setup floors at 30s, so the 20s budget always wins"
        );
    }
}
