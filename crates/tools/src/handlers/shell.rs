//! `shell` — run a command on a node through `sh -c`.
//!
//! Safety shape of every run, in one place:
//!
//! * the child gets **its own process group** (`process_group(0)`), and the
//!   whole group dies — `killpg(SIGKILL)` — on timeout, on cancellation, when
//!   the streaming transport goes away, and after a normal exit whose pipes are
//!   still held open by background children. Invariant: **when `shell` returns,
//!   nothing it started is still attached to it.** A command that wants to
//!   leave a daemon behind must detach it (`setsid` with redirected
//!   descriptors);
//! * the child environment is **scrubbed to an allowlist** — the brain's
//!   process holds API keys and node secrets, and a model-typed command line
//!   must never be one `env` away from reading them;
//! * `cwd` goes through [`PathPolicy`](crate::PathPolicy), so a command
//!   cannot be aimed outside the node's roots;
//! * each output stream is capped at [`SHELL_OUTPUT_LIMIT_BYTES`]; the buffered
//!   result then goes through the registry's spill like any other tool, and the
//!   streaming shape spills its overflow itself to a 0600 file in the caller's
//!   spill directory (see [`ShellOptions::spill_dir`]).
//!
//! # Two shapes, one implementation
//!
//! [`exec_shell`] is the *buffered* shape: one [`ToolResult`], which is what a
//! model reads and what [`crate::LocalExecutor`] hands back to the brain.
//! [`exec_shell_streaming`] is the *streaming* shape: `Started{pid,pgid}` before
//! any output, chunks as they are produced, and an exit that says how the run
//! ended — what a socket needs, and what `crates/node/src/shell.rs` adapts onto
//! the wire. Both live here, on purpose: they were once two copies in two
//! crates whose spawn/killpg/spill invariants had to be kept in sync by hand
//! and across a crate boundary. What they share is the part that decides how a
//! command is run — [`build_shell_command`] (own process group, scrubbed
//! environment, null stdin, both pipes), the timing constants and the
//! process-group helpers. What they do *not* share is the run loop itself: the
//! buffered one waits for one exit and formats a `ToolResult`, the streaming one
//! feeds an emitter and can be cut short by its transport. Those two loops are
//! adjacent in this file on purpose, so a change to one is a change you can see
//! next to the other instead of a change in another crate.

use std::collections::BTreeMap;
use std::future::Future;
use std::io::Write as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use swarmbot_contracts::{
    SHELL_OUTPUT_LIMIT_BYTES, Surface, ToolError, ToolResult, ToolSpec, Toolset,
};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{
    Gate, TOOL_SHELL, ToolEnv, forward_remote, gate_call, opt_env_map, opt_str, opt_u64, req_str,
    stamp_meta, target_node,
};
use crate::handler::{NodeCall, ToolContext, ToolHandler};

/// Default per-command budget.
pub const DEFAULT_SHELL_TIMEOUT_MS: u64 = 120_000;
/// Hard ceiling for `timeout_ms`: a bot command is not a batch job, and a
/// hung command must not pin a turn for longer than this.
pub const MAX_SHELL_TIMEOUT_MS: u64 = 15 * 60 * 1000;

/// The only variables a child inherits. Everything else — API keys, node
/// keys, `DATABASE_URL`, the owner's shell profile — stays in the brain.
pub const ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TERM", "TMPDIR"];

/// `PATH` for processes that do not export one (daemons started from init):
/// without it even `ls` would fail and the model could not diagnose why.
pub const FALLBACK_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// After a *normal* exit, give background children this long to release the
/// output pipes before the group is killed (see the module docs).
pub const EXIT_GRACE: Duration = Duration::from_millis(500);
/// After a kill, this long for the readers to see EOF before we take whatever
/// they captured so far.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// After killing a group whose pipes were held open, let the pumps finish
/// flushing into the shared captures before snapshotting.
pub const SETTLE: Duration = Duration::from_millis(200);

pub fn spec() -> ToolSpec {
    ToolSpec::new(
        TOOL_SHELL,
        "Run a command with `sh -c` on a node. The child runs in its own process group with a \
         scrubbed environment (allowlist: PATH, HOME, LANG, LC_ALL, TERM, TMPDIR, plus your \
         `env`) and is killed — group and all — on timeout (default 120000 ms, maximum 900000) \
         or when the owner stops the turn; nothing survives the call, so detach daemons with \
         setsid and redirected output. stdout and stderr are captured separately and reported \
         as `exit=<code>` plus sections; a non-zero exit is a tool error. Output beyond the \
         result cap is spilled to a file you can page with read_file. `cwd` must lie inside a \
         node root.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The command line, run with `sh -c`."
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory (inside a node root); defaults to the bot's workspace."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 900000,
                    "description": "Kill the command after this many milliseconds; default 120000, hard maximum 900000."
                },
                "env": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Extra environment variables for this command only."
                },
                "node": {
                    "type": "string",
                    "description": "Node id (or \"local\") to run on; defaults to the bot's host node."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
        Toolset::Node,
    )
    .needs_node()
}

pub struct ShellHandler {
    env: Arc<ToolEnv>,
}

impl ShellHandler {
    pub fn new(env: Arc<ToolEnv>) -> Self {
        Self { env }
    }
}

impl std::fmt::Debug for ShellHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellHandler").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl ToolHandler for ShellHandler {
    fn spec(&self) -> ToolSpec {
        spec()
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let command = req_str(&args, "command")?;
        let cwd_arg = opt_str(&args, "cwd");
        let timeout_ms = opt_u64(&args, "timeout_ms")?.unwrap_or(DEFAULT_SHELL_TIMEOUT_MS);
        let env = opt_env_map(&args)?.unwrap_or_default();
        let node = target_node(ctx, &args)?;

        if !node.is_local() {
            return forward_remote(
                ctx,
                &self.env,
                TOOL_SHELL,
                node,
                NodeCall::ShellExec {
                    command,
                    cwd: cwd_arg,
                    timeout_ms: Some(timeout_ms),
                    env: Some(env),
                },
            )
            .await;
        }

        // The cwd only needs to exist inside a root, not be writable: a
        // read-only root may still be a fine place to run `ls` or a build
        // that writes nowhere. What the command does inside is the ceiling's
        // business, not the path policy's.
        let cwd = match &cwd_arg {
            Some(raw) => self
                .env
                .policy
                .resolve(Path::new(raw), &ctx.workspace, false)?,
            None => ctx.workspace.clone(),
        };

        // `shell` is the one handler whose paths do not go through
        // `PathPolicy::resolve`: it hands a string to a shell. Scanning for a
        // deny-listed path is a word heuristic, not a sandbox, but it turns the
        // direct spelling (`cat ~/.ssh/id_rsa`) from "works" into a refusal.
        // See `PathPolicy::denied_reference` for what it does not cover.
        if let Some((word, resolved)) = self.env.policy.denied_reference(&command, &cwd) {
            let gate = Gate::Refused(format!(
                "`{word}` resolves to `{resolved}`, which is on this node's deny list"
            ));
            let mut result = gate
                .blocked_result(TOOL_SHELL, &command)
                .unwrap_or_else(|| ToolResult::error("refused by policy"));
            stamp_meta(&mut result, &gate, Surface::Shell, &command, &node);
            return Ok(result);
        }

        let gate = gate_call(
            ctx,
            &self.env,
            Surface::Shell,
            TOOL_SHELL,
            &command,
            false,
            Some(&command),
        )
        .await?;
        if let Some(mut result) = gate.blocked_result(TOOL_SHELL, &command) {
            stamp_meta(&mut result, &gate, Surface::Shell, &command, &node);
            return Ok(result);
        }

        let mut result = exec_shell(&command, &cwd, timeout_ms, &env, &ctx.cancel).await?;
        stamp_meta(&mut result, &gate, Surface::Shell, &command, &node);
        Ok(result)
    }
}

/// One captured stream: bytes up to the cap, and a count of what was dropped
/// after it. Sharing through a mutex lets the pump keep running while the
/// caller decides whether to wait for EOF — a snapshot is always available.
#[derive(Default)]
struct Capture {
    buf: Mutex<Vec<u8>>,
    dropped: AtomicUsize,
}

