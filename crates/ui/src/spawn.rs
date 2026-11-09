//! `--spawn-brain` / `--offline`: supervise `swarmbot-brain` as a child process.
//!
//! A supervisor, not an embedding (D7.2): the boundary does not move, the child
//! is a separate process the UI talks to over loopback HTTP exactly like a remote
//! brain. Everything here is [`std::process`] — no tokio in the UI process.
//!
//! Honesty rules this obeys:
//! * if the `swarmbot-brain` binary is not built, [`spawn`] returns
//!   [`SpawnError::NotFound`] naming the path it looked at — it never pretends the
//!   brain came up;
//! * the child is only considered started once `GET /api/health` answers `ok`,
//!   so "spawned" always means "reachable";
//! * `--offline` passes the brain's own `--offline` (the scripted fake backend)
//!   and, when the demo roster file is present, `--team`, so the app demos with
//!   no model credentials. If the roster file is missing it says so rather than
//!   inventing bots.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use swarmbot_client::{BrainClient, ClientError};

use crate::args::Args;

/// How long to wait for `/api/health` before declaring the brain unhealthy.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(20);
/// How often to poll health while waiting.
const HEALTH_POLL: Duration = Duration::from_millis(200);

/// Why a brain could not be spawned. Errors are values; each variant names the
/// concrete thing that went wrong so the window can show it.
#[derive(Debug)]
pub enum SpawnError {
    /// The `swarmbot-brain` binary was not found next to this executable.
    NotFound { path: PathBuf },
    /// `std::process` refused to start the child.
    Io { reason: String },
    /// The child started but `/api/health` never answered `ok` in time.
    Unhealthy { reason: String },
    /// The `--brain` URL could not be turned into a `HOST:PORT` to listen on.
    BadAddr { url: String },
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { path } => write!(
                f,
                "swarmbot-brain not found at {} (build it with `cargo build -p swarmbot-brain`)",
                path.display()
            ),
            Self::Io { reason } => write!(f, "cannot start swarmbot-brain: {reason}"),
            Self::Unhealthy { reason } => {
                write!(f, "swarmbot-brain did not become healthy: {reason}")
            }
            Self::BadAddr { url } => write!(f, "cannot derive a listen address from `{url}`"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// A running brain child. Killing it and (for a temp data dir) removing the
/// directory happen on drop, so an early return or a panic does not leak it.
pub struct SpawnedBrain {
    child: Child,
    /// The exact argv it was started with, kept for the report and for honesty.
    pub argv: Vec<String>,
    data_dir: PathBuf,
    /// Whether `data_dir` is a temp dir this struct created (and must remove).
    owns_data_dir: bool,
}

impl SpawnedBrain {
    /// The child's process id.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// The data directory the child was started with. The UI can read the owner
    /// token the brain minted there (see [`read_owner_token`]) so a spawned brain
    /// needs no separate credential.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Stop the child. SIGKILL via [`Child::kill`]: the spawned brain's data dir
    /// is disposable (a temp dir by default), so a graceful shutdown is not worth
    /// the extra failure modes here. This is stated plainly rather than dressed
    /// up as a clean stop.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read the owner token the brain minted at `<data-dir>/owner-token` (0600, the
/// raw token followed by a newline — the same value the brain puts in its
/// one-time login URL, so it authenticates REST and `/ws/live` alike).
///
/// Returns the path alongside the token so the caller can log the *source*
/// without ever logging the token. Every failure is an [`std::io::Error`] whose
/// message names the path tried, so the UI can fall back to the login prompt with
/// one honest line instead of a silent 401 loop.
pub fn read_owner_token(data_dir: &Path) -> std::io::Result<(PathBuf, String)> {
    let path = data_dir.join("owner-token");
    let raw = std::fs::read_to_string(&path)
        .map_err(|err| std::io::Error::new(err.kind(), format!("{}: {err}", path.display())))?;
    let token = raw.trim().to_string();
    if token.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} exists but is empty", path.display()),
        ));
    }
    Ok((path, token))
}

impl Drop for SpawnedBrain {
    fn drop(&mut self) {
        self.kill();
        if self.owns_data_dir {
            let _ = std::fs::remove_dir_all(&self.data_dir);
        }
    }
}

/// Spawn the brain described by `args`, wait for it to become healthy, and
/// return the guard. `args.offline` implies `args.spawn_brain`.
pub fn spawn(args: &Args) -> Result<SpawnedBrain, SpawnError> {
    let bin = brain_binary()?;
    let (data_dir, owns) = resolve_data_dir(args)?;
    let addr = listen_addr(args)?;

    let mut command = Command::new(&bin);
    command
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--addr")
        .arg(&addr)
        // Keep the child's logs out of the UI's stdout so `--report` stays clean;
        // inherit stderr so a crash is visible.
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    if args.offline {
        // The brain's own `--offline` forces the scripted fake backend.
        command.arg("--offline");
        if let Some(team) = find_team_file() {
            command.arg("--team").arg(&team);
        }
    }

    let argv: Vec<String> = std::iter::once(bin.display().to_string())
        .chain(command_args(&command))
        .collect();

    let child = command.spawn().map_err(|err| SpawnError::Io {
        reason: err.to_string(),
    })?;

    let spawned = SpawnedBrain {
        child,
        argv,
        data_dir,
        owns_data_dir: owns,
    };

    // Wait for health before handing back the guard; if it never comes, drop
    // the guard (which kills the child) and report why.
    match wait_healthy(&args.brain) {
        Ok(()) => Ok(spawned),
        Err(err) => {
            drop(spawned);
            Err(err)
        }
    }
}

