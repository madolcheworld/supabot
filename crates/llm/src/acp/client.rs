//! The ACP connection: one agent child process, newline-delimited JSON-RPC
//! 2.0 over its stdio, in **both** directions.
//!
//! Layout (an actor, not a socket loop):
//!
//! * a **writer** task owns the child's stdin: one task, so lines from
//!   concurrent turns cannot interleave mid-line;
//! * a **reader** task owns stdout and routes each line: responses wake the
//!   waiting caller by id, `session/update` notifications go to the watcher
//!   registered for their session, and agent→client *requests* are answered
//!   from their own task (a permission decision may be slow — in a later
//!   milestone it is an owner card — and a blocked reader would stall every
//!   session on the socket);
//! * a **supervisor** owns the [`tokio::process::Child`]: it reaps the child,
//!   records how it exited, and on a kill signal group-kills it — `process_group(0)`
//!   plus `killpg` is what makes an `npx …` wrapper die together with the
//!   `node` grandchild it spawned (D5.6: no orphan nobody owns);
//! * the agent's **stderr** is drained to `debug` logs, capped per line.
//!
//! [`Connection::shutdown`] (and `Drop`, as the leak-proof backstop) closes
//! the agent's stdin first — EOF is what makes a well-behaved stdio agent exit
//! *and collect its own children* — and escalates to a process-group kill
//! after [`SHUTDOWN_GRACE`]. Both halves matter: the group kill is what an
//! `npx` wrapper cannot survive, and the EOF is what reaches children the
//! wrapper detached into their own groups (`opencode acp` spawns its `serve`
//! child with `setsid`-like detachment — observed on opencode 2.0.28 — which
//! no `killpg` of the wrapper's group can touch).
//!
//! Everything below speaks the operator-facing error discipline of the crate:
//! failures are [`LlmError`] values, and neither spawn errors nor logs ever
//! render [`AgentLaunch::env`] values — one of them may be an API key.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::backend::LlmError;
use crate::openai::truncate_utf8;

use super::proto::{
    self, Incoming, InitializeResult, McpCapabilities, PROTOCOL_VERSION, PermissionRequest,
    RpcError,
};
use super::{AcpPermissionAsk, AcpPermissionDecision, AcpPermissions};

/// What to spawn, and how. Built by [`super::AcpAgent`] from a
/// `BackendSpec`; `env` values may be secrets and are never rendered.
pub struct AgentLaunch {
    pub command: String,
    pub args: Vec<String>,
    /// Environment additions/overrides for the child. Values are secret-bearing.
    pub env: std::collections::BTreeMap<String, String>,
    /// Working directory for the child (and the default `cwd` of its sessions).
    pub cwd: Option<PathBuf>,
    /// `false` scrubs the child's environment down to a small whitelist plus
    /// `env` (D5.7-style). The default inherits, because agents like `claude`
    /// need the owner's own HOME/login environment to work at all.
    pub inherit_env: bool,
}

impl AgentLaunch {
    /// How the launch reads in logs and errors: command line only, never env.
    pub fn label(&self) -> String {
        let mut label = self.command.clone();
        for arg in &self.args {
            label.push(' ');
            label.push_str(arg);
        }
        label
    }
}

impl fmt::Debug for AgentLaunch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Manual: `env` values may be API keys. Names are diagnostics; values
        // are nobody's business outside the child's own environment.
        f.debug_struct("AgentLaunch")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field("inherit_env", &self.inherit_env)
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// The environment whitelist a scrubbed child (`inherit_env = false`) keeps.
/// Shared by the agent spawn and the `models_command` lister so both scrub
/// identically (D5.7).
pub(crate) const SCRUBBED_ENV: [&str; 8] = [
    "PATH", "HOME", "USER", "SHELL", "LANG", "LC_ALL", "TERM", "TMPDIR",
];

