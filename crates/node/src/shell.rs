//! The streaming `shell.exec` runner, and the process-group bookkeeping that
//! makes "nothing survives the call" true on a socket.
//!
//! # Relationship with `swarmbot_tools`
//!
//! Every `fs.*` call in this crate goes through [`swarmbot_tools::LocalExecutor`]
//! so the local and remote paths cannot drift. `shell.exec` streams incremental
//! chunks over the wire, and both sides now share the same process-spawning
//! invariants, environment scrubbing allowlist, and process group lifecycle
//! from [`swarmbot_tools::handlers::shell`]:
//!
//! 1. `build_shell_command` (`sh -c <command>` with `stdin` null, both output pipes captured,
//!    `process_group(0)`, `kill_on_drop(true)`);
//! 2. `ENV_ALLOWLIST` and fallback `PATH`;
//! 3. `kill_process_group` and `process_group_gone`;
//! 4. `EXIT_GRACE`, `DRAIN_TIMEOUT`, and `SETTLE` timing constants;
//! 5. `DEFAULT_SHELL_TIMEOUT_MS`, `MAX_SHELL_TIMEOUT_MS`, and `SHELL_OUTPUT_LIMIT_BYTES`.
//!
//! What is wire-specific here is the streaming half: chunks through a
//! [`ChunkSink`](swarmbot_node_protocol::ChunkSink), whose `send` awaits the brain's Ack
//! and is therefore the back-pressure that keeps a chatty command from
//! outrunning the socket; one emitter task, because the mux allows a single
//! chunk in flight per request; and a [`ProcGuard`] whose `Drop` kills the
//! group — which is what runs when the mux drops this future on an `Interrupt`.
//!
//! # Limits, stated plainly
//!
//! `seq` orders chunks but nothing reassembles a multi-byte character split
//! across two reads by the *kernel*: this module carries the incomplete tail
//! over to the next read, so a UTF-8 character is never cut in half, and an
//! invalid byte becomes U+FFFD rather than stalling the stream. `data` is
//! text, never base64, so a binary-producing command yields replacement
//! characters — the protocol document pins this, and a model consumes text.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use swarmbot_contracts::SHELL_OUTPUT_LIMIT_BYTES;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use swarmbot_node_protocol::rpc::{OutputStream, ShellChunk};
use swarmbot_node_protocol::{ChunkSink, ProtocolError};

/// Re-exported from `swarmbot_tools` so the daemon and local runner share
/// identical execution and process isolation primitives.
pub use swarmbot_tools::handlers::shell::{
    DEFAULT_SHELL_TIMEOUT_MS, DRAIN_TIMEOUT, ENV_ALLOWLIST, EXIT_GRACE, FALLBACK_PATH,
    MAX_SHELL_TIMEOUT_MS, SETTLE, build_shell_command, kill_process_group, process_group_gone,
};

/// Un-forwarded chunks allowed between the pumps and the emitter. Small on
/// purpose: the emitter awaits the brain's Ack per chunk, so a large buffer
/// would only move the back-pressure into memory.
const EMIT_QUEUE: usize = 32;

/// Every process group this node has started, so a shutdown can kill them all
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

    /// Kill everything still registered. Called on SIGINT/SIGTERM before the
    /// socket is closed, so a stopping node leaves no orphan behind.
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
/// which is the whole point: when the mux drops a handler future on an
/// `Interrupt` (§4.7), or when the control socket dies, this runs — no code
/// path has to remember to.
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

    /// Kill the group now and disarm, so `Drop` does not kill it a second
    /// time. The second kill matters: once the leader is reaped and the group
    /// empties, the kernel may hand the id to an unrelated process.
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
    /// `None` when a signal killed the child, exactly as the wire wants it.
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// Set when this node killed the group instead of letting it finish.
    /// Reported as a stderr chunk *before* `Exit`, because the `Exit` shape has
    /// no room for a reason and a silent kill would read like a crash.
    pub killed: Option<String>,
}

