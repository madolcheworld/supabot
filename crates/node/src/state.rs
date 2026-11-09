//! The live-state file: what `swarmbot-node status` reads to answer "is a
//! daemon running, and is it connected?" without guessing.
//!
//! The rule this module exists to enforce is **do not guess**. A `status` that
//! infers liveness from the age of a file would report a crashed daemon as
//! running and a busy one as dead. So `run` writes `<data>/node-state.json` on
//! start, updates it on every connection change, and removes it on a clean
//! exit; `status` reads it and then checks the recorded pid with `kill(pid, 0)`,
//! which distinguishes "running" from "the file outlived the process".
//!
//! Writes go through a temporary file and a rename, so a reader never sees half
//! a JSON document — a `status` that fails to parse would be indistinguishable
//! from a `status` that finds nothing.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use swarmbot_contracts::{NodeRoot, PolicyMap};

use crate::NodeError;
use crate::config::NodePaths;

/// The state file's name inside the data dir.
pub const STATE_FILE: &str = "node-state.json";

/// One snapshot of a running (or recently running) node.
///
/// Nothing here is secret: no key material, no signatures, no pairing code.
/// The brain origin and node id are already on the wire in every upgrade.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeState {
    pub pid: u32,
    pub node_id: String,
    pub label: String,
    /// The brain origin pinned at pairing — the origin every signature binds to.
    pub brain_origin: String,
    /// The control-socket URL this daemon is dialing.
    pub brain_url: Option<String>,
    pub data_dir: PathBuf,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// True while a control socket is up **and** `node.hello` has verified.
    pub connected: bool,
    pub connected_at: Option<DateTime<Utc>>,
    /// Why the last dial failed, when it did. Kept so `status` can say
    /// "refused" instead of just "not connected".
    pub last_error: Option<String>,
    pub active_invocations: u32,
    /// The configured facts `status` reports: what this node allows.
    pub roots: Vec<NodeRoot>,
    pub ceilings: PolicyMap,
    pub shell_enabled: bool,
    pub capabilities: Vec<String>,
}

impl NodeState {
    /// A disconnected snapshot of a node that is about to start dialing.
    ///
    /// One argument per reported fact rather than a parameter struct: every
    /// caller already holds these values separately (a config, an identity, a
    /// runtime), and bundling them would only add a type that exists to be
    /// unpacked again.
    #[allow(clippy::too_many_arguments)]
    pub fn starting(
        pid: u32,
        node_id: &str,
        label: &str,
        brain_origin: &str,
        brain_url: Option<String>,
        data_dir: &Path,
        roots: Vec<NodeRoot>,
        ceilings: PolicyMap,
        shell_enabled: bool,
        capabilities: Vec<String>,
    ) -> NodeState {
        let now = Utc::now();
        NodeState {
            pid,
            node_id: node_id.to_string(),
            label: label.to_string(),
            brain_origin: brain_origin.to_string(),
            brain_url,
            data_dir: data_dir.to_path_buf(),
            started_at: now,
            updated_at: now,
            connected: false,
            connected_at: None,
            last_error: None,
            active_invocations: 0,
            roots,
            ceilings,
            shell_enabled,
            capabilities,
        }
    }
}

/// What `status` concludes from the file plus the pid check.
#[derive(Clone, Debug, PartialEq)]
pub enum RunStatus {
    /// No state file: no daemon has run here (or the last one exited cleanly).
    NotRunning,
    /// A state file whose pid is gone. Reported distinctly, because it means
    /// the daemon was killed rather than stopped — and the owner should know
    /// which.
    Crashed { state: NodeState },
    /// The daemon is alive; `state.connected` says whether it has a socket.
    Running { state: NodeState },
}

impl RunStatus {
    pub fn state(&self) -> Option<&NodeState> {
        match self {
            RunStatus::NotRunning => None,
            RunStatus::Crashed { state } | RunStatus::Running { state } => Some(state),
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self, RunStatus::Running { .. })
    }
}

/// Maintains `<data>/node-state.json` for one running daemon.
#[derive(Clone, Debug)]
pub struct StateWriter {
    path: PathBuf,
}

impl StateWriter {
    /// Write the starting snapshot. The file exists from the first moment the
    /// daemon is up, so `status` never reports "not running" for a node that is
    /// mid-dial.
    pub fn start(paths: &NodePaths, state: &NodeState) -> Result<StateWriter, NodeError> {
        let writer = StateWriter {
            path: paths.state(),
        };
        if let Some(parent) = writer.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| NodeError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        writer.write(state)?;
        Ok(writer)
    }

