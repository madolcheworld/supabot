//! CLI-harness adapters: backends that drive an agent CLI instead of an HTTP
//! API. This milestone ships `claude -p` far enough to run a turn: `chat`
//! spawns the harness, sends one stream-json user message, reads its NDJSON
//! events back and returns the assembled reply. What it does **not** yet do is
//! steer a running turn or carry a native session across a restart, so
//! `capabilities()` says so (see the two flags below). What `chat` does today:
//!
//! * spawn
//!   `claude -p --output-format stream-json --input-format stream-json --mcp-config <file> [--strict-mcp-config] [--resume <id>]`
//!   as a child with piped stdio, per turn; `--strict-mcp-config` is passed
//!   only when the turn supplied its own servers (see "Our tools reach the
//!   harness" below);
//! * write the turn as a stream-json user message on stdin and then **close
//!   stdin** (`drop(stdin)` in `chat`). Steering would mean writing another
//!   message while the turn runs, but `ChatRequest` has no channel to carry
//!   one — the runner's steering is a queue-level delivery, not a model call —
//!   so `capabilities().steer` is `false`. The flag must not be flipped
//!   without both a steer channel in the port and an open stdin;
//!   `steer_capability_matches_what_chat_does` pins the flag to the observed
//!   stdin behaviour;
//! * read the harness's NDJSON events from stdout and normalize them into
//!   [`LlmEvent`]s: assistant text deltas, `thinking` blocks as
//!   `ReasoningDelta`, `tool_use`/`tool_result` as informational events (the
//!   harness executes tools itself through the MCP servers our
//!   `mcp_config` points at), and the final `result` message as `Finished`;
//! * keep the `session_id` from the harness's `init` message so the next turn
//!   in *this process* resumes with `--resume <id>`. That id lives only in
//!   this adapter's memory: nothing writes it to
//!   `BotSessionRecord::native_session_id` and nothing replays it through
//!   `extra_body["native_session_id"]`, so a brain restart starts a fresh
//!   harness session — hence `capabilities().survives_restart == false`.
//!
//! The crucial capability invariant is already load-bearing today:
//! `tool_calls == false`. The harness runs its **own** tool loop through our
//! MCP server, so the runner must not drive one — if it did, tool calls would
//! execute twice or deadlock waiting for results the harness already
//! consumed. [`BackendKind::owns_tool_loop`] says the same thing from the
//! config side.
//!
//! Also real today: construction from a [`BackendSpec`] and an honest
//! [`LlmBackend::probe`] that checks the binary exists and answers
//! `--version`, so the Settings screen can show "claude CLI installed / not
//! found" without spending a model call.
//!
//! Our tools reach the harness through MCP, not through the response, and the
//! servers are the ones **this turn** registered: when `ChatRequest::mcp_servers`
//! is non-empty (the runner registers a turn-scoped endpoint whose URL carries a
//! one-time token), the adapter writes those specs to a private `0600` file in
//! `options.mcp_config_dir` (default: the OS temp dir), passes it as
//! `--mcp-config` together with `--strict-mcp-config` (so no user-level server
//! silently widens the turn's tool set), and deletes the file when the turn
//! ends. The file, not an inline JSON argument, because argv is visible in `ps`
//! and the URL is a credential.
//!
//! A request with **no** registered servers falls back to the static config the
//! owner configured: `options.mcp_config_path`, then
//! `<config_dir>/swarmbot-mcp.json`, then `options.data_dir/swarmbot-mcp.json`
//! (the brain writes that one, 0600, with the owner token in an `Authorization`
//! header). The spec has to name one of those; the adapter does not guess the
//! brain's data dir. The static file is deliberately *not* consulted for a turn
//! that registered its own servers — two sources would mean the model saw tools
//! the runner never offered, and the audit line would name a bot whose tools
//! were not the ones it ran.
//!
//! Still missing, and therefore never claimed: a mid-turn steer channel in the
//! port. Steering is a queue-level delivery today, so no `ChatRequest` can
//! carry one.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use swarmbot_contracts::{BackendKind, BackendSpec, Usage};

use crate::backend::{LlmBackend, LlmError, emit};
use crate::openai::truncate_utf8;
use crate::types::{
    BackendStatus, Capabilities, ChatRequest, ChatResponse, FinishReason, LlmEvent, McpServerSpec,
    Role,
};

/// The `claude -p` harness adapter.
#[derive(Debug)]
pub struct ClaudeCli {
    spec: BackendSpec,
    /// Executable to spawn (`options.binary`, default `claude` — resolved via
    /// PATH).
    binary: PathBuf,
    /// The harness's config dir (`options.config_dir`, default `~/.claude`),
    /// where the adapter finds login state and session files.
    config_dir: PathBuf,
    /// The MCP config file the adapter passes to `--mcp-config`
    /// (`options.mcp_config_path`, default `<config_dir>/swarmbot-mcp.json`);
    /// the brain writes it, pointing the harness at our MCP servers.
    mcp_config_path: PathBuf,
    /// Most recent native session id returned by the CLI (`subtype: "init"` or
    /// `"result"`), used to resume multi-turn conversations via `--resume`
    /// when `request.cwd` is unset.
    last_session_id: Arc<Mutex<Option<String>>>,
    /// Per-workspace native session ids so bots with distinct `request.cwd`
    /// directories sharing one `ClaudeCli` backend never resume each other's
    /// sessions.
    sessions_by_cwd: Arc<Mutex<HashMap<PathBuf, String>>>,
}

