//! CLI-harness adapters: backends that drive an agent CLI instead of an HTTP
//! API. This milestone ships the **skeleton** for `claude -p`; a later
//! milestone finishes it. What the finished adapter will do:
//!
//! * spawn
//!   `claude -p --output-format stream-json --input-format stream-json --mcp-config <file> [--resume <id>]`
//!   as a long-lived child with piped stdio;
//! * write each turn as a stream-json user message on stdin (steering =
//!   writing another message while the turn runs, which the harness
//!   supports — hence `capabilities().steer == true`);
//! * read the harness's NDJSON events from stdout and normalize them into
//!   [`LlmEvent`]s: assistant text deltas, `thinking` blocks as
//!   `ReasoningDelta`, `tool_use`/`tool_result` as informational events (the
//!   harness executes tools itself through the MCP servers our
//!   `mcp_config` points at), and the final `result` message as `Finished`;
//! * keep the `session_id` from the harness's `init` message so the next turn
//!   resumes with `--resume <id>` — that is what makes
//!   `capabilities().survives_restart` true and what lands in
//!   `BotSessionRecord::native_session_id`.
//!
//! The crucial capability invariant is already load-bearing today:
//! `tool_calls == false`. The harness runs its **own** tool loop through our
//! MCP server, so the runner must not drive one — if it did, tool calls would
//! execute twice or deadlock waiting for results the harness already
//! consumed. [`BackendKind::owns_tool_loop`] says the same thing from the
//! config side.
//!
//! What already works in this milestone: construction from a
//! [`BackendSpec`] and an honest [`LlmBackend::probe`] that checks the binary
//! exists and answers `--version` — enough for the Settings screen to show
//! "claude CLI installed / not found" before the adapter itself exists.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use swarmbot_contracts::{BackendKind, BackendSpec, Usage};

use crate::backend::{LlmBackend, LlmError, emit};
use crate::openai::truncate_utf8;
use crate::types::{
    BackendStatus, Capabilities, ChatRequest, ChatResponse, FinishReason, LlmEvent, Role,
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
            // stream-json input accepts a new user message mid-turn.
            steer: true,
            // `--resume <session_id>` picks the session up after a restart.
            survives_restart: true,
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

        let mcp_cfg =
            if self.spec.option_str("mcp_config_path").is_some() || self.mcp_config_path.exists() {
                Some(self.mcp_config_path.clone())
            } else if let Some(data_dir) = self.spec.option_str("data_dir").map(PathBuf::from)
                && data_dir.join("swarmbot-mcp.json").exists()
            {
                Some(data_dir.join("swarmbot-mcp.json"))
            } else {
                None
            };
        if let Some(ref path) = mcp_cfg {
            cmd.arg("--mcp-config").arg(path);
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
        } else if let Ok(status) = exit_status
            && !status.success()
        {
            return Err(LlmError::Unavailable {
                backend: self.spec.id.clone(),
                reason: format!(
                    "`{}` exited prematurely with status: {status}",
                    self.binary.display()
                ),
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
            }),
            Err(error) => Ok(BackendStatus {
                backend,
                ok: false,
                detail: format!("cannot run `{}`: {error}", self.binary.display()),
                model,
                version: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
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

        let (tx, mut rx) = mpsc::channel(16);
        let response = harness
            .chat(
                ChatRequest::new("claude-sonnet", "You are helpful"),
                tx,
                CancellationToken::new(),
            )
            .await
            .expect("chat completes");

        assert_eq!(response.text, "Mock response!");
        assert_eq!(response.usage.input_tokens, 15);
        assert_eq!(response.usage.output_tokens, 4);
        assert_eq!(harness.last_session_id(), Some("mock-session-42".into()));

        let mut received = Vec::new();
        while let Ok(event) = rx.try_recv() {
            received.push(event);
        }
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

        let (tx, _rx) = mpsc::channel(16);
        let resp = harness
            .chat(
                ChatRequest::new("claude-sonnet", ""),
                tx,
                CancellationToken::new(),
            )
            .await
            .expect("chat completes");

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

        let (tx, _rx) = mpsc::channel(16);
        let err = harness
            .chat(
                ChatRequest::new("claude-sonnet", ""),
                tx,
                CancellationToken::new(),
            )
            .await
            .expect_err("should fail on is_error");

        match err {
            LlmError::Unavailable { reason, .. } => {
                assert!(reason.contains("Not logged in"), "{reason}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
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

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });

        let (tx, _rx) = mpsc::channel(16);
        let err = harness
            .chat(ChatRequest::new("claude-sonnet", ""), tx, cancel)
            .await
            .expect_err("should cancel");

        assert!(matches!(err, LlmError::Cancelled));
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
        assert!(capabilities.steer);
        assert!(capabilities.survives_restart);
        assert_eq!(harness.kind(), BackendKind::ClaudeCli);
        assert!(BackendKind::ClaudeCli.owns_tool_loop());
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
        let (tx, _rx) = mpsc::channel(4);
        harness
            .chat(req_a1, tx, CancellationToken::new())
            .await
            .expect("a1");

        // Turn 1 for bot-b: must NOT resume bot-a's sid-bot-a or global-leak-id.
        let mut req_b1 = ChatRequest::new("claude-sonnet", "");
        req_b1.cwd = Some(ws_b.clone());
        let (tx, _rx) = mpsc::channel(4);
        harness
            .chat(req_b1, tx, CancellationToken::new())
            .await
            .expect("b1");

        // Turn 2 for bot-a: resumes bot-a's own sid-bot-a.
        let mut req_a2 = ChatRequest::new("claude-sonnet", "");
        req_a2.cwd = Some(ws_a.clone());
        let (tx, _rx) = mpsc::channel(4);
        harness
            .chat(req_a2, tx, CancellationToken::new())
            .await
            .expect("a2");

        // Turn 3 for bot-a with explicit native_session_id: null forces fresh session.
        let mut req_a3 = ChatRequest::new("claude-sonnet", "");
        req_a3.cwd = Some(ws_a.clone());
        req_a3
            .extra_body
            .insert("native_session_id".into(), Value::Null);
        let (tx, _rx) = mpsc::channel(4);
        harness
            .chat(req_a3, tx, CancellationToken::new())
            .await
            .expect("a3");

        let logged = fs::read_to_string(&log_file).expect("read log");
        let lines: Vec<&str> = logged.lines().collect();
        assert_eq!(lines.len(), 4, "{logged}");
        assert!(!lines[0].contains("--resume"), "a1: {}", lines[0]);
        assert!(!lines[1].contains("--resume"), "b1: {}", lines[1]);
        assert!(lines[2].contains("--resume sid-bot-a"), "a2: {}", lines[2]);
        assert!(!lines[3].contains("--resume"), "a3: {}", lines[3]);
    }
}