impl Capture {
    fn snapshot(&self) -> (String, usize) {
        // The mutex is never held across an await and the locked code cannot
        // panic, so a poison error is impossible; recover defensively anyway
        // because a tool call must never take the process down.
        let buf = match self.buf.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        (
            String::from_utf8_lossy(&buf).into_owned(),
            self.dropped.load(Ordering::SeqCst),
        )
    }
}

async fn pump<R: AsyncRead + Unpin + 'static>(mut reader: R, sink: Arc<Capture>) {
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                let dropped = {
                    let mut buf = match sink.buf.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    let room = SHELL_OUTPUT_LIMIT_BYTES.saturating_sub(buf.len());
                    let take = n.min(room);
                    buf.extend_from_slice(&chunk[..take]);
                    n - take
                };
                sink.dropped.fetch_add(dropped, Ordering::SeqCst);
            }
            // A read error is treated as EOF; the exit status tells the real
            // story and the partial output stays available.
            Err(_) => break,
        }
    }
}

/// Kill a whole process group. Returns `true` when the group is gone
/// afterwards, which includes "was already gone" (`ESRCH`) — the outcome the
/// caller wanted either way.
pub fn kill_process_group(pgid: u32) -> bool {
    let Ok(pid) = i32::try_from(pgid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    match killpg(Pid::from_raw(pid), Signal::SIGKILL) {
        Ok(_) => true,
        Err(nix::errno::Errno::ESRCH) => true,
        Err(error) => {
            tracing::warn!(pgid, %error, "killpg failed; process group may still be running");
            false
        }
    }
}

/// Is a process group gone? `killpg(pgid, 0)` answering `ESRCH` — the check the
/// tests use and the one an operator would run.
pub fn process_group_gone(pgid: u32) -> bool {
    let Ok(pid) = i32::try_from(pgid) else {
        return true;
    };
    if pid <= 0 {
        return true;
    }
    use nix::sys::signal::killpg;
    use nix::unistd::Pid;
    match killpg(Pid::from_raw(pid), None) {
        Ok(_) => false,
        Err(nix::errno::Errno::ESRCH) => true,
        Err(_) => false,
    }
}

/// Kill the child's whole process group if a pid is present.
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = kill_process_group(pid);
    }
}

/// Build the standard `sh -c` command with process isolation, environment scrubbing,
/// and allowlisted variables. Shared by local and daemon runners so execution invariants
/// cannot drift.
pub fn build_shell_command(
    command: &str,
    cwd: &Path,
    extra_env: &BTreeMap<String, String>,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The child must die even if this process panics between spawn and
        // the explicit kill.
        .kill_on_drop(true)
        // Own process group: one killpg then reaches every grandchild the
        // command started, which a plain child.kill() never would.
        .process_group(0);

    // Scrub, then allowlist, then the caller's explicit env (which may
    // override an allowlisted value but can never reveal an unlisted one).
    cmd.env_clear();
    for key in ENV_ALLOWLIST {
        if let Ok(value) = std::env::var(key) {
            cmd.env(key, value);
        }
    }
    if std::env::var_os("PATH").is_none() {
        cmd.env("PATH", FALLBACK_PATH);
    }
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd
}

/// Await both pumps, tolerating already-finished or absent tasks.
async fn join_pumps(stdout: Option<JoinHandle<()>>, stderr: Option<JoinHandle<()>>) {
    let left = async {
        if let Some(task) = stdout {
            let _ = task.await;
        }
    };
    let right = async {
        if let Some(task) = stderr {
            let _ = task.await;
        }
    };
    tokio::join!(left, right);
}

/// The **buffered** implementation: one `ToolResult` at the end, which is what
/// the handler (local node) and [`crate::LocalExecutor`] (a node daemon
/// answering an `fs`-shaped call) use, so both behave identically. The streaming
/// sibling is [`exec_shell_streaming`]; both spawn through
/// [`build_shell_command`], so the isolation rules are one body of code.
///
/// `cwd` must already be resolved through the executing node's path policy,
/// and `command` must already have passed the approval gate.
pub async fn exec_shell(
    command: &str,
    cwd: &Path,
    timeout_ms: u64,
    extra_env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<ToolResult, ToolError> {
    // The hard cap applies on every path in, including a NodeCall from a
    // remote brain that never saw the schema.
    let timeout = Duration::from_millis(timeout_ms.clamp(1, MAX_SHELL_TIMEOUT_MS));
    let started = std::time::Instant::now();

    let mut cmd = build_shell_command(command, cwd, extra_env);

    let mut child = cmd.spawn().map_err(|error| {
        ToolError::failed(format!(
            "cannot run `sh -c` (cwd {}): {error}",
            cwd.display()
        ))
    })?;
    // Grab the pid before anything consumes the child: with process_group(0)
    // it is also the group id used for killing.
    let pid = child.id();

    let stdout_cap = Arc::new(Capture::default());
    let stderr_cap = Arc::new(Capture::default());
    let stdout_task = child.stdout.take().map(|pipe| {
        let sink = Arc::clone(&stdout_cap);
        tokio::spawn(pump(pipe, sink))
    });
    let stderr_task = child.stderr.take().map(|pipe| {
        let sink = Arc::clone(&stderr_cap);
        tokio::spawn(pump(pipe, sink))
    });

    enum End {
        Exited(std::process::ExitStatus),
        TimedOut,
        Cancelled,
    }
    let end = tokio::select! {
        status = child.wait() => match status {
            Ok(status) => End::Exited(status),
            Err(error) => {
                kill_group(pid);
                return Err(ToolError::failed(format!("cannot wait for the command: {error}")));
            }
        },
        _ = tokio::time::sleep(timeout) => End::TimedOut,
        _ = cancel.cancelled() => End::Cancelled,
    };

    match end {
        End::Cancelled => {
            // The owner stopped the turn: kill first, reap second, and never
            // spend time on output nobody will read. The pump tasks are
            // dropped (detached); their captures die with them.
            kill_group(pid);
            let _ = child.wait().await;
            Err(ToolError::Cancelled)
        }
        End::TimedOut => {
            kill_group(pid);
            // Reap the leader and let the pumps observe EOF so the partial
            // output in the error is complete up to the kill.
            let _ = child.wait().await;
            let _ = tokio::time::timeout(DRAIN_TIMEOUT, join_pumps(stdout_task, stderr_task)).await;
            let (stdout, out_dropped) = stdout_cap.snapshot();
            let (stderr, err_dropped) = stderr_cap.snapshot();
            let mut text = format!(
                "ERROR: command timed out after {} ms; killed its process group",
                timeout.as_millis()
            );
            append_sections(&mut text, &stdout, &stderr, out_dropped + err_dropped);
            let mut result = ToolResult {
                content: vec![swarmbot_contracts::Content::Text { text }],
                is_error: true,
                meta: serde_json::Map::new(),
            };
            result.meta.insert("timed_out".into(), Value::Bool(true));
            result.meta.insert(
                "duration_ms".into(),
                Value::from(started.elapsed().as_millis() as u64),
            );
            Ok(result)
        }
        End::Exited(status) => {
            // Normal exit — but a `cmd &` child may still hold the pipes
            // open. Wait briefly for EOF; if the pipes stay open, kill the
            // group so the call returns and nothing outlives it (module
            // docs), then snapshot whatever the pumps captured.
            if tokio::time::timeout(EXIT_GRACE, join_pumps(stdout_task, stderr_task))
                .await
                .is_err()
            {
                tracing::debug!(
                    command,
                    "background children kept the output pipes open; killing the process group"
                );
                kill_group(pid);
                tokio::time::sleep(SETTLE).await;
            }
            let (stdout, out_dropped) = stdout_cap.snapshot();
            let (stderr, err_dropped) = stderr_cap.snapshot();

            let code_text = match status.code() {
                Some(code) => code.to_string(),
                // Killed by a signal (segfault, an external kill): say which.
                None => format!("signal:{}", status.signal().unwrap_or(-1)),
            };
            let mut text = format!("exit={code_text}");
            append_sections(&mut text, &stdout, &stderr, out_dropped + err_dropped);

            let mut result = ToolResult {
                content: vec![swarmbot_contracts::Content::Text { text }],
                // A non-zero exit is an error *result* so models treat it as
                // a failure — but the text keeps the `exit=<code>` shape
                // instead of an `ERROR: ` prefix, because the exit line
                // already reads as a failure on its own and the code must
                // stay machine-findable (contracts/tool.rs result rules).
                is_error: !status.success(),
                meta: serde_json::Map::new(),
            };
            if let Some(code) = status.code() {
                result.meta.insert("exit_code".into(), Value::from(code));
            }
            result.meta.insert(
                "duration_ms".into(),
                Value::from(started.elapsed().as_millis() as u64),
            );
            Ok(result)
        }
    }
}

/// `exit=<code>` result body: stdout and stderr sections, empty ones omitted,
/// plus an honest note when a stream was capped.
fn append_sections(text: &mut String, stdout: &str, stderr: &str, dropped: usize) {
    if !stdout.is_empty() {
        text.push_str("\n--- stdout ---\n");
        text.push_str(stdout);
        if !stdout.ends_with('\n') {
            text.push('\n');
        }
    }
    if !stderr.is_empty() {
        text.push_str("--- stderr ---\n");
        text.push_str(stderr);
        if !stderr.ends_with('\n') {
            text.push('\n');
        }
    }
    if dropped > 0 {
        text.push_str(&format!(
            "[output capped: {dropped} more bytes were discarded after {SHELL_OUTPUT_LIMIT_BYTES} bytes per stream]"
        ));
    }
    // Trim the very last newline: result text is displayed as-is.
    if text.ends_with('\n') {
        text.pop();
    }
}

// ---------------------------------------------------------------------------
// Streaming execution
// ---------------------------------------------------------------------------
//
// A node daemon cannot use the buffered shape above: it must tell the brain the
// pid *before* output arrives (an interrupt has to kill a real process group),
// forward chunks as they are produced, and report an exit that distinguishes
// "exited 3" from "timed out" from "the brain went away". This half of the
// module is that shape.
//
// The invariants are the buffered path's, because it is the same command being
// run — own process group, `killpg` on timeout/cancel/transport failure/
// shutdown, scrubbed environment, per-stream cap with a 0600 spill file — and
// the two paths share `build_shell_command`, the timing constants and the group
// helpers. What is added is the streaming half: one emitter task, because the
// transport takes a single event at a time and the emitter's `await` on it is
// the back-pressure; and a guard whose `Drop` kills the group, which is what
// runs when the caller drops this future on an interrupt.

/// Un-forwarded events allowed between the pumps and the emitter. Small on
/// purpose: the emitter awaits the transport's acknowledgement per event, so a
/// larger buffer would only move the back-pressure into memory.
const EMIT_QUEUE: usize = 32;

/// Which output stream a chunk came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellStream {
    Stdout,
    Stderr,
}

