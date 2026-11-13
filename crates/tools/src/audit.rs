//! The audit log: one JSONL line per invocation, appended under
//! `<data>/audit/<node id>.jsonl`.
//!
//! Invariant: **the audit log never fails a tool call.** An append that
//! errors (full disk, permissions, a path that is not writable) is reported
//! through `tracing::error!` and dropped. Rationale, stated plainly: an audit
//! log that breaks the work is worse than one that misses a line — the work
//! is what the owner asked for, and a hard failure here would turn a
//! bookkeeping problem into a bot that cannot act. The flip side (a silently
//! missing line) is why the error is logged loudly rather than ignored.
//!
//! What the file *is* is stated just as plainly: one JSON object per line,
//! appended by whatever turns share this node, written 0600 into a 0700
//! directory (the line records paths and command lines, which are the owner's
//! business and nobody else's), and **not** fsynced — see [`AuditLog`].

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use swarmbot_contracts::{BotId, Invocation, InvocationId, NodeId, TaskId, TurnId};

/// The gate outcome as the audit line records it.
///
/// `run` = ran without asking, `ask` = a question was raised but never
/// answered (gate error), `allow`/`deny` = asked and answered, `refuse` =
/// refused by a `Never` ceiling before anyone was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditDecision {
    Run,
    Ask,
    Allow,
    Deny,
    Refuse,
}

impl AuditDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Ask => "ask",
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Refuse => "refuse",
        }
    }

    /// Parse the label a handler put into `ToolResult::meta`; unknown labels
    /// fall back to `Run` (the tool did execute, after all).
    pub fn from_label(label: &str) -> Self {
        match label {
            "ask" => Self::Ask,
            "allow" => Self::Allow,
            "deny" => Self::Deny,
            "refuse" => Self::Refuse,
            _ => Self::Run,
        }
    }
}

/// Whether the invocation produced a result or an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Ok,
    Error,
}

impl AuditOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }
}

/// One invocation, as one JSONL line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub ts: DateTime<Utc>,
    pub invocation_id: InvocationId,
    pub bot_id: BotId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<NodeId>,
    pub tool: String,
    /// The policy surface label (`shell`, `read_file`, …); empty for tools
    /// that have no surface (messaging, tasks).
    #[serde(default)]
    pub surface: String,
    /// The command line or path the call acted on. Never file contents.
    #[serde(default)]
    pub target: String,
    pub decision: AuditDecision,
    pub outcome: AuditOutcome,
    pub duration_ms: u64,
    pub result_bytes: usize,
}

impl AuditEntry {
    /// Start a line from the invocation it belongs to; `build()` stamps the
    /// timestamp.
    pub fn builder(invocation: &Invocation, tool: impl Into<String>) -> AuditEntryBuilder {
        AuditEntryBuilder {
            entry: AuditEntry {
                ts: Utc::now(),
                invocation_id: invocation.id.clone(),
                bot_id: invocation.bot_id.clone(),
                turn_id: invocation.turn_id.clone(),
                task_id: invocation.task_id.clone(),
                node_id: invocation.node_id.clone(),
                tool: tool.into(),
                surface: String::new(),
                target: String::new(),
                decision: AuditDecision::Run,
                outcome: AuditOutcome::Ok,
                duration_ms: 0,
                result_bytes: 0,
            },
        }
    }
}

/// Fluent builder for [`AuditEntry`]: every field the caller did not set
/// keeps a sane default, so a handler can record a partial line rather than
/// no line.
#[derive(Clone, Debug)]
pub struct AuditEntryBuilder {
    entry: AuditEntry,
}

impl AuditEntryBuilder {
    pub fn node(mut self, node_id: NodeId) -> Self {
        self.entry.node_id = Some(node_id);
        self
    }

    pub fn surface(mut self, surface: impl Into<String>) -> Self {
        self.entry.surface = surface.into();
        self
    }

    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.entry.target = target.into();
        self
    }

    pub fn decision(mut self, decision: AuditDecision) -> Self {
        self.entry.decision = decision;
        self
    }

    pub fn outcome(mut self, outcome: AuditOutcome) -> Self {
        self.entry.outcome = outcome;
        self
    }

    pub fn duration_ms(mut self, duration_ms: u64) -> Self {
        self.entry.duration_ms = duration_ms;
        self
    }

    pub fn result_bytes(mut self, result_bytes: usize) -> Self {
        self.entry.result_bytes = result_bytes;
        self
    }

    pub fn build(mut self) -> AuditEntry {
        self.entry.ts = Utc::now();
        self.entry
    }
}

