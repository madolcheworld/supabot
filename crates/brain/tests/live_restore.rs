//! Restoring a backup **into a brain that is still running** — the procedure the
//! README listed as "not tested" for two rounds.
//!
//! Two things are proven here, and they are different in kind:
//!
//! 1. **The refusal is real, against a real process.** A live `swarmbot-brain`
//!    holds `swarmbot.sqlite` open. `swarmbot_store::restore` finds that process
//!    by its file descriptor, refuses, and names its pid — so the owner is told
//!    *why* rather than discovering later that the brain kept writing to the old
//!    inode and undid the restore.
//! 2. **The documented procedure works once the brain is stopped.** Stop it,
//!    restore, start it again, and the brain serves the backup's data.
//!
//! The brain is the **real binary** (`CARGO_BIN_EXE_swarmbot-brain`, this crate's
//! own bin), on a real loopback listener, over a real data dir. The restore is
//! called through `swarmbot_store` rather than through the CLI binary because
//! `CARGO_BIN_EXE_*` is only defined for a package's own bins — the CLI's
//! `backup restore` command is covered in `crates/cli/tests/backup.rs`, which
//! asserts the file-level behaviour and the `--brain` refusal; the live-process
//! half is the part that needs the brain's own binary and so lives here.
//!
//! Nothing touches the network: the brain runs `--offline` with the demo team,
//! which needs no credential.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};

use serde_json::Value;

const BRAIN_BIN: &str = env!("CARGO_BIN_EXE_swarmbot-brain");

/// Generous, because the thing being waited for is a real process binding a
/// socket on a machine that may be loaded; bounded, so a wedged child fails the
/// test instead of the budget.
const BUDGET: Duration = Duration::from_secs(30);

/// A directory that removes itself, hand-rolled for the reason `crates/cli/src/tmp.rs`
/// gives: adding `tempfile` here would rewrite the shared `Cargo.lock`.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "swarmbot-live-restore-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("a temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A brain child that is killed on drop, so a panicking test leaves no daemon.
struct Brain {
    child: Child,
    addr: String,
    token: String,
}

impl Drop for Brain {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Brain {
    /// `GET path` with the owner token, as JSON. Hand-rolled because the request
    /// is a status line and a body, and a client dependency here would rewrite the
    /// shared lock file.
    fn get(&self, path: &str) -> Result<Value, String> {
        let mut stream = TcpStream::connect(&self.addr).map_err(|error| error.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|error| error.to_string())?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            self.addr, self.token
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|error| error.to_string())?;
        let mut raw = String::new();
        stream
            .read_to_string(&mut raw)
            .map_err(|error| error.to_string())?;
        let body = raw
            .split("\r\n\r\n")
            .nth(1)
            .ok_or_else(|| format!("no body in the response: {raw}"))?;
        serde_json::from_str(body).map_err(|error| format!("{error}: {body}"))
    }
}

/// Start `swarmbot-brain` on a loopback port of its own choosing, with the team
/// file at `team` and no credentials, and wait until `/api/health` answers.
///
/// The team file is how a data dir gets a roster *through the product*: booting
/// provisions every bot in it that is not there yet, which is additive — proven
/// by the `extra` bot appearing on a second boot in the procedure test.
///
/// The address comes from the brain's own log line rather than from a port the
/// test bound and handed over: that hand-over is the TOCTOU shape `AGENTS.md`
/// records as a flake source, and the brain already announces `listener=<addr>`
/// when it binds.
fn start_brain(data_dir: &Path, team: &Path) -> Brain {
    let mut child = Command::new(BRAIN_BIN)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--addr")
        .arg("127.0.0.1:0")
        .arg("--offline")
        .arg("--team")
        .arg(team)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the swarmbot-brain binary spawns");
    let stdout = child.stdout.take().expect("the child's stdout is piped");
    let (tx, rx): (Sender<String>, _) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let token = std::fs::read_to_string(data_dir.join("owner-token"))
        .map(|text| text.trim().to_string())
        .unwrap_or_default();

    let mut brain = Brain {
        child,
        addr: String::new(),
        token,
    };
    let deadline = Instant::now() + BUDGET;
    loop {
        if brain
            .child
            .try_wait()
            .expect("the child is pollable")
            .is_some()
        {
            panic!("the brain exited before it bound a socket");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("the brain never printed its listener address within {BUDGET:?}");
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(line) => {
                if let Some(addr) = listener_addr(&line) {
                    brain.addr = addr;
                    if brain.get("/api/health").is_ok() {
                        return brain;
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the brain's log stream ended before it bound a socket");
            }
        }
    }
}

/// The address out of a `listener=127.0.0.1:PORT` field in the brain's log.
///
/// A helper rather than an inline `split`: the tracing prefix is full of digits
/// and colons, and matching the wrong token out of it is exactly the bug
/// `AGENTS.md` records from `real_brain`'s `401`-inside-a-timestamp failure. The
/// test below pins it.
fn listener_addr(line: &str) -> Option<String> {
    let rest = line.split("listener=").nth(1)?;
    let candidate: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ':')
        .collect();
    (candidate.contains(':') && !candidate.ends_with(':')).then_some(candidate)
}

/// A team file with one bot, so a roster exists without the demo team's four and
/// a second file can add exactly one more.
fn team_file(dir: &TempDir, name: &str, bots: &[(&str, &str)]) -> PathBuf {
    let path = dir.path().join(name);
    let mut body = String::new();
    for (id, label) in bots {
        body.push_str(&format!(
            "[[bots]]\nname = \"{id}\"\nlabel = \"{label}\"\ninstructions = \"you are {id}\"\n\n"
        ));
    }
    std::fs::write(&path, body).expect("the team file");
    path
}

/// A data dir with one bot in it, made by a brain that booted once and was
/// stopped — the product's own provisioning path, not a hand-built database.
fn seeded_data_dir(dir: &TempDir, team: &Path) -> PathBuf {
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).expect("the data dir");
    drop(start_brain(&data, team));
    assert!(
        data.join("swarmbot.sqlite").is_file(),
        "booting once wrote the database"
    );
    data
}