/// A spawned agent, handed to the connection. The streams are boxed so test
/// transports can be in-memory duplexes instead of processes.
pub struct AgentHandle {
    /// Operator-facing name, e.g. `opencode acp (pid 4242)`.
    pub label: String,
    pub stdin: Box<dyn AsyncWrite + Unpin + Send>,
    pub stdout: Box<dyn AsyncRead + Unpin + Send>,
    pub stderr: Option<Box<dyn AsyncRead + Unpin + Send>>,
    /// Best-effort kill signal (idempotent). For [`ProcessTransport`] this
    /// triggers the supervisor's group-kill.
    pub kill: Arc<dyn Fn() + Send + Sync>,
    /// The real process to supervise, when the transport spawned one.
    pub process: Option<AgentProcess>,
}

impl fmt::Debug for AgentHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentHandle")
            .field("label", &self.label)
            .finish()
    }
}

/// The child process plus the private end of its kill channel. The supervisor
/// owns both: it is the only place that kills, and it only kills a pid it has
/// not reaped yet — no pid-reuse window.
pub struct AgentProcess {
    pub child: tokio::process::Child,
    pub pid: Option<u32>,
    pub kill_rx: mpsc::UnboundedReceiver<()>,
}

/// The spawn port. [`ProcessTransport`] is the real implementation; tests
/// inject in-memory agents, so the whole protocol runs without a process.
#[async_trait::async_trait]
pub trait AcpTransport: Send + Sync {
    /// Start the agent. The error string is operator-facing: it names the
    /// command and the OS reason, and must never contain an env value.
    async fn spawn(&self, launch: &AgentLaunch) -> Result<AgentHandle, String>;
}

/// Spawns the configured command as a child process in its own process group.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessTransport;

#[async_trait::async_trait]
impl AcpTransport for ProcessTransport {
    async fn spawn(&self, launch: &AgentLaunch) -> Result<AgentHandle, String> {
        let mut command = tokio::process::Command::new(&launch.command);
        command.args(&launch.args);
        if !launch.inherit_env {
            command.env_clear();
            for name in SCRUBBED_ENV {
                if let Ok(value) = std::env::var(name) {
                    command.env(name, value);
                }
            }
        }
        for (name, value) in &launch.env {
            command.env(name, value);
        }
        if let Some(cwd) = &launch.cwd {
            command.current_dir(cwd);
        }
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Own process group, so shutdown can kill the wrapper *and* what it
        // spawned (`npx` → `node`): a survivor nobody owns is worse than a
        // killed one (D5.6).
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command
            .spawn()
            .map_err(|err| format!("cannot run `{}`: {err}", launch.label()))?;
        let pid = child.id();
        let (kill_tx, kill_rx) = mpsc::unbounded_channel::<()>();
        let missing =
            |what: &str| format!("the agent's {what} was not piped — this is a bug in this build");
        let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
        let stderr = child
            .stderr
            .take()
            .map(|stderr| Box::new(stderr) as Box<dyn AsyncRead + Unpin + Send>);
        Ok(AgentHandle {
            label: format!(
                "{} (pid {})",
                launch.label(),
                pid.map_or_else(|| "?".to_string(), |pid| pid.to_string())
            ),
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr,
            kill: Arc::new(move || {
                // Signalling is best-effort: a dead supervisor means a dead
                // child, and there is nothing left to kill.
                let _ = kill_tx.send(());
            }),
            process: Some(AgentProcess {
                child,
                pid,
                kill_rx,
            }),
        })
    }
}

/// Why one request could not be answered. Callers map this to [`LlmError`]
/// with the context they have (which method, which retry policy).
#[derive(Clone, Debug)]
pub(crate) enum AcpRequestError {
    /// The agent answered with a JSON-RPC error object.
    Rpc(RpcError),
    /// The agent — or the pipe to it — is gone. Carries the exit detail.
    Exited(String),
}