    /// Open the existing state file without writing, so `run` can update a
    /// snapshot another part of the daemon created.
    pub fn at(paths: &NodePaths) -> StateWriter {
        StateWriter {
            path: paths.state(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read-modify-write one snapshot. `edit` returns the value to store.
    ///
    /// A failed update is returned, not swallowed: `status` would otherwise
    /// keep reporting a connection state that stopped being true, which is the
    /// one thing this file must not do.
    pub fn update(&self, edit: impl FnOnce(&mut NodeState)) -> Result<NodeState, NodeError> {
        let mut state = read_file(&self.path)?.unwrap_or_else(|| {
            // The file vanished under us (an owner cleaned the data dir while
            // the daemon ran). Re-create it from nothing rather than lose the
            // pid: a missing file reads as "not running".
            let now = Utc::now();
            NodeState {
                pid: std::process::id(),
                node_id: String::new(),
                label: String::new(),
                brain_origin: String::new(),
                brain_url: None,
                data_dir: self
                    .path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default(),
                started_at: now,
                updated_at: now,
                connected: false,
                connected_at: None,
                last_error: None,
                active_invocations: 0,
                roots: Vec::new(),
                ceilings: PolicyMap::default(),
                shell_enabled: false,
                capabilities: Vec::new(),
            }
        });
        edit(&mut state);
        state.updated_at = Utc::now();
        self.write(&state)?;
        Ok(state)
    }

    /// Remove the file on a clean exit. Best effort: a leftover file is
    /// reported as `Crashed` by the pid check, which is honest, whereas
    /// failing the shutdown would leave the daemon running.
    pub fn remove(&self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                %error,
                "could not remove the state file; `status` will report it as crashed until it is deleted"
            );
        }
    }

    fn write(&self, state: &NodeState) -> Result<(), NodeError> {
        let text = serde_json::to_string_pretty(state)
            .map_err(|error| NodeError::Json(error.to_string()))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|source| NodeError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, &self.path).map_err(|source| {
            let _ = std::fs::remove_file(&tmp);
            NodeError::Io {
                path: self.path.clone(),
                source,
            }
        })?;
        Ok(())
    }
}

/// Read the state file, if there is one.
pub fn read_file(path: &Path) -> Result<Option<NodeState>, NodeError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(NodeError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let state: NodeState = serde_json::from_str(&text).map_err(|error| {
        NodeError::Config(format!(
            "{} is not a state file this build wrote: {error}",
            path.display()
        ))
    })?;
    Ok(Some(state))
}

/// Read the state file and check the pid, which is the only honest way to tell
/// "running" from "the file outlived the process".
pub fn read(paths: &NodePaths) -> Result<RunStatus, NodeError> {
    match read_file(&paths.state())? {
        None => Ok(RunStatus::NotRunning),
        Some(state) if pid_alive(state.pid) => Ok(RunStatus::Running { state }),
        Some(state) => Ok(RunStatus::Crashed { state }),
    }
}

/// Is a process with this pid alive? `kill(pid, 0)` sends nothing and reports
/// existence; `EPERM` means it exists but belongs to somebody else, which still
/// counts as alive (and is the right answer when a node runs as another user).
pub fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs no action on the target; it only sets errno.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::{NodeRoot, PolicyCeiling};

    fn state(paths: &NodePaths, pid: u32) -> NodeState {
        NodeState::starting(
            pid,
            "nod_0189fbd3a4d37f0e9c0e6a3f8f1c9d2e",
            "bench",
            "https://brain.example",
            Some("wss://brain.example/node".into()),
            &paths.data_dir,
            vec![NodeRoot::read_write("work", "/srv/work")],
            PolicyMap::uniform(PolicyCeiling::Ask),
            true,
            vec!["fs@1".into(), "shell@1".into()],
        )
    }

    #[test]
    fn no_file_means_not_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(dir.path());
        assert_eq!(read(&paths).expect("reads"), RunStatus::NotRunning);
    }

    #[test]
    fn a_live_pid_reads_as_running_and_updates_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(dir.path());
        let writer =
            StateWriter::start(&paths, &state(&paths, std::process::id())).expect("starts");
        match read(&paths).expect("reads") {
            RunStatus::Running { state } => {
                assert!(!state.connected, "a fresh daemon is not connected yet");
                assert_eq!(state.node_id, "nod_0189fbd3a4d37f0e9c0e6a3f8f1c9d2e");
            }
            other => panic!("expected Running, got {other:?}"),
        }

        writer
            .update(|state| {
                state.connected = true;
                state.connected_at = Some(Utc::now());
                state.active_invocations = 3;
            })
            .expect("updates");
        let updated = read(&paths).expect("reads");
        let state = updated.state().expect("a state");
        assert!(state.connected);
        assert_eq!(state.active_invocations, 3);
        assert!(state.connected_at.is_some());

        writer.remove();
        assert_eq!(read(&paths).expect("reads"), RunStatus::NotRunning);
        // Removing twice is not an error.
        writer.remove();
    }

    #[test]
    fn a_dead_pid_reads_as_crashed_not_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(dir.path());
        // Pid 1 exists but is not ours; a pid that cannot exist is what a
        // crashed daemon leaves behind. Use a pid above the kernel maximum so
        // the test cannot accidentally point at a live process.
        let dead = i32::MAX as u32;
        assert!(!pid_alive(dead), "the test needs a pid that is not alive");
        StateWriter::start(&paths, &state(&paths, dead)).expect("starts");
        match read(&paths).expect("reads") {
            RunStatus::Crashed { state } => assert_eq!(state.pid, dead),
            other => panic!("expected Crashed, got {other:?}"),
        }
    }

    #[test]
    fn our_own_pid_is_alive_and_zero_is_not() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0), "pid 0 is the scheduler, not a process");
    }

    #[test]
    fn a_state_file_never_carries_key_material() {
        // `status` prints this file's contents; the identity must not be in it.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(dir.path());
        StateWriter::start(&paths, &state(&paths, std::process::id())).expect("starts");
        let text = std::fs::read_to_string(paths.state()).expect("readable");
        for field in ["secret", "private", "secretKey", "signing"] {
            assert!(!text.to_ascii_lowercase().contains(field), "{text}");
        }
        // It does carry the facts status reports.
        for expected in ["nod_0189", "brain.example", "fs@1", "read_write"] {
            assert!(text.contains(expected), "{expected} missing from {text}");
        }
    }

    #[test]
    fn a_corrupt_state_file_is_an_error_not_a_guess() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(dir.path());
        std::fs::write(paths.state(), "{ not json").expect("write");
        assert!(matches!(read(&paths), Err(NodeError::Config(_))));
    }
}