/// Everything a streamed run tells its caller, in the order it happens:
/// exactly one `Started`, then any number of `Output`, then exactly one `Exit`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellEvent {
    /// Sent before any output can arrive, so the caller knows which process
    /// group an interrupt has to kill.
    Started {
        pid: u32,
        pgid: u32,
    },
    Output {
        stream: ShellStream,
        data: String,
        /// One sequence for both streams, so the reader can put them back into
        /// the order they were produced.
        seq: u64,
    },
    /// A diagnostic from the runner itself, not output the command produced —
    /// currently only the reason a run was killed. It is an event rather than
    /// part of [`ShellExit`] because it must reach the reader *before* the exit
    /// and must not be confused with program output; the transport decides how
    /// to render it (the node puts it on stderr, prefixed).
    Note {
        data: String,
        seq: u64,
    },
    Exit(ShellExit),
}

/// The last event of a run.
///
/// A non-zero `code` is **not** a failure: the command ran and reported its
/// status, which is data the model should see. `killed` is set when this side
/// ended the run instead of the command ending itself; the caller is expected to
/// surface it as a diagnostic, because a silent kill reads like a crash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellExit {
    /// `None` when a signal ended the child.
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// True when a stream hit the cap and the overflow went to `spill_path`.
    pub output_truncated: bool,
    pub spill_path: Option<PathBuf>,
    pub killed: Option<String>,
}

/// Where a streamed run hands its events, one at a time.
///
/// `send` must not resolve until the transport has actually taken the event:
/// that await *is* the back-pressure that keeps a chatty command from
/// outrunning a socket, and it is why a bounded queue of [`EMIT_QUEUE`] events
/// is enough. An `Err` means the stream is gone — the run then stops and kills
/// the group rather than producing output nobody receives.
///
/// The future is boxed by hand instead of via `async-trait` so that an
/// implementor in another crate needs no extra dependency; `swarmbot-node`'s
/// `ChunkSink` adapter is exactly that implementor.
pub trait ShellEventSink: Send + Sync {
    fn send<'a>(
        &'a self,
        event: ShellEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellFailure>> + Send + 'a>>;

    /// Resolves when the caller no longer wants this run; the runner selects on
    /// it and kills the group when it trips.
    ///
    /// A future rather than a shared token because the transport owns its own
    /// cancellation primitive: `swarmbot-node-protocol` has its own
    /// `CancellationToken` (it does not depend on `tokio-util`), and this crate
    /// must not force one on it.
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Every process group a caller has started, so a shutdown can kill them all
/// even if a handler's future was leaked rather than dropped.
#[derive(Clone, Debug, Default)]
pub struct ProcRegistry(Arc<Mutex<Vec<u32>>>);

impl ProcRegistry {
    pub fn new() -> ProcRegistry {
        ProcRegistry::default()
    }

    /// Number of live groups.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take ownership of a group for the lifetime of the returned guard.
    pub fn register(&self, pgid: u32) -> ProcGuard {
        self.lock().push(pgid);
        ProcGuard {
            registry: Arc::clone(&self.0),
            pgid,
            armed: true,
        }
    }

    /// Kill everything still registered. Called on SIGINT/SIGTERM before a node
    /// socket is closed, so a stopping daemon leaves no orphan behind.
    pub fn kill_all(&self) {
        let groups = std::mem::take(&mut *self.lock());
        for pgid in groups {
            kill_process_group(pgid);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<u32>> {
        // A poisoned lock only means a panic between lock and unlock; the vec
        // itself is consistent, so recover instead of propagating (AGENTS.md).
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// A process group this call owns. **Killing it is the `Drop` behaviour**,
/// which is the whole point: when a caller drops the handler future on an
/// interrupt, or the control socket dies, this runs — no code path has to
/// remember to.
#[derive(Debug)]
pub struct ProcGuard {
    registry: Arc<Mutex<Vec<u32>>>,
    pgid: u32,
    armed: bool,
}

impl ProcGuard {
    pub fn pgid(&self) -> u32 {
        self.pgid
    }

    /// Kill the group now and disarm, so `Drop` does not kill it a second time.
    /// The second kill matters: once the leader is reaped and the group empties,
    /// the kernel may hand the id to an unrelated process.
    pub fn kill(&mut self) {
        if self.armed {
            kill_process_group(self.pgid);
            self.armed = false;
        }
    }

    /// The run ended cleanly and the group is empty: do not kill on drop.
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcGuard {
    fn drop(&mut self) {
        self.kill();
        // Bind the guard *before* the `if let`: a temporary created in an
        // `if let` scrutinee lives until the end of the whole expression, so
        // locking again inside the body would deadlock on this same mutex.
        let mut groups = self
            .registry
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(index) = groups.iter().position(|entry| *entry == self.pgid) {
            groups.remove(index);
        }
    }
}

/// How the run ended, as facts rather than a formatted string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitInfo {
    /// `None` when a signal killed the child.
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// Set when this side killed the group instead of letting it finish.
    pub killed: Option<String>,
}

/// The outcome of one streamed run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellRun {
    pub exit: ExitInfo,
    pub pid: u32,
    pub pgid: u32,
    /// Bytes forwarded as `Output` events — the audit line's `result_bytes`.
    pub output_bytes: u64,
    pub truncated: bool,
    pub spill_path: Option<PathBuf>,
}

/// Why a streamed run could not produce an `Exit` event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellFailure {
    /// `sh` could not be started, or could not be waited for.
    Spawn { reason: String },
    /// The event stream broke: the transport closed or stopped accepting
    /// events. The process group is dead by the time this is returned.
    Transport { reason: String },
    /// The caller interrupted this call. The caller usually drops the future on
    /// an interrupt, so this value only reaches it when the cancellation came
    /// from somewhere the transport does not own.
    Interrupted { reason: String },
}

impl std::fmt::Display for ShellFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShellFailure::Spawn { reason } => write!(f, "cannot run `sh -c`: {reason}"),
            ShellFailure::Transport { reason } => write!(f, "the output stream ended: {reason}"),
            ShellFailure::Interrupted { reason } => write!(f, "interrupted: {reason}"),
        }
    }
}