enum WriterCommand {
    Line(String),
    /// Drop stdin (EOF for the agent) and stop.
    Shutdown,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;
type Watchers = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>>;

/// How long shutdown waits between closing the agent's stdin (EOF — the
/// polite half) and group-killing it (the certain half). Observed behaviour
/// that makes the polite half load-bearing: `opencode acp` detaches its
/// `serve` child into its *own* process group, which no `killpg` of the
/// wrapper's group can reach — but a clean EOF makes the wrapper exit and
/// collect that child itself.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// A live, initialized connection to one agent process. Cheap to clone
/// (everything is `Arc`), safe to share between concurrent turns: sessions are
/// routed by id, requests by JSON-RPC id.
pub(crate) struct Connection {
    backend: String,
    label: String,
    next_id: AtomicU64,
    cmd_tx: mpsc::UnboundedSender<WriterCommand>,
    pending: Pending,
    watchers: Watchers,
    dead: Arc<AtomicBool>,
    exit_status: Arc<Mutex<Option<String>>>,
    /// Filled after the handshake when the agent advertises auth methods:
    /// the hint that turns "session refused" into "log the agent in".
    auth_hint: Arc<Mutex<Option<String>>>,
    /// Advertised `agentCapabilities.mcpCapabilities` from `initialize`.
    mcp_capabilities: OnceLock<McpCapabilities>,
    kill: Arc<dyn Fn() + Send + Sync>,
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection")
            .field("backend", &self.backend)
            .field("label", &self.label)
            .field("alive", &self.alive())
            .finish()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // The leak-proof backstop: a connection nobody holds must not leave an
        // agent process running (D9.5's lesson, applied to children).
        self.shutdown();
    }
}