/// The outcome of one streamed run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellRun {
    pub exit: ExitInfo,
    pub pid: u32,
    pub pgid: u32,
    /// Bytes forwarded as `Output` chunks — the audit line's `result_bytes`.
    pub output_bytes: u64,
    pub truncated: bool,
    pub spill_path: Option<PathBuf>,
}

/// Why a streamed run could not produce an `Exit` chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellFailure {
    /// `sh` could not be started, or could not be waited for.
    Spawn { reason: String },
    /// The chunk stream broke: the socket closed or the brain stopped Acking.
    /// The process group is dead by the time this is returned.
    Transport { reason: String },
    /// The brain interrupted this call. The mux answers with an `Interrupt`
    /// cause and drops the future, so this value only reaches a caller when the
    /// interrupt came from somewhere the mux does not own.
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

/// Everything the runner needs that is not part of the call itself.
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
    /// One sequence for both streams, so the brain can interleave them back
    /// into the order they were produced.
    seq: AtomicU64,
    /// Set when a chunk could not be delivered. The run watches it: continuing
    /// would produce output nobody receives, and would let a command run to
    /// its own timeout after the brain is already gone.
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
/// There is exactly **one** emitter because the mux keeps at most one chunk in
/// flight per request: two pumps sending concurrently would make the driver
/// fail the older waiter as "superseded by another chunk". Serializing here is
/// what lets both pipes stream at once.
enum Emit {
    Data(Value),
    /// The last chunk; the emitter stops after sending it.
    Finish(Value),
}

/// Run `command` and stream it. `cwd` must already be resolved through this
/// node's [`PathPolicy`](swarmbot_tools::PathPolicy), and the ceiling must
/// already be settled — this function executes, it does not decide.
pub async fn run_streaming(
    command: &str,
    cwd: &Path,
    timeout_ms: u64,
    env: &BTreeMap<String, String>,
    options: &ShellOptions,
    sink: &ChunkSink,
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
    // dropping the handler future on an `Interrupt` would not reach it.
    let mut guard = options.registry.register(pgid);
    let run_token = CancellationToken::new();
    let (shared, mut broken_rx) = Shared::new();

    let (tx, mut rx) = mpsc::channel::<Emit>(EMIT_QUEUE);
    let emitter = {
        let sink = sink.clone();
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            while let Some(item) = rx.recv().await {
                let (value, last) = match item {
                    Emit::Data(value) => (value, false),
                    Emit::Finish(value) => (value, true),
                };
                if let Err(error) = sink.send(value).await {
                    // Nobody is listening any more. Record it so the run stops
                    // instead of producing output into the void, and stop here:
                    // an unacked stream cannot be resumed.
                    shared.mark_broken(error.to_string());
                    return Err(error);
                }
                if last {
                    break;
                }
            }
            Ok::<(), ProtocolError>(())
        })
    };

    let stdout_task = child.stdout.take().map(|pipe| {
        spawn_pump(
            pipe,
            OutputStream::Stdout,
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
            OutputStream::Stderr,
            tx.clone(),
            Arc::clone(&shared),
            limit,
            options.clone(),
            run_token.clone(),
        )
    });

    // `Started` goes first, so the brain knows which pid an interrupt has to
    // kill before any output can arrive.
    let started_chunk = json!(ShellChunk::Started { pid, pgid });
    if tx.send(Emit::Data(started_chunk)).await.is_err() {
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
        // The brain sent `Interrupt` for this request (§4.7).
        _ = sink.cancellation().cancelled() => End::Interrupted,
        // SIGINT/SIGTERM. Same kill, different reason: an operator reading the
        // audit log must be able to tell "the brain stopped it" from "the node
        // was stopped".
        _ = options.shutdown.cancelled() => End::Shutdown,
        // The chunk stream broke. Stop now rather than let the command run to
        // its own timeout with nobody listening.
        _ = broken_rx.changed() => End::Transport,
    };

    match end {
        End::Transport => {
            // The brain stopped Acking or the socket died. Kill the group first:
            // a command nobody is watching is exactly the one that must not be
            // left running.
            let reason = shared
                .reason()
                .unwrap_or_else(|| "the chunk stream ended".to_string());
            Err(stop(guard, run_token, child, emitter, &shared, Some(reason)).await)
        }
        End::Interrupted | End::Shutdown => {
            let reason = match end {
                End::Interrupted => {
                    format!("the brain interrupted this call; killed process group {pgid}")
                }
                _ => format!("this node is shutting down; killed process group {pgid}"),
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
            // These chunks usually go nowhere: the mux answers an interrupt
            // with an `Interrupt` cause and drops this future. They are still
            // offered, because when the interrupt came from our own shutdown
            // path the socket is alive and the brain deserves an honest exit
            // rather than a hang.
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
            // killing it cannot hit a recycled id — and a call that returns
            // must not leave anything attached to it.
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
                // The pipes are closed and the leader is reaped, so the group
                // is empty: a kill now could hit a recycled id.
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
    emitter: JoinHandle<Result<(), ProtocolError>>,
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
            reason: "the chunk stream ended".into(),
        },
    }
}