/// Everything a streamed run needs that is not part of the call itself.
#[derive(Clone)]
pub struct ShellOptions {
    /// Where output beyond the cap is spilled.
    pub spill_dir: PathBuf,
    /// Base name for the spill file (the invocation id). Never a caller-supplied
    /// path, so a caller cannot aim the spill outside this directory.
    pub spill_name: String,
    /// Per-stream cap in bytes; `0` means the default
    /// ([`SHELL_OUTPUT_LIMIT_BYTES`]).
    pub output_limit_bytes: usize,
    /// Tripped by daemon shutdown; kills the group like an interrupt does.
    pub shutdown: CancellationToken,
    /// Live process groups, so a shutdown can kill them all.
    pub registry: ProcRegistry,
}

/// What the pumps, the emitter and the run share.
struct Shared {
    output_bytes: AtomicU64,
    truncated: AtomicBool,
    /// One sequence for both streams, so the reader can interleave them back
    /// into the order they were produced.
    seq: AtomicU64,
    /// Set when an event could not be delivered. The run watches it: continuing
    /// would produce output nobody receives, and would let a command run to its
    /// own timeout after the reader is already gone.
    broken: watch::Sender<bool>,
    reason: Mutex<Option<String>>,
    spill: Mutex<Option<(PathBuf, std::fs::File)>>,
}

impl Shared {
    fn new() -> (Arc<Shared>, watch::Receiver<bool>) {
        let (broken, rx) = watch::channel(false);
        (
            Arc::new(Shared {
                output_bytes: AtomicU64::new(0),
                truncated: AtomicBool::new(false),
                seq: AtomicU64::new(0),
                broken,
                reason: Mutex::new(None),
                spill: Mutex::new(None),
            }),
            rx,
        )
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst)
    }

    fn mark_broken(&self, reason: String) {
        {
            let mut slot = self.reason.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                *slot = Some(reason);
            }
        }
        let _ = self.broken.send(true);
    }

    fn reason(&self) -> Option<String> {
        self.reason
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn is_broken(&self) -> bool {
        self.reason().is_some() || *self.broken.subscribe().borrow()
    }
}

/// One queued item for the single emitter task.
///
/// There is exactly **one** emitter because a transport takes one event at a
/// time: two pumps sending concurrently would interleave a chunk with an exit,
/// or trip a "one chunk in flight" rule on the wire. Serializing here is what
/// lets both pipes stream at once.
enum Emit {
    Data(ShellEvent),
    /// The last event; the emitter stops after sending it.
    Finish(ShellEvent),
}

/// Run `command` and stream it as [`ShellEvent`]s.
///
/// `cwd` must already be resolved through the executing side's
/// [`PathPolicy`](crate::PathPolicy), and the approval ceiling must already be
/// settled — this function executes, it does not decide. The caller supplies the
/// sink because only the caller knows what the events travel over; `send` is
/// awaited per event, so a slow transport slows the pumps instead of growing a
/// buffer.
///
/// On return — `Ok` or `Err` — **nothing this call started is still attached to
/// it**: the group was reaped and, where the run was cut short, killed.
pub async fn exec_shell_streaming(
    command: &str,
    cwd: &Path,
    timeout_ms: u64,
    env: &BTreeMap<String, String>,
    options: &ShellOptions,
    sink: Arc<dyn ShellEventSink>,
) -> Result<ShellRun, ShellFailure> {
    let started = std::time::Instant::now();
    // The hard cap applies on every path in, including a call from a brain that
    // never saw the schema.
    let timeout = Duration::from_millis(timeout_ms.clamp(1, MAX_SHELL_TIMEOUT_MS));
    let limit = if options.output_limit_bytes == 0 {
        SHELL_OUTPUT_LIMIT_BYTES
    } else {
        options.output_limit_bytes
    };

    let mut cmd = build_shell_command(command, cwd, env);

    let mut child = cmd.spawn().map_err(|error| ShellFailure::Spawn {
        reason: format!("(cwd {}): {error}", cwd.display()),
    })?;
    let Some(pid) = child.id() else {
        return Err(ShellFailure::Spawn {
            reason: "the child process reported no pid".into(),
        });
    };
    // `process_group(0)` makes the child its own group leader: pid == pgid.
    let pgid = pid;
    // This guard must be a local of *this* frame, not of a spawned task, or
    // dropping the caller's future on an interrupt would not reach it.
    let mut guard = options.registry.register(pgid);
    let run_token = CancellationToken::new();
    let (shared, mut broken_rx) = Shared::new();

    let (tx, mut rx) = mpsc::channel::<Emit>(EMIT_QUEUE);
    let emitter = {
        let sink = Arc::clone(&sink);
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            while let Some(item) = rx.recv().await {
                let (event, last) = match item {
                    Emit::Data(event) => (event, false),
                    Emit::Finish(event) => (event, true),
                };
                if let Err(error) = sink.send(event).await {
                    // Nobody is listening any more. Record it so the run stops
                    // instead of producing output into the void, and stop here:
                    // an unacknowledged stream cannot be resumed.
                    shared.mark_broken(error.to_string());
                    return Err(error);
                }
                if last {
                    break;
                }
            }
            Ok::<(), ShellFailure>(())
        })
    };

    let stdout_task = child.stdout.take().map(|pipe| {
        spawn_pump(
            pipe,
            ShellStream::Stdout,
            tx.clone(),
            Arc::clone(&shared),
            limit,
            options.clone(),
            run_token.clone(),
        )
    });
    let stderr_task = child.stderr.take().map(|pipe| {
        spawn_pump(
            pipe,
            ShellStream::Stderr,
            tx.clone(),
            Arc::clone(&shared),
            limit,
            options.clone(),
            run_token.clone(),
        )
    });

    // `Started` goes first, so the caller knows which pid an interrupt has to
    // kill before any output can arrive.
    if tx
        .send(Emit::Data(ShellEvent::Started { pid, pgid }))
        .await
        .is_err()
    {
        return Err(stop(guard, run_token, child, emitter, &shared, None).await);
    }

    enum End {
        Exited(std::process::ExitStatus),
        TimedOut,
        Interrupted,
        Shutdown,
        Transport,
    }
    let end = tokio::select! {
        status = child.wait() => match status {
            Ok(status) => End::Exited(status),
            Err(error) => {
                let reason = format!("cannot wait for the command: {error}");
                let _ = stop(guard, run_token, child, emitter, &shared, None).await;
                return Err(ShellFailure::Spawn { reason });
            }
        },
        _ = tokio::time::sleep(timeout) => End::TimedOut,
        // The caller interrupted this call.
        _ = sink.cancelled() => End::Interrupted,
        // A daemon is stopping. Same kill, different reason: an operator reading
        // the audit log must be able to tell "the caller stopped it" from "this
        // machine was stopped".
        _ = options.shutdown.cancelled() => End::Shutdown,
        // The event stream broke. Stop now rather than let the command run to
        // its own timeout with nobody listening.
        _ = broken_rx.changed() => End::Transport,
    };

    match end {
        End::Transport => {
            // The transport stopped taking events. Kill the group first: a
            // command nobody is watching is exactly the one that must not be
            // left running.
            let reason = shared
                .reason()
                .unwrap_or_else(|| "the event stream ended".to_string());
            Err(stop(guard, run_token, child, emitter, &shared, Some(reason)).await)
        }
        End::Interrupted | End::Shutdown => {
            let reason = match end {
                End::Interrupted => {
                    format!("the caller interrupted this call; killed process group {pgid}")
                }
                _ => format!("this daemon is shutting down; killed process group {pgid}"),
            };
            // Kill before reaping: while the leader is unreaped the group
            // cannot be recycled, so `killpg` cannot hit an unrelated process.
            guard.kill();
            run_token.cancel();
            let status = child.wait().await.ok();
            drain_pumps(stdout_task, stderr_task).await;
            let exit = ExitInfo {
                code: status.as_ref().and_then(|status| status.code()),
                signal: status
                    .as_ref()
                    .and_then(|status| status.signal())
                    .or(Some(libc::SIGKILL)),
                timed_out: false,
                duration_ms: started.elapsed().as_millis() as u64,
                killed: Some(reason.clone()),
            };
            // These events usually go nowhere: an interrupt typically drops the
            // future that would have consumed them. They are still offered,
            // because when the interrupt came from our own shutdown path the
            // transport is alive and the caller deserves an honest exit rather
            // than a hang.
            let truncated = shared.truncated.load(Ordering::SeqCst);
            note_and_exit(&tx, &shared, &exit, truncated, None).await;
            Err(ShellFailure::Interrupted { reason })
        }
        End::TimedOut => {
            guard.kill();
            run_token.cancel();
            let status = child.wait().await.ok();
            drain_pumps(stdout_task, stderr_task).await;
            let exit = ExitInfo {
                code: status.as_ref().and_then(|status| status.code()),
                signal: status
                    .as_ref()
                    .and_then(|status| status.signal())
                    .or(Some(libc::SIGKILL)),
                timed_out: true,
                duration_ms: started.elapsed().as_millis() as u64,
                killed: Some(format!(
                    "timed out after {} ms; killed process group {pgid}",
                    timeout.as_millis()
                )),
            };
            finish_run(guard, emitter, &shared, &tx, exit, pid, pgid).await
        }
        End::Exited(status) => {
            // A `cmd &` child may still hold the pipes open. Wait briefly for
            // EOF; if the pipes stay open the group is still non-empty, so
            // killing it cannot hit a recycled id — and a call that returns must
            // not leave anything attached to it.
            let mut killed = None;
            if tokio::time::timeout(EXIT_GRACE, join_pumps(stdout_task, stderr_task))
                .await
                .is_err()
            {
                guard.kill();
                run_token.cancel();
                tokio::time::sleep(SETTLE).await;
                killed = Some(format!(
                    "background children kept the output pipes open; killed process group {pgid}"
                ));
            } else {
                // The pipes are closed and the leader is reaped, so the group is
                // empty: a kill now could hit a recycled id.
                guard.disarm();
            }
            let exit = ExitInfo {
                code: status.code(),
                signal: status.signal(),
                timed_out: false,
                duration_ms: started.elapsed().as_millis() as u64,
                killed,
            };
            finish_run(guard, emitter, &shared, &tx, exit, pid, pgid).await
        }
    }
}