impl ClaudeCli {
    /// Build from a spec; all paths come from `spec.options` with sane
    /// defaults. Construction never spawns anything.
    pub fn from_spec(spec: BackendSpec) -> Result<Self, LlmError> {
        if !matches!(spec.kind, BackendKind::ClaudeCli) {
            return Err(LlmError::Unavailable {
                backend: spec.id.clone(),
                reason: format!("spec kind {:?} is not claude_cli", spec.kind),
            });
        }
        let binary = PathBuf::from(spec.option_str("binary").unwrap_or("claude"));
        let config_dir = spec
            .option_str("config_dir")
            .map(PathBuf::from)
            .unwrap_or_else(default_config_dir);
        let mcp_config_path = spec
            .option_str("mcp_config_path")
            .map(PathBuf::from)
            .unwrap_or_else(|| config_dir.join("swarmbot-mcp.json"));
        Ok(Self {
            spec,
            binary,
            config_dir,
            mcp_config_path,
            last_session_id: Arc::new(Mutex::new(None)),
            sessions_by_cwd: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn mcp_config_path(&self) -> &Path {
        &self.mcp_config_path
    }

    /// Retrieve the last recorded native session id, if any.
    pub fn last_session_id(&self) -> Option<String> {
        self.last_session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Update the last recorded native session id.
    pub fn set_last_session_id(&self, session_id: Option<String>) {
        *self
            .last_session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = session_id;
    }
}

/// No `dirs` dependency in this crate: `$HOME/.claude` is all the harness
/// default needs, and when HOME is unset a CWD-relative path keeps
/// construction infallible (probe then reports the binary honestly).
fn default_config_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
}

/// Serial number for turn config file names, so two turns in one process (or
/// two tests in one binary) never collide even inside one clock tick.
static TURN_MCP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One turn's `--mcp-config` document: the same shape the brain writes for the
/// static route (`{"mcpServers": {…}}`), so a harness parses one format
/// whichever path supplied it.
fn mcp_config_document(specs: &[McpServerSpec]) -> Value {
    let mut servers = serde_json::Map::new();
    for spec in specs {
        let (name, entry) = match spec {
            McpServerSpec::Http { name, url, headers } => {
                let mut entry = serde_json::Map::new();
                entry.insert("type".to_string(), json!("http"));
                entry.insert("url".to_string(), json!(url));
                if !headers.is_empty() {
                    // A JSON object, which is the static config's shape — not
                    // ACP's list of `{name, value}` pairs.
                    entry.insert("headers".to_string(), json!(headers));
                }
                (name, Value::Object(entry))
            }
            McpServerSpec::Stdio {
                name,
                command,
                args,
                env,
            } => {
                let mut entry = serde_json::Map::new();
                entry.insert("type".to_string(), json!("stdio"));
                entry.insert("command".to_string(), json!(command));
                if !args.is_empty() {
                    entry.insert("args".to_string(), json!(args));
                }
                if !env.is_empty() {
                    entry.insert("env".to_string(), json!(env));
                }
                (name, Value::Object(entry))
            }
        };
        servers.insert(name.clone(), entry);
    }
    json!({ "mcpServers": servers })
}

/// A private `--mcp-config` file for one turn, removed when it goes out of
/// scope.
///
/// A file rather than inline JSON because `claude --mcp-config` accepts both
/// and argv is world-readable on Linux: a turn-scoped server's URL is
/// `…/mcp/t/<token>`, a one-time bearer that must never show up in `ps`. It
/// lives in `options.mcp_config_dir` (default: the OS temp directory, which
/// honours `TMPDIR`) because that is somewhere a short-lived file can always be
/// created, and it is created `0600` with `create_new` (no shared temp name
/// another process could pre-create and read). `Drop` removes it on every exit
/// path — success, failure and cancellation alike — because the guard lives in
/// `chat` for exactly as long as the turn does.
struct TurnMcpConfig {
    path: PathBuf,
}

impl TurnMcpConfig {
    fn write(dir: &Path, specs: &[McpServerSpec]) -> Result<Self, std::io::Error> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let serial = TURN_MCP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            "swarmbot-turn-mcp-{}-{nanos}-{serial}.json",
            std::process::id()
        ));

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // The URL in this file authenticates a turn, exactly like the
            // static config the brain writes 0600.
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        let body =
            serde_json::to_string(&mcp_config_document(specs)).map_err(std::io::Error::other)?;
        file.write_all(body.as_bytes())?;
        file.flush()?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TurnMcpConfig {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path) {
            // Loud, not silent: the file names a live credential, so a failed
            // cleanup is something an operator should be able to find. Its
            // token stops authenticating when the turn's MCP registration
            // drops, which is why this is a warning rather than a failure.
            tracing::warn!(
                path = %self.path.display(),
                %error,
                "could not remove this turn's MCP config file"
            );
        }
    }
}