impl Connection {
    /// Spawn the agent and run the `initialize` handshake. The returned
    /// connection is ready for `session/new`; the agent's advertised auth
    /// methods live on in [`Connection::auth_hint`].
    pub(crate) async fn connect(
        backend: String,
        transport: &Arc<dyn AcpTransport>,
        launch: &AgentLaunch,
        permissions: Arc<dyn AcpPermissions>,
        handshake_timeout: Duration,
    ) -> Result<Arc<Self>, LlmError> {
        let handle = transport
            .spawn(launch)
            .await
            .map_err(|reason| LlmError::Unavailable {
                backend: backend.clone(),
                reason,
            })?;
        let AgentHandle {
            label,
            stdin,
            stdout,
            stderr,
            kill,
            process,
        } = handle;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let watchers: Watchers = Arc::new(Mutex::new(HashMap::new()));
        let dead = Arc::new(AtomicBool::new(false));
        let exit_status: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<WriterCommand>();

        tokio::spawn(writer_task(stdin, cmd_rx));
        tokio::spawn(reader_task(
            backend.clone(),
            stdout,
            Arc::clone(&pending),
            Arc::clone(&watchers),
            permissions,
            cmd_tx.clone(),
            Arc::clone(&dead),
        ));
        if let Some(stderr) = stderr {
            tokio::spawn(stderr_task(backend.clone(), stderr));
        }
        if let Some(process) = process {
            tokio::spawn(supervisor_task(
                process,
                Arc::clone(&dead),
                Arc::clone(&exit_status),
            ));
        }

        let auth_hint: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let connection = Arc::new(Self {
            backend: backend.clone(),
            label: label.clone(),
            next_id: AtomicU64::new(0),
            cmd_tx,
            pending,
            watchers,
            dead,
            exit_status,
            auth_hint: Arc::clone(&auth_hint),
            mcp_capabilities: OnceLock::new(),
            kill,
        });

        // We declare *no* client capabilities: no fs, no terminal. The agent
        // then uses its own tools (under its own sandbox) and asks us only for
        // permissions — which is the one intervention point this build
        // actually enforces. Advertising capabilities we would not policy-check
        // would be the bigger lie.
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {},
        });
        let init: InitializeResult =
            match tokio::time::timeout(handshake_timeout, connection.rpc("initialize", params))
                .await
            {
                Err(_) => {
                    connection.shutdown();
                    return Err(LlmError::Timeout {
                        secs: handshake_timeout.as_secs(),
                    });
                }
                Ok(Err(error)) => {
                    connection.shutdown();
                    return Err(connection.map_error("initialize", error));
                }
                Ok(Ok(value)) => match serde_json::from_value(value) {
                    Ok(init) => init,
                    Err(err) => {
                        connection.shutdown();
                        return Err(LlmError::Malformed {
                            backend,
                            reason: format!("unreadable `initialize` result: {err}"),
                        });
                    }
                },
            };
        if init.protocol_version != PROTOCOL_VERSION {
            connection.shutdown();
            return Err(LlmError::Malformed {
                backend,
                reason: format!(
                    "agent `{label}` speaks ACP v{}, this build speaks v{PROTOCOL_VERSION}",
                    init.protocol_version
                ),
            });
        }
        if let Some(mcp_caps) = init
            .agent_capabilities
            .and_then(|caps| caps.mcp_capabilities)
        {
            let _ = connection.mcp_capabilities.set(mcp_caps);
        }
        if !init.auth_methods.is_empty() {
            let names: Vec<&str> = init
                .auth_methods
                .iter()
                .map(|method| method.id.as_str())
                .collect();
            *auth_hint
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(format!(
                "the agent advertises auth methods: {}; if it refuses sessions, log the agent in with its own CLI or provide its API key environment variable",
                names.join(", ")
            ));
        }
        Ok(connection)
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn mcp_capabilities(&self) -> Option<McpCapabilities> {
        self.mcp_capabilities.get().copied()
    }

    /// The login hint built from the agent's advertised auth methods, when it
    /// advertised any. Rendered next to a refused `session/new` so "not logged
    /// in" reads as an actionable sentence, not a mystery error code.
    pub(crate) fn auth_hint(&self) -> Option<String> {
        self.auth_hint
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn alive(&self) -> bool {
        !self.dead.load(Ordering::SeqCst)
    }

    /// How the agent exited, once we know: part of every "agent is gone" error.
    pub(crate) fn exit_detail(&self) -> String {
        self.exit_status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| "exited (no status recorded)".to_string())
    }

    /// Send one request and wait for its response. Timeouts are the caller's
    /// job (the turn loop needs an *idle* timeout that updates reset, not a
    /// flat one).
    pub(crate) async fn rpc(&self, method: &str, params: Value) -> Result<Value, AcpRequestError> {
        if !self.alive() {
            return Err(AcpRequestError::Exited(self.exit_detail()));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, tx);
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();
        if self.cmd_tx.send(WriterCommand::Line(line)).is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            return Err(AcpRequestError::Exited(self.exit_detail()));
        }
        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(AcpRequestError::Rpc(error)),
            // The reader dropped every pending sender: the agent is gone.
            Err(_) => Err(AcpRequestError::Exited(self.exit_detail())),
        }
    }

    /// A JSON-RPC error as an [`LlmError`]: a method the agent does not have
    /// is malformed *for this agent* (not retryable), everything else reads as
    /// transport-level unavailability (retryable — the process may restart).
    pub(crate) fn map_error(&self, method: &str, error: AcpRequestError) -> LlmError {
        match error {
            AcpRequestError::Rpc(rpc) => {
                if rpc.is_method_not_found() {
                    LlmError::Malformed {
                        backend: self.backend.clone(),
                        reason: format!("agent `{}` does not implement `{method}`", self.label),
                    }
                } else {
                    let mut reason = format!(
                        "agent `{}` refused `{method}`: [{}] {}",
                        self.label,
                        rpc.code,
                        truncate_utf8(&rpc.message, 512)
                    );
                    // Agents put the specifics (auth failure kinds, model
                    // lists) in `data`; an operator-facing error without them
                    // would make the agent's own explanation invisible.
                    if let Some(data) = &rpc.data {
                        reason.push_str("; data: ");
                        reason.push_str(&truncate_utf8(&data.to_string(), 256));
                    }
                    LlmError::Unavailable {
                        backend: self.backend.clone(),
                        reason,
                    }
                }
            }
            AcpRequestError::Exited(detail) => LlmError::Unavailable {
                backend: self.backend.clone(),
                reason: format!("agent `{}` is gone: {detail}", self.label),
            },
        }
    }

    /// Fire-and-forget (`session/cancel`). A dead connection is not an error
    /// worth failing a cancelling turn over.
    pub(crate) fn notify(&self, method: &str, params: Value) {
        let line = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        })
        .to_string();
        let _ = self.cmd_tx.send(WriterCommand::Line(line));
    }

    /// The `session/update` stream for one session, until `unsubscribe` (or
    /// the connection dies, which closes the stream — the turn loop reads that
    /// as "agent exited mid-turn").
    pub(crate) fn subscribe(&self, session_id: &str) -> mpsc::UnboundedReceiver<Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.watchers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id.to_string(), tx);
        rx
    }

    pub(crate) fn unsubscribe(&self, session_id: &str) {
        self.watchers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
    }

    /// Shut the agent down: EOF first (a well-behaved stdio agent exits and
    /// collects its own children — see [`SHUTDOWN_GRACE`]), then the group-kill
    /// as the certain half. Idempotent, and safe from `Drop`: the escalation
    /// runs on a plain OS thread, because a `Handle::spawn` after the runtime
    /// shut down would panic, and a panic in `Drop` aborts the process.
    pub(crate) fn shutdown(&self) {
        self.dead.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(WriterCommand::Shutdown);
        let kill = Arc::clone(&self.kill);
        let exit_status = Arc::clone(&self.exit_status);
        std::thread::spawn(move || {
            std::thread::sleep(SHUTDOWN_GRACE);
            // The supervisor records the exit once the child is reaped; a
            // recorded exit means the polite half worked and there is nothing
            // left to kill.
            let exited = exit_status
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some();
            if !exited {
                (kill)();
            }
        });
    }
}

