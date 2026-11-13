//! `swarmbot backup` against the real binary.
//!
//! The store crate proves the snapshot is correct; this file proves the *command*
//! is honest about it, because the command is what an owner actually runs and a
//! green unit test behind a command that prints "ok" for a bad backup is worse
//! than no command at all.
//!
//! Two things are pinned here that a store-level test cannot see:
//!
//! * `backup create` **verifies what it just wrote** before it reports success.
//!   A snapshot that fails `integrity_check` must make the command exit non-zero,
//!   never print a cheerful summary.
//! * `backup create` refuses `--brain`. The database lives on the daemon's
//!   machine; this CLI cannot reach it, and backing up a local directory that
//!   merely shares a path would be a backup of the wrong thing.
//!
//! Every test builds its own `TempDir` data dir by running `swarmbot demo`
//! (offline, in-process, no network) and asserts on the real process's status and
//! stdout. Nothing touches the network.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_swarmbot");

/// A directory that removes itself when it goes out of scope.
///
/// Hand-rolled for the reason `src/tmp.rs` gives: `tempfile` is in the workspace
/// lock file, but adding the dependency edge from this crate would make cargo
/// rewrite the shared `Cargo.lock`, which is off-limits during parallel work. A
/// dev-dependency would still be a lock rewrite.
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
            "swarmbot-test-{tag}-{}-{nanos}-{n}",
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

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("the swarmbot binary spawns (CARGO_BIN_EXE_swarmbot)")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// A data dir with a real database and transcript tree in it, made by the
/// product's own demo rather than by hand: if `demo` ever stops writing those
/// files, a backup test that seeded them itself would not notice.
fn a_data_dir(dir: &Path) -> PathBuf {
    let data = dir.join("data");
    let demo = run(&["--data-dir", data.to_str().expect("utf-8 path"), "demo"]);
    assert_eq!(
        demo.status.code(),
        Some(0),
        "the demo seeds the data dir: {}",
        stderr_of(&demo)
    );
    assert!(
        data.join("swarmbot.sqlite").is_file(),
        "the demo wrote a database"
    );
    data
}

#[test]
fn a_backup_is_created_and_verified_by_the_command_itself() {
    let dir = TempDir::new("backup");
    let data = a_data_dir(dir.path());
    let out = dir.path().join("backup");

    let create = run(&[
        "--data-dir",
        data.to_str().expect("utf-8 path"),
        "backup",
        "create",
        "--out",
        out.to_str().expect("utf-8 path"),
    ]);
    let stdout = stdout_of(&create);
    assert_eq!(
        create.status.code(),
        Some(0),
        "backup create succeeds: {stdout}{}",
        stderr_of(&create)
    );
    // The command verifies its own output, and says so — a backup nobody opened
    // is a guess, so "verified" is part of the success line rather than a
    // separate command the owner has to remember.
    assert!(
        stdout.contains("verified: integrity_check=ok"),
        "the command verified what it wrote: {stdout}"
    );
    assert!(out.join("swarmbot.sqlite").is_file(), "{stdout}");
    assert!(out.join("MANIFEST.json").is_file(), "{stdout}");

    // And a later `verify` agrees, from disk, without the create's report.
    let verify = run(&["backup", "verify", out.to_str().expect("utf-8 path")]);
    let verified = stdout_of(&verify);
    assert_eq!(
        verify.status.code(),
        Some(0),
        "backup verify succeeds: {verified}{}",
        stderr_of(&verify)
    );
    assert!(verified.contains("integrity_check=ok"), "{verified}");
    assert!(
        verified.contains("bots="),
        "the row counts are shown: {verified}"
    );
}

#[test]
fn a_tampered_manifest_fails_the_verify_command() {
    let dir = TempDir::new("backup");
    let data = a_data_dir(dir.path());
    let out = dir.path().join("backup");
    let create = run(&[
        "--data-dir",
        data.to_str().expect("utf-8 path"),
        "backup",
        "create",
        "--out",
        out.to_str().expect("utf-8 path"),
    ]);
    assert_eq!(create.status.code(), Some(0), "{}", stderr_of(&create));

    // A manifest that lies about a row count: the store's `verify` re-derives the
    // numbers from the snapshot, so this is a detected failure rather than a
    // green check on a hand-edited file.
    let manifest_path = out.join("MANIFEST.json");
    let text = std::fs::read_to_string(&manifest_path).expect("the manifest is readable");
    let mut value: serde_json::Value = serde_json::from_str(&text).expect("the manifest is JSON");
    let tables = value
        .get_mut("tables")
        .and_then(serde_json::Value::as_object_mut)
        .expect("the manifest has a tables object");
    tables.insert("bots".to_string(), serde_json::json!(9999));
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&value).expect("re-encode"),
    )
    .expect("the manifest is writable");

    let verify = run(&["backup", "verify", out.to_str().expect("utf-8 path")]);
    assert_ne!(
        verify.status.code(),
        Some(0),
        "a manifest that lies is a failure, not a warning: {}",
        stdout_of(&verify)
    );
    let stderr = stderr_of(&verify);
    assert!(
        stderr.contains("9999"),
        "the error names the claim that was false: {stderr}"
    );
}