/// Appends [`AuditEntry`] lines to one JSONL file.
///
/// What is actually guaranteed, since the earlier doc comment here claimed
/// more than the code did:
///
/// * **One line per entry.** The entry and its newline are built into one
///   buffer and handed to a single `write_all` on an `O_APPEND` handle. The
///   kernel places a write of this size (a few hundred bytes, occasionally a
///   few KiB) atomically at the end of the file, so concurrent turns cannot
///   interleave *inside* a line. The previous two-call shape (line, then
///   newline) had a scheduling point between the calls and produced lines
///   with two JSON objects on them — see
///   `concurrent_appends_keep_one_json_object_per_line`.
/// * **No lock is needed** for that, and none is taken: a lock would only
///   cover writers in this process, which is the smaller half of the problem
///   (a daemon and a brain write to different files, but two turns do not).
/// * **Not durable past the process.** There is no `fsync`: an fsync per tool
///   call would put a disk round-trip on the critical path of every action,
///   and this log is bookkeeping, not a ledger. A crash or power loss can
///   therefore lose the last line(s) the OS had buffered; a torn tail line is
///   possible and a reader should skip it rather than fail (the JSONL mirror
///   in `store` follows the same rule).
/// * **Not created world-readable.** New files are 0600 in a 0700 directory;
///   an existing file keeps the mode it already has, because a mode is only
///   applied at creation.
#[derive(Clone, Debug)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    /// A log writing to exactly this file (parents created on first write).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The conventional location: `<data_dir>/audit/<node id>.jsonl`, one
    /// file per node so a daemon and a brain never share a handle.
    pub fn for_node(data_dir: &Path, node_id: &NodeId) -> Self {
        Self::new(data_dir.join("audit").join(format!("{node_id}.jsonl")))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one line. Errors are logged and swallowed — see the module
    /// docs for why this must never fail the caller.
    pub fn record(&self, entry: AuditEntry) {
        let line = match serde_json::to_string(&entry) {
            Ok(line) => line,
            Err(error) => {
                tracing::error!(%error, tool = %entry.tool, "audit entry does not serialize; line dropped");
                return;
            }
        };
        if let Err(error) = self.append(&line) {
            tracing::error!(
                %error,
                path = %self.path.display(),
                tool = %entry.tool,
                invocation = %entry.invocation_id,
                "audit append failed; line dropped (tool call unaffected)"
            );
        }
    }

    fn append(&self, line: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            create_private_dir_all(parent)?;
        }
        // One buffer, one `write_all`: the newline travels with the JSON so
        // there is no scheduling point between "the object" and "its line
        // break" where another turn's line could land. `write_all` may issue a
        // second `write` only after a short write (a signal interrupting a
        // multi-page write), which a line this size does not produce.
        let mut buffer = String::with_capacity(line.len() + 1);
        buffer.push_str(line);
        buffer.push('\n');
        let mut file = open_append_private(&self.path)?;
        file.write_all(buffer.as_bytes())?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Private file creation
// ---------------------------------------------------------------------------

/// Create `dir` (and any missing parents) so that only its owner can enter it
/// (0700 on unix).
///
/// The audit, spool and spill directories hold command lines, paths and
/// command output — other people's data — so the process umask must not be
/// what decides who may read them. `registry::spill` calls this too: the two
/// places this crate creates a directory both want the same rule, and one
/// helper is how they cannot drift.
#[cfg(unix)]
pub(crate) fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Non-unix builds have no mode bits to set here; the directory inherits the
/// parent's ACL and there is nothing to tighten from this crate.
#[cfg(not(unix))]
pub(crate) fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Write `bytes` to `path` as a file only its owner can read (0600 on unix).
///
/// Like [`open_append_private`], the mode applies when the file is *created*:
/// an existing file keeps whatever mode it has, so this cannot loosen a file
/// an operator tightened by hand — and it also cannot tighten a 0644 spill
/// left behind by an older build.
#[cfg(unix)]
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// Open `path` for appending, creating it 0600 on unix. The audit line is
/// 0600 for the same reason the directory is 0700: it names the paths and
/// command lines somebody's bot acted on.
#[cfg(unix)]
fn open_append_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_append_private(path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use swarmbot_contracts::{BotId, ConversationId};
    use tempfile::TempDir;

    fn invocation() -> Invocation {
        let mut inv = Invocation::new(
            BotId::from_suffix("a".repeat(32).as_str()),
            ConversationId::new(),
        );
        inv.turn_id = Some(TurnId::new());
        inv.task_id = Some(TaskId::new());
        inv.node_id = Some(NodeId::local());
        inv
    }

    #[test]
    fn records_one_jsonl_line_per_entry() {
        let tmp = TempDir::new().expect("tempdir");
        let log = AuditLog::for_node(tmp.path(), &NodeId::local());
        let inv = invocation();
        log.record(
            AuditEntry::builder(&inv, "shell")
                .surface("shell")
                .target("echo hi")
                .decision(AuditDecision::Allow)
                .outcome(AuditOutcome::Ok)
                .duration_ms(42)
                .result_bytes(9)
                .build(),
        );
        log.record(
            AuditEntry::builder(&inv, "read_file")
                .decision(AuditDecision::Run)
                .outcome(AuditOutcome::Error)
                .build(),
        );

        let content = std::fs::read_to_string(log.path()).expect("the log exists");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "one line per invocation");
        assert_eq!(
            log.path().file_name().expect("name").to_str(),
            Some("nod_local.jsonl")
        );

        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSON");
        assert_eq!(first["tool"], "shell");
        assert_eq!(first["surface"], "shell");
        assert_eq!(first["target"], "echo hi");
        assert_eq!(first["decision"], "allow");
        assert_eq!(first["outcome"], "ok");
        assert_eq!(first["duration_ms"], 42);
        assert_eq!(first["result_bytes"], 9);
        assert_eq!(first["invocation_id"], inv.id.as_str());
        assert_eq!(first["bot_id"], inv.bot_id.as_str());
        assert_eq!(
            first["turn_id"],
            inv.turn_id.as_ref().expect("turn").as_str()
        );
        assert_eq!(
            first["task_id"],
            inv.task_id.as_ref().expect("task").as_str()
        );
        assert!(first["ts"].as_str().expect("timestamp").starts_with('2'));

        let second: serde_json::Value = serde_json::from_str(lines[1]).expect("valid JSON");
        assert_eq!(second["decision"], "run");
        assert_eq!(second["outcome"], "error");
    }

    /// The append is two `write_all` calls unless the newline is folded into
    /// the same buffer, and then a concurrent turn's line can land *between*
    /// them: two JSON objects on one line, which is a corrupt audit log.
    ///
    /// Several threads appending long lines is what makes that reproduce: a
    /// multi-kilobyte write holds the file's inode lock long enough that the
    /// next thread is descheduled waiting for it, and the scheduling point
    /// falls exactly between the two syscalls.
    #[test]
    fn concurrent_appends_keep_one_json_object_per_line() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 200;

        let tmp = TempDir::new().expect("tempdir");
        let log = Arc::new(AuditLog::for_node(tmp.path(), &NodeId::local()));
        // Padding so each line is a few KiB; the suffix keeps them distinct.
        let padding = "p".repeat(2048);

        let mut handles = Vec::new();
        for thread in 0..THREADS {
            let log = Arc::clone(&log);
            let padding = padding.clone();
            handles.push(std::thread::spawn(move || {
                let inv = invocation();
                for index in 0..PER_THREAD {
                    log.record(
                        AuditEntry::builder(&inv, "read_file")
                            .surface("read_file")
                            .target(format!("{padding}/{thread}/{index}"))
                            .decision(AuditDecision::Run)
                            .build(),
                    );
                }
            }));
        }
        for handle in handles {
            handle.join().expect("an appending thread finishes");
        }

        let content = std::fs::read_to_string(log.path()).expect("the log exists");
        let mut malformed = Vec::new();
        for (number, line) in content.lines().enumerate() {
            if let Err(error) = serde_json::from_str::<serde_json::Value>(line) {
                malformed.push(format!("line {}: {error}", number + 1));
            }
        }
        assert!(
            malformed.is_empty(),
            "{} line(s) are not one JSON object each: {:?}",
            malformed.len(),
            &malformed[..malformed.len().min(3)]
        );
        assert_eq!(
            content.lines().count(),
            THREADS * PER_THREAD,
            "one line per entry: a missing line means two objects were merged"
        );
    }

    #[test]
    fn an_unwritable_path_never_fails_the_caller() {
        let tmp = TempDir::new().expect("tempdir");
        // A regular file where the audit *directory* would have to be: every
        // create_dir_all/open must fail — and `record` must absorb it.
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "not a directory").expect("write");
        let log = AuditLog::new(blocker.join("audit").join("nod_local.jsonl"));
        log.record(AuditEntry::builder(&invocation(), "shell").build());
        // Reaching this line without a panic or an error return is the assertion.
    }

    /// The audit line names paths and command lines, so the file it lives in
    /// is the owner's business: 0600 in a 0700 directory, not 0644/0755 by
    /// umask.
    #[cfg(unix)]
    #[test]
    fn the_audit_file_is_created_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new().expect("tempdir");
        let log = AuditLog::for_node(tmp.path(), &NodeId::local());
        log.record(AuditEntry::builder(&invocation(), "read_file").build());

        let mode = std::fs::metadata(log.path())
            .expect("audit metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the audit log is readable by its owner only (mode {mode:o})"
        );
        let dir = log.path().parent().expect("an audit directory");
        let dir_mode = std::fs::metadata(dir)
            .expect("audit dir metadata")
            .permissions()
            .mode();
        assert_eq!(
            dir_mode & 0o777,
            0o700,
            "the audit dir is owner-traversable only"
        );
    }

    #[test]
    fn decision_labels_round_trip() {
        for decision in [
            AuditDecision::Run,
            AuditDecision::Ask,
            AuditDecision::Allow,
            AuditDecision::Deny,
            AuditDecision::Refuse,
        ] {
            assert_eq!(AuditDecision::from_label(decision.as_str()), decision);
        }
        assert_eq!(
            AuditDecision::from_label("nonsense"),
            AuditDecision::Run,
            "unknown labels fall back to run"
        );
    }
}