async fn writer_task(
    mut stdin: Box<dyn AsyncWrite + Unpin + Send>,
    mut rx: mpsc::UnboundedReceiver<WriterCommand>,
) {
    while let Some(command) = rx.recv().await {
        match command {
            WriterCommand::Line(line) => {
                // serde_json's compact output has no embedded newlines, so one
                // write_all per line keeps messages atomic enough; a write
                // error means the agent is gone and the reader will see EOF.
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
            WriterCommand::Shutdown => break,
        }
    }
    // Dropping stdin gives the agent EOF — the polite half of shutdown, next
    // to the group-kill the supervisor performs.
    drop(stdin);
}

async fn reader_task(
    backend: String,
    stdout: Box<dyn AsyncRead + Unpin + Send>,
    pending: Pending,
    watchers: Watchers,
    permissions: Arc<dyn AcpPermissions>,
    cmd_tx: mpsc::UnboundedSender<WriterCommand>,
    dead: Arc<AtomicBool>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match proto::classify(&line) {
                    Ok(Incoming::Response { id, result }) => {
                        let slot = pending
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&id);
                        if let Some(tx) = slot {
                            let _ = tx.send(result);
                        } else {
                            tracing::debug!(backend = %backend, id, "response for a request nobody waits for any more");
                        }
                    }
                    Ok(Incoming::Request { id, method, params }) => {
                        // Answered from its own task: a permission decision
                        // may take as long as an owner card does, and the
                        // reader must keep streaming updates meanwhile.
                        tokio::spawn(answer_agent_request(
                            backend.clone(),
                            id,
                            method,
                            params,
                            Arc::clone(&permissions),
                            cmd_tx.clone(),
                        ));
                    }
                    Ok(Incoming::Notification { method, params }) => {
                        if method == "session/update" {
                            let session = params
                                .get("sessionId")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            if let Some(session) = session {
                                let tx = watchers
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                                    .get(&session)
                                    .cloned();
                                if let Some(tx) = tx {
                                    // A slow consumer bounds itself: the channel
                                    // is unbounded, but a turn reads it in a
                                    // select loop, so it cannot fall behind.
                                    let _ = tx.send(params);
                                }
                            }
                        } else {
                            tracing::debug!(backend = %backend, method = %method, "ignoring agent notification");
                        }
                    }
                    Err(reason) => {
                        // Agents occasionally print non-protocol chatter on
                        // stdout. Skipping one junk line beats killing a turn.
                        tracing::debug!(backend = %backend, reason = %reason, "unreadable line on the agent's stdout, skipped");
                    }
                }
            }
            Ok(None) => break,
            Err(err) => {
                tracing::debug!(backend = %backend, error = %err, "the agent's stdout broke");
                break;
            }
        }
    }
    // EOF: the agent is gone. Fail everybody waiting instead of letting them
    // hang: dropping the senders is the message (rpc turns RecvError into
    // Exited), and dropping the watchers closes every turn's update stream.
    dead.store(true, Ordering::SeqCst);
    pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    watchers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