/// The `swarmbot-brain` binary, expected next to this executable (both live in
/// `target/<profile>/`).
fn brain_binary() -> Result<PathBuf, SpawnError> {
    let exe = std::env::current_exe().map_err(|err| SpawnError::Io {
        reason: format!("cannot locate this executable: {err}"),
    })?;
    let candidate = exe.with_file_name("swarmbot-brain");
    if candidate.exists() {
        Ok(candidate)
    } else {
        Err(SpawnError::NotFound { path: candidate })
    }
}

/// The data dir: `--data-dir` when given (not owned, not removed on exit), else
/// a fresh temp dir named for this process (owned, removed on exit).
fn resolve_data_dir(args: &Args) -> Result<(PathBuf, bool), SpawnError> {
    if let Some(dir) = &args.data_dir {
        let path = PathBuf::from(dir);
        std::fs::create_dir_all(&path).map_err(|err| SpawnError::Io {
            reason: format!("cannot create the data dir {}: {err}", path.display()),
        })?;
        return Ok((path, false));
    }
    let dir = std::env::temp_dir().join(format!("swarmbot-ui-brain-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|err| SpawnError::Io {
        reason: format!("cannot create the temp data dir {}: {err}", dir.display()),
    })?;
    Ok((dir, true))
}

/// `HOST:PORT` for the child to listen on: `--brain-addr` when given, else
/// derived from the `--brain` URL's authority.
fn listen_addr(args: &Args) -> Result<String, SpawnError> {
    if let Some(addr) = &args.brain_addr {
        return Ok(addr.clone());
    }
    let url = url::Url::parse(&args.brain).map_err(|_| SpawnError::BadAddr {
        url: args.brain.clone(),
    })?;
    let host = url.host_str().ok_or_else(|| SpawnError::BadAddr {
        url: args.brain.clone(),
    })?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| SpawnError::BadAddr {
            url: args.brain.clone(),
        })?;
    Ok(format!("{host}:{port}"))
}

/// Find the demo roster so `--offline` has bots to show. Looked for relative to
/// the current dir and to the workspace root inferred from the executable path
/// (`target/<profile>/swarmbot-ui` → up three). `None` when it is not present.
fn find_team_file() -> Option<PathBuf> {
    let relative = Path::new("config/team.example.toml");
    if relative.exists() {
        return Some(relative.to_path_buf());
    }
    if let Ok(exe) = std::env::current_exe() {
        // exe = <root>/target/<profile>/swarmbot-ui
        if let Some(root) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
            let candidate = root.join("config/team.example.toml");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Poll `GET /api/health` until the brain is serving HTTP, or the timeout
/// passes.
///
/// A `401 Unauthorized` counts as **up**: the child's HTTP server is answering
/// and enforcing auth, which is exactly what a supervisor needs to know. Whether
/// *this* client is authenticated is a separate concern — the app shows its login
/// prompt (or uses `--token`) — so the supervisor must not treat "needs a token"
/// as "failed to start". Only a transport failure (nothing listening yet) or a
/// timeout means the brain did not come up.
fn wait_healthy(brain_url: &str) -> Result<(), SpawnError> {
    let client = BrainClient::new(brain_url).map_err(|err| SpawnError::Unhealthy {
        reason: err.to_string(),
    })?;
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    let mut last = "no answer yet".to_string();
    while Instant::now() < deadline {
        match client.health() {
            Ok(_) => return Ok(()),
            // Up and enforcing auth: the UI's login prompt takes it from here.
            Err(ClientError::Unauthorized) => return Ok(()),
            Err(err) => last = err.to_string(),
        }
        std::thread::sleep(HEALTH_POLL);
    }
    Err(SpawnError::Unhealthy { reason: last })
}

/// The argv a [`Command`] will run, as strings, for the report. `Command` does
/// not expose its args, so this re-derives them via the `Debug` shape; good
/// enough for a log line and clearly labelled as such.
fn command_args(command: &Command) -> Vec<String> {
    format!("{command:?}")
        .split_whitespace()
        .skip(1)
        .map(|token| token.trim_matches('"').to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A hand-rolled temp dir (the crate cannot add `tempfile`), removed on drop
    /// so a failed assertion does not leak it.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "swarmbot-ui-spawn-{}-{}-{}",
                tag,
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&path).expect("create the temp dir");
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_a_hand_written_owner_token_and_trims_it() {
        let dir = TempDir::new("read");
        std::fs::write(dir.path().join("owner-token"), "sekrit-token\n").expect("write");
        let (path, token) = read_owner_token(dir.path()).expect("reads the minted token");
        assert_eq!(token, "sekrit-token", "the trailing newline is trimmed");
        assert_eq!(path, dir.path().join("owner-token"));
    }

    #[test]
    fn a_missing_token_file_is_an_error_that_names_the_path() {
        let dir = TempDir::new("missing");
        let err = read_owner_token(dir.path()).expect_err("no file yet");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(
            err.to_string().contains("owner-token"),
            "the honest message names the path tried: {err}"
        );
    }

    #[test]
    fn an_empty_token_file_is_rejected() {
        let dir = TempDir::new("empty");
        std::fs::write(dir.path().join("owner-token"), "   \n").expect("write");
        let err = read_owner_token(dir.path()).expect_err("empty is not a token");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
