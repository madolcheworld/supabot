//! The exit-code contract, checked against the real binary.
//!
//! A wrapper script reads `$?`, not this crate's helpers, so the codes are
//! pinned where a script sees them: `swarmbot` is spawned and its status
//! inspected. Nothing here touches the network or a data dir — a usage error and
//! `--help` are decided before anything is dialled or built.
//!
//! The table itself lives in `main.rs`'s module docs and in `--help`; these
//! tests are what keep the binary honest about it.

use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_swarmbot");

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("the swarmbot binary spawns (CARGO_BIN_EXE_swarmbot)")
}

#[test]
fn a_usage_error_exits_with_the_usage_code_not_the_timeout_code() {
    // A bad flag, an unknown subcommand and a missing required argument are all
    // "I typed the command wrong" — clap's default 2 would say "the team timed
    // out", which is a different fact with a different reaction.
    for args in [
        &["--nonsense"][..],
        &["nope"][..],
        &["run"][..],
        &["bots", "nope"][..],
    ] {
        let output = run(args);
        let code = output.status.code().expect("the process exits normally");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(code, 64, "{args:?} is a usage error: {stderr}");
        assert_ne!(code, 2, "{args:?}: 2 means `run`/`demo` timed out");
    }
}

#[test]
fn help_and_version_are_answers_and_exit_0() {
    let help = run(&["--help"]);
    assert_eq!(help.status.code(), Some(0), "--help is not an error");
    let text = String::from_utf8_lossy(&help.stdout);
    // The documented table is in the help text, where a user looks for it.
    assert!(text.contains("Exit codes"), "{text}");
    for line in ["0   success", "1   an error", "2   ", "3   ", "64  "] {
        assert!(text.contains(line), "missing `{line}` in:\n{text}");
    }

    let version = run(&["--version"]);
    assert_eq!(version.status.code(), Some(0), "--version is not an error");
}