fn backup_into(dir: &TempDir, data: &Path) -> PathBuf {
    let out = dir.path().join("backup");
    swarmbot_store::backup::backup(data, &out).expect("a backup is written");
    swarmbot_store::backup::verify(&out).expect("the backup verifies");
    out
}

#[test]
fn listener_addr_reads_the_field_and_never_a_timestamp() {
    // A log line with a tracing prefix full of digits and colons: the parser must
    // take the `listener=` field and nothing from the clock.
    let line = "2026-11-13T10:54:46.448745Z  INFO request{http.method=GET}: \
                swarmbot_brain::server: the public listener is up: REST, /ws/live and the node \
                gateway listener=127.0.0.1:38001";
    assert_eq!(listener_addr(line).as_deref(), Some("127.0.0.1:38001"));
    assert_eq!(listener_addr("no field here"), None);
    // A bind that has not finished has no port, and must not be mistaken for one.
    assert_eq!(listener_addr("listener=127.0.0.1:"), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restore_into_a_running_brain_is_refused_and_names_the_process() {
    let dir = TempDir::new("running");
    let team = team_file(&dir, "one.toml", &[("lead", "Lead")]);
    let data = seeded_data_dir(&dir, &team);
    let out = backup_into(&dir, &data);

    // The brain is running and holds the database open.
    let brain = start_brain(&data, &team);

    let refused = swarmbot_store::restore::restore(&out, &data);
    let message = refused
        .as_ref()
        .expect_err("a restore into a running brain is refused")
        .to_string();
    assert!(
        message.contains("Stop the brain"),
        "the refusal tells the owner what to do: {message}"
    );
    // Naming the pid is the only way an owner can tell "my brain" from some other
    // tool that happens to hold the file.
    let pid = brain.child.id().to_string();
    assert!(
        message.contains(&pid),
        "the refusal names pid {pid}: {message}"
    );
    assert!(
        message.contains("would keep writing to the old file"),
        "the refusal explains why it is dangerous: {message}"
    );
    // And the data dir was not touched: the brain still serves its roster, which
    // it could not do if its database had been switched underneath it.
    let roster = brain.get("/api/bots").expect("the brain still answers");
    assert!(
        roster.as_array().is_some_and(|rows| !rows.is_empty()),
        "the running brain kept its roster: {roster}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_documented_procedure_stop_restore_start_serves_the_backup() {
    let dir = TempDir::new("procedure");
    let one = team_file(&dir, "one.toml", &[("lead", "Lead")]);
    let two = team_file(&dir, "two.toml", &[("lead", "Lead"), ("extra", "Extra")]);
    let data = seeded_data_dir(&dir, &one);
    let out = backup_into(&dir, &data);
    assert_eq!(
        roster_names(start_brain(&data, &one)),
        vec!["lead".to_string()],
        "the seeded data dir holds exactly the one bot"
    );

    // The data dir changes after the backup. Provisioning is additive, so booting
    // with the two-bot team adds `extra` and leaves `lead` alone — which is what
    // makes "the backup's roster came back" a real assertion rather than "the
    // bytes never moved".
    let after_add = roster_names(start_brain(&data, &two));
    assert!(
        after_add.iter().any(|name| name == "extra"),
        "the second team is in the roster: {after_add:?}"
    );

    // Stop the brain (every `start_brain` above was dropped by `roster_names`),
    // then restore — the procedure the docs tell an owner to follow.
    let restored = swarmbot_store::restore::restore(&out, &data).expect("the restore happens");
    assert!(
        restored.safety.is_some(),
        "the previous data dir was preserved, so a restore of the wrong snapshot has a way back"
    );

    // Started again with the **one-bot** team, so nothing is re-provisioned and
    // the roster is purely what the database holds: the bot added after the
    // backup is gone.
    let names = roster_names(start_brain(&data, &one));
    assert_eq!(
        names,
        vec!["lead".to_string()],
        "the restored database serves the backup's roster, not the post-backup one"
    );

    // And the safety copy puts the added bot back: the rollback is real, not a
    // directory that merely exists.
    let safety = restored.safety.expect("a safety dir");
    swarmbot_store::restore::restore(&safety, &data).expect("the rollback happens");
    let names = roster_names(start_brain(&data, &one));
    assert!(
        names.iter().any(|name| name == "extra"),
        "the rollback brought the added bot back: {names:?}"
    );
}

/// The bot names a freshly started brain serves, then the brain stops (its
/// `Drop` kills it) when this returns.
fn roster_names(brain: Brain) -> Vec<String> {
    let roster = brain.get("/api/bots").expect("the brain answers /api/bots");
    roster
        .as_array()
        .unwrap_or_else(|| panic!("a roster array, got {roster}"))
        .iter()
        .filter_map(|bot| bot.get("name").and_then(Value::as_str).map(str::to_string))
        .collect()
}