/// Kill the spawned child process group cleanly to avoid orphan grand-children.
async fn kill_child(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Render the prompt sent as a user message on the harness's stdin.
///
/// When resuming an existing session, only the latest user message is sent,
/// avoiding duplicate context in the CLI's native session. For a fresh session
/// with multiple messages, prior turns are rendered as a transcript.
pub(crate) fn render_prompt(request: &ChatRequest, is_resumed: bool) -> String {
    if is_resumed && let Some(last) = request.messages.last() {
        return last.content.clone();
    }
    if request.messages.len() <= 1 {
        if let Some(first) = request.messages.first() {
            return first.content.clone();
        }
        return "Hello".to_string();
    }
    let mut lines = Vec::with_capacity(request.messages.len() + 1);
    lines.push("Conversation history:".to_string());
    for msg in &request.messages {
        let speaker = match msg.role {
            Role::System => "System",
            Role::User => msg.name.as_deref().unwrap_or("User"),
            Role::Assistant => msg.name.as_deref().unwrap_or("Assistant"),
            Role::Tool => "Tool Result",
        };
        if !msg.content.is_empty() {
            lines.push(format!("{speaker}: {}", msg.content));
        }
        if !msg.tool_calls.is_empty() {
            let names: Vec<&str> = msg.tool_calls.iter().map(|c| c.name.as_str()).collect();
            lines.push(format!("[Called tools: {}]", names.join(", ")));
        }
    }
    lines.join("\n")
}

#[async_trait]
impl LlmBackend for ClaudeCli {
    fn id(&self) -> &str {
        &self.spec.id
    }

    fn kind(&self) -> BackendKind {
        BackendKind::ClaudeCli
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: true,
            // The harness runs its own tool loop through our MCP server: the
            // runner must NOT drive one (see the module docs).
            tool_calls: false,
            // `thinking` blocks arrive in the NDJSON stream.
            reasoning: true,
            // NOT steered. `chat` writes the turn's user message and drops
            // stdin before reading stdout, so a second message cannot reach the
            // harness; and there is nowhere to get one from — `ChatRequest`
            // carries no mid-turn channel, and the runner's steering is a
            // queue-level delivery between rounds, not a model call. Claiming
            // `true` here would advertise something nothing can exercise; the
            // `steer_capability_matches_what_chat_does` test pins this flag to
            // the stdin behaviour it depends on.
            steer: false,
            // NOT across a brain restart. `--resume <id>` works when a caller
            // supplies the id, but the id lives only in this adapter's memory:
            // nothing persists it into `BotSessionRecord::native_session_id`
            // and nothing sends it back through
            // `extra_body["native_session_id"]`, so a restarted brain starts a
            // fresh harness session. Within one process the adapter's own map
            // still bridges turns; that is not what this flag promises.
            survives_restart: false,
        }
    }

    async fn chat(
        &self,
        request: ChatRequest,
        events: mpsc::Sender<LlmEvent>,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, LlmError> {
        let explicit_session = request
            .extra_body
            .get("native_session_id")
            .or_else(|| request.extra_body.get("session_id"));
        let resume_id = match explicit_session {
            // Explicit string resumes that session; explicit null/empty
            // requests a fresh session without falling back to shared state.
            Some(val) => val
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from),
            None => match &request.cwd {
                Some(cwd) => self
                    .sessions_by_cwd
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(cwd)
                    .cloned(),
                None => self.last_session_id(),
            },
        };

        let mut cmd = tokio::process::Command::new(&self.binary);
        cmd.arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--include-partial-messages");

        if let Some(ref sid) = resume_id {
            cmd.arg("--resume").arg(sid);
        }

        // The servers this turn registered win, and they are the *only* ones:
        // the runner's per-turn endpoint carries a one-time token that expires
        // with the turn, while the static file names the owner-scoped route.
        // Passing both (or the static one alone for a turn that registered
        // servers) would hand the model tools the runner never offered, and the
        // turn's audit line would then describe a bot whose tools were not the
        // ones it ran. The guard must outlive the child, so it is bound here
        // and dropped when `chat` returns.
        let turn_mcp = if request.mcp_servers.is_empty() {
            None
        } else {
            let dir = self
                .spec
                .option_str("mcp_config_dir")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            Some(
                TurnMcpConfig::write(&dir, &request.mcp_servers).map_err(|error| {
                    LlmError::Unavailable {
                        backend: self.spec.id.clone(),
                        reason: format!(
                            "cannot write this turn's MCP config ({error}); refusing the turn \
                             rather than running it without the tools the caller registered"
                        ),
                    }
                })?,
            )
        };
        let mcp_cfg = match turn_mcp {
            Some(ref config) => Some(config.path().to_path_buf()),
            None => {
                if self.spec.option_str("mcp_config_path").is_some()
                    || self.mcp_config_path.exists()
                {
                    Some(self.mcp_config_path.clone())
                } else if let Some(data_dir) = self.spec.option_str("data_dir").map(PathBuf::from)
                    && data_dir.join("swarmbot-mcp.json").exists()
                {
                    Some(data_dir.join("swarmbot-mcp.json"))
                } else {
                    None
                }
            }
        };
        if let Some(ref path) = mcp_cfg {
            cmd.arg("--mcp-config").arg(path);
            if turn_mcp.is_some() {
                // Only the turn's own servers: a user-level `~/.claude.json`
                // server would otherwise be visible to a bot whose toolsets
                // were gated, and the transcript would name a tool the runner
                // never offered.
                cmd.arg("--strict-mcp-config");
            }
        }

        let model = if !request.model.trim().is_empty() && request.model != "default" {
            Some(request.model.as_str())
        } else {
            self.spec.model.as_deref()
        };
        if let Some(m) = model {
            cmd.arg("--model").arg(m);
        }

        if !request.instructions.trim().is_empty() {
            cmd.arg("--append-system-prompt").arg(&request.instructions);
        }

        if let Some(mode) = self.spec.option_str("permission_mode") {
            cmd.arg("--permission-mode").arg(mode);
        }

        if let Some(ref cwd) = request.cwd {
            cmd.current_dir(cwd);
        } else if let Some(cwd) = self.spec.option_str("cwd") {
            cmd.current_dir(cwd);
        }

        if let Some(env_map) = self.spec.options.get("env").and_then(|v| v.as_object()) {
            for (k, v) in env_map {
                if let Some(val_str) = v.as_str() {
                    cmd.env(k, val_str);
                }
            }
        }

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(unix)]
        {
            cmd.process_group(0);
        }

        let mut child = cmd.spawn().map_err(|err| LlmError::Unavailable {
            backend: self.spec.id.clone(),
            reason: format!("cannot spawn `{}`: {err}", self.binary.display()),
        })?;

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "swarmbot::claude", "{}", truncate_utf8(&line, 512));
                }
            });
        }

        let is_resumed = resume_id.is_some();
        let prompt_text = render_prompt(&request, is_resumed);
        let user_msg = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": prompt_text
                    }
                ]
            }
        });

        let mut stdin = child.stdin.take().ok_or_else(|| LlmError::Unavailable {
            backend: self.spec.id.clone(),
            reason: "failed to open child stdin".to_string(),
        })?;

        let payload = user_msg.to_string();
        if let Err(e) = stdin.write_all(payload.as_bytes()).await {
            kill_child(&mut child).await;
            return Err(LlmError::Unavailable {
                backend: self.spec.id.clone(),
                reason: format!("failed to write to child stdin: {e}"),
            });
        }
        if let Err(e) = stdin.write_all(b"\n").await {
            kill_child(&mut child).await;
            return Err(LlmError::Unavailable {
                backend: self.spec.id.clone(),
                reason: format!("failed to write newline to child stdin: {e}"),
            });
        }
        if let Err(e) = stdin.flush().await {
            kill_child(&mut child).await;
            return Err(LlmError::Unavailable {
                backend: self.spec.id.clone(),
                reason: format!("failed to flush child stdin: {e}"),
            });
        }
        // Closing stdin is what makes mid-turn steering impossible: a steer
        // would be another line written here while stdout is being read. There
        // is no channel to get one from (`ChatRequest` has none), so the pipe
        // is closed and `capabilities().steer` is false. The two facts are
        // pinned together by `steer_capability_matches_what_chat_does`.
        drop(stdin);

        let stdout = child.stdout.take().ok_or_else(|| LlmError::Unavailable {
            backend: self.spec.id.clone(),
            reason: "failed to open child stdout".to_string(),
        })?;

        let mut reader = BufReader::new(stdout).lines();
        let mut accumulated_text = String::new();
        let mut accumulated_reasoning = String::new();
        let mut current_session_id: Option<String> = resume_id;
        let mut stream_deltas_emitted = false;
        let mut final_result: Option<Value> = None;
        let timeout_secs = if self.spec.timeout_secs > 0 {
            self.spec.timeout_secs
        } else {
            60
        };
        let timeout_dur = Duration::from_secs(timeout_secs);

        loop {
            let line_result = tokio::select! {
                res = reader.next_line() => res,
                _ = cancel.cancelled() => {
                    kill_child(&mut child).await;
                    return Err(LlmError::Cancelled);
                }
                _ = tokio::time::sleep(timeout_dur) => {
                    kill_child(&mut child).await;
                    return Err(LlmError::Timeout { secs: timeout_dur.as_secs() });
                }
            };

            let line = match line_result {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(e) => {
                    kill_child(&mut child).await;
                    return Err(LlmError::Unavailable {
                        backend: self.spec.id.clone(),
                        reason: format!(
                            "failed to read stdout from `{}`: {e}",
                            self.binary.display()
                        ),
                    });
                }
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let parsed: Value = match serde_json::from_str(trimmed) {
                Ok(val) => val,
                Err(_) => {
                    tracing::trace!(target: "swarmbot::claude", "ignoring non-json line: {}", trimmed);
                    continue;
                }
            };

            let msg_type = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match msg_type {
                "system" => {
                    if parsed.get("subtype").and_then(|v| v.as_str()) == Some("init")
                        && let Some(sid) = parsed.get("session_id").and_then(|v| v.as_str())
                    {
                        current_session_id = Some(sid.to_string());
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = parsed.get("delta") {
                        let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if delta_type == "text_delta"
                            && let Some(text) = delta.get("text").and_then(|v| v.as_str())
                        {
                            accumulated_text.push_str(text);
                            stream_deltas_emitted = true;
                            if emit(&events, LlmEvent::TextDelta(text.to_string()))
                                .await
                                .is_err()
                            {
                                kill_child(&mut child).await;
                                return Err(LlmError::Cancelled);
                            }
                        } else if delta_type == "thinking_delta"
                            && let Some(thinking) = delta.get("thinking").and_then(|v| v.as_str())
                        {
                            accumulated_reasoning.push_str(thinking);
                            if emit(&events, LlmEvent::ReasoningDelta(thinking.to_string()))
                                .await
                                .is_err()
                            {
                                kill_child(&mut child).await;
                                return Err(LlmError::Cancelled);
                            }
                        }
                    }
                }
                "assistant" => {
                    if let Some(msg) = parsed.get("message")
                        && !stream_deltas_emitted
                        && let Some(content) = msg.get("content").and_then(|v| v.as_array())
                    {
                        for block in content {
                            let b_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            if b_type == "text"
                                && let Some(t) = block.get("text").and_then(|v| v.as_str())
                            {
                                accumulated_text.push_str(t);
                                let _ = emit(&events, LlmEvent::TextDelta(t.to_string())).await;
                            } else if b_type == "thinking"
                                && let Some(th) = block.get("thinking").and_then(|v| v.as_str())
                            {
                                accumulated_reasoning.push_str(th);
                                let _ =
                                    emit(&events, LlmEvent::ReasoningDelta(th.to_string())).await;
                            }
                        }
                    }
                }
                "result" => {
                    final_result = Some(parsed);
                    break;
                }
                _ => {}
            }
        }

        let exit_status = child.wait().await;

        let mut usage = Usage {
            llm_calls: 1,
            ..Usage::default()
        };

        if let Some(res) = final_result {
            if let Some(sid) = res.get("session_id").and_then(|v| v.as_str()) {
                current_session_id = Some(sid.to_string());
            }

            if res.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                let err_text = res
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("error reported by claude CLI");
                return Err(LlmError::Unavailable {
                    backend: self.spec.id.clone(),
                    reason: format!("claude CLI error: {err_text}"),
                });
            }

            if let Some(u) = res.get("usage") {
                if let Some(inp) = u.get("input_tokens").and_then(|v| v.as_u64()) {
                    usage.input_tokens = inp;
                }
                if let Some(out) = u.get("output_tokens").and_then(|v| v.as_u64()) {
                    usage.output_tokens = out;
                }
            }

            if accumulated_text.is_empty()
                && let Some(r) = res.get("result").and_then(|v| v.as_str())
            {
                accumulated_text = r.to_string();
                let _ = emit(&events, LlmEvent::TextDelta(r.to_string())).await;
            }
        } else {
            // No `result` message: the harness never said the turn was
            // finished. A clean exit is not a terminator — every stream-json
            // turn ends with a `result` object (it carries the usage and the
            // session id this adapter already depends on), so its absence means
            // the output was cut short: a wrapper swallowed the line, or the
            // harness died after flushing what it had. Returning the text that
            // did arrive would put half an answer in the transcript — the same
            // lie `consume_sse` refuses for the HTTP adapter.
            let backend = self.spec.id.clone();
            return Err(match exit_status {
                Ok(status) if !status.success() => LlmError::Unavailable {
                    backend,
                    reason: format!(
                        "`{}` exited prematurely with status: {status}",
                        self.binary.display()
                    ),
                },
                Ok(status) => LlmError::Malformed {
                    backend,
                    reason: format!(
                        "`{}` exited with status {status} without a `result` message: the \
                         stream ended before the turn was finished, so the reply is incomplete \
                         and was not used",
                        self.binary.display()
                    ),
                },
                Err(error) => LlmError::Unavailable {
                    backend,
                    reason: format!("cannot wait for `{}`: {error}", self.binary.display()),
                },
            });
        }

        if let Some(ref sid) = current_session_id {
            if let Some(ref cwd) = request.cwd {
                self.sessions_by_cwd
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(cwd.clone(), sid.clone());
            } else {
                self.set_last_session_id(Some(sid.clone()));
            }
        }

        let _ = emit(
            &events,
            LlmEvent::Finished {
                finish_reason: FinishReason::Stop,
                usage,
                text: accumulated_text.clone(),
                reasoning: accumulated_reasoning.clone(),
                tool_calls: Vec::new(),
            },
        )
        .await;

        Ok(ChatResponse {
            text: accumulated_text,
            reasoning: accumulated_reasoning,
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage,
        })
    }

    async fn probe(&self) -> Result<BackendStatus, LlmError> {
        // This part is real: the Settings screen must be able to say
        // "claude CLI installed (version X)" or "not found" today.
        let backend = self.spec.id.clone();
        let model = self.spec.model.clone();
        match tokio::process::Command::new(&self.binary)
            .arg("--version")
            .output()
            .await
        {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let version = stdout.lines().next().unwrap_or("").trim().to_string();
                Ok(BackendStatus {
                    backend,
                    ok: !version.is_empty(),
                    detail: format!(
                        "`{}` answered --version; mcp config: {}",
                        self.binary.display(),
                        self.mcp_config_path.display()
                    ),
                    model,
                    version: (!version.is_empty()).then_some(version),
                    // `--version` says the binary exists; it says nothing about
                    // a login, and this build does not guess one.
                    auth_methods: Vec::new(),
                    auth_required: false,
                    login_command: None,
                })
            }
            Ok(output) => Ok(BackendStatus {
                backend,
                ok: false,
                detail: format!(
                    "`{}` --version exited with {}: {}",
                    self.binary.display(),
                    output.status,
                    truncate_utf8(&String::from_utf8_lossy(&output.stderr), 256)
                ),
                model,
                version: None,
                auth_methods: Vec::new(),
                auth_required: false,
                login_command: None,
            }),
            Err(error) => Ok(BackendStatus {
                backend,
                ok: false,
                detail: format!("cannot run `{}`: {error}", self.binary.display()),
                model,
                version: None,
                auth_methods: Vec::new(),
                auth_required: false,
                login_command: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::types::ChatMessage;
    use serde_json::json;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "swarmbot-claude-test-{tag}-{}-{nanos}-{n}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn spec_with_options(options: serde_json::Map<String, serde_json::Value>) -> BackendSpec {
        BackendSpec {
            id: "claude".into(),
            kind: BackendKind::ClaudeCli,
            base_url: None,
            api_key_env: None,
            model: Some("claude-sonnet".into()),
            extra_body: Default::default(),
            extra_headers: Default::default(),
            timeout_secs: 60,
            max_retries: 0,
            options,
        }
    }

    /// One turn against a mock script, with the test-harness race handled.
    ///
    /// Why this exists: a mock harness is a script file the test writes and
    /// immediately executes, while sibling tests spawn children in parallel. A
    /// `fork` from a sibling can inherit the write fd of *this* script, and the
    /// kernel then refuses our `exec` with `ETXTBSY` ("Text file busy"). That is
    /// a property of running spawn tests inside one process, not of the
    /// adapter, so the turn is retried — bounded (20 tries, 10 ms apart) and
    /// only for that exact error, so every real failure still surfaces on the
    /// first attempt.
    async fn chat_turn(
        harness: &ClaudeCli,
        request: ChatRequest,
    ) -> (Result<ChatResponse, LlmError>, Vec<LlmEvent>) {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let (tx, mut rx) = mpsc::channel(16);
            let result = harness
                .chat(request.clone(), tx, CancellationToken::new())
                .await;
            let mut events = Vec::new();
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
            let busy = matches!(
                &result,
                Err(LlmError::Unavailable { reason, .. }) if reason.contains("Text file busy")
            );
            if !busy || attempt >= 20 {
                return (result, events);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn probe_works_with_a_stand_in_binary() {
        // /bin/echo stands in for `claude`: the probe must only depend on
        // "binary runs and prints a version line", never on claude being
        // installed on this machine.
        let mut options = serde_json::Map::new();
        options.insert("binary".into(), json!("/bin/echo"));
        let harness = ClaudeCli::from_spec(spec_with_options(options)).expect("valid spec");

        let status = harness.probe().await.expect("probe reports, never throws");
        assert!(status.ok, "{status:?}");
        let version = status.version.as_ref().expect("a version line");
        assert!(!version.is_empty());
        assert_eq!(status.model.as_deref(), Some("claude-sonnet"));
        assert!(status.detail.contains("/bin/echo"), "{status:?}");
    }

    #[tokio::test]
    async fn probe_reports_a_missing_binary_honestly() {
        let mut options = serde_json::Map::new();
        options.insert("binary".into(), json!("/nonexistent/swarmbot-test/claude"));
        let harness = ClaudeCli::from_spec(spec_with_options(options)).expect("valid spec");
        let status = harness.probe().await.expect("probe reports, never throws");
        assert!(!status.ok);
        assert!(status.detail.contains("cannot run"), "{status:?}");
        assert!(status.version.is_none());
    }

    #[tokio::test]
    async fn chat_reports_missing_binary_cleanly() {
        let mut options = serde_json::Map::new();
        options.insert("binary".into(), json!("/nonexistent/swarmbot-test/claude"));
        let harness = ClaudeCli::from_spec(spec_with_options(options)).expect("valid");
        let (tx, rx) = mpsc::channel(4);
        let result = harness
            .chat(ChatRequest::new("claude", ""), tx, CancellationToken::new())
            .await;
        drop(rx);
        match result {
            Err(LlmError::Unavailable { reason, .. }) => {
                assert!(reason.contains("cannot spawn"));
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_streams_deltas_and_records_session_id() {
        let temp = TempDir::new("stream");
        let script = temp.path().join("mock_claude.sh");
        fs::write(
            &script,
            r#"#!/bin/sh
cat << 'EOF'
{"type":"system","subtype":"init","session_id":"mock-session-42"}
{"type":"content_block_delta","delta":{"type":"text_delta","text":"Mock "}}
{"type":"content_block_delta","delta":{"type":"text_delta","text":"response!"}}
{"type":"result","result":"Mock response!","session_id":"mock-session-42","usage":{"input_tokens":15,"output_tokens":4},"is_error":false}
EOF
"#,
        )
        .expect("write mock script");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let mut options = serde_json::Map::new();
        options.insert("binary".into(), json!(script.display().to_string()));
        let harness = ClaudeCli::from_spec(spec_with_options(options)).expect("valid spec");

        let (result, received) = chat_turn(
            &harness,
            ChatRequest::new("claude-sonnet", "You are helpful"),
        )
        .await;
        let response = result.expect("chat completes");

        assert_eq!(response.text, "Mock response!");
        assert_eq!(response.usage.input_tokens, 15);
        assert_eq!(response.usage.output_tokens, 4);
        assert_eq!(harness.last_session_id(), Some("mock-session-42".into()));

        assert_eq!(
            received,
            vec![
                LlmEvent::TextDelta("Mock ".into()),
                LlmEvent::TextDelta("response!".into()),
                LlmEvent::Finished {
                    finish_reason: FinishReason::Stop,
                    usage: Usage {
                        input_tokens: 15,
                        output_tokens: 4,
                        llm_calls: 1,
                        ..Usage::default()
                    },
                    text: "Mock response!".into(),
                    reasoning: "".into(),
                    tool_calls: Vec::new(),
                }
            ]
        );
    }

    #[tokio::test]
    async fn chat_resumes_with_last_session_id() {
        let temp = TempDir::new("resume");
        let script = temp.path().join("mock_resume.sh");
        let args_file = temp.path().join("args.txt");
        let args_path_str = args_file.display().to_string();

        fs::write(
            &script,
            format!(
                r#"#!/bin/sh
echo "$@" > "{args_path_str}"
cat << 'EOF'
{{"type":"system","subtype":"init","session_id":"mock-session-99"}}
{{"type":"result","result":"Resumed turn!","session_id":"mock-session-99","usage":{{"input_tokens":5,"output_tokens":2}},"is_error":false}}
EOF
"#
            ),
        )
        .expect("write mock script");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let mut options = serde_json::Map::new();
        options.insert("binary".into(), json!(script.display().to_string()));
        let harness = ClaudeCli::from_spec(spec_with_options(options)).expect("valid spec");
        harness.set_last_session_id(Some("prior-session-123".into()));

        let (result, _events) = chat_turn(&harness, ChatRequest::new("claude-sonnet", "")).await;
        let resp = result.expect("chat completes");

        assert_eq!(resp.text, "Resumed turn!");
        assert_eq!(harness.last_session_id(), Some("mock-session-99".into()));

        let captured_args = fs::read_to_string(&args_file).expect("read captured args");
        assert!(
            captured_args.contains("--resume prior-session-123"),
            "args: {captured_args}"
        );
    }

    #[tokio::test]
    async fn chat_reports_error_honestly() {
        let temp = TempDir::new("error");
        let script = temp.path().join("mock_error.sh");
        fs::write(
            &script,
            r#"#!/bin/sh
cat << 'EOF'
{"type":"system","subtype":"init","session_id":"err-session"}
{"type":"result","result":"Not logged in · Please run /login","session_id":"err-session","is_error":true}
EOF
"#,
        )
        .expect("write mock script");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let mut options = serde_json::Map::new();
        options.insert("binary".into(), json!(script.display().to_string()));
        let harness = ClaudeCli::from_spec(spec_with_options(options)).expect("valid spec");

        let (result, _events) = chat_turn(&harness, ChatRequest::new("claude-sonnet", "")).await;
        let err = result.expect_err("should fail on is_error");

        match err {
            LlmError::Unavailable { reason, .. } => {
                assert!(reason.contains("Not logged in"), "{reason}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    /// A harness that exits cleanly after streaming part of an answer, without
    /// the stream-json `result` message, did not finish the turn. Committing
    /// what arrived is the same lie as accepting an SSE stream that never sent
    /// `[DONE]`: half an answer in the transcript that no retry can replace.
    #[tokio::test]
    async fn a_turn_without_a_result_message_is_refused_not_half_saved() {
        let temp = TempDir::new("no-result");
        let script = temp.path().join("mock_no_result.sh");
        fs::write(
            &script,
            r#"#!/bin/sh
cat << 'EOF'
{"type":"system","subtype":"init","session_id":"cut-session"}
{"type":"content_block_delta","delta":{"type":"text_delta","text":"half an ans"}}
EOF
exit 0
"#,
        )
        .expect("write mock script");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let mut options = serde_json::Map::new();
        options.insert("binary".into(), json!(script.display().to_string()));
        let harness = ClaudeCli::from_spec(spec_with_options(options)).expect("valid spec");

        let (result, events) = chat_turn(&harness, ChatRequest::new("", "")).await;
        match result {
            Err(LlmError::Malformed { reason, .. }) => {
                assert!(reason.contains("result"), "{reason}");
                assert!(reason.contains("incomplete"), "{reason}");
                assert!(reason.contains("was not used"), "{reason}");
            }
            other => panic!("a turn without a `result` message must be refused, got {other:?}"),
        }
        // The partial delta may have been streamed, but no `Finished` claims the
        // turn is complete.
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LlmEvent::Finished { .. })),
            "{events:?}"
        );
        // Nothing was recorded as the session to resume.
        assert_eq!(harness.last_session_id(), None);
    }

    #[tokio::test]
    async fn chat_handles_cancellation_cleanly() {
        let temp = TempDir::new("cancel");
        let script = temp.path().join("mock_sleep.sh");
        fs::write(
            &script,
            r#"#!/bin/sh
cat << 'EOF'
{"type":"system","subtype":"init","session_id":"sleep-session"}
EOF
sleep 10
"#,
        )
        .expect("write mock script");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let mut options = serde_json::Map::new();
        options.insert("binary".into(), json!(script.display().to_string()));
        let harness = ClaudeCli::from_spec(spec_with_options(options)).expect("valid spec");

        // A sibling test's `fork` can inherit the fd of the mock script being
        // executed, so the first spawn can fail with ETXTBSY before the
        // cancellation ever matters. Retry that one error, exactly as
        // `chat_turn` does, so this test measures cancellation rather than a
        // fork race — the same bounded retry, never a blanket sleep.
        let mut attempt = 0u32;
        let err = loop {
            attempt += 1;
            let cancel = CancellationToken::new();
            let cancel_clone = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                cancel_clone.cancel();
            });
            let (tx, _rx) = mpsc::channel(16);
            match harness.chat(ChatRequest::new("", ""), tx, cancel).await {
                Ok(_) => panic!("a cancelled turn must not report success"),
                Err(LlmError::Unavailable { reason, .. })
                    if reason.contains("Text file busy") && attempt < 20 =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
                Err(err) => break err,
            }
        };

        assert!(matches!(err, LlmError::Cancelled), "got {err:?}");
    }

    #[test]
    fn render_prompt_single_message_vs_history() {
        let mut req = ChatRequest::new("claude", "Instructions");
        req.messages.push(ChatMessage::user("Single prompt"));
        assert_eq!(render_prompt(&req, false), "Single prompt");

        req.messages.push(ChatMessage::assistant("Response 1"));
        req.messages.push(ChatMessage::user("Followup"));
        let rendered = render_prompt(&req, false);
        assert!(rendered.contains("Conversation history:"));
        assert!(rendered.contains("User: Single prompt"));
        assert!(rendered.contains("Assistant: Response 1"));
        assert!(rendered.contains("User: Followup"));

        // When resumed, only the last message is rendered
        assert_eq!(render_prompt(&req, true), "Followup");
    }

    #[test]
    fn capabilities_say_the_harness_owns_the_tool_loop() {
        let harness = ClaudeCli::from_spec(spec_with_options(Default::default())).expect("valid");
        let capabilities = harness.capabilities();
        assert!(capabilities.streaming);
        // The load-bearing one: the runner must not drive a tool loop.
        assert!(!capabilities.tool_calls);
        // Neither of these is implemented by the port yet, so neither may be
        // claimed: steering needs a mid-turn channel `ChatRequest` does not
        // have (and `chat` closes stdin), and a restart loses the native
        // session id because nothing persists it. The behaviour tests below
        // pin both to what the adapter actually does.
        assert!(!capabilities.steer);
        assert!(!capabilities.survives_restart);
        assert_eq!(harness.kind(), BackendKind::ClaudeCli);
        assert!(BackendKind::ClaudeCli.owns_tool_loop());
    }

    /// The `steer` flag must equal what `chat()` can actually do. `chat` writes
    /// the turn's user message and then drops stdin before reading stdout, so a
    /// second (steering) message cannot reach the harness. The mock harness
    /// reports which of the two happened; if someone keeps stdin open to
    /// implement steering, this test fails until the flag (and the port) say
    /// so — the flag may never be a lie in either direction.
    #[tokio::test]
    async fn steer_capability_matches_what_chat_does() {
        let temp = TempDir::new("steer");
        let script = temp.path().join("mock_steer_probe.sh");
        fs::write(
            &script,
            r#"#!/bin/sh
# The adapter writes one user message and then either keeps stdin open for a
# mid-turn steer or closes it. Report which, as the turn's answer.
IFS= read -r first
if IFS= read -r second; then
  steer_state="open"
else
  steer_state="closed"
fi
printf '{"type":"system","subtype":"init","session_id":"steer-probe"}\n'
printf '{"type":"result","result":"steer:%s","session_id":"steer-probe","usage":{"input_tokens":1,"output_tokens":1},"is_error":false}\n' "$steer_state"
"#,
        )
        .expect("write mock script");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let mut options = serde_json::Map::new();
        options.insert("binary".into(), json!(script.display().to_string()));
        let mut spec = spec_with_options(options);
        // A harness that kept stdin open without ever closing it would hang on
        // the second read; the timeout turns that into a failure instead of a
        // hung suite.
        spec.timeout_secs = 10;
        let harness = ClaudeCli::from_spec(spec).expect("valid spec");

        let (result, _events) = chat_turn(&harness, ChatRequest::new("", "")).await;
        let response = result.expect("chat completes");

        assert_eq!(
            response.text, "steer:closed",
            "chat must close stdin after the prompt"
        );
        assert!(
            !harness.capabilities().steer,
            "steer must stay false while chat() closes stdin and ChatRequest has no steer channel"
        );
    }

    /// `survives_restart` means a *new* adapter (what a restarted brain builds)
    /// picks the harness session back up. It cannot: the session id lives only
    /// in the old adapter's memory, because nothing writes
    /// `BotSessionRecord::native_session_id` and nothing replays it through
    /// `extra_body["native_session_id"]`. This pins the claim to that fact.
    #[test]
    fn a_fresh_adapter_after_a_restart_has_no_session_to_resume() {
        let temp = TempDir::new("restart");
        let mut options = serde_json::Map::new();
        options.insert("binary".into(), json!("/bin/echo"));
        options.insert(
            "config_dir".into(),
            json!(temp.path().display().to_string()),
        );

        let before = ClaudeCli::from_spec(spec_with_options(options.clone())).expect("valid");
        before.set_last_session_id(Some("session-before-restart".into()));
        assert_eq!(
            before.last_session_id().as_deref(),
            Some("session-before-restart")
        );

        // The adapter a restarted brain constructs knows nothing about it.
        let after_restart = ClaudeCli::from_spec(spec_with_options(options)).expect("valid");
        assert_eq!(after_restart.last_session_id(), None);
        assert!(
            !after_restart.capabilities().survives_restart,
            "no persisted session id means no native resume across a restart"
        );
    }

    #[test]
    fn options_override_paths_and_defaults_are_sane() {
        let default = ClaudeCli::from_spec(spec_with_options(Default::default())).expect("valid");
        assert_eq!(default.binary(), Path::new("claude"));
        assert!(default.config_dir().ends_with(".claude"));
        assert!(
            default
                .mcp_config_path()
                .ends_with(".claude/swarmbot-mcp.json")
        );

        let mut options = serde_json::Map::new();
        options.insert("binary".into(), json!("/opt/bin/claude"));
        options.insert("config_dir".into(), json!("/tmp/cfg"));
        options.insert("mcp_config_path".into(), json!("/tmp/mcp.json"));
        let custom = ClaudeCli::from_spec(spec_with_options(options)).expect("valid");
        assert_eq!(custom.binary(), Path::new("/opt/bin/claude"));
        assert_eq!(custom.config_dir(), Path::new("/tmp/cfg"));
        assert_eq!(custom.mcp_config_path(), Path::new("/tmp/mcp.json"));
    }

    #[test]
    fn a_wrong_kind_spec_is_rejected() {
        let mut spec = spec_with_options(Default::default());
        spec.kind = BackendKind::OpenAiCompat;
        let error = ClaudeCli::from_spec(spec).expect_err("wrong kind");
        assert!(matches!(error, LlmError::Unavailable { .. }));
    }

    #[tokio::test]
    async fn chat_isolates_sessions_per_request_cwd_and_respects_explicit_null() {
        let temp = TempDir::new("cwd-iso");
        let ws_a = temp.path().join("bot-a");
        let ws_b = temp.path().join("bot-b");
        fs::create_dir_all(&ws_a).expect("ws_a");
        fs::create_dir_all(&ws_b).expect("ws_b");
        let log_file = temp.path().join("invocations.log");
        let script = temp.path().join("mock_iso.sh");
        let log_str = log_file.display().to_string();

        fs::write(
            &script,
            format!(
                r#"#!/bin/sh
echo "CWD=$PWD ARGS=$*" >> "{log_str}"
SID="sid-$(basename "$PWD")"
cat << EOF
{{"type":"system","subtype":"init","session_id":"$SID"}}
{{"type":"result","result":"ok","session_id":"$SID","usage":{{"input_tokens":1,"output_tokens":1}},"is_error":false}}
EOF
"#
            ),
        )
        .expect("write script");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let mut options = serde_json::Map::new();
        options.insert("binary".into(), json!(script.display().to_string()));
        let harness = ClaudeCli::from_spec(spec_with_options(options)).expect("valid spec");
        harness.set_last_session_id(Some("global-leak-id".into()));

        // Turn 1 for bot-a: must NOT resume global-leak-id.
        let mut req_a1 = ChatRequest::new("claude-sonnet", "");
        req_a1.cwd = Some(ws_a.clone());
        chat_turn(&harness, req_a1).await.0.expect("a1");

        // Turn 1 for bot-b: must NOT resume bot-a's sid-bot-a or global-leak-id.
        let mut req_b1 = ChatRequest::new("claude-sonnet", "");
        req_b1.cwd = Some(ws_b.clone());
        chat_turn(&harness, req_b1).await.0.expect("b1");

        // Turn 2 for bot-a: resumes bot-a's own sid-bot-a.
        let mut req_a2 = ChatRequest::new("claude-sonnet", "");
        req_a2.cwd = Some(ws_a.clone());
        chat_turn(&harness, req_a2).await.0.expect("a2");

        // Turn 3 for bot-a with explicit native_session_id: null forces fresh session.
        let mut req_a3 = ChatRequest::new("claude-sonnet", "");
        req_a3.cwd = Some(ws_a.clone());
        req_a3
            .extra_body
            .insert("native_session_id".into(), Value::Null);
        chat_turn(&harness, req_a3).await.0.expect("a3");

        let logged = fs::read_to_string(&log_file).expect("read log");
        let lines: Vec<&str> = logged.lines().collect();
        assert_eq!(lines.len(), 4, "{logged}");
        assert!(!lines[0].contains("--resume"), "a1: {}", lines[0]);
        assert!(!lines[1].contains("--resume"), "b1: {}", lines[1]);
        assert!(lines[2].contains("--resume sid-bot-a"), "a2: {}", lines[2]);
        assert!(!lines[3].contains("--resume"), "a3: {}", lines[3]);
    }

    /// A stand-in harness that reports, from inside the turn, which
    /// `--mcp-config` it was handed: the argv line, the file's path, a copy of
    /// its bytes (it is deleted when the turn ends) and the file's mode.
    #[cfg(unix)]
    fn mcp_probe_script(temp: &TempDir, log: &Path, copy: &Path) -> PathBuf {
        let script = temp.path().join("mock_mcp_probe.sh");
        let body = r#"#!/bin/sh
LOG="@LOG@"
COPY="@COPY@"
CFG=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--mcp-config" ]; then CFG="$a"; fi
  prev="$a"
done
printf 'ARGS=%s\n' "$*" >> "$LOG"
printf 'CFG=%s\n' "$CFG" >> "$LOG"
if [ -n "$CFG" ] && [ -f "$CFG" ]; then
  cp "$CFG" "$COPY"
  stat -c '%a' "$CFG" >> "$LOG"
fi
printf '%s\n' '{"type":"system","subtype":"init","session_id":"mcp-turn"}'
printf '%s\n' '{"type":"result","result":"ok","session_id":"mcp-turn","usage":{"input_tokens":1,"output_tokens":1},"is_error":false}'
"#
        .replace("@LOG@", &log.display().to_string())
        .replace("@COPY@", &copy.display().to_string());
        fs::write(&script, body).expect("write mock script");
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        script
    }

    /// The per-turn servers a caller registered are what the harness gets:
    /// `ChatRequest::mcp_servers` is materialized into a private config file
    /// that names the turn's own URL, and the file is gone when the turn ends.
    ///
    /// This is the test that fails without the fix: before it, `chat` read only
    /// the static path, so the runner's per-turn endpoint was registered and
    /// then silently discarded — the model had no team tools while the audit
    /// line said the turn had them.
    #[cfg(unix)]
    #[tokio::test]
    async fn per_turn_mcp_servers_are_written_to_a_private_config_file_and_deleted() {
        let temp = TempDir::new("turn-mcp");
        let log = temp.path().join("probe.log");
        let copy = temp.path().join("config-copy.json");
        let script = mcp_probe_script(&temp, &log, &copy);

        // A static config exists and is *not* what this turn may use: it names
        // the owner-scoped route with a different credential.
        let static_cfg = temp.path().join("static-mcp.json");
        fs::write(
            &static_cfg,
            json!({
                "mcpServers": {
                    "swarmbot": {
                        "type": "http",
                        "url": "http://static.invalid/mcp",
                        "headers": {"Authorization": "Bearer static-token"},
                    }
                }
            })
            .to_string(),
        )
        .expect("write static config");

        let mut options = serde_json::Map::new();
        options.insert("binary".into(), json!(script.display().to_string()));
        options.insert(
            "mcp_config_path".into(),
            json!(static_cfg.display().to_string()),
        );
        // The turn's config file is created here, inside this test's TempDir:
        // no test writes outside one, and the deletion assertion below is about
        // a directory this test owns.
        options.insert(
            "mcp_config_dir".into(),
            json!(temp.path().display().to_string()),
        );
        let harness = ClaudeCli::from_spec(spec_with_options(options)).expect("valid spec");

        let mut request = ChatRequest::new("", "");
        request.mcp_servers = vec![
            McpServerSpec::Http {
                name: "swarmbot".into(),
                url: "http://127.0.0.1:9/mcp/t/turn-token-9f3".into(),
                headers: BTreeMap::from([(
                    "Authorization".to_string(),
                    "Bearer turn-secret".to_string(),
                )]),
            },
            McpServerSpec::Stdio {
                name: "local-tools".into(),
                command: "/bin/true".into(),
                args: vec!["--flag".into()],
                env: BTreeMap::from([("KEY".to_string(), "value".to_string())]),
            },
        ];
        chat_turn(&harness, request).await.0.expect("the turn runs");

        let logged = fs::read_to_string(&log).expect("read log");
        let cfg_path = logged
            .lines()
            .find_map(|line| line.strip_prefix("CFG="))
            .expect("the harness reports its --mcp-config argument");
        let cfg_path = PathBuf::from(cfg_path);
        assert_ne!(
            cfg_path, static_cfg,
            "a turn that registered servers must not fall back to the static config"
        );
        assert!(
            cfg_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("swarmbot-turn-mcp-")),
            "{cfg_path:?}"
        );
        assert!(
            logged.contains("--strict-mcp-config"),
            "only the turn's servers may be visible: {logged}"
        );

        let content = fs::read_to_string(&copy).expect("the child saw a config file");
        let parsed: Value = serde_json::from_str(&content).expect("the config is JSON");
        assert_eq!(parsed["mcpServers"]["swarmbot"]["type"], json!("http"));
        assert_eq!(
            parsed["mcpServers"]["swarmbot"]["url"],
            json!("http://127.0.0.1:9/mcp/t/turn-token-9f3"),
            "the file must name this turn's URL"
        );
        assert_eq!(
            parsed["mcpServers"]["swarmbot"]["headers"]["Authorization"],
            json!("Bearer turn-secret")
        );
        assert_eq!(parsed["mcpServers"]["local-tools"]["type"], json!("stdio"));
        assert_eq!(
            parsed["mcpServers"]["local-tools"]["command"],
            json!("/bin/true")
        );
        assert_eq!(
            parsed["mcpServers"]["local-tools"]["args"][0],
            json!("--flag")
        );
        assert_eq!(
            parsed["mcpServers"]["local-tools"]["env"]["KEY"],
            json!("value")
        );
        assert!(
            !content.contains("static-token"),
            "the static credential must not leak into the turn's file: {content}"
        );

        assert!(
            logged.lines().any(|line| line == "600"),
            "the file carries a bearer token and must be 0600: {logged}"
        );
        assert!(
            !cfg_path.exists(),
            "the turn's MCP config must be deleted once the turn ends"
        );
    }

    /// A caller that registered nothing keeps the documented static-path
    /// behaviour (the owner's own file, no `--strict-mcp-config`). This pins
    /// the fallback so the per-turn fix cannot quietly drop it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_turn_without_registered_servers_keeps_the_static_mcp_config() {
        let temp = TempDir::new("static-mcp");
        let log = temp.path().join("probe.log");
        let copy = temp.path().join("config-copy.json");
        let script = mcp_probe_script(&temp, &log, &copy);

        let static_cfg = temp.path().join("static-mcp.json");
        fs::write(&static_cfg, json!({"mcpServers": {}}).to_string()).expect("write config");

        let mut options = serde_json::Map::new();
        options.insert("binary".into(), json!(script.display().to_string()));
        options.insert(
            "mcp_config_path".into(),
            json!(static_cfg.display().to_string()),
        );
        let harness = ClaudeCli::from_spec(spec_with_options(options)).expect("valid spec");

        chat_turn(&harness, ChatRequest::new("", ""))
            .await
            .0
            .expect("the turn runs");

        let logged = fs::read_to_string(&log).expect("read log");
        assert!(
            logged.contains(&format!("--mcp-config {}", static_cfg.display())),
            "{logged}"
        );
        assert!(
            !logged.contains("--strict-mcp-config"),
            "the static path is the operator's own config; nothing narrows it: {logged}"
        );
    }

    /// If the turn's config cannot be written, the turn is **refused** rather
    /// than run without the tools the caller registered. Running anyway is the
    /// silent-lie case this whole item exists to remove: the runner would have
    /// recorded an MCP-bound turn whose model never saw a single team tool.
    #[tokio::test]
    async fn a_turn_whose_mcp_config_cannot_be_written_is_refused_not_run() {
        let temp = TempDir::new("mcp-write-fail");
        let told = temp.path().join("spawned.log");
        let script = temp.path().join("mock_never.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho spawned >> \"{}\"\nprintf '%s\\n' '{{\"type\":\"result\",\"result\":\"ran\",\"is_error\":false}}'\n",
                told.display()
            ),
        )
        .expect("write script");
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let mut options = serde_json::Map::new();
        options.insert("binary".into(), json!(script.display().to_string()));
        // A directory that does not exist: the write must fail.
        options.insert(
            "mcp_config_dir".into(),
            json!(temp.path().join("missing-dir").display().to_string()),
        );
        let harness = ClaudeCli::from_spec(spec_with_options(options)).expect("valid spec");

        let mut request = ChatRequest::new("", "");
        request.mcp_servers = vec![McpServerSpec::http("swarmbot", "http://127.0.0.1:9/mcp")];
        let (result, _events) = chat_turn(&harness, request).await;

        match result {
            Err(LlmError::Unavailable { reason, .. }) => {
                assert!(reason.contains("MCP config"), "{reason}");
                assert!(
                    reason.contains("refusing the turn"),
                    "the refusal must say the turn did not run: {reason}"
                );
            }
            other => panic!("a turn that cannot carry its MCP servers must be refused: {other:?}"),
        }
        assert!(
            !told.exists(),
            "the harness must never be spawned for a refused turn"
        );
    }
}