/// Kill, reap, stop the pumps and the emitter, and turn the reason into a
/// failure. Used by every path that cannot produce a normal `Exit`.
async fn stop(
    mut guard: ProcGuard,
    run_token: CancellationToken,
    mut child: tokio::process::Child,
    emitter: JoinHandle<Result<(), ShellFailure>>,
    shared: &Shared,
    reason: Option<String>,
) -> ShellFailure {
    guard.kill();
    run_token.cancel();
    let _ = child.wait().await;
    let _ = emitter.await;
    match reason.or_else(|| shared.reason()) {
        Some(reason) => ShellFailure::Transport { reason },
        None => ShellFailure::Transport {
            reason: "the event stream ended".into(),
        },
    }
}

/// Emit the `Exit` event, then report the run.
async fn finish_run(
    guard: ProcGuard,
    emitter: JoinHandle<Result<(), ShellFailure>>,
    shared: &Shared,
    tx: &mpsc::Sender<Emit>,
    exit: ExitInfo,
    pid: u32,
    pgid: u32,
) -> Result<ShellRun, ShellFailure> {
    // Dropping the guard here (rather than at the end of the function) is
    // deliberate: a clean run disarmed it, and a killed one already killed the
    // group, so nothing is left to do — and holding it across the awaits below
    // would keep the registry entry alive while the caller is already reading
    // the exit event.
    drop(guard);
    let spill_path = take_spill(shared);
    let truncated = shared.truncated.load(Ordering::SeqCst);
    let run = ShellRun {
        exit: exit.clone(),
        pid,
        pgid,
        output_bytes: shared.output_bytes.load(Ordering::SeqCst),
        truncated,
        spill_path: spill_path.clone(),
    };
    note_and_exit(tx, shared, &exit, truncated, spill_path.as_ref()).await;
    // The emitter is done once it has sent `Finish`; awaiting it means the
    // events were handed to the transport, not merely queued.
    let delivered = emitter.await;
    if let Some(reason) = shared.reason() {
        return Err(ShellFailure::Transport { reason });
    }
    if let Ok(Err(error)) = delivered {
        return Err(error);
    }
    Ok(run)
}

/// Send the `Exit` event. Best effort: a transport that is gone is reported by
/// the caller through [`Shared::reason`].
async fn note_and_exit(
    tx: &mpsc::Sender<Emit>,
    shared: &Shared,
    exit: &ExitInfo,
    truncated: bool,
    spill_path: Option<&PathBuf>,
) {
    if let Some(note) = &exit.killed {
        let event = ShellEvent::Note {
            data: note.clone(),
            seq: shared.next_seq(),
        };
        let _ = tx.send(Emit::Data(event)).await;
    }
    let event = ShellEvent::Exit(ShellExit {
        code: exit.code,
        signal: exit.signal,
        timed_out: exit.timed_out,
        duration_ms: exit.duration_ms,
        output_truncated: truncated,
        spill_path: spill_path.cloned(),
        killed: exit.killed.clone(),
    });
    let _ = tx.send(Emit::Finish(event)).await;
}

/// One output pipe: decode incrementally, forward through the single emitter,
/// spill whatever exceeds the cap.
fn spawn_pump<R>(
    reader: R,
    stream: ShellStream,
    tx: mpsc::Sender<Emit>,
    shared: Arc<Shared>,
    limit: usize,
    options: ShellOptions,
    run_token: CancellationToken,
) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = reader;
        let mut buf = [0u8; 8192];
        let mut pending: Vec<u8> = Vec::new();
        let mut sent = 0usize;
        loop {
            let read = tokio::select! {
                biased;
                _ = run_token.cancelled() => break,
                result = reader.read(&mut buf) => result,
            };
            let n = match read {
                Ok(0) => break,
                Ok(n) => n,
                // A read error is treated as EOF: the exit status tells the
                // real story, and what was already forwarded stays valid.
                Err(_) => break,
            };
            pending.extend_from_slice(&buf[..n]);
            let (text, tail_len) = decode_prefix(&pending);
            pending = pending.split_off(pending.len() - tail_len);
            if text.is_empty() {
                // Only an incomplete multi-byte character so far: wait for the
                // rest rather than sending a replacement character for it.
                continue;
            }
            if !forward(&tx, &shared, stream, &text, limit, &mut sent, &options).await {
                break;
            }
        }
        // Flush a trailing incomplete sequence as replacement characters rather
        // than dropping the bytes silently.
        if !pending.is_empty() && !run_token.is_cancelled() && !shared.is_broken() {
            let text = String::from_utf8_lossy(&pending).into_owned();
            shared
                .output_bytes
                .fetch_add(text.len() as u64, Ordering::SeqCst);
            let event = ShellEvent::Output {
                stream,
                data: text,
                seq: shared.next_seq(),
            };
            let _ = tx.send(Emit::Data(event)).await;
        }
    })
}

/// Forward one decoded slice, splitting at the cap and spilling the overflow.
/// Returns `false` when the emitter is gone and the pump must stop.
async fn forward(
    tx: &mpsc::Sender<Emit>,
    shared: &Shared,
    stream: ShellStream,
    text: &str,
    limit: usize,
    sent: &mut usize,
    options: &ShellOptions,
) -> bool {
    let room = limit.saturating_sub(*sent);
    if room == 0 {
        shared.truncated.store(true, Ordering::SeqCst);
        spill_stream(shared, options, stream, text.as_bytes());
        return !shared.is_broken();
    }
    let cut = floor_char_boundary(text, room);
    let (head, rest) = text.split_at(cut);
    if !head.is_empty() {
        *sent += head.len();
        shared
            .output_bytes
            .fetch_add(head.len() as u64, Ordering::SeqCst);
        let event = ShellEvent::Output {
            stream,
            data: head.to_string(),
            seq: shared.next_seq(),
        };
        if tx.send(Emit::Data(event)).await.is_err() {
            shared.mark_broken("the event emitter is gone".into());
            return false;
        }
    }
    if !rest.is_empty() {
        shared.truncated.store(true, Ordering::SeqCst);
        spill_stream(shared, options, stream, rest.as_bytes());
    }
    !shared.is_broken()
}