/// Emit the kill note (if any) and the `Exit` chunk, then report the run.
async fn finish_run(
    guard: ProcGuard,
    emitter: JoinHandle<Result<(), ProtocolError>>,
    shared: &Shared,
    tx: &mpsc::Sender<Emit>,
    exit: ExitInfo,
    pid: u32,
    pgid: u32,
) -> Result<ShellRun, ShellFailure> {
    // Dropping the guard here (rather than at the end of the function) is
    // deliberate: a clean run disarmed it, and a killed one already killed the
    // group, so nothing is left to do — and holding it across the awaits below
    // would keep the registry entry alive while the brain is already reading
    // the exit chunk.
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
    // chunks were handed to the socket, not merely queued.
    let delivered = emitter.await;
    if let Some(reason) = shared.reason() {
        return Err(ShellFailure::Transport { reason });
    }
    if let Ok(Err(error)) = delivered {
        return Err(ShellFailure::Transport {
            reason: error.to_string(),
        });
    }
    Ok(run)
}

/// Send the kill note (stderr, so it reads as a diagnostic and not as program
/// output) and then the `Exit` chunk. Both are best effort: a socket that is
/// gone is reported by the caller through `Shared::reason`.
async fn note_and_exit(
    tx: &mpsc::Sender<Emit>,
    shared: &Shared,
    exit: &ExitInfo,
    truncated: bool,
    spill_path: Option<&PathBuf>,
) {
    if let Some(note) = &exit.killed {
        let chunk = json!(ShellChunk::Output {
            stream: OutputStream::Stderr,
            data: format!("[swarmbot-node] {note}\n"),
            seq: shared.next_seq(),
        });
        let _ = tx.send(Emit::Data(chunk)).await;
    }
    let exit_chunk = json!(ShellChunk::Exit {
        code: exit.code,
        signal: exit.signal,
        timed_out: exit.timed_out,
        duration_ms: exit.duration_ms,
        output_truncated: Some(truncated),
        spill_path: spill_path.map(|path| path.display().to_string()),
    });
    let _ = tx.send(Emit::Finish(exit_chunk)).await;
}

/// One output pipe: decode incrementally, forward through the single emitter,
/// spill whatever exceeds the cap.
fn spawn_pump<R>(
    reader: R,
    stream: OutputStream,
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
            let chunk = json!(ShellChunk::Output {
                stream,
                data: text.clone(),
                seq: shared.next_seq(),
            });
            shared
                .output_bytes
                .fetch_add(text.len() as u64, Ordering::SeqCst);
            let _ = tx.send(Emit::Data(chunk)).await;
        }
    })
}

