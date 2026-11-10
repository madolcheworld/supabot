//! `shell` — run a command on a node through `sh -c`.
//!
//! Safety shape of every run, in one place:
//!
//! * the child gets **its own process group** (`process_group(0)`), and the
//!   whole group dies — `killpg(SIGKILL)` — on timeout, on cancellation, and
//!   after a normal exit whose pipes are still held open by background
//!   children. Invariant: **when `shell` returns, nothing it started is still
//!   attached to it.** A command that wants to leave a daemon behind must
//!   detach it (`setsid` with redirected descriptors);
//! * the child environment is **scrubbed to an allowlist** — the brain's
//!   process holds API keys and node secrets, and a model-typed command line
//!   must never be one `env` away from reading them;
//! * `cwd` goes through [`PathPolicy`](crate::PathPolicy), so a command
//!   cannot be aimed outside the node's roots;
//! * each output stream is capped at [`SHELL_OUTPUT_LIMIT_BYTES`]; the result
//!   then goes through the registry's spill like any other tool.

use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use swarmbot_contracts::{
    SHELL_OUTPUT_LIMIT_BYTES, Surface, ToolError, ToolResult, ToolSpec, Toolset,
};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{
    TOOL_SHELL, ToolEnv, forward_remote, gate_call, opt_env_map, opt_str, opt_u64, req_str,
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

/// The shared implementation, used by the handler (local node) and by
/// [`crate::LocalExecutor`] (node daemon) so both behave identically.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::testkit::Rig;
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
}