/// Split a byte buffer into the longest valid UTF-8 prefix and the length of the
/// incomplete tail (at most three bytes). A tail that is *invalid* rather than
/// incomplete is converted with replacement characters, because holding it back
/// would stall a stream that is never going to become valid.
fn decode_prefix(buf: &[u8]) -> (String, usize) {
    match std::str::from_utf8(buf) {
        Ok(text) => (text.to_string(), 0),
        Err(error) => {
            let valid = error.valid_up_to();
            if error.error_len().is_some() {
                (String::from_utf8_lossy(buf).into_owned(), 0)
            } else {
                (
                    String::from_utf8_lossy(&buf[..valid]).into_owned(),
                    buf.len() - valid,
                )
            }
        }
    }
}

fn floor_char_boundary(text: &str, limit: usize) -> usize {
    if text.len() <= limit {
        return text.len();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Append overflow to `<spill_dir>/<spill_name>-<stream>.log`. The handle is
/// opened lazily and kept, so a chatty command does not reopen it per chunk.
///
/// Blocking file IO inside an async task is deliberate here: this path only runs
/// *after* a stream has exceeded the cap, and the alternative — losing the
/// overflow — is worse than one slow write.
///
/// The file is created 0600 inside a 0700 directory (unix). What lands here is a
/// command's output: `cat ~/.config/…`, an environment dump, a token echoed by a
/// failing tool. On a shared machine a 0644 spill file is that output readable
/// by every other account, so the umask is not allowed to decide. The mode
/// applies at creation only — a spill file left by an older build keeps its
/// mode, and is not rewritten.
fn spill_stream(shared: &Shared, options: &ShellOptions, stream: ShellStream, bytes: &[u8]) {
    let name = match stream {
        ShellStream::Stdout => "stdout",
        ShellStream::Stderr => "stderr",
    };
    let mut slot = shared.spill.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((path, file)) = slot.as_mut()
        && path.ends_with(name)
    {
        let _ = file.write_all(bytes);
        return;
    }
    if let Err(error) = create_private_dir_all(&options.spill_dir) {
        tracing::warn!(
            dir = %options.spill_dir.display(),
            %error,
            "cannot create the spill directory; overflowing output is dropped"
        );
        return;
    }
    let path = options
        .spill_dir
        .join(format!("{}-{name}.log", options.spill_name));
    match open_append_private(&path) {
        Ok(mut file) => {
            if file.write_all(bytes).is_err() {
                tracing::warn!(path = %path.display(), "spill write failed; output dropped");
            }
            *slot = Some((path, file));
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot open the spill file; output dropped");
        }
    }
}

/// Create the spill directory so only its owner can enter it (0700 on unix).
///
/// The registry's own spill (`ToolRegistry`) makes the same two-mode promise for
/// the brain's spool; this is the streaming path's copy of that two-line helper
/// rather than a shared function, because the two write to different places and
/// neither should grow a mode knob.
#[cfg(unix)]
fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Open the spill file for appending, creating it 0600 on unix.
#[cfg(unix)]
fn open_append_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_append_private(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

fn take_spill(shared: &Shared) -> Option<PathBuf> {
    if !shared.truncated.load(Ordering::SeqCst) {
        return None;
    }
    shared
        .spill
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|(path, _)| path.clone())
}

/// Same as [`join_pumps`], but bounded: after a kill the pumps should see EOF at
/// once, and one that does not must not hold the call open.
async fn drain_pumps(stdout: Option<JoinHandle<()>>, stderr: Option<JoinHandle<()>>) {
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, join_pumps(stdout, stderr)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::testkit::Rig;
    use crate::policy::PathPolicy;
    use nix::sys::signal::killpg;
    use nix::unistd::Pid;
    use serde_json::json;
    use swarmbot_contracts::{PolicyCeiling, PolicyMap};

    fn handler(rig: &Rig) -> ShellHandler {
        ShellHandler::new(Arc::clone(&rig.env))
    }

    /// The process group of a finished call must be gone: `killpg(pid, 0)`
    /// has to answer ESRCH. Retried briefly because orphans are reaped by
    /// init asynchronously.
    async fn assert_group_gone(pgid: i32) {
        for _ in 0..40 {
            if let Err(nix::errno::Errno::ESRCH) = killpg(Pid::from_raw(pgid), None) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("process group {pgid} still exists after the call");
    }

    /// Pull the shell's own pid (== its process-group id) out of a result's
    /// stdout section; the test commands start with `echo $$`.
    fn pgid_from(text: &str) -> i32 {
        text.lines()
            .skip_while(|line| *line != "--- stdout ---")
            .nth(1)
            .and_then(|line| line.trim().parse().ok())
            .expect("the stdout section carries the pgid")
    }

    #[tokio::test]
    async fn a_deny_listed_path_in_the_command_is_refused_before_it_runs() {
        // `shell` never went through the path policy for the paths it touches,
        // so `cat <denied>` used to run inside a root with the deny list
        // ignored. The marker file proves nothing executed: if it did, the
        // command would have created it.
        let rig = Rig::new();
        let vault = rig.tmp.path().join("vault");
        std::fs::create_dir(&vault).expect("vault");
        let marker = vault.join("created-by-the-command");
        let secret = vault.join("key.pem");
        std::fs::write(&secret, b"secret").expect("secret");

        let mut env = (*rig.env).clone();
        env.policy = Arc::new(PathPolicy::new(
            vec![swarmbot_contracts::NodeRoot::read_write(
                "tmp",
                rig.tmp.path().display().to_string(),
            )],
            vec![vault.clone()],
        ));
        let handler = ShellHandler::new(Arc::new(env));

        let result = rig
            .call(
                &handler,
                json!({"command": format!("cat {} && touch {}", secret.display(), marker.display())}),
            )
            .await;

        assert!(
            result.is_error,
            "a denied path must not run: {}",
            result.text_only()
        );
        assert!(
            result.text_only().contains("deny list"),
            "the refusal must name the reason: {}",
            result.text_only()
        );
        assert!(!marker.exists(), "the command must not have run at all");
        assert_eq!(
            result.meta.get("decision").and_then(|v| v.as_str()),
            Some("refuse"),
            "the audit line must read as a refusal, not a run"
        );
    }

    #[tokio::test]
    async fn an_ordinary_command_still_runs_with_a_deny_list_configured() {
        // The guard must not turn `shell` off: a path inside a root and outside
        // the deny list is fine, and so is a command with no path at all.
        let rig = Rig::new();
        let vault = rig.tmp.path().join("vault");
        std::fs::create_dir(&vault).expect("vault");
        let mut env = (*rig.env).clone();
        env.policy = Arc::new(PathPolicy::new(
            vec![swarmbot_contracts::NodeRoot::read_write(
                "tmp",
                rig.tmp.path().display().to_string(),
            )],
            vec![vault],
        ));
        let handler = ShellHandler::new(Arc::new(env));

        let result = rig.call(&handler, json!({"command": "echo hello"})).await;
        assert!(!result.is_error, "{}", result.text_only());
        assert!(result.text_only().contains("hello"));
    }

    #[tokio::test]
    async fn captures_stdout_and_exit_code() {
        let rig = Rig::new();
        let result = rig
            .call(&handler(&rig), json!({"command": "echo hello"}))
            .await;
        assert!(!result.is_error);
        let text = result.text_only();
        assert!(text.starts_with("exit=0"), "{text}");
        assert!(text.contains("--- stdout ---\nhello"), "{text}");
        // No stderr section when stderr is empty.
        assert!(!text.contains("--- stderr ---"), "{text}");
    }

    #[tokio::test]
    async fn separates_stderr_and_reports_non_zero_exit_as_error() {
        let rig = Rig::new();
        let result = rig
            .call(
                &handler(&rig),
                json!({"command": "echo out; echo err >&2; exit 3"}),
            )
            .await;
        assert!(result.is_error, "a non-zero exit is an error result");
        let text = result.text_only();
        assert!(text.starts_with("exit=3"), "{text}");
        assert!(text.contains("--- stdout ---\nout"), "{text}");
        assert!(text.contains("--- stderr ---\nerr"), "{text}");
    }

    #[tokio::test]
    async fn runs_in_the_given_cwd_inside_the_roots() {
        let rig = Rig::new();
        std::fs::create_dir(rig.workspace.join("sub")).expect("mkdir");
        let sub = rig.workspace.join("sub").canonicalize().expect("canon");
        let result = rig
            .call(
                &handler(&rig),
                json!({"command": "pwd", "cwd": sub.display().to_string()}),
            )
            .await;
        assert!(result.text_only().contains(&sub.display().to_string()));
    }

    #[tokio::test]
    async fn refuses_a_cwd_outside_the_roots() {
        let rig = Rig::new();
        let result = rig
            .call(&handler(&rig), json!({"command": "pwd", "cwd": "/etc"}))
            .await;
        assert!(result.is_error);
        let text = result.text_only();
        assert!(text.contains("path denied"), "{text}");
        assert!(text.contains("outside roots"), "{text}");
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        let rig = Rig::new();
        let started = std::time::Instant::now();
        let result = rig
            .call(
                &handler(&rig),
                json!({"command": "echo $$; sleep 30 & sleep 30", "timeout_ms": 300}),
            )
            .await;
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(2), "returned in {elapsed:?}");
        assert!(result.is_error);
        let text = result.text_only();
        assert!(text.contains("timed out after 300 ms"), "{text}");
        // Neither the foreground nor the backgrounded `sleep 30` survives:
        // the whole group is gone.
        let pgid = pgid_from(&text);
        assert_group_gone(pgid).await;
    }

    #[tokio::test]
    async fn background_children_do_not_outlive_a_normal_exit() {
        let rig = Rig::new();
        let started = std::time::Instant::now();
        // The shell exits at once, but the background sleep keeps stdout
        // open; the handler must not wait 30 s for EOF and must not leave
        // the sleeper behind.
        let result = rig
            .call(
                &handler(&rig),
                json!({"command": "echo $$; sleep 30 & echo done"}),
            )
            .await;
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(5), "returned in {elapsed:?}");
        assert!(!result.is_error);
        let text = result.text_only();
        assert!(text.contains("done"), "{text}");
        assert_group_gone(pgid_from(&text)).await;
    }

    #[tokio::test]
    async fn cancellation_trips_mid_command() {
        let rig = Rig::new();
        let shell = Arc::new(handler(&rig));
        let ctx = rig.ctx.clone();
        let cancel = ctx.cancel.clone();
        let started = std::time::Instant::now();
        let task = tokio::spawn(async move {
            shell
                .call(&ctx, json!({"command": "echo $$; sleep 30"}))
                .await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
        let outcome = task.await.expect("the call task finishes");
        assert!(started.elapsed() < Duration::from_secs(5));
        let error = outcome.expect_err("a cancelled call returns ToolError::Cancelled");
        assert_eq!(error, ToolError::Cancelled);
    }

    #[tokio::test]
    async fn the_environment_is_scrubbed_to_the_allowlist() {
        // A secret in *this* process's environment must never reach a child;
        // an explicit `env` entry must. The variable name is unique to this
        // test, so the process-global set/remove cannot disturb other tests.
        unsafe {
            std::env::set_var("SWARMBOT_SHELL_SCRUB_TEST_SECRET", "hunter2");
        }
        let rig = Rig::new();
        let result = rig
            .call(
                &handler(&rig),
                json!({
                    "command": "printf '%s' \"${SWARMBOT_SHELL_SCRUB_TEST_SECRET:-unset}\"; printf '|'; printf '%s' \"${GREETING:-unset}\"; printf '|'; printf '%s' \"${PATH:+path-is-set}\"",
                    "env": {"GREETING": "hi"}
                }),
            )
            .await;
        unsafe {
            std::env::remove_var("SWARMBOT_SHELL_SCRUB_TEST_SECRET");
        }
        let text = result.text_only();
        assert!(text.contains("unset|hi|path-is-set"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[tokio::test]
    async fn a_destructive_command_asks_even_when_ceilings_are_always() {
        // Always/Always ceilings, but a *denying* gate: the point is that the
        // ask happened at all (and that the denied command never ran).
        let rig = Rig::with_engine(
            PolicyMap::uniform(PolicyCeiling::Always),
            PolicyMap::uniform(PolicyCeiling::Always),
            true,
        );
        let gate = crate::handlers::testkit::TestGate::denying();
        let ctx = ToolContext {
            approval: Arc::clone(&gate) as Arc<dyn crate::handler::ApprovalGate>,
            ..rig.ctx.clone()
        };
        let shell = handler(&rig);
        let outcome = shell
            .call(&ctx, json!({"command": "sudo ls /tmp"}))
            .await
            .expect("a denial is a result, not an error");
        let text = outcome.text_only();
        assert!(outcome.is_error);
        assert!(text.starts_with("ERROR: denied by the owner"), "{text}");
        assert_eq!(gate.ask_count(), 1);
        let ask = gate.last_ask().expect("one ask");
        assert_eq!(ask.surface, Surface::Shell);
        let reason = ask.reason.clone().expect("a reason");
        assert!(reason.contains("destructive"), "{ask:?}");
        assert_eq!(outcome.meta["decision"], "deny");
    }

    #[tokio::test]
    async fn a_remote_node_call_is_forwarded_not_executed() {
        let rig = Rig::new();
        let remote = swarmbot_contracts::NodeId::new();
        let result = rig
            .call(
                &handler(&rig),
                json!({"command": "echo hi", "node": remote.as_str()}),
            )
            .await;
        // The default test executor has no response wired, so the forward
        // fails — which is exactly the proof the local shell never ran.
        assert!(result.is_error);
        let recorded = rig.nodes.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, remote);
        assert_eq!(
            recorded[0].1,
            NodeCall::ShellExec {
                command: "echo hi".into(),
                cwd: None,
                timeout_ms: Some(DEFAULT_SHELL_TIMEOUT_MS),
                env: Some(BTreeMap::new()),
            }
        );
        // (The failing executor's `Err` carries no meta; the successful
        // forward test below asserts the node stamp.)
    }

    #[tokio::test]
    async fn a_forwarded_call_returns_the_nodes_answer_with_meta() {
        let rig = Rig::with_nodes(crate::handlers::testkit::TestNodes::responding(
            ToolResult::text("exit=0\n--- stdout ---\nhi"),
        ));
        let remote = swarmbot_contracts::NodeId::new();
        let result = rig
            .call(
                &handler(&rig),
                json!({"command": "echo hi", "node": remote.as_str()}),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.text_only(), "exit=0\n--- stdout ---\nhi");
        // The gate ran *here* (Always ceilings → no ask), and the result is
        // stamped with the remote node so the audit line names it.
        assert_eq!(result.meta["decision"], "run");
        assert_eq!(result.meta["surface"], "shell");
        assert_eq!(result.meta["node"], remote.as_str());
    }

    // --- streaming execution ------------------------------------------------

    /// A sink that keeps every event, so a test can assert on the *order* the
    /// run produced them — which is half of what the streaming shape promises.
    struct RecordingSink {
        events: Mutex<Vec<ShellEvent>>,
        cancel: CancellationToken,
    }

    impl RecordingSink {
        fn new() -> Arc<RecordingSink> {
            Arc::new(RecordingSink {
                events: Mutex::new(Vec::new()),
                cancel: CancellationToken::new(),
            })
        }

        fn events(&self) -> Vec<ShellEvent> {
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }

    impl ShellEventSink for RecordingSink {
        fn send<'a>(
            &'a self,
            event: ShellEvent,
        ) -> Pin<Box<dyn Future<Output = Result<(), ShellFailure>> + Send + 'a>> {
            Box::pin(async move {
                self.events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(event);
                Ok(())
            })
        }

        fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            let token = self.cancel.clone();
            Box::pin(async move { token.cancelled().await })
        }
    }

    fn streaming_options(spill_dir: &Path, output_limit_bytes: usize) -> ShellOptions {
        ShellOptions {
            spill_dir: spill_dir.to_path_buf(),
            spill_name: "inv_test".to_string(),
            output_limit_bytes,
            shutdown: CancellationToken::new(),
            registry: ProcRegistry::new(),
        }
    }

    /// What a spill file holds is a command's output — an environment dump, a
    /// file it printed, a token echoed by a failing tool. Opened with the
    /// default mode it was 0644 on a shared machine; it must be 0600 in a 0700
    /// directory, because the umask is not a permission policy.
    #[cfg(unix)]
    #[test]
    fn a_spilled_stream_is_created_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let spill_dir = dir.path().join("spill");
        let options = streaming_options(&spill_dir, 0);
        let (shared, _broken) = Shared::new();
        spill_stream(&shared, &options, ShellStream::Stdout, b"overflow");

        let path = spill_dir.join("inv_test-stdout.log");
        assert_eq!(std::fs::read(&path).expect("the spill file"), b"overflow");
        let mode = std::fs::metadata(&path)
            .expect("spill metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "spilled command output is readable by its owner only (mode {mode:o})"
        );
        let dir_mode = std::fs::metadata(&spill_dir)
            .expect("spill dir metadata")
            .permissions()
            .mode();
        assert_eq!(
            dir_mode & 0o777,
            0o700,
            "the spill dir is owner-traversable only"
        );
    }

    #[test]
    fn decoding_keeps_multibyte_characters_intact() {
        let (text, tail) = decode_prefix("héllo".as_bytes());
        assert_eq!(text, "héllo");
        assert_eq!(tail, 0);

        // Split a two-byte character in half: the prefix is what is valid and
        // the tail is the one dangling byte.
        let bytes = "é".as_bytes();
        assert_eq!(bytes.len(), 2);
        let (text, tail) = decode_prefix(&bytes[..1]);
        assert_eq!(text, "");
        assert_eq!(tail, 1);

        // An invalid byte is converted, not held back forever.
        let (text, tail) = decode_prefix(&[0x61, 0xff, 0x62]);
        assert!(text.starts_with('a') && text.ends_with('b'), "{text}");
        assert!(text.contains('\u{fffd}'), "{text}");
        assert_eq!(tail, 0);
    }

    #[test]
    fn char_boundaries_are_respected_when_splitting() {
        let text = "aé文";
        assert_eq!(floor_char_boundary(text, 100), text.len());
        assert_eq!(floor_char_boundary(text, 2), 1, "never split inside `é`");
        assert!(text.is_char_boundary(floor_char_boundary(text, 4)));
    }

    #[test]
    fn a_shared_sequence_orders_both_streams() {
        let (shared, _rx) = Shared::new();
        assert_eq!(shared.next_seq(), 0);
        assert_eq!(shared.next_seq(), 1);
        assert!(!shared.is_broken());
        shared.mark_broken("gone".into());
        assert!(shared.is_broken());
        assert_eq!(shared.reason().as_deref(), Some("gone"));
        // The first reason wins: a cascade of failures must not hide the cause.
        shared.mark_broken("second".into());
        assert_eq!(shared.reason().as_deref(), Some("gone"));
    }

    #[tokio::test]
    async fn the_streaming_run_sends_started_before_any_output() {
        // The whole reason the streaming shape exists: a caller that wants to
        // stop the call needs the process group *before* the first byte of
        // output, not after the command has finished.
        let dir = tempfile::tempdir().expect("tempdir");
        let options = streaming_options(&dir.path().join("spill"), 0);
        let sink = RecordingSink::new();
        let run = exec_shell_streaming(
            "echo $$; echo hi",
            dir.path(),
            30_000,
            &BTreeMap::new(),
            &options,
            Arc::clone(&sink) as Arc<dyn ShellEventSink>,
        )
        .await
        .expect("the run succeeds");

        let events = sink.events();
        assert!(
            matches!(events.first(), Some(ShellEvent::Started { .. })),
            "Started must be the first event: {events:?}"
        );
        let Some(ShellEvent::Started { pid, pgid }) = events.first() else {
            return;
        };
        assert!(*pid > 0);
        assert_eq!(*pid, *pgid, "`process_group(0)` makes the child leader");
        assert_eq!(run.pid, *pid);
        assert_eq!(run.pgid, *pgid);
        // Then output, then exactly one Exit.
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ShellEvent::Output { .. })),
            "{events:?}"
        );
        assert!(
            matches!(events.last(), Some(ShellEvent::Exit(_))),
            "{events:?}"
        );
        let Some(ShellEvent::Exit(exit)) = events.last() else {
            return;
        };
        assert_eq!(exit.code, Some(0));
        assert!(!exit.timed_out);
        assert!(!exit.output_truncated);
        assert_eq!(exit.spill_path, None);
        // The group is gone now — reaping is asynchronous, so poll rather than
        // sleeping once and hoping.
        for _ in 0..40 {
            if process_group_gone(*pgid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            process_group_gone(*pgid),
            "the group must be gone after the call"
        );
    }

    #[tokio::test]
    async fn a_cancelled_streaming_run_kills_the_process_group() {
        // Cancellation is the reason for the `Started` event: the caller kills
        // a group it learned about from the stream, and the runner must do the
        // same when its own cancellation token trips.
        let dir = tempfile::tempdir().expect("tempdir");
        let options = streaming_options(&dir.path().join("spill"), 0);
        let sink = RecordingSink::new();
        let task = {
            let sink = Arc::clone(&sink);
            let options = options.clone();
            let cwd = dir.path().to_path_buf();
            tokio::spawn(async move {
                exec_shell_streaming(
                    "echo $$; sleep 30",
                    &cwd,
                    30_000,
                    &BTreeMap::new(),
                    &options,
                    sink as Arc<dyn ShellEventSink>,
                )
                .await
            })
        };
        // Wait for the pid the runner reports, then cancel: killing a group we
        // have not been told about yet would prove nothing.
        let mut pgid = None;
        for _ in 0..200 {
            if let Some(ShellEvent::Started { pgid: reported, .. }) = sink.events().first() {
                pgid = Some(*reported);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pgid = pgid.expect("the run reports its process group first");
        sink.cancel.cancel();
        let outcome = task.await.expect("the call task finishes");
        assert!(
            matches!(outcome, Err(ShellFailure::Interrupted { .. })),
            "a cancelled run is interrupted, not a clean exit: {outcome:?}"
        );
        for _ in 0..100 {
            if process_group_gone(pgid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            process_group_gone(pgid),
            "process group {pgid} outlived the cancelled call"
        );
    }

    #[tokio::test]
    async fn output_beyond_the_cap_is_spilled_to_a_file_the_result_names() {
        // The spill path is a promise to the reader: it must be reported, it
        // must exist, and it must hold the bytes the stream did not carry.
        let dir = tempfile::tempdir().expect("tempdir");
        let spill_dir = dir.path().join("spill");
        let options = streaming_options(&spill_dir, 32);
        let sink = RecordingSink::new();
        let run = exec_shell_streaming(
            "i=0; while [ $i -lt 20 ]; do printf '0123456789'; i=$((i+1)); done",
            dir.path(),
            30_000,
            &BTreeMap::new(),
            &options,
            Arc::clone(&sink) as Arc<dyn ShellEventSink>,
        )
        .await
        .expect("the run succeeds");

        assert!(
            run.truncated,
            "200 bytes against a 32-byte cap is truncated"
        );
        let spill = run.spill_path.clone().expect("the overflow is spilled");
        assert!(
            spill.starts_with(&spill_dir),
            "the spill stays inside the configured directory: {}",
            spill.display()
        );
        let spilled = std::fs::read_to_string(&spill).expect("the spill file is readable");
        assert!(
            !spilled.is_empty(),
            "the overflow was written somewhere: {}",
            spill.display()
        );
        // The forwarded chunks plus the spill add up to the whole output.
        let forwarded: String = sink
            .events()
            .iter()
            .filter_map(|event| match event {
                ShellEvent::Output { data, .. } => Some(data.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            forwarded.len() + spilled.len(),
            200,
            "no byte is lost: {forwarded:?} + {} spilled",
            spilled.len()
        );
        let Some(ShellEvent::Exit(exit)) = sink.events().last().cloned() else {
            panic!("the run ends with an Exit event");
        };
        assert!(exit.output_truncated, "{exit:?}");
        assert_eq!(exit.spill_path.as_ref(), Some(&spill));
    }
}