async fn answer_agent_request(
    backend: String,
    id: u64,
    method: String,
    params: Value,
    permissions: Arc<dyn AcpPermissions>,
    cmd_tx: mpsc::UnboundedSender<WriterCommand>,
) {
    let response = match method.as_str() {
        "session/request_permission" => {
            match serde_json::from_value::<PermissionRequest>(params) {
                Ok(request) => {
                    let ask = AcpPermissionAsk::from_protocol(request);
                    let decision = permissions.decide(&ask).await;
                    // The only audit trail this crate may write: one info line
                    // per decision, naming the tool and the option chosen.
                    tracing::info!(
                        backend = %backend,
                        session = %ask.session_id,
                        tool = %ask.title,
                        kind = ask.kind.as_deref().unwrap_or("?"),
                        decision = %decision_label(&ask, &decision),
                        "ACP permission decided"
                    );
                    let outcome = match &decision {
                        AcpPermissionDecision::Select(option_id) => json!({
                            "outcome": {"outcome": "selected", "optionId": option_id},
                        }),
                        AcpPermissionDecision::Cancel => json!({
                            "outcome": {"outcome": "cancelled"},
                        }),
                    };
                    json!({"jsonrpc": "2.0", "id": id, "result": outcome})
                }
                Err(err) => rpc_error(
                    id,
                    -32602,
                    format!("cannot read the permission request: {err}"),
                ),
            }
        }
        // We never declared the fs/terminal client capabilities, so a
        // well-behaved agent does not call these. If one does anyway, the
        // answer is a typed refusal — never a silent drop, never a fake ok.
        "fs/read_text_file" | "fs/write_text_file" => rpc_error(
            id,
            -32601,
            "this client declares no fs capability; use your own file tools",
        ),
        other if other.starts_with("terminal/") => rpc_error(
            id,
            -32601,
            "this client declares no terminal capability; run commands with your own tools",
        ),
        other => rpc_error(
            id,
            -32601,
            format!("`{other}` is not implemented by this client"),
        ),
    };
    let _ = cmd_tx.send(WriterCommand::Line(response.to_string()));
}

/// How a decision reads in the log: the option's own name when one was
/// selected, so "allow" never appears without saying *what* the agent offered.
fn decision_label(ask: &AcpPermissionAsk, decision: &AcpPermissionDecision) -> String {
    match decision {
        AcpPermissionDecision::Select(option_id) => ask
            .options
            .iter()
            .find(|option| &option.option_id == option_id)
            .map_or_else(
                || format!("select({option_id})"),
                |option| format!("select({}: {})", option_id, option.name),
            ),
        AcpPermissionDecision::Cancel => "cancel".to_string(),
    }
}

fn rpc_error(id: u64, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message.into()},
    })
}

async fn stderr_task(backend: String, stderr: Box<dyn AsyncRead + Unpin + Send>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(backend = %backend, "[acp agent stderr] {}", truncate_utf8(&line, 512));
    }
}

async fn supervisor_task(
    process: AgentProcess,
    dead: Arc<AtomicBool>,
    exit_status: Arc<Mutex<Option<String>>>,
) {
    let AgentProcess {
        mut child,
        pid,
        mut kill_rx,
    } = process;
    let status = tokio::select! {
        status = child.wait() => status,
        _ = kill_rx.recv() => {
            // The child is unreaped in this branch, so the pid is still the
            // child's: group-killing here is safe and reaches wrapper
            // grandchildren (npx → node). start_kill covers the non-unix case
            // and races the group-kill harmlessly.
            #[cfg(unix)]
            if let Some(pid) = pid {
                use nix::sys::signal::{Signal, killpg};
                use nix::unistd::Pid;
                let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
            }
            #[cfg(not(unix))]
            let _ = pid;
            let _ = child.start_kill();
            child.wait().await
        }
    };
    let detail = match status {
        Ok(status) => format!("exited with {status}"),
        Err(err) => format!("exited (wait failed: {err})"),
    };
    *exit_status
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(detail);
    dead.store(true, Ordering::SeqCst);
}