/// Forward one decoded slice, splitting at the cap and spilling the overflow.
/// Returns `false` when the emitter is gone and the pump must stop.
async fn forward(
    tx: &mpsc::Sender<Emit>,
    shared: &Shared,
    stream: OutputStream,
    text: &str,
    limit: usize,
    sent: &mut usize,
    options: &ShellOptions,
) -> bool {
    let room = limit.saturating_sub(*sent);
    if room == 0 {
        shared.truncated.store(true, Ordering::SeqCst);
        spill(shared, options, stream, text.as_bytes());
        return !shared.is_broken();
    }
    let cut = floor_char_boundary(text, room);
    let (head, rest) = text.split_at(cut);
    if !head.is_empty() {
        *sent += head.len();
        shared
            .output_bytes
            .fetch_add(head.len() as u64, Ordering::SeqCst);
        let chunk = json!(ShellChunk::Output {
            stream,
            data: head.to_string(),
            seq: shared.next_seq(),
        });
        if tx.send(Emit::Data(chunk)).await.is_err() {
            shared.mark_broken("the chunk emitter is gone".into());
            return false;
        }
    }
    if !rest.is_empty() {
        shared.truncated.store(true, Ordering::SeqCst);
        spill(shared, options, stream, rest.as_bytes());
    }
    !shared.is_broken()
}

/// Split a byte buffer into the longest valid UTF-8 prefix and the length of
/// the incomplete tail (at most three bytes). A tail that is *invalid* rather
/// than incomplete is converted with replacement characters, because holding it
/// back would stall a stream that is never going to become valid.
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
/// Blocking file IO inside an async task is deliberate here: this path only
/// runs *after* a stream has exceeded a megabyte, and the alternative — losing
/// the overflow — is worse than one slow write.
fn spill(shared: &Shared, options: &ShellOptions, stream: OutputStream, bytes: &[u8]) {
    let name = match stream {
        OutputStream::Stdout => "stdout",
        OutputStream::Stderr => "stderr",
    };
    let mut slot = shared.spill.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((path, file)) = slot.as_mut()
        && path.ends_with(name)
    {
        let _ = file.write_all(bytes);
        return;
    }
    if let Err(error) = std::fs::create_dir_all(&options.spill_dir) {
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
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
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

/// Await both pumps, tolerating already-finished or absent ones.
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

/// Same, but bounded: after a kill the pumps should see EOF at once, and one
/// that does not must not hold the call open.
async fn drain_pumps(stdout: Option<JoinHandle<()>>, stderr: Option<JoinHandle<()>>) {
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, join_pumps(stdout, stderr)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_kills_its_group_on_drop_and_releases_its_slot() {
        let registry = ProcRegistry::new();
        // A pid that cannot exist: the guard must treat ESRCH as success rather
        // than report a failure, because "already gone" is the goal.
        let bogus = i32::MAX as u32;
        {
            let guard = registry.register(bogus);
            assert_eq!(guard.pgid(), bogus);
            assert_eq!(registry.len(), 1);
        }
        assert_eq!(registry.len(), 0, "the guard releases its slot on drop");
        assert!(process_group_gone(bogus));
    }

    #[test]
    fn a_disarmed_guard_does_not_kill() {
        let registry = ProcRegistry::new();
        let mut guard = registry.register(i32::MAX as u32);
        guard.disarm();
        drop(guard);
        assert!(registry.is_empty());
    }

    #[test]
    fn kill_all_empties_the_registry() {
        let registry = ProcRegistry::new();
        let _a = registry.register(i32::MAX as u32);
        registry.kill_all();
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn a_zero_pgid_is_neither_killed_nor_alive() {
        // pid 0 means "every process in my group" to killpg; sending a signal
        // there would be a disaster, so it is refused outright.
        assert!(!kill_process_group(0));
        assert!(process_group_gone(0));
        assert!(process_group_gone(i32::MAX as u32));
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
    fn the_timeout_constants_are_the_tools_ones() {
        // The point of re-exporting: if `tools` changes the budget, this crate
        // changes with it instead of drifting.
        assert_eq!(DEFAULT_SHELL_TIMEOUT_MS, 120_000);
        assert_eq!(MAX_SHELL_TIMEOUT_MS, 15 * 60 * 1000);
    }

    #[test]
    fn the_env_allowlist_is_the_documented_one() {
        assert_eq!(
            ENV_ALLOWLIST,
            &["PATH", "HOME", "LANG", "LC_ALL", "TERM", "TMPDIR"]
        );
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
}