#[test]
fn a_non_empty_target_is_refused_instead_of_overwritten() {
    let dir = TempDir::new("backup");
    let data = a_data_dir(dir.path());
    let out = dir.path().join("backup");
    std::fs::create_dir_all(&out).expect("the target dir");
    std::fs::write(out.join("keep.txt"), b"an older backup").expect("a file in the target");

    let create = run(&[
        "--data-dir",
        data.to_str().expect("utf-8 path"),
        "backup",
        "create",
        "--out",
        out.to_str().expect("utf-8 path"),
    ]);
    assert_ne!(
        create.status.code(),
        Some(0),
        "a backup must not overwrite an older one: {}",
        stdout_of(&create)
    );
    assert!(
        stderr_of(&create).contains("not empty"),
        "the refusal says why: {}",
        stderr_of(&create)
    );
    // The file that was there is untouched: a refusal that still wrote would be
    // the same data-loss bug with a message attached.
    assert_eq!(
        std::fs::read(out.join("keep.txt")).expect("still there"),
        b"an older backup"
    );
    assert!(!out.join("MANIFEST.json").exists(), "nothing was completed");
}

#[test]
fn backup_create_refuses_a_remote_brain() {
    // No daemon is started and none is needed: the refusal happens before any
    // dial, because the answer does not depend on the daemon being up — its
    // database is on its machine either way.
    let dir = TempDir::new("backup");
    let out = dir.path().join("backup");
    let create = run(&[
        "--brain",
        "http://127.0.0.1:9",
        "--token",
        "not-a-real-token",
        "backup",
        "create",
        "--out",
        out.to_str().expect("utf-8 path"),
    ]);
    assert_eq!(create.status.code(), Some(1), "{}", stdout_of(&create));
    let stderr = stderr_of(&create);
    assert!(
        stderr.contains("reads the data directory on this machine"),
        "the refusal names the reason: {stderr}"
    );
    assert!(
        !out.exists(),
        "nothing was written for a backup that cannot be taken"
    );
}

// --- `swarmbot usage` ---------------------------------------------------------

/// `usage --json` prints the documented report, and its numbers are the ones in
/// the store rather than a placeholder.
///
/// The embedded half is what this pins: it folds its own database through the
/// same `UsageReport::window` the brain's `GET /api/usage` uses, so a local run
/// and a daemon run cannot disagree. The scripted demo backend reports no token
/// usage, which is exactly the case the command has to be honest about — it says
/// so instead of printing "0 tokens" and letting a reader conclude the team was
/// free.
#[test]
fn usage_prints_the_report_and_says_when_nothing_was_reported() {
    let dir = TempDir::new("usage");
    let data = a_data_dir(dir.path());
    let data_arg = data.to_str().expect("utf-8 path");

    let json = run(&["--data-dir", data_arg, "usage", "--json"]);
    assert_eq!(json.status.code(), Some(0), "{}", stderr_of(&json));
    let report: serde_json::Value =
        serde_json::from_str(stdout_of(&json).trim()).expect("one JSON object");
    // The five documented keys, and nothing invented.
    let keys: Vec<&str> = report
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    for key in ["since", "until", "turns", "totals", "by_bot"] {
        assert!(keys.contains(&key), "missing `{key}` in {keys:?}");
    }
    assert_eq!(
        report["turns"],
        serde_json::json!(6),
        "the demo ran six turns: {report}"
    );
    assert!(
        report["by_bot"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "per-bot rows are present: {report}"
    );

    // The prose form must not imply the turns were free.
    let prose = run(&["--data-dir", data_arg, "usage"]);
    assert_eq!(prose.status.code(), Some(0), "{}", stderr_of(&prose));
    let text = stdout_of(&prose);
    assert!(
        text.contains("6 turns"),
        "the turn count is on screen: {text}"
    );
    assert!(
        text.contains("not an estimate"),
        "a backend that reported nothing is called out: {text}"
    );

    // An empty window is a different answer from an expensive one.
    let future = run(&[
        "--data-dir",
        data_arg,
        "usage",
        "--since",
        "2999-01-01T00:00:00Z",
    ]);
    assert_ne!(
        future.status.code(),
        Some(0),
        "a future `since` is a client bug"
    );
    assert!(
        stderr_of(&future).contains("in the future"),
        "{}",
        stderr_of(&future)
    );
}
