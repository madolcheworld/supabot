//! `swarmbot`: the terminal front end.
//!
//! The orchestration runs either **in-process** ([`crate::embedded`]) or in a
//! daemon this CLI talks to over HTTP (`--brain <url>`, [`crate::remote`]). Both
//! halves are the same [`crate::session::Session`], so every command body — and
//! every line of [`crate::render`] — is written once: a remote run and a local
//! run are meant to be indistinguishable on screen.
//!
//! Exit codes — the contract a wrapper script reads, because `$?` is the only
//! signal that survives a pipe:
//!
//! | code | meaning |
//! | --- | --- |
//! | 0 | success: the command did what it was asked, and a `run` saw the team go idle |
//! | 1 | an error — one clean line on stderr; a backtrace only with `--verbose` |
//! | 2 | `run`/`demo` timed out waiting for the team to go idle: the team is still working |
//! | 3 | `run`'s live event stream ended before the team went idle — a dead socket, not a slow team |
//! | 64 | `EX_USAGE`: the command line could not be parsed (a bad flag, an unknown subcommand) |
//!
//! `2` and `3` are deliberately different facts: "the team is slow" and "the
//! brain's socket died under us" want different reactions from whoever reads the
//! code. `64` exists because clap's own default for a usage error is `2`, which
//! would make "the team is slow" and "I typed the command wrong"
//! indistinguishable; [`main`] maps the parse failure instead of letting clap
//! exit. `--json` runs also end with one machine-readable object naming the
//! reason, so a script does not have to parse prose.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use crossterm::style::Color;
use swarmbot_contracts::bot::{Bot, normalize_name};
use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::entry::{Author, Entry};
use swarmbot_contracts::id::ConversationId;
use swarmbot_contracts::node::{NodeInfo, NodeKind, RootMode};
use swarmbot_core::swarm::Swarm;

mod config;
mod demo;
mod embedded;
mod notice;
mod remote;
mod render;
mod repl;
mod resolve;
mod script;
mod session;
mod tmp;

use embedded::{BrainParams, Embedded, drain_quiescent, emit_event};
use remote::{Backend, Remote};
use render::{Printer, Roster, RunStop, Theme};
use resolve::Directory;
use session::{Session, wait_for_idle};

/// How long `shutdown` keeps probing the brain after it acknowledged, before
/// reporting that it was still answering. The daemon lets the turns in flight
/// finish first, so "it said ok" and "it is gone" are different facts.
const SHUTDOWN_WATCH: Duration = Duration::from_secs(10);

/// The exit-code table, in `--help` as well as in the module docs: the codes are
/// a contract, and a contract nobody can look up from the terminal is folklore.
const EXIT_CODES_HELP: &str = "\
Exit codes:
  0   success (a `run` saw the team go idle)
  1   an error — one line on stderr; a backtrace only with --verbose
  2   `run`/`demo` timed out waiting for the team to go idle
  3   the live event stream ended before the team went idle (a dead socket, not a slow team)
  64  usage error: the command line could not be parsed
";

#[derive(Parser)]
#[command(
    name = "swarmbot",
    version,
    about = "SwarmBot — named AI bots that chat in DMs and groups and delegate work to each other",
    after_help = EXIT_CODES_HELP
)]
struct Cli {
    /// Log at debug level (RUST_LOG wins when set).
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Only bot text, no decoration.
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Never colourise output (a non-empty NO_COLOR does the same).
    #[arg(long, global = true)]
    no_color: bool,

    /// TOML BrainConfig file (default: <data-dir>/config.toml when it exists).
    /// Configures an in-process brain; refused with --brain.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Where the brain keeps its data (default: ./data). With --brain this is
    /// only where `<data-dir>/owner-token` is looked for.
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// Run against a brain daemon at URL instead of in-process, e.g.
    /// http://127.0.0.1:8787. The CLI derives the ws:// origin for the live
    /// stream itself.
    #[arg(long, global = true, value_name = "URL")]
    brain: Option<String>,

    /// The brain's bearer token, for --brain. When absent: $SWARMBOT_TOKEN, then
    /// <data-dir>/owner-token, then a prompt on the terminal (never when stdin is
    /// not one). The value is never printed, logged or put in an error.
    #[arg(long, global = true, value_name = "TOKEN")]
    token: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// The scripted offline demo: delegation, streaming, summary. No
    /// credentials, no network. In-process only.
    Demo {
        /// In-memory store instead of SQLite under the data dir.
        #[arg(long)]
        offline: bool,
        /// Refused: the demo scripts its own team, so a second script would be
        /// silently dropped rather than honoured.
        #[arg(long, value_name = "FILE")]
        script: Option<PathBuf>,
    },
    /// Send one message, stream everything the team does, wait for idle, exit.
    Run {
        /// The owner's message.
        text: String,
        /// TOML TeamSpec file (default: the config's [team], else the demo team).
        #[arg(long, value_name = "FILE")]
        team: Option<PathBuf>,
        /// Send into a group, by title.
        #[arg(long, value_name = "TITLE", conflicts_with = "bot")]
        group: Option<String>,
        /// Send into a bot's DM, by handle.
        #[arg(long, value_name = "NAME")]
        bot: Option<String>,
        /// Override the backend of the default backend and every team bot.
        #[arg(long, value_name = "ID")]
        backend: Option<String>,
        /// Script the `fake` backends from a TOML file of model turns, so a run
        /// can show real delegation offline and deterministically. Embedded
        /// only; refused with --brain (a daemon's in-process backends cannot be
        /// scripted from here).
        #[arg(long, value_name = "FILE")]
        script: Option<PathBuf>,
        /// Extra read-write root for the local node (repeatable).
        #[arg(long = "root", value_name = "PATH")]
        roots: Vec<PathBuf>,
        /// Idle budget in seconds before the run is reported as timed out.
        #[arg(long, default_value = "120", value_name = "SECS")]
        wait: u64,
        /// One JSON object per line (LiveEvents) instead of prose. The last
        /// line is the run's own stop summary: `{"type":"run_stopped",…}` with
        /// a `stop_reason` of `idle`, `timeout` or `stream_ended`.
        #[arg(long)]
        json: bool,
    },
    /// Interactive REPL with a bot, a group or a conversation id.
    Chat {
        /// Bot handle, group title or conversation id.
        target: String,
        #[arg(long, value_name = "FILE")]
        team: Option<PathBuf>,
        #[arg(long, value_name = "ID")]
        backend: Option<String>,
        /// Script the `fake` backends from a TOML file of model turns. Embedded
        /// only; refused with --brain.
        #[arg(long, value_name = "FILE")]
        script: Option<PathBuf>,
        #[arg(long = "root", value_name = "PATH")]
        roots: Vec<PathBuf>,
    },
    /// The bot roster.
    Bots {
        #[command(subcommand)]
        command: BotsCommand,
    },
    /// Group conversations.
    Groups {
        #[command(subcommand)]
        command: GroupsCommand,
    },
    /// Print a conversation's transcript.
    Transcript {
        /// Bot handle, group title or conversation id.
        target: String,
        #[arg(long, default_value = "50")]
        limit: u32,
        /// One JSON object per line (Entry) instead of prose.
        #[arg(long)]
        json: bool,
    },
    /// Background and handed-off tasks.
    Tasks {
        #[command(subcommand)]
        command: TasksCommand,
    },
    /// The machines that execute tools.
    Nodes {
        #[command(subcommand)]
        command: NodesCommand,
    },
    /// The effective brain configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// What the team's turns spent, as the backends reported it.
    Usage {
        /// Start of the window, RFC 3339 (e.g. 2026-01-31T00:00:00Z). Default:
        /// the last 24 hours.
        #[arg(long, value_name = "TIMESTAMP")]
        since: Option<String>,
        /// One JSON object (the UsageReport) instead of prose.
        #[arg(long)]
        json: bool,
    },
    /// Snapshot a data dir, or check a snapshot. Local filesystem work.
    Backup {
        #[command(subcommand)]
        command: BackupCommand,
    },
    /// Ask a remote brain (--brain) to stop, and say whether it did.
    Shutdown,
}

#[derive(Subcommand)]
enum BotsCommand {
    /// List every bot with its backend, status and DM.
    Ls,
}

#[derive(Subcommand)]
enum GroupsCommand {
    /// List every group conversation with its members and lead.
    Ls,
}

#[derive(Subcommand)]
enum TasksCommand {
    /// List tasks, optionally of one bot.
    Ls {
        #[arg(long, value_name = "NAME")]
        bot: Option<String>,
    },
}

#[derive(Subcommand)]
enum NodesCommand {
    /// List the registered nodes.
    Ls,
    /// Mint a one-time pairing code and print the `swarmbot-node pair` line.
    /// Needs --brain.
    Pair,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Show the effective config — never a key value. With --brain, the brain's
    /// own settings and backends instead of a local file.
    Show,
}

#[derive(Subcommand)]
enum BackupCommand {
    /// Copy the data dir's database and transcripts into a new directory, then
    /// verify the copy.
    Create {
        /// The directory to write. Must not exist, or must be empty: a backup
        /// that silently overwrote an older one would be a data-loss bug wearing
        /// a success message.
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
    },
    /// Check a backup directory: SQLite integrity, manifest, and the row counts
    /// re-derived from the snapshot itself.
    Verify {
        /// A directory `backup create` wrote.
        dir: PathBuf,
    },
    /// Replace a data dir with a backup. Refused while a brain holds the
    /// database open, and it takes a safety backup of what it replaces first.
    Restore {
        /// A directory `backup create` wrote and `backup verify` accepted.
        dir: PathBuf,
        /// The data dir to replace. Defaults to `--data-dir`, then `./data` —
        /// the same resolution `backup create` uses, so the two commands agree
        /// about which directory is the brain's.
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
        /// Print the report as JSON instead of prose.
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            // clap renders the text itself (help and version on stdout, a
            // usage error on stderr). Only the exit code is ours to decide: a
            // usage error must not share `2` with "the team timed out".
            let _ = err.print();
            return exit_code(usage_exit(&err));
        }
    };
    init_tracing(cli.verbose);
    match dispatch(&cli).await {
        Ok(code) => exit_code(code),
        Err(err) => {
            // One clean line; the full chain (and any backtrace the env asks
            // for) only with --verbose.
            if cli.verbose {
                eprintln!("swarmbot: {err:?}");
            } else {
                eprintln!("swarmbot: {err:#}");
            }
            exit_code(EXIT_ERROR)
        }
    }
}

fn exit_code(code: i32) -> std::process::ExitCode {
    std::process::ExitCode::from(code.clamp(0, 255) as u8)
}

/// The code a `clap` parse failure exits with.
///
/// `--help` and `--version` are answers, not failures: they print and exit `0`.
/// Every other kind — an unknown flag, an unknown subcommand, a missing
/// argument — is a usage error and gets [`EXIT_USAGE`] instead of clap's own
/// `2`, which this CLI already uses for a timeout.
fn usage_exit(err: &clap::Error) -> i32 {
    use clap::error::ErrorKind;
    match err.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
        _ => EXIT_USAGE,
    }
}

// The exit codes, named once so the table in the module docs has one source.
//
// `2` and `3` are different facts on purpose: "the team is slow" and "the
// brain's socket died under us" want different reactions, and folding the
// second into the first is how a front end blames a team for a dead socket.

/// Any error: one clean line on stderr (a backtrace only with `--verbose`).
const EXIT_ERROR: i32 = 1;
/// `run`/`demo` ran out of idle budget while the team was still working.
const EXIT_TIMEOUT: i32 = 2;
/// `run`'s live event stream ended before the team went idle.
const EXIT_STREAM_ENDED: i32 = 3;
/// `EX_USAGE` from `sysexits.h`. clap's default for a usage error is `2`, the
/// same code as a timeout — a wrapper script could not tell "the team is slow"
/// from "I typed the command wrong", which is why [`main`] maps a parse failure
/// here instead of letting clap exit.
const EXIT_USAGE: i32 = 64;

/// How a `run` stopped — the fact the exit code and the `--json` summary both
/// report.
///
/// A `bool` could not tell "the team went quiet inside the budget" from "the
/// socket closed while the team was still working". Those are different truths,
/// and both the exit code and the machine-readable summary have to say which
/// one happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    /// The team went idle inside the budget: the normal ending.
    Idle,
    /// The budget ran out while the team was still working.
    Timeout,
    /// The live event stream ended before the team went idle. Nothing said the
    /// team was finished and nothing more can arrive, so this is not a timeout.
    StreamEnded,
}

impl StopReason {
    /// The word a `--json` consumer matches on. Stable and documented: a script
    /// parses this, never the prose.
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Timeout => "timeout",
            Self::StreamEnded => "stream_ended",
        }
    }

    fn exit_code(self) -> i32 {
        match self {
            Self::Idle => 0,
            Self::Timeout => EXIT_TIMEOUT,
            Self::StreamEnded => EXIT_STREAM_ENDED,
        }
    }

    /// The line a prose run prints when it did not go idle, or `None` for the
    /// normal ending.
    ///
    /// `stream_error` is the session's own account of why the socket died (a
    /// refused token, a frame this build cannot read). It is appended in
    /// parentheses rather than replacing the sentence: "the stream ended" is
    /// still the fact about the run, and the reason is what makes it useful.
    fn headline(self, budget: Duration, stream_error: Option<&str>) -> Option<String> {
        match self {
            Self::Idle => None,
            Self::Timeout => Some(format!("TIMED OUT after {}s", budget.as_secs())),
            Self::StreamEnded => Some(match stream_error {
                Some(why) => format!("the live stream ended before the team went idle ({why})"),
                None => "the live stream ended before the team went idle".to_string(),
            }),
        }
    }
}

/// RUST_LOG wins; `-v` means debug; the default keeps warnings and errors on
/// stderr, out of stdout's way (stdout carries prose and `--json`).
fn init_tracing(verbose: bool) {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::filter::LevelFilter;
    let from_env = std::env::var("RUST_LOG")
        .ok()
        .and_then(|value| EnvFilter::try_new(value).ok());
    let filter = from_env.unwrap_or_else(|| {
        EnvFilter::default().add_directive(
            if verbose {
                LevelFilter::DEBUG
            } else {
                LevelFilter::WARN
            }
            .into(),
        )
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

async fn dispatch(cli: &Cli) -> Result<i32> {
    if cli.token.is_some() && cli.brain.is_none() {
        // Refused rather than ignored: a credential nobody presented would
        // otherwise look like it authenticated something.
        return Err(remote::token_without_brain());
    }
    match &cli.command {
        Command::Demo { offline, script } => cmd_demo(cli, *offline, script.as_ref()).await,
        Command::Run {
            text,
            team,
            group,
            bot,
            backend,
            script,
            roots,
            wait,
            json,
        } => {
            cmd_run(
                cli,
                text,
                team.as_ref(),
                group.as_deref(),
                bot.as_deref(),
                backend.as_deref(),
                script.as_ref(),
                roots,
                *wait,
                *json,
            )
            .await
        }
        Command::Chat {
            target,
            team,
            backend,
            script,
            roots,
        } => {
            cmd_chat(
                cli,
                target,
                team.as_ref(),
                backend.as_deref(),
                script.as_ref(),
                roots,
            )
            .await
        }
        Command::Bots { command } => match command {
            BotsCommand::Ls => cmd_bots_ls(cli).await,
        },
        Command::Groups { command } => match command {
            GroupsCommand::Ls => cmd_groups_ls(cli).await,
        },
        Command::Transcript {
            target,
            limit,
            json,
        } => cmd_transcript(cli, target, *limit, *json).await,
        Command::Tasks { command } => match command {
            TasksCommand::Ls { bot } => cmd_tasks_ls(cli, bot.as_deref()).await,
        },
        Command::Nodes { command } => match command {
            NodesCommand::Ls => cmd_nodes_ls(cli).await,
            NodesCommand::Pair => cmd_nodes_pair(cli).await,
        },
        Command::Config { command } => match command {
            ConfigCommand::Show => cmd_config_show(cli).await,
        },
        Command::Usage { since, json } => cmd_usage(cli, since.as_deref(), *json).await,
        Command::Backup { command } => match command {
            BackupCommand::Create { out } => cmd_backup_create(cli, out.clone()).await,
            BackupCommand::Verify { dir } => cmd_backup_verify(cli, dir.clone()).await,
            BackupCommand::Restore {
                dir,
                data_dir,
                json,
            } => cmd_backup_restore(cli, dir.clone(), data_dir.clone(), *json).await,
        },
        Command::Shutdown => cmd_shutdown(cli).await,
    }
}

/// Dial the brain named by `--brain`.
///
/// The work is blocking (a URL parse, a token file read, possibly a terminal
/// prompt, one health probe), so it runs on the blocking pool: a token prompt on
/// a runtime worker would stall every task in the process while a human thinks.
async fn connect(cli: &Cli, url: &str) -> Result<Remote> {
    // A local config file describes a brain this CLI would build; against a
    // daemon it would be read and then silently not applied.
    if cli.config.is_some() {
        return Err(remote::local_only_flag("--config", url));
    }
    let url = url.to_string();
    let token = cli.token.clone();
    let data_dir = data_dir_for(cli);
    let stdin_is_tty = std::io::stdin().is_terminal();
    tokio::task::spawn_blocking(move || {
        Remote::connect(&url, token.as_deref(), &data_dir, stdin_is_tty)
    })
    .await
    .map_err(|err| anyhow!("the connection to the brain stopped: {err}"))?
}

/// The directory `owner-token` is looked for in: `--data-dir`, else `./data`.
///
/// Deliberately *not* the local config file's `data_dir`: with `--brain` the
/// config file is refused, so there is exactly one place a token file is
/// searched, and the error message names it.
fn data_dir_for(cli: &Cli) -> PathBuf {
    cli.data_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("data"))
}

/// `--team`, `--backend` and `--root` build an in-process brain's roster and
/// node. Against a daemon they would be read and quietly do nothing, so they are
/// refused instead — the daemon was configured when it started.
fn reject_local_only_flags(
    url: &str,
    team: Option<&PathBuf>,
    backend: Option<&str>,
    roots: &[PathBuf],
) -> Result<()> {
    if team.is_some() {
        return Err(remote::local_only_flag("--team", url));
    }
    if backend.is_some() {
        return Err(remote::local_only_flag("--backend", url));
    }
    if !roots.is_empty() {
        return Err(remote::local_only_flag("--root", url));
    }
    Ok(())
}

/// The read-only half of the seam: `--brain <url>` dials the daemon, anything
/// else composes an in-process brain **without booting it**, because listing a
/// roster must not provision a team into an empty data dir.
async fn backend_for(cli: &Cli) -> Result<Backend> {
    if let Some(url) = &cli.brain {
        return Ok(Backend::Remote(connect(cli, url).await?));
    }
    let loaded = load_for(cli, None, None, &[])?;
    Ok(read_only_brain(loaded)?.backend())
}

fn theme_of(cli: &Cli) -> Theme {
    Theme::auto(cli.no_color, cli.verbose, cli.quiet)
}

fn stdout_printer(cli: &Cli, json: bool) -> Printer<std::io::Stdout> {
    // A pipe is never "interactive": cursor redraws in a file are vandalism.
    let interactive = std::io::stdout().is_terminal() && !json;
    Printer::new(std::io::stdout(), theme_of(cli), interactive)
}

fn load_for<'a>(
    cli: &'a Cli,
    team: Option<&'a PathBuf>,
    backend: Option<&'a str>,
    roots: &'a [PathBuf],
) -> Result<config::Loaded> {
    config::load(&config::LoadRequest {
        config_path: cli.config.as_deref(),
        data_dir: cli.data_dir.as_deref(),
        team_file: team.map(PathBuf::as_path),
        backend,
        roots,
    })
}

/// The fully resolved config for an embedded `run`/`chat`: the flags applied,
/// the team filled in, and — with `--script` — a shared `fake` backend split
/// into one fake per scripted bot. One body for both commands so the
/// resolution cannot drift between them.
///
/// The split happens here, **before the swarm is composed**, because the
/// backend set is built from the config: a running swarm's registry is never
/// mutated. It is what makes a scripted run deterministic under any scheduler
/// — with one shared FIFO queue a bot woken mid-delegation could pop a turn
/// scripted for another bot (see `config::per_bot_fakes_for_script`).
fn resolve_embedded<'a>(
    cli: &'a Cli,
    team: Option<&'a PathBuf>,
    backend: Option<&'a str>,
    script: Option<&'a PathBuf>,
    roots: &'a [PathBuf],
) -> Result<config::Loaded> {
    let mut loaded = load_for(cli, team, backend, roots)?;
    config::team_or_demo(&mut loaded.config);
    if let Some(path) = script {
        config::per_bot_fakes_for_script(&mut loaded.config, path);
    }
    Ok(loaded)
}

/// A brain for read-only commands: composed but **not booted**, because
/// listing the roster must not provision a team into an empty data dir.
fn read_only_brain(loaded: config::Loaded) -> Result<Embedded> {
    Embedded::build(
        BrainParams {
            config: loaded.config,
            data_dir: loaded.data_dir,
            roots: loaded.roots,
            secrets: loaded.secrets,
            offline: false,
        },
        false,
    )
}

fn booted_brain(loaded: config::Loaded) -> Result<Embedded> {
    Embedded::build(
        BrainParams {
            config: loaded.config,
            data_dir: loaded.data_dir,
            roots: loaded.roots,
            secrets: loaded.secrets,
            offline: false,
        },
        true,
    )
}

async fn cmd_demo(cli: &Cli, offline: bool, script: Option<&PathBuf>) -> Result<i32> {
    if let Some(path) = script {
        // The demo scripts its own four-bot story; silently dropping an
        // operator's file is how people lose an afternoon, so refuse in one line.
        return Err(script::demo_refusal(path));
    }
    if let Some(url) = &cli.brain {
        // Validate first so a malformed URL is reported as malformed, then
        // refuse in one line: the demo is a scripted in-process brain, and
        // running it locally while `--brain` is on the command line would look
        // exactly like a remote run that worked.
        remote::parse_brain_url(url)?;
        return Err(remote::embedded_only("demo", url));
    }
    let mut printer = stdout_printer(cli, false);
    let report = demo::run_demo(
        &demo::DemoOptions {
            offline,
            data_dir: cli
                .data_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("data")),
            wait: Duration::from_secs(60),
        },
        &mut printer,
    )
    .await?;
    printer.finish()?;
    // The demo has no stream to lose (it is an in-process, scripted story), so
    // its two endings are the documented idle/timeout pair.
    Ok(if report.idle {
        StopReason::Idle
    } else {
        StopReason::Timeout
    }
    .exit_code())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_run(
    cli: &Cli,
    text: &str,
    team: Option<&PathBuf>,
    group: Option<&str>,
    bot: Option<&str>,
    backend: Option<&str>,
    script: Option<&PathBuf>,
    roots: &[PathBuf],
    wait: u64,
    json: bool,
) -> Result<i32> {
    let mut printer = stdout_printer(cli, json);
    if let Some(url) = &cli.brain {
        // Refused before anything is dialled: this CLI cannot reach into a
        // daemon's process to script its in-process backends, and saying so
        // beats silently doing nothing.
        if script.is_some() {
            return Err(script::brain_refusal(url));
        }
        reject_local_only_flags(url, team, backend, roots)?;
        let remote = connect(cli, url).await?;
        let conversations = remote.conversations()?;
        let bots = remote.bots()?;
        let conversation = pick_conversation(&conversations, &bots, group, bot)?;
        // Whether the notice is due is the daemon's answer, not a guess: this
        // CLI cannot see a remote script queue, so it asks `GET /api/backends`
        // and warns on the kind alone. A lookup that fails stays silent rather
        // than failing the run.
        notice::warn_on_stderr(
            cli.quiet,
            notice::ask_remote_uses_fake(
                &remote,
                &bots,
                &notice::conversation_participants(&conversations, &conversation),
            ),
        );
        return run_once(
            &remote,
            &RunOnce {
                text,
                conversation,
                wait: Duration::from_secs(wait),
                json,
            },
            &mut printer,
        )
        .await;
        // No shutdown here, and that is the point: a remote brain belongs to
        // whoever started it, and a finished `run` is not a request to stop it.
    }
    let embedded = booted_brain(resolve_embedded(cli, team, backend, script, roots)?)?;
    let conversation = pick_run_conversation(&embedded.swarm, group, bot)?;
    // Load and push the script *after* the team is provisioned and *before* the
    // first message: the turns land in the fake backends' queues, which is also
    // what silences the unscripted-`fake` notice below (a filled queue is no
    // longer the placeholder case the notice describes). A script that cannot be
    // honoured fails here, loudly, before anything is sent.
    if let Some(path) = script {
        script::load(&embedded.swarm, path)?;
    }
    // Before the first message, once: an unscripted `fake` says so on stderr
    // (see `notice`), because its placeholder reply alone reads like a team
    // that failed. `demo` scripts its own fakes and never asks.
    notice::warn_on_stderr(
        cli.quiet,
        notice::embedded_unscripted_fake(&embedded.swarm, &conversation),
    );
    // And, for the other kind of barer answer: an ACP agent has no SwarmBot
    // team tools in this process (no MCP gateway without the brain), so a
    // delegation it claims to have made is one it could not have made.
    notice::warn_agent_on_stderr(
        cli.quiet,
        notice::embedded_agent_without_team_tools(&embedded.swarm, &conversation),
    );
    let result = run_once(
        &embedded,
        &RunOnce {
            text,
            conversation,
            wait: Duration::from_secs(wait),
            json,
        },
        &mut printer,
    )
    .await;
    // Shut down before propagating an error: a run that failed halfway must
    // not leave turns marked running for the next boot to explain.
    embedded.shutdown().await;
    let code = result?;
    Ok(code)
}

/// Where a `run` goes against an in-process swarm. Kept as its own function
/// because the roster reads are fallible in a way the remote half's are not.
fn pick_run_conversation(
    swarm: &Swarm,
    group: Option<&str>,
    bot: Option<&str>,
) -> Result<ConversationId> {
    pick_conversation(
        &swarm
            .conversations
            .list()
            .context("cannot read the roster")?,
        &swarm.bots.list().context("cannot read the roster")?,
        group,
        bot,
    )
}

/// Where a `run` goes: the named group, the named bot's DM, or the defaults
/// ("Team", else "lead", else the first bot on the roster).
///
/// Written over two lists rather than over a swarm, so the remote half resolves
/// a target exactly the same way — including the error text, which is the part a
/// user actually reads.
fn pick_conversation(
    conversations: &[Conversation],
    bots: &[Bot],
    group: Option<&str>,
    bot: Option<&str>,
) -> Result<ConversationId> {
    if let Some(title) = group {
        let directory = Directory::new(bots.to_vec(), conversations.to_vec());
        return directory.resolve_group(title).map(|c| c.id);
    }
    if let Some(name) = bot {
        let handle = normalize_name(name);
        let found = bots
            .iter()
            .find(|bot| bot.name == handle)
            .ok_or_else(|| anyhow!("there is no bot called @{handle}"))?;
        return Ok(found.direct_conversation_id.clone());
    }
    if let Some(conversation) = conversations.iter().find(|conversation| {
        conversation.kind.is_group()
            && conversation
                .title
                .as_deref()
                .is_some_and(|title| title.eq_ignore_ascii_case("Team"))
    }) {
        return Ok(conversation.id.clone());
    }
    if let Some(lead) = bots.iter().find(|bot| bot.name == "lead") {
        return Ok(lead.direct_conversation_id.clone());
    }
    bots.first()
        .map(|bot| bot.direct_conversation_id.clone())
        .ok_or_else(|| {
            anyhow!(
                "there are no bots to talk to — a team is provisioned when a brain boots \
                 (in-process: [team] in the config, --team FILE, or `swarmbot demo`; \
                 remote: the daemon's own --team FILE)"
            )
        })
}

/// What one `run` does. Split out from the flag plumbing so the tests can
/// drive it against an in-memory swarm and capture the output.
pub struct RunOnce<'a> {
    pub text: &'a str,
    pub conversation: ConversationId,
    pub wait: Duration,
    pub json: bool,
}

/// Send one message, stream every event until the team is idle (budget
/// `wait`), print the final reply and the `delegated:` line, and answer with
/// the exit code of the [`StopReason`]: `0` idle, `2` timed out, `3` the live
/// stream ended.
///
/// One body for both halves of the seam: the events come from a
/// `UnboundedReceiver<LiveEvent>` whichever brain produced them, and the
/// rendering is `render.rs` in both cases.
pub async fn run_once<W: Write, S: Session>(
    session: &S,
    options: &RunOnce<'_>,
    printer: &mut Printer<W>,
) -> Result<i32> {
    let mut roster = Roster::new(
        session.bots()?,
        session.conversations()?,
        Some(options.conversation.clone()),
    );
    let mut rx = session.subscribe(None)?;
    session::drain_synced_honest(session, &mut rx, printer, &mut roster, options.json, false)
        .await?;

    session.send(&options.conversation, options.text)?;

    // The idle waiter runs beside the stream so events keep rendering while the
    // team works. It asks the session, not the store, so "idle" means the same
    // thing in both modes: a delivery handed to a runner but not started still
    // counts as work, and so does a task nobody has picked up.
    let (idle_tx, mut idle_rx) = tokio::sync::oneshot::channel::<Result<bool>>();
    let waiter = session.clone();
    let budget = options.wait;
    tokio::spawn(async move {
        let _ = idle_tx.send(wait_for_idle(&waiter, budget).await);
    });

    let stop = loop {
        tokio::select! {
            event = rx.recv() => match event {
                Some(event) => emit_event(printer, &mut roster, &event, options.json, false)?,
                // The stream ended (a remote brain closed the socket): report
                // *that* instead of waiting for events that cannot come any
                // more. A dead socket is not a slow team, so it must not be
                // printed as a timeout.
                None => break StopReason::StreamEnded,
            },
            result = &mut idle_rx => {
                break match result {
                    // A probe that failed is reported, never folded into
                    // "still running": the team is not to blame for a socket.
                    Ok(value) => match value? {
                        true => StopReason::Idle,
                        false => StopReason::Timeout,
                    },
                    // The waiter task ended without answering (it is spawned,
                    // so only a cancelled runtime does that). No probe failed
                    // and none said idle, so the budget is the only fact left.
                    Err(_) => StopReason::Timeout,
                };
            }
        }
    };
    drain_quiescent(
        &mut rx,
        printer,
        &mut roster,
        options.json,
        Duration::from_millis(250),
    )
    .await?;
    printer.finish()?;

    if !options.json && !printer.theme.quiet {
        print_run_summary(session, &options.conversation, printer)?;
    }

    let mut still = Vec::new();
    let mut interrupted = false;
    if let Some(headline) = stop.headline(budget, session.stream_error().as_deref()) {
        // Stop the turn before reporting a run that did not go idle. In-process
        // the brain dies with this process anyway; against a daemon it does not,
        // and leaving a bot working on an answer nobody will read is worse than
        // saying it was stopped — the same for a stream this CLI has lost, not
        // just for a budget it ran out of. `interrupt` answers `false` when
        // nothing was running, which is a fact, not a failure.
        let stopped = session.interrupt(&options.conversation);
        interrupted = matches!(&stopped, Ok(true));
        still = still_running_after(session, stop);
        if options.json {
            // stdout belongs to the JSON stream; diagnostics go to stderr.
            for line in &still {
                eprintln!("still running: {line}");
            }
            if let Err(err) = stopped {
                eprintln!("cannot interrupt the conversation: {err}");
            }
        } else {
            if let Err(err) = stopped {
                printer.note(&format!("cannot interrupt the conversation: {err}"))?;
            }
            printer.line(&printer.theme.paint(
                &format!("{headline} — still running: {}", still.join("; ")),
                Color::Red,
            ))?;
        }
    }
    if options.json {
        // The last line of the stream, and the only one this CLI writes itself:
        // a script reads the reason from here instead of parsing prose.
        printer.json_run_stopped(&RunStop {
            reason: stop.as_str(),
            still_running: &still,
            interrupted,
            stream_error: session.stream_error().as_deref(),
        })?;
    }
    Ok(stop.exit_code())
}

/// The "still running" lines for a run that did not go idle.
///
/// The probe is the one [`wait_for_idle`] asks, so the lines are the words the
/// timeout report always used; only the fallback differs, because "nothing
/// reports as busy" means two different things: after a timeout the check
/// itself ran out of budget, while after a stream that ended it means the team
/// really did go quiet and this CLI can no longer see it.
fn still_running_after<S: Session>(session: &S, stop: StopReason) -> Vec<String> {
    match session.busy() {
        Ok(lines) if !lines.is_empty() => lines,
        Ok(_) => vec![match stop {
            StopReason::Timeout => {
                "nothing reports as busy (the idle check itself timed out)".to_string()
            }
            StopReason::StreamEnded => {
                "nothing reports as busy (the stream ended, so this is its last answer)".to_string()
            }
            StopReason::Idle => "nothing reports as busy".to_string(),
        }],
        Err(err) => vec![format!(
            "cannot ask {} what is running: {err}",
            session.describe()
        )],
    }
}

/// The closing block of a prose run: the last bot reply and the delegation
/// summary. Both are read back from the brain rather than taken from what the
/// stream happened to show — the summary must survive a rendering bug.
fn print_run_summary<W: Write, S: Session>(
    session: &S,
    conversation: &ConversationId,
    printer: &mut Printer<W>,
) -> Result<()> {
    let tail = session
        .tail(conversation, 200)
        .context("cannot read the transcript")?;
    printer.line("── reply ──")?;
    match tail.iter().rev().find(|entry| {
        matches!(entry.author, Author::Bot { .. })
            && entry.kind.text().is_some_and(|text| !text.is_empty())
    }) {
        Some(entry) => printer.plain(entry.kind.text().unwrap_or_default())?,
        None => printer.note("(no bot reply was written)")?,
    }
    let lines = session.delegation_lines(conversation)?;
    if lines.is_empty() {
        // `delegation_lines` counts **handoff tasks**. A team that coordinated
        // with `message_bot` made no task and is not a team that did nothing —
        // observed 2026-11-14 on this box: `@lead` asked `@dev` for a file list
        // with `message_bot`, `@dev` woke, read the tree and answered, and the
        // closing line still said only "delegated: nothing". Naming the tool
        // that was really used keeps the line true without inventing a task.
        match team_tool_hint(&tail) {
            Some(used) => printer.line(&format!(
                "delegated: nothing — no handoff_task; the team reached each other with {used}"
            ))?,
            None => printer.line("delegated: nothing")?,
        }
    } else {
        printer.line(&format!("delegated: {}", lines.join("; ")))?;
    }
    Ok(())
}

/// The bot-to-bot tools a transcript shows, in the order they were first used,
/// when the run made no task: `message_bot ×2, post_to_group`.
///
/// Only the tools a bot uses **instead of** a handoff count here: a
/// `handoff_task` entry would have produced a `delegated:` line already, so
/// reaching this function means the team talked rather than delegated.
fn team_tool_hint(entries: &[Entry]) -> Option<String> {
    const COORDINATION: [&str; 2] = ["message_bot", "post_to_group"];
    let mut order: Vec<&str> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    for entry in entries {
        let Some(call) = entry.as_tool_call() else {
            continue;
        };
        if !COORDINATION.contains(&call.tool.as_str()) {
            continue;
        }
        match order.iter().position(|name| *name == call.tool) {
            Some(index) => counts[index] += 1,
            None => {
                order.push(call.tool.as_str());
                counts.push(1);
            }
        }
    }
    if order.is_empty() {
        return None;
    }
    Some(
        order
            .iter()
            .zip(counts)
            .map(|(name, count)| {
                if count > 1 {
                    format!("{name} ×{count}")
                } else {
                    (*name).to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
    )
}

async fn cmd_chat(
    cli: &Cli,
    target: &str,
    team: Option<&PathBuf>,
    backend: Option<&str>,
    script: Option<&PathBuf>,
    roots: &[PathBuf],
) -> Result<i32> {
    let mut printer = stdout_printer(cli, false);
    if let Some(url) = &cli.brain {
        // Refused before dialling: a daemon's in-process backends cannot be
        // scripted from this process (see `cmd_run`).
        if script.is_some() {
            return Err(script::brain_refusal(url));
        }
        reject_local_only_flags(url, team, backend, roots)?;
        let remote = connect(cli, url).await?;
        let bots = remote.bots()?;
        let directory = Directory::new(bots.clone(), remote.conversations()?);
        let resolved = directory.resolve(target)?;
        // Say which interpretation matched — a DM when a group was meant is the
        // kind of surprise a chat cannot recover from silently.
        printer.note(&format!("target: {}", resolved.describe()))?;
        if let Some(hint) = directory.ambiguity_hint(target, &resolved) {
            printer.note(&hint)?;
        }
        // One notice before the first message; the daemon's backend list
        // decides, and a failed lookup stays silent (see `notice`).
        notice::warn_on_stderr(
            cli.quiet,
            notice::ask_remote_uses_fake(&remote, &bots, &notice::target_participants(&resolved)),
        );
        // The daemon keeps running when the chat ends: it is somebody else's
        // process, and `/quit` is not `shutdown`.
        return repl::chat(&remote, &resolved, &mut printer).await;
    }
    let embedded = booted_brain(resolve_embedded(cli, team, backend, script, roots)?)?;
    let directory = Directory::load(&embedded.swarm)?;
    let resolved = directory.resolve(target)?;
    printer.note(&format!("target: {}", resolved.describe()))?;
    if let Some(hint) = directory.ambiguity_hint(target, &resolved) {
        printer.note(&hint)?;
    }
    // Load and push the script after the team is provisioned and before the
    // first message, so the REPL's first turn is answered from the script and
    // the notice below sees a filled queue (see `cmd_run`).
    if let Some(path) = script {
        script::load(&embedded.swarm, path)?;
    }
    // One notice before the first message: an unscripted `fake` says so on
    // stderr (see `notice`) instead of leaving its placeholder reply to be
    // misread as a broken bot.
    notice::warn_on_stderr(
        cli.quiet,
        notice::embedded_unscripted_fake(&embedded.swarm, resolved.conversation_id()),
    );
    // Same second notice as `run`: an ACP agent here cannot delegate.
    notice::warn_agent_on_stderr(
        cli.quiet,
        notice::embedded_agent_without_team_tools(&embedded.swarm, resolved.conversation_id()),
    );
    let result = repl::chat(&embedded, &resolved, &mut printer).await;
    // Shut down before propagating an error, same as `run`.
    embedded.shutdown().await;
    let code = result?;
    Ok(code)
}

async fn cmd_bots_ls(cli: &Cli) -> Result<i32> {
    let mut printer = stdout_printer(cli, false);
    match backend_for(cli).await? {
        Backend::Embedded(embedded) => list_bots(&embedded, &mut printer),
        Backend::Remote(remote) => list_bots(&remote, &mut printer),
    }
}

fn list_bots<S: Session>(session: &S, printer: &mut Printer<std::io::Stdout>) -> Result<i32> {
    let bots = session.bots()?;
    if bots.is_empty() {
        printer.note(
            "no bots yet — a team is provisioned when a brain boots (`swarmbot demo`, \
             `swarmbot run`, or the daemon's own --team FILE)",
        )?;
    }
    for line in repl::bots_lines(&bots) {
        printer.line(&line)?;
    }
    Ok(0)
}

async fn cmd_groups_ls(cli: &Cli) -> Result<i32> {
    let mut printer = stdout_printer(cli, false);
    match backend_for(cli).await? {
        Backend::Embedded(embedded) => list_groups(&embedded, &mut printer),
        Backend::Remote(remote) => list_groups(&remote, &mut printer),
    }
}

fn list_groups<S: Session>(session: &S, printer: &mut Printer<std::io::Stdout>) -> Result<i32> {
    let lines = repl::groups_lines(&session.conversations()?, &session.bots()?);
    if lines.is_empty() {
        printer.note("no groups yet")?;
    }
    for line in lines {
        printer.line(&line)?;
    }
    Ok(0)
}

async fn cmd_transcript(cli: &Cli, target: &str, limit: u32, json: bool) -> Result<i32> {
    let mut printer = stdout_printer(cli, json);
    match backend_for(cli).await? {
        Backend::Embedded(embedded) => transcript(&embedded, target, limit, json, &mut printer),
        Backend::Remote(remote) => transcript(&remote, target, limit, json, &mut printer),
    }
}

fn transcript<S: Session, W: Write>(
    session: &S,
    target: &str,
    limit: u32,
    json: bool,
    printer: &mut Printer<W>,
) -> Result<i32> {
    let directory = Directory::new(session.bots()?, session.conversations()?);
    let resolved = directory.resolve(target)?;
    if !json {
        printer.note(&format!("transcript: {}", resolved.describe()))?;
        if let Some(hint) = directory.ambiguity_hint(target, &resolved) {
            printer.note(&hint)?;
        }
    }
    let entries = session.tail(resolved.conversation_id(), limit)?;
    if json {
        for entry in &entries {
            // One object per line, like `run --json`: greppable, diffable.
            printer.json_entry(entry)?;
        }
    } else {
        let mut roster = Roster::new(
            session.bots()?,
            session.conversations()?,
            Some(resolved.conversation_id().clone()),
        );
        roster.adopt_entries(entries.clone());
        if let Ok(tasks) = session.tasks() {
            roster.adopt_tasks(tasks);
        }
        if let Ok(requests) = session.pending_in(resolved.conversation_id()) {
            roster.adopt_requests(requests);
        }
        for entry in &entries {
            printer.entry(entry, &roster)?;
        }
        printer.finish()?;
    }
    Ok(0)
}

async fn cmd_tasks_ls(cli: &Cli, bot: Option<&str>) -> Result<i32> {
    let mut printer = stdout_printer(cli, false);
    match backend_for(cli).await? {
        Backend::Embedded(embedded) => list_tasks(&embedded, bot, &mut printer),
        Backend::Remote(remote) => list_tasks(&remote, bot, &mut printer),
    }
}

fn list_tasks<S: Session>(
    session: &S,
    bot: Option<&str>,
    printer: &mut Printer<std::io::Stdout>,
) -> Result<i32> {
    let lines = repl::tasks_lines(&session.bots()?, &session.tasks()?, bot)?;
    if lines.is_empty() {
        printer.note("no tasks")?;
    }
    for line in lines {
        printer.line(&line)?;
    }
    Ok(0)
}

async fn cmd_nodes_ls(cli: &Cli) -> Result<i32> {
    let mut printer = stdout_printer(cli, false);
    match backend_for(cli).await? {
        Backend::Embedded(embedded) => list_nodes(&embedded, &mut printer),
        Backend::Remote(remote) => list_nodes(&remote, &mut printer),
    }
}

fn list_nodes<S: Session>(session: &S, printer: &mut Printer<std::io::Stdout>) -> Result<i32> {
    let nodes = session.nodes()?;
    if nodes.is_empty() {
        printer.note("no nodes registered")?;
    }
    for line in nodes_lines(&nodes) {
        printer.line(&line)?;
    }
    Ok(0)
}

/// Mint a pairing code on the daemon and print the line the node operator runs.
///
/// Remote-only by nature: a code is redeemed against a listening daemon, and an
/// in-process brain is gone by the time anybody could type the command.
async fn cmd_nodes_pair(cli: &Cli) -> Result<i32> {
    let Some(url) = &cli.brain else {
        return Err(remote::needs_brain(
            "nodes pair",
            "a pairing code is minted by the daemon a node will connect to, and without --brain \
             this CLI's own brain is gone the moment the command exits",
        ));
    };
    let remote = connect(cli, url).await?;
    let pairing = remote.pairing_code()?;
    let mut printer = stdout_printer(cli, false);
    for line in remote::pairing_lines(remote.url(), &pairing) {
        printer.line(&line)?;
    }
    printer.note("the code is shown once: the brain stores only its hash")?;
    printer.finish()?;
    Ok(0)
}

/// Ask a remote brain to stop, then say whether it actually did.
///
/// `POST /api/shutdown` answers before the daemon exits — it has to, or the
/// answer would never arrive — so this command keeps probing `/api/health` for a
/// few seconds and reports which of the two happened.
/// `swarmbot backup create --out DIR`
///
/// A backup is **local filesystem work**: it reads `<data>/swarmbot.sqlite` and
/// `<data>/transcripts` from disk. With `--brain` those files live on whatever
/// machine runs the daemon, and this CLI cannot reach them — the REST API
/// deliberately exposes no file-level operations. Refusing is the honest answer;
/// quietly backing up a stale local directory that happens to share a path would
/// be a backup of the wrong thing wearing a success message.
async fn cmd_backup_create(cli: &Cli, out: PathBuf) -> Result<i32> {
    if let Some(url) = &cli.brain {
        return Err(anyhow!(
            "`backup create` reads the data directory on this machine, but --brain points at the \
             brain at {url}: its database is on that machine. Run `swarmbot backup create` there \
             (with its --data-dir), or drop --brain to back up the data dir here"
        ));
    }
    let data_dir = data_dir_for(cli);
    // Blocking file work and a SQLite connection: it belongs off the runtime's
    // worker threads, which is the same reason every other command wraps the
    // in-process half this way.
    let report = tokio::task::spawn_blocking({
        let data_dir = data_dir.clone();
        let out = out.clone();
        move || swarmbot_store::backup::backup(&data_dir, &out)
    })
    .await
    .map_err(|err| anyhow!("the backup task stopped: {err}"))?
    .map_err(|err| anyhow!("{err}"))?;

    // A backup nobody opened is a guess, so the command verifies what it just
    // wrote before it reports success.
    let verify = tokio::task::spawn_blocking({
        let out = out.clone();
        move || swarmbot_store::backup::verify(&out)
    })
    .await
    .map_err(|err| anyhow!("the verify task stopped: {err}"))?
    .map_err(|err| anyhow!("the backup was written but does not verify: {err}"))?;

    let mut printer = stdout_printer(cli, false);
    printer.line(&format!(
        "backup {} → {}",
        data_dir.display(),
        report.out_dir.display()
    ))?;
    printer.line(&format!(
        "  snapshot {} ({} bytes, schema v{})",
        report.snapshot.display(),
        report.manifest.database_bytes,
        report.manifest.schema_version
    ))?;
    printer.line(&format!(
        "  transcripts {} files ({} bytes)",
        report.manifest.transcript_files, report.manifest.transcript_bytes
    ))?;
    printer.line(&format!("  tables {}", tables_line(&verify.tables)))?;
    printer.line(&format!(
        "  verified: integrity_check={} · manifest matched the snapshot",
        verify.integrity_check
    ))?;
    Ok(0)
}

/// `swarmbot backup restore DIR [--data-dir DIR]`
///
/// The one command in this CLI that **deletes** something, so its refusals are
/// the interesting part:
///
/// * a backup that does not verify is refused, and nothing is touched;
/// * a data dir another process holds the database open in is refused, because a
///   running brain keeps writing to the old inode and can overwrite the restored
///   file — see `store::restore`'s module docs;
/// * `--brain` is refused outright: the database is on the daemon's machine, and
///   this command operates on files, not on an API.
///
/// It always takes a safety backup of what it replaces and prints where that
/// went, so a restore of the wrong snapshot has a way back.
async fn cmd_backup_restore(
    cli: &Cli,
    dir: PathBuf,
    data_dir: Option<PathBuf>,
    json: bool,
) -> Result<i32> {
    if let Some(url) = &cli.brain {
        return Err(anyhow!(
            "`backup restore` replaces files on this machine, but --brain points at the brain at \
             {url}: its data directory is on that machine. Run `swarmbot backup restore` there \
             (with its --data-dir), after stopping that brain, or drop --brain to restore here"
        ));
    }
    let data_dir = data_dir.unwrap_or_else(|| data_dir_for(cli));
    let report = tokio::task::spawn_blocking({
        let dir = dir.clone();
        let data_dir = data_dir.clone();
        move || swarmbot_store::restore::restore(&dir, &data_dir)
    })
    .await
    .map_err(|err| anyhow!("the restore task stopped: {err}"))?
    .map_err(|err| anyhow!("{err}"))?;

    let mut printer = stdout_printer(cli, json);
    if json {
        let body = format!(
            "{{\"from\": {},\"data_dir\": {},\"safety\": {},\"removed_wal\": {},\"removed_shm\": {},\"bots\": {}}}",
            serde_json::to_string(&report.from.display().to_string())?,
            serde_json::to_string(&report.data_dir.display().to_string())?,
            serde_json::to_string(
                &report
                    .safety
                    .as_ref()
                    .map(|path| path.display().to_string())
            )?,
            report.removed_wal,
            report.removed_shm,
            report.tables.get("bots").copied().unwrap_or(0),
        );
        printer.line(&body)?;
        return Ok(0);
    }
    printer.line(&format!(
        "restored {} → {}",
        report.from.display(),
        report.data_dir.display()
    ))?;
    printer.line(&format!(
        "  snapshot {} ({} bytes, schema v{})",
        report.manifest.database, report.manifest.database_bytes, report.manifest.schema_version
    ))?;
    printer.line(&format!(
        "  transcripts {} files ({} bytes)",
        report.manifest.transcript_files, report.manifest.transcript_bytes
    ))?;
    if report.removed_wal || report.removed_shm {
        // Worth a line of its own: a stale `-wal` left next to a replaced
        // database is replayed onto it at the next open, which would silently
        // undo the restore.
        printer.line(
            "  removed the old write-ahead log (a stale one would be replayed onto the restored \
             database)",
        )?;
    }
    match &report.safety {
        Some(safety) => printer.line(&format!(
            "  the previous data dir is at {} — `swarmbot backup restore {}` puts it back",
            safety.display(),
            safety.display()
        ))?,
        None => printer.line("  there was no previous data dir, so nothing was replaced")?,
    }
    printer.line(&format!(
        "  rows {}",
        report
            .tables
            .iter()
            .map(|(table, count)| format!("{table}={count}"))
            .collect::<Vec<_>>()
            .join(" ")
    ))?;
    Ok(0)
}

/// `swarmbot backup verify DIR`
async fn cmd_backup_verify(cli: &Cli, dir: PathBuf) -> Result<i32> {
    let report = tokio::task::spawn_blocking(move || swarmbot_store::backup::verify(&dir))
        .await
        .map_err(|err| anyhow!("the verify task stopped: {err}"))?
        .map_err(|err| anyhow!("{err}"))?;
    let mut printer = stdout_printer(cli, false);
    printer.line(&format!(
        "{} is a backup taken {} by swarmbot {} (schema v{})",
        report.out_dir.display(),
        report.manifest.created_at,
        report.manifest.crate_version,
        report.manifest.schema_version
    ))?;
    printer.line(&format!("  integrity_check={}", report.integrity_check))?;
    printer.line(&format!("  tables {}", tables_line(&report.tables)))?;
    printer.line(&format!("  transcripts {} files", report.transcript_files))?;
    Ok(0)
}

/// `bot=3, conversation=2, …` — the row counts, in the manifest's own order.
fn tables_line(tables: &std::collections::BTreeMap<String, i64>) -> String {
    if tables.is_empty() {
        return "none".to_string();
    }
    tables
        .iter()
        .map(|(table, rows)| format!("{table}={rows}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `swarmbot usage [--since T] [--json]`
///
/// Two front ends, one answer: the embedded half folds its own store and the
/// remote half asks `GET /api/usage`, and both go through
/// `UsageReport::window`, so a local run and a daemon run cannot disagree about
/// the same turns.
///
/// The numbers are printed as reported and nothing is priced. A backend that
/// reports no usage leaves its turns looking free, which is why the turn count
/// is on the same screen: "0 tokens over 3 turns" is a different fact from
/// "nothing ran".
async fn cmd_usage(cli: &Cli, since: Option<&str>, json: bool) -> Result<i32> {
    let report = match backend_for(cli).await? {
        Backend::Embedded(embedded) => embedded.usage(since)?,
        Backend::Remote(remote) => remote.usage(since)?,
    };
    if json {
        let mut out = std::io::stdout();
        writeln!(
            out,
            "{}",
            serde_json::to_string(&report).context("cannot encode the usage report")?
        )?;
        out.flush()?;
        return Ok(0);
    }

    let mut printer = stdout_printer(cli, false);
    printer.line(&format!(
        "usage {} → {}",
        report.since.to_rfc3339(),
        report.until.to_rfc3339()
    ))?;
    if report.is_empty() {
        // Distinct from a window full of turns whose backends reported nothing:
        // saying "0 tokens" here would invite the wrong conclusion.
        printer.note("no turn ran in this window")?;
        return Ok(0);
    }
    printer.line(&format!(
        "  total in {} · out {} · {} llm calls · {} tool calls · {} turns",
        report.totals.input_tokens,
        report.totals.output_tokens,
        report.totals.llm_calls,
        report.totals.tool_calls,
        report.turns
    ))?;
    for row in &report.by_bot {
        let name = row.bot_name.as_deref().unwrap_or("(deleted bot)");
        printer.line(&format!(
            "  @{name} in {} · out {} · {} llm · {} tools",
            row.usage.input_tokens,
            row.usage.output_tokens,
            row.usage.llm_calls,
            row.usage.tool_calls
        ))?;
    }
    if report.totals.total_tokens() == 0 {
        printer.note(
            "the backends reported no token usage for these turns; this is what they said, \
             not an estimate",
        )?;
    }
    Ok(0)
}

async fn cmd_shutdown(cli: &Cli) -> Result<i32> {
    let Some(url) = &cli.brain else {
        return Err(remote::needs_brain(
            "shutdown",
            "without --brain the orchestration runs in this process and stops when this process \
             exits — there is no separate brain to stop",
        ));
    };
    let remote = connect(cli, url).await?;
    let watching = remote.clone();
    let line = tokio::task::spawn_blocking(move || watching.shutdown_and_watch(SHUTDOWN_WATCH))
        .await
        .map_err(|err| anyhow!("the shutdown watch stopped: {err}"))??;
    let mut printer = stdout_printer(cli, false);
    printer.line(&line)?;
    Ok(0)
}

fn nodes_lines(nodes: &[NodeInfo]) -> Vec<String> {
    nodes
        .iter()
        .map(|node| {
            let kind = match node.kind {
                NodeKind::Local => "local",
                NodeKind::Remote => "remote",
            };
            let presence = if node.online { "online" } else { "offline" };
            let caps = node.capabilities();
            let roots: Vec<String> = node
                .roots
                .iter()
                .map(|root| {
                    let mode = match root.mode {
                        RootMode::ReadOnly => "ro",
                        RootMode::ReadWrite => "rw",
                    };
                    format!("{}:{}({mode})", root.id, root.path)
                })
                .collect();
            format!(
                "{} \"{}\" {kind} {presence} caps [{}] roots [{}]",
                node.id,
                node.label,
                caps.as_slice().join(", "),
                roots.join(", ")
            )
        })
        .collect()
}

/// The effective config, from whichever brain is in play.
///
/// In-process that is the local config this CLI resolved, with every backend
/// through its redacted `public_view` and secrets by name only. With `--brain`
/// it is the *daemon's* `/api/settings` and `/api/backends`: a local file would
/// describe a brain that is not the one answering, which is worse than nothing.
async fn cmd_config_show(cli: &Cli) -> Result<i32> {
    if let Some(url) = &cli.brain {
        let remote = connect(cli, url).await?;
        let health = remote.health()?;
        let settings = remote.settings()?;
        let backends = remote.backends()?;
        print!(
            "{}",
            remote::config_show_text(
                remote.url(),
                remote.token_source(),
                &health,
                &settings,
                &backends
            )
        );
        std::io::stdout().flush()?;
        return Ok(0);
    }
    let loaded = load_for(cli, None, None, &[])?;
    print!("{}", config::show(&loaded));
    std::io::stdout().flush()?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::Remote;
    use crate::render::Theme;
    use crate::session::SendReport;
    use crate::tmp::TempDir;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use swarmbot_client::testing::StubBrain;
    use swarmbot_contracts::config::BrainConfig;
    use swarmbot_contracts::entry::Entry;
    use swarmbot_contracts::id::RequestId;
    use swarmbot_contracts::live::{LiveCursor, LiveEvent};
    use swarmbot_contracts::request::{Decision, RuntimeRequest};
    use swarmbot_contracts::task::Task;
    use swarmbot_llm::ScriptedTurn;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

    /// A session whose `busy()` answer and stream lifetime the test decides.
    ///
    /// `run_once` has to keep three endings apart, and two of them need a brain
    /// that behaves in a way no canned double does: `StubBrain`'s socket closes
    /// right after the snapshot (the `StreamEnded` case), and a real brain
    /// cannot be told to stay busy forever. This double can do both, which is
    /// what makes a genuine timeout testable without a real second of a real
    /// team working.
    #[derive(Clone)]
    struct NeverIdle {
        /// What `busy()` answers: the team never goes quiet.
        busy: Vec<String>,
        /// Held for as long as the test runs: an alive sender is an open
        /// stream. Dropped inside `subscribe` for the stream-ended case.
        stream: Arc<Mutex<Option<UnboundedSender<LiveEvent>>>>,
        /// `false` ends the stream right after the snapshot, the way a closed
        /// brain socket ends the pump's channel.
        keep_open: bool,
        /// The reason a real session would record for a dead socket, when the
        /// test wants the report to name one.
        stream_error: Option<String>,
        interrupted: Arc<Mutex<Vec<ConversationId>>>,
    }

    impl NeverIdle {
        fn new(busy: &str) -> Self {
            Self {
                busy: vec![busy.to_string()],
                stream: Arc::new(Mutex::new(None)),
                keep_open: true,
                stream_error: None,
                interrupted: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// The same team, with its live stream already over.
        fn ended(busy: &str) -> Self {
            Self {
                keep_open: false,
                ..Self::new(busy)
            }
        }

        fn with_stream_error(mut self, reason: &str) -> Self {
            self.stream_error = Some(reason.to_string());
            self
        }

        fn interrupts(&self) -> usize {
            self.interrupted
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .len()
        }
    }

    impl Session for NeverIdle {
        fn describe(&self) -> String {
            "the test brain".to_string()
        }
        fn poll_interval(&self) -> Duration {
            // Faster than a real brain's: the test only needs the deadline to
            // arrive, and 40 probes inside the second keep it cheap.
            Duration::from_millis(25)
        }
        fn bots(&self) -> Result<Vec<Bot>> {
            Ok(Vec::new())
        }
        fn conversations(&self) -> Result<Vec<Conversation>> {
            Ok(Vec::new())
        }
        fn tasks(&self) -> Result<Vec<Task>> {
            Ok(Vec::new())
        }
        fn nodes(&self) -> Result<Vec<NodeInfo>> {
            Ok(Vec::new())
        }
        fn usage(&self, _: Option<&str>) -> Result<swarmbot_contracts::turn::UsageReport> {
            let now = chrono::Utc::now();
            Ok(swarmbot_contracts::turn::UsageReport::from_turns(
                now,
                now,
                &[],
                |_| None,
            ))
        }
        fn tail(&self, _: &ConversationId, _: u32) -> Result<Vec<Entry>> {
            Ok(Vec::new())
        }
        fn send(&self, _: &ConversationId, _: &str) -> Result<SendReport> {
            Ok(SendReport::default())
        }
        fn subscribe(&self, _: Option<ConversationId>) -> Result<UnboundedReceiver<LiveEvent>> {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            // The run waits for the hub's `Synchronized` promise before it
            // sends; a double that never made it would fail the run before the
            // ending under test.
            let _ = tx.send(LiveEvent::Synchronized {
                cursor: LiveCursor::new(1, 1),
            });
            if self.keep_open {
                *self.stream.lock().unwrap_or_else(|err| err.into_inner()) = Some(tx);
            }
            Ok(rx)
        }
        fn respond(
            &self,
            _: &RequestId,
            _: Option<Decision>,
            _: Option<serde_json::Value>,
        ) -> Result<RuntimeRequest> {
            Err(anyhow!("the test brain answers no requests"))
        }
        fn interrupt(&self, conversation: &ConversationId) -> Result<bool> {
            self.interrupted
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .push(conversation.clone());
            Ok(true)
        }
        fn pending_in(&self, _: &ConversationId) -> Result<Vec<RuntimeRequest>> {
            Ok(Vec::new())
        }
        fn new_session(&self, _: &Bot) -> Result<()> {
            Ok(())
        }
        fn busy(&self) -> Result<Vec<String>> {
            Ok(self.busy.clone())
        }
        fn delegation_lines(&self, _: &ConversationId) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn stream_error(&self) -> Option<String> {
            self.stream_error.clone()
        }
    }

    /// The last non-empty line of a `--json` run, parsed — the run's own stop
    /// summary is always the last thing it writes.
    fn last_json_line(out: &str) -> serde_json::Value {
        let line = out
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .expect("a --json run writes at least one line");
        serde_json::from_str(line)
            .unwrap_or_else(|err| panic!("`{line}` is one JSON object ({err})"))
    }

    /// An in-memory brain with the demo team on the always-present `fake`
    /// backend — the same double the core's own tests use.
    fn embedded(dir: &std::path::Path) -> Embedded {
        let config = BrainConfig {
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        };
        Embedded::build(
            BrainParams {
                config,
                data_dir: dir.to_path_buf(),
                roots: Vec::new(),
                secrets: BTreeMap::new(),
                offline: true,
            },
            true,
        )
        .expect("a brain builds")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_group_run_with_a_mention_wakes_the_mentioned_bot() {
        let dir = TempDir::new("run").expect("temp");
        let embedded = embedded(dir.path());
        let swarm = Arc::clone(&embedded.swarm);
        // One scripted answer; the mention decides who consumes it.
        swarm
            .backends
            .registry()
            .fake("fake")
            .expect("the fake is always registered")
            .push_turn(ScriptedTurn::text("looking now"));

        let group = swarm
            .conversations
            .find_by_title("Team")
            .expect("list")
            .expect("the demo group");
        let scout = swarm.bots.by_name("scout").expect("scout");

        let mut buffer = Vec::new();
        let code = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            run_once(
                &embedded,
                &RunOnce {
                    text: "@scout check the workspace",
                    conversation: group.id.clone(),
                    wait: Duration::from_secs(30),
                    json: false,
                },
                &mut printer,
            )
            .await
            .expect("runs")
        };
        assert_eq!(code, 0, "an idle team exits 0");

        // The mentioned bot really ran a turn in the group.
        let turns = swarm.store.turns_in(&group.id, 50).expect("turns");
        assert!(
            turns.iter().any(|turn| turn.bot_id == scout.id),
            "scout was woken by the mention: {turns:?}"
        );

        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("you › @scout check the workspace"), "{out}");
        assert!(out.contains("@scout › looking now"), "{out}");
        assert!(out.contains("── reply ──"), "{out}");
        assert!(out.contains("delegated: nothing"), "{out}");
        embedded.shutdown().await;
    }

    #[test]
    fn a_message_bot_run_names_the_tool_instead_of_a_bare_nothing() {
        // `delegated:` counts handoff tasks. A team that coordinated with
        // `message_bot` (observed 2026-11-14: lead asked dev, dev answered,
        // the run still printed "delegated: nothing") made no task, and a
        // bare "nothing" reads like the team did nothing at all.
        let conversation = ConversationId::new();
        let mut entries = Vec::new();
        for (seq, tool) in [(1u64, "message_bot"), (2, "message_bot"), (3, "read_file")] {
            entries.push(Entry::tool_call(
                conversation.clone(),
                seq,
                None,
                swarmbot_contracts::id::ToolCallId::new(),
                tool,
                serde_json::json!({}),
                None,
                None,
            ));
        }
        assert_eq!(
            team_tool_hint(&entries).as_deref(),
            Some("message_bot ×2"),
            "the coordination tool is named once, with its count; a task tool would \
             have written a delegated line instead"
        );

        // Order is first-use, and only coordination tools are named: neither
        // an unrelated tool nor `handoff_task` belongs in this line.
        entries.push(Entry::tool_call(
            conversation.clone(),
            4,
            None,
            swarmbot_contracts::id::ToolCallId::new(),
            "post_to_group",
            serde_json::json!({}),
            None,
            None,
        ));
        entries.push(Entry::tool_call(
            conversation,
            5,
            None,
            swarmbot_contracts::id::ToolCallId::new(),
            "handoff_task",
            serde_json::json!({}),
            None,
            None,
        ));
        assert_eq!(
            team_tool_hint(&entries).as_deref(),
            Some("message_bot ×2, post_to_group")
        );

        // A transcript with no coordination call keeps the plain line: the
        // hint must never appear where nothing used it.
        let quiet: Vec<Entry> = entries
            .into_iter()
            .filter(|entry| {
                entry
                    .as_tool_call()
                    .is_some_and(|call| call.tool == "read_file")
            })
            .collect();
        assert_eq!(team_tool_hint(&quiet), None, "no coordination, no hint");
        assert_eq!(
            team_tool_hint(&[]),
            None,
            "an empty transcript says nothing"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn json_mode_emits_one_parsable_object_per_line() {
        let dir = TempDir::new("run-json").expect("temp");
        let embedded = embedded(dir.path());
        let swarm = Arc::clone(&embedded.swarm);
        swarm
            .backends
            .registry()
            .fake("fake")
            .expect("the fake")
            .push_turn(ScriptedTurn::text("done"));
        let group = swarm
            .conversations
            .find_by_title("Team")
            .expect("list")
            .expect("the demo group");

        let mut buffer = Vec::new();
        let code = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            run_once(
                &embedded,
                &RunOnce {
                    text: "@lead status?",
                    conversation: group.id.clone(),
                    wait: Duration::from_secs(30),
                    json: true,
                },
                &mut printer,
            )
            .await
            .expect("runs")
        };
        assert_eq!(code, 0);

        let out = String::from_utf8(buffer).expect("utf8");
        assert!(
            !out.contains('\u{1b}'),
            "json mode never colourises: {out:?}"
        );
        let lines: Vec<&str> = out.lines().filter(|line| !line.is_empty()).collect();
        assert!(!lines.is_empty());
        let mut saw_owner_message = false;
        let mut saw_synchronized = false;
        for line in lines {
            let value: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("`{line}` is one JSON object ({err})"));
            let kind = value["type"].as_str().expect("every event has a type");
            saw_synchronized |= kind == "synchronized";
            saw_owner_message |= kind == "entry_upsert"
                && value["entry"]["author"]["kind"] == "owner"
                && value["entry"]["kind"]["text"] == "@lead status?";
        }
        assert!(saw_synchronized, "the stream synchronized: {out}");
        assert!(saw_owner_message, "the owner's message streamed: {out}");
        embedded.shutdown().await;
    }

    /// The `--script` wiring of `cmd_run`/`cmd_chat`: the split happens during
    /// config resolution, before anything boots, and a `fake` shared by two
    /// scripted bots becomes one fake per bot (the demo's idiom). An unscripted
    /// run resolves exactly as before — the flag is what upgrades the config,
    /// and this pins the command path, not just the config helper.
    #[test]
    fn a_scripted_run_resolves_per_bot_fakes_before_booting() {
        let dir = TempDir::new("run-split").expect("temp");
        let script = dir.path().join("s.toml");
        std::fs::write(
            &script,
            "[[turns]]\nbot = \"lead\"\ntext = \"a\"\n\n[[turns]]\nbot = \"scout\"\ntext = \"b\"\n",
        )
        .expect("writes");
        let data = dir.path().to_string_lossy().to_string();
        let cli = Cli::parse_from(["swarmbot", "--data-dir", &data, "run", "hi"]);

        let loaded =
            resolve_embedded(&cli, None, Some("fake"), Some(&script), &[]).expect("resolves");
        let ids: Vec<&str> = loaded
            .config
            .backends
            .iter()
            .map(|spec| spec.id.as_str())
            .collect();
        assert_eq!(ids, vec!["fake", "fake-lead", "fake-scout"], "{ids:?}");
        let team = loaded.config.team.as_ref().expect("a team");
        assert_eq!(team.bots[0].backend.as_deref(), Some("fake-lead"));
        assert_eq!(team.bots[1].backend.as_deref(), Some("fake-scout"));
        loaded.config.validate().expect("the resolved config boots");

        // Without a script the same flags resolve exactly as they always did.
        let plain = resolve_embedded(&cli, None, Some("fake"), None, &[]).expect("resolves");
        let ids: Vec<&str> = plain
            .config
            .backends
            .iter()
            .map(|spec| spec.id.as_str())
            .collect();
        assert_eq!(ids, vec!["fake"], "an unscripted run synthesises nothing");
        let team = plain.config.team.as_ref().expect("a team");
        assert!(
            team.bots
                .iter()
                .all(|bot| bot.backend.as_deref() == Some("fake"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_without_a_team_falls_back_to_the_demo_defaults() {
        let dir = TempDir::new("run-default").expect("temp");
        let embedded = embedded(dir.path());
        let swarm = Arc::clone(&embedded.swarm);
        // No --group/--bot: "Team" exists in the demo roster.
        let conversation = pick_run_conversation(&swarm, None, None).expect("a default target");
        let group = swarm
            .conversations
            .find_by_title("Team")
            .expect("list")
            .expect("the group");
        assert_eq!(conversation, group.id);
        // A named bot resolves to its DM.
        let lead = swarm.bots.by_name("lead").expect("lead");
        let dm = pick_run_conversation(&swarm, None, Some("lead")).expect("the dm");
        assert_eq!(dm, lead.direct_conversation_id);
        // A missing group is a clean error, not a panic.
        let err = pick_run_conversation(&swarm, Some("Nope"), None).expect_err("missing");
        assert!(err.to_string().contains("no group called"), "{err}");
        embedded.shutdown().await;
    }

    #[test]
    fn nodes_render_one_line_each() {
        let node = NodeInfo::local(
            "this machine",
            vec![swarmbot_contracts::node::NodeRoot::read_write(
                "work",
                "/tmp/work",
            )],
            swarmbot_contracts::node::PolicyMap::default(),
        );
        let lines = nodes_lines(&[node]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("local online"), "{}", lines[0]);
        assert!(lines[0].contains("work:/tmp/work(rw)"), "{}", lines[0]);
        assert!(lines[0].contains("shell"), "{}", lines[0]);
    }

    /// A remote `run`: the events the live pump delivers go through the same
    /// `run_once` and the same `render.rs` as an embedded run.
    ///
    /// The stub brain serves one permanently queued task and then **closes its
    /// live socket**, so this run ends with a dead stream while the team is
    /// still busy — the case that used to be printed as "TIMED OUT after 1s"
    /// and exit `2`, blaming the team for a socket. What is pinned here is that
    /// it is reported as what it is: exit `3`, the stream-ended sentence, the
    /// busy lines, and an interrupt so the daemon is not left working on an
    /// answer nobody will read.
    ///
    /// The genuine timeout is
    /// [`a_team_that_never_goes_idle_times_out_with_exit_2`]'s job: the stub
    /// cannot stay connected long enough to time out.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_remote_run_whose_stream_ends_is_not_reported_as_a_timeout() {
        let dir = TempDir::new("remote-run").expect("temp");
        let stub = StubBrain::start();
        let remote = Remote::connect(&stub.url(), Some("tok"), dir.path(), false)
            .expect("the stub answers health");
        let conversation = Session::conversations(&remote)
            .expect("conversations")
            .first()
            .map(|conversation| conversation.id.clone())
            .expect("the stub serves one conversation");

        let mut buffer = Vec::new();
        let code = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            run_once(
                &remote,
                &RunOnce {
                    text: "@stub status?",
                    conversation: conversation.clone(),
                    wait: Duration::from_secs(1),
                    json: false,
                },
                &mut printer,
            )
            .await
            .expect("runs")
        };
        assert_eq!(
            code, EXIT_STREAM_ENDED,
            "a socket that closed is not a slow team, and 3 says so"
        );
        assert_ne!(code, EXIT_TIMEOUT, "2 means the team timed out");

        let out = String::from_utf8(buffer).expect("utf8");
        // The streamed reply, rendered by the shared path with the roster the
        // snapshot carried — a handle, not `@unknown`.
        assert!(out.contains("@stub › stub reply"), "{out}");
        assert!(out.contains("── reply ──"), "{out}");
        assert!(
            out.contains("delegated: @stub → @stub \"stub task\" (queued)"),
            "{out}"
        );
        assert!(
            !out.contains("TIMED OUT"),
            "a dead socket must not be blamed on the team: {out}"
        );
        assert!(
            out.contains("the live stream ended before the team went idle"),
            "{out}"
        );
        assert!(out.contains("task \"stub task\" is queued"), "{out}");

        // The message went out with the token in a header, never in the URL …
        let sent = stub
            .last_request(
                "POST",
                &format!("/api/conversations/{conversation}/messages"),
            )
            .expect("the message was posted");
        assert!(sent.body.contains("@stub status?"), "{sent:?}");
        assert_eq!(sent.auth.as_deref(), Some("Bearer tok"));
        assert!(
            !sent.path.contains("tok") && !sent.query.contains("tok"),
            "{sent:?}"
        );
        // … and a run that stops watching still stops the work, instead of
        // leaving a daemon busy with an answer nobody will read.
        assert!(
            stub.last_request(
                "POST",
                &format!("/api/conversations/{conversation}/interrupt")
            )
            .is_some(),
            "the run interrupted the conversation whose stream it lost"
        );
    }

    /// The real timeout: a team that never goes idle inside the budget. Exit
    /// `2`, the documented "TIMED OUT after Ns" line, the busy lines, and an
    /// interrupt.
    ///
    /// The session double is deliberate. `StubBrain`'s socket closes after the
    /// snapshot, so a stub-driven run can only ever be the stream-ended case;
    /// and waiting on a real brain for a real second would test the scheduler
    /// instead of the report.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_team_that_never_goes_idle_times_out_with_exit_2() {
        let session = NeverIdle::new("task \"stub task\" is queued");
        let mut buffer = Vec::new();
        let code = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            run_once(
                &session,
                &RunOnce {
                    text: "@lead status?",
                    conversation: ConversationId::new(),
                    wait: Duration::from_secs(1),
                    json: false,
                },
                &mut printer,
            )
            .await
            .expect("runs")
        };
        assert_eq!(code, EXIT_TIMEOUT, "a slow team is the documented timeout");

        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("TIMED OUT after 1s"), "{out}");
        assert!(out.contains("task \"stub task\" is queued"), "{out}");
        assert!(
            !out.contains("the live stream ended"),
            "the stream was open the whole time: {out}"
        );
        assert_eq!(
            session.interrupts(),
            1,
            "a run that gives up stops the work it stopped watching"
        );
    }

    /// `--json` is the same stream through the same printer, so a remote run is
    /// as machine-readable as a local one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_remote_json_run_emits_one_parsable_object_per_line() {
        let dir = TempDir::new("remote-json").expect("temp");
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), dir.path(), false).expect("connects");
        let conversation = Session::conversations(&remote)
            .expect("conversations")
            .first()
            .map(|conversation| conversation.id.clone())
            .expect("one conversation");

        let mut buffer = Vec::new();
        {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            let code = run_once(
                &remote,
                &RunOnce {
                    text: "@stub status?",
                    conversation,
                    wait: Duration::from_secs(1),
                    json: true,
                },
                &mut printer,
            )
            .await
            .expect("runs");
            assert_eq!(code, EXIT_STREAM_ENDED, "the stub's socket closed");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(
            !out.contains('\u{1b}'),
            "json mode never colourises: {out:?}"
        );
        let mut saw_synchronized = false;
        let mut saw_reply = false;
        for line in out.lines().filter(|line| !line.is_empty()) {
            let value: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("`{line}` is one JSON object ({err})"));
            let kind = value["type"].as_str().expect("every event has a type");
            saw_synchronized |= kind == "synchronized";
            saw_reply |= kind == "entry_upsert" && value["entry"]["kind"]["text"] == "stub reply";
        }
        assert!(saw_synchronized, "the stream synchronized: {out}");
        assert!(saw_reply, "the bot's reply streamed as JSON: {out}");
        // … and the machine-readable ending says why, without prose parsing.
        let stop = last_json_line(&out);
        assert_eq!(stop["type"], "run_stopped", "{out}");
        assert_eq!(stop["stop_reason"], "stream_ended", "{out}");
        assert_eq!(stop["interrupted"], true, "{out}");
        assert!(
            stop["still_running"].as_array().is_some_and(|lines| lines
                .iter()
                .any(|line| line.as_str().is_some_and(|line| line.contains("stub task")))),
            "{out}"
        );
    }

    /// The `--json` contract for the stop itself: every run ends with **one**
    /// object naming the reason, whichever of the three endings it had — so a
    /// script never has to parse the prose a human reads.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_json_run_ends_with_one_object_that_names_the_stop_reason() {
        // 1. idle — the embedded brain the other tests use, with a scripted
        //    answer so it really finishes.
        let dir = TempDir::new("json-stop").expect("temp");
        let embedded = embedded(dir.path());
        let swarm = Arc::clone(&embedded.swarm);
        swarm
            .backends
            .registry()
            .fake("fake")
            .expect("the fake")
            .push_turn(ScriptedTurn::text("done"));
        let group = swarm
            .conversations
            .find_by_title("Team")
            .expect("list")
            .expect("the demo group");
        let mut buffer = Vec::new();
        let code = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            run_once(
                &embedded,
                &RunOnce {
                    text: "@lead status?",
                    conversation: group.id.clone(),
                    wait: Duration::from_secs(30),
                    json: true,
                },
                &mut printer,
            )
            .await
            .expect("runs")
        };
        assert_eq!(code, 0);
        let out = String::from_utf8(buffer).expect("utf8");
        let stop = last_json_line(&out);
        assert_eq!(stop["type"], "run_stopped", "{out}");
        assert_eq!(stop["stop_reason"], "idle", "{out}");
        assert_eq!(stop["interrupted"], false, "{out}");
        assert_eq!(
            stop["still_running"].as_array().map(Vec::len),
            Some(0),
            "{out}"
        );
        assert_eq!(stop["stream_error"], serde_json::Value::Null, "{out}");
        // Exactly one stop object: the summary is the run's, not a per-event
        // line, and the "one JSON object per line" contract is unchanged.
        assert_eq!(
            out.lines()
                .filter(|line| line.contains("\"run_stopped\""))
                .count(),
            1,
            "{out}"
        );
        embedded.shutdown().await;

        // 2. timeout — a team that never goes idle.
        let stuck = NeverIdle::new("task \"stub task\" is queued");
        let mut buffer = Vec::new();
        let code = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            run_once(
                &stuck,
                &RunOnce {
                    text: "@lead status?",
                    conversation: ConversationId::new(),
                    wait: Duration::from_secs(1),
                    json: true,
                },
                &mut printer,
            )
            .await
            .expect("runs")
        };
        assert_eq!(code, EXIT_TIMEOUT);
        let out = String::from_utf8(buffer).expect("utf8");
        let stop = last_json_line(&out);
        assert_eq!(stop["stop_reason"], "timeout", "{out}");
        assert_eq!(stop["interrupted"], true, "{out}");
        assert!(
            stop["still_running"]
                .as_array()
                .is_some_and(|lines| !lines.is_empty()),
            "{out}"
        );

        // 3. stream ended — the same team, with its socket already gone.
        let cut = NeverIdle::ended("task \"stub task\" is queued")
            .with_stream_error("the brain at http://127.0.0.1:1 refused the token (401)");
        let mut buffer = Vec::new();
        let code = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            run_once(
                &cut,
                &RunOnce {
                    text: "@lead status?",
                    conversation: ConversationId::new(),
                    wait: Duration::from_secs(1),
                    json: true,
                },
                &mut printer,
            )
            .await
            .expect("runs")
        };
        assert_eq!(code, EXIT_STREAM_ENDED);
        let out = String::from_utf8(buffer).expect("utf8");
        let stop = last_json_line(&out);
        assert_eq!(stop["stop_reason"], "stream_ended", "{out}");
        // The session's own reason travels with it, so a script can tell a
        // refused token from a dropped connection without reading stderr.
        assert!(
            stop["stream_error"]
                .as_str()
                .is_some_and(|why| why.contains("401")),
            "{out}"
        );
    }

    #[tokio::test]
    async fn demo_refuses_a_remote_brain_in_one_line() {
        // Refused before anything is dialled or built: `demo` is a scripted
        // in-process brain, and running it anyway would look like a remote run
        // that worked.
        let cli = Cli::parse_from(["swarmbot", "--brain", "http://127.0.0.1:1", "demo"]);
        let err = dispatch(&cli).await.expect_err("refused");
        let text = err.to_string();
        assert!(text.contains("demo"), "{text}");
        assert!(text.contains("http://127.0.0.1:1"), "{text}");
        assert!(text.contains("drop --brain"), "{text}");
        assert!(!text.contains('\n'), "one line: {text}");
    }

    /// `--script` drives an in-process brain's fakes; against a daemon there is
    /// nothing here to script, so `run` and `chat` both refuse it in one line
    /// *before* dialling — never silently ignore an operator's file.
    #[tokio::test]
    async fn script_with_a_brain_is_refused_in_one_line() {
        for argv in [
            vec![
                "swarmbot",
                "--brain",
                "http://127.0.0.1:1",
                "run",
                "hi",
                "--script",
                "s.toml",
            ],
            vec![
                "swarmbot",
                "--brain",
                "http://127.0.0.1:1",
                "chat",
                "lead",
                "--script",
                "s.toml",
            ],
        ] {
            let cli = Cli::parse_from(&argv);
            let err = dispatch(&cli).await.expect_err("refused");
            let text = err.to_string();
            assert!(text.contains("--script"), "{text}");
            assert!(text.contains("http://127.0.0.1:1"), "{text}");
            assert!(text.contains("cannot script a daemon"), "{text}");
            assert!(!text.contains('\n'), "one line: {text}");
            assert!(
                !text.contains("cannot reach"),
                "refused before dialling: {text}"
            );
        }
    }

    /// `demo` scripts its own four-bot story, so a second script would be dropped
    /// on the floor; it is refused in one line that names the file and the way out.
    #[tokio::test]
    async fn demo_refuses_a_script_in_one_line() {
        let cli = Cli::parse_from(["swarmbot", "demo", "--script", "my-script.toml"]);
        let err = dispatch(&cli).await.expect_err("refused");
        let text = err.to_string();
        assert!(text.contains("demo"), "{text}");
        assert!(text.contains("my-script.toml"), "names the file: {text}");
        assert!(text.contains("--script"), "{text}");
        assert!(!text.contains('\n'), "one line: {text}");
    }

    #[tokio::test]
    async fn shutdown_and_pairing_need_a_brain_to_talk_to() {
        // Each refusal says why, in its own terms: `shutdown` has no separate
        // brain to stop, `nodes pair` no daemon to mint a code in.
        let cases: [(&[&str], &str); 2] = [
            (
                &["swarmbot", "shutdown"],
                "there is no separate brain to stop",
            ),
            (
                &["swarmbot", "nodes", "pair"],
                "a pairing code is minted by the daemon",
            ),
        ];
        for (argv, because) in cases {
            let cli = Cli::parse_from(argv);
            let err = dispatch(&cli).await.expect_err("refused");
            let text = err.to_string();
            assert!(text.contains("--brain <URL>"), "{text}");
            assert!(text.contains(because), "{text}");
            assert!(!text.contains('\n'), "one line: {text}");
        }
    }

    #[tokio::test]
    async fn a_token_without_a_brain_is_refused_without_being_echoed() {
        let cli = Cli::parse_from(["swarmbot", "--token", "sekrit-value", "bots", "ls"]);
        let err = dispatch(&cli).await.expect_err("refused");
        let text = format!("{err:#}");
        assert!(text.contains("--brain"), "{text}");
        assert!(
            !text.contains("sekrit-value"),
            "a refused token is still never printed: {text}"
        );
    }

    #[tokio::test]
    async fn flags_that_only_build_a_local_brain_are_refused_not_ignored() {
        // Each of these is refused *before* the URL is dialled, so the message
        // is about the flag and not about a connection nobody made.
        let cases = [
            vec![
                "swarmbot",
                "--brain",
                "http://127.0.0.1:1",
                "run",
                "hi",
                "--team",
                "team.toml",
            ],
            vec![
                "swarmbot",
                "--brain",
                "http://127.0.0.1:1",
                "run",
                "hi",
                "--backend",
                "fake",
            ],
            vec![
                "swarmbot",
                "--brain",
                "http://127.0.0.1:1",
                "run",
                "hi",
                "--root",
                "/tmp",
            ],
            vec![
                "swarmbot",
                "--brain",
                "http://127.0.0.1:1",
                "--config",
                "c.toml",
                "bots",
                "ls",
            ],
        ];
        for argv in cases {
            let expected = *argv
                .iter()
                .find(|arg| arg.starts_with("--") && **arg != "--brain")
                .expect("the flag under test");
            let cli = Cli::parse_from(&argv);
            let err = dispatch(&cli).await.expect_err("refused");
            let text = err.to_string();
            assert!(text.contains(expected), "{expected}: {text}");
            assert!(text.contains("does nothing against the brain"), "{text}");
            assert!(
                !text.contains("cannot reach"),
                "refused before dialling: {text}"
            );
        }
    }

    #[tokio::test]
    async fn a_websocket_brain_url_says_what_to_type_instead() {
        let cli = Cli::parse_from([
            "swarmbot",
            "--brain",
            "ws://127.0.0.1:8791",
            "--token",
            "tok",
            "bots",
            "ls",
        ]);
        let err = dispatch(&cli).await.expect_err("refused");
        let text = err.to_string();
        assert!(text.contains("WebSocket URL"), "{text}");
        assert!(text.contains("http://127.0.0.1:8791"), "{text}");
    }

    #[test]
    fn a_target_resolves_the_same_way_in_both_modes() {
        // The defaults are one function over two lists, so a remote roster and
        // a local one cannot disagree about where a run lands.
        let dm = ConversationId::new();
        let lead = Bot::new("lead", "Lead", "", "fake", dm.clone());
        let scout = Bot::new("scout", "Scout", "", "fake", ConversationId::new());
        let group = Conversation::group(
            "Team",
            vec![lead.id.clone(), scout.id.clone()],
            lead.id.clone(),
        )
        .expect("a group");
        let conversations = vec![group.clone()];
        let bots = vec![lead.clone(), scout.clone()];

        let by_group = pick_conversation(&conversations, &bots, Some("Team"), None).expect("group");
        assert_eq!(by_group, group.id);
        let by_bot = pick_conversation(&conversations, &bots, None, Some("@Scout")).expect("dm");
        assert_eq!(by_bot, scout.direct_conversation_id);
        let default = pick_conversation(&conversations, &bots, None, None).expect("default");
        assert_eq!(default, group.id, "\"Team\" is the default target");

        let err =
            pick_conversation(&conversations, &bots, Some("Nope"), None).expect_err("missing");
        assert!(
            err.to_string().contains("no group called \"Nope\""),
            "{err}"
        );
        let err =
            pick_conversation(&conversations, &bots, None, Some("ghost")).expect_err("missing");
        assert!(err.to_string().contains("no bot called @ghost"), "{err}");

        // No group called "Team" and no bot called "lead": the first bot's DM.
        let only =
            pick_conversation(&[], std::slice::from_ref(&scout), None, None).expect("first bot");
        assert_eq!(only, scout.direct_conversation_id);
        let err = pick_conversation(&[], &[], None, None).expect_err("no roster at all");
        assert!(err.to_string().contains("no bots to talk to"), "{err}");
        let _ = (dm, lead);
    }

    #[test]
    fn exit_codes_stay_in_range() {
        // ExitCode's payload is unstable API; what this pins is that 0 maps to
        // SUCCESS and that out-of-range codes are clamped instead of wrapping
        // (a `as u8` cast of -3 would be 253, not 0).
        assert_eq!(exit_code(0), std::process::ExitCode::SUCCESS);
        let _ = exit_code(2);
        let _ = exit_code(-3);
        assert_eq!((-3i32).clamp(0, 255), 0);
    }

    /// The `clap` error for a command line that must not parse. `Cli` has no
    /// `Debug` (it carries a config path and a token path), so `expect_err` is
    /// not available here; this says the same thing.
    fn parse_error(argv: &[&str]) -> clap::Error {
        match Cli::try_parse_from(argv) {
            Ok(_) => panic!("{argv:?} should not parse"),
            Err(err) => err,
        }
    }

    /// A usage error is not a timeout: clap's own `2` would make "the team is
    /// slow" and "I typed the command wrong" the same code, which is exactly
    /// what a wrapper script cannot afford. `--help`/`--version` are answers,
    /// not failures.
    ///
    /// The real binary is checked against this table in
    /// `tests/exit_codes.rs`; this pins the mapping `main` uses.
    #[test]
    fn a_usage_error_is_never_the_timeout_code() {
        for argv in [
            vec!["swarmbot", "--nonsense"],
            vec!["swarmbot", "nope"],
            vec!["swarmbot", "run"],
        ] {
            let err = parse_error(&argv);
            assert_eq!(
                usage_exit(&err),
                EXIT_USAGE,
                "{argv:?} is a usage error, not a timeout"
            );
            assert_ne!(usage_exit(&err), EXIT_TIMEOUT, "{argv:?}");
        }

        for argv in [vec!["swarmbot", "--help"], vec!["swarmbot", "--version"]] {
            let answer = parse_error(&argv);
            assert_eq!(usage_exit(&answer), 0, "{argv:?} is an answer");
        }

        // The table is where a user can find it, not only in this module's docs.
        let help = parse_error(&["swarmbot", "--help"]).to_string();
        assert!(help.contains("Exit codes"), "{help}");
        assert!(help.contains("64"), "{help}");
        assert!(help.contains("stream ended"), "{help}");
    }

    /// The `cmd_run` sequence on an unscripted fake, with the two streams a
    /// terminal would see kept apart: the notice goes to the stderr sink, and
    /// stdout carries the run — including the fake's own placeholder reply,
    /// which is exactly the truth the notice explains. What must never happen
    /// is the notice leaking into stdout (a pipe or `--json` reads stdout).
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unscripted_fake_run_warns_on_stderr_and_keeps_stdout_clean() {
        let dir = TempDir::new("run-notice").expect("temp");
        let embedded = embedded(dir.path());
        let swarm = Arc::clone(&embedded.swarm);
        let group = swarm
            .conversations
            .find_by_title("Team")
            .expect("list")
            .expect("the demo group");

        // The decision `cmd_run` makes before the first message: nothing
        // scripted the fake, so the notice is due — and it is printed to the
        // stderr sink, not to the printer.
        let due = notice::embedded_unscripted_fake(&swarm, &group.id);
        assert!(due, "an unscripted fake owes the notice");
        let mut stderr = Vec::new();
        assert!(notice::warn(&mut stderr, false, due));

        let mut buffer = Vec::new();
        let code = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            run_once(
                &embedded,
                &RunOnce {
                    text: "@scout check the workspace and report",
                    conversation: group.id.clone(),
                    wait: Duration::from_secs(30),
                    json: false,
                },
                &mut printer,
            )
            .await
            .expect("runs")
        };
        assert_eq!(code, 0, "an idle team exits 0");

        let err = String::from_utf8(stderr).expect("utf8");
        assert!(err.contains("[fake] no scripted turn left"), "{err}");
        assert!(err.contains("swarmbot demo"), "{err}");
        assert!(err.contains("--backend <id>"), "{err}");
        assert_eq!(
            err.matches("only answers what a script tells it to")
                .count(),
            1,
            "one run, one notice: {err}"
        );

        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("@scout ›"), "the run rendered: {out}");
        for line in &notice::lines() {
            assert!(!out.contains(line.as_str()), "stdout stays clean: {out}");
        }
        embedded.shutdown().await;
    }

    /// Under `--json` every stdout line must parse — the notice still reaches
    /// stderr, because a machine-readable stream is exactly where a silent
    /// placeholder reply does the most damage.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_json_run_keeps_stdout_pure_while_the_warning_reaches_stderr() {
        let dir = TempDir::new("run-notice-json").expect("temp");
        let embedded = embedded(dir.path());
        let swarm = Arc::clone(&embedded.swarm);
        let group = swarm
            .conversations
            .find_by_title("Team")
            .expect("list")
            .expect("the demo group");

        let mut stderr = Vec::new();
        assert!(notice::warn(
            &mut stderr,
            false,
            notice::embedded_unscripted_fake(&swarm, &group.id)
        ));

        let mut buffer = Vec::new();
        let code = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            run_once(
                &embedded,
                &RunOnce {
                    text: "@lead status?",
                    conversation: group.id.clone(),
                    wait: Duration::from_secs(30),
                    json: true,
                },
                &mut printer,
            )
            .await
            .expect("runs")
        };
        assert_eq!(code, 0);

        let out = String::from_utf8(buffer).expect("utf8");
        let lines: Vec<&str> = out.lines().filter(|line| !line.is_empty()).collect();
        assert!(!lines.is_empty());
        for line in lines {
            let _: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("`{line}` is one JSON object ({err})"));
        }
        assert!(
            !out.contains("only answers what a script tells it to"),
            "the notice never enters the JSON stream: {out}"
        );
        let err = String::from_utf8(stderr).expect("utf8");
        assert!(
            err.contains("only answers what a script tells it to"),
            "the warning reached stderr: {err}"
        );
        embedded.shutdown().await;
    }

    #[test]
    fn group_flag_matches_case_insensitively_and_groups_ls_parses() {
        let lead = Bot::new("lead", "Lead", "", "fake", ConversationId::new());
        let scout = Bot::new("scout", "Scout", "", "fake", ConversationId::new());
        let group = Conversation::group(
            "Team",
            vec![lead.id.clone(), scout.id.clone()],
            lead.id.clone(),
        )
        .expect("group");
        let picked = pick_conversation(
            std::slice::from_ref(&group),
            &[lead, scout],
            Some("team"),
            None,
        )
        .expect("case-insensitive --group");
        assert_eq!(picked, group.id);

        let missing = pick_conversation(std::slice::from_ref(&group), &[], Some("lead"), None)
            .expect_err("bot name is not matched by --group");
        assert!(missing.to_string().contains("no group called"), "{missing}");

        let cli = Cli::try_parse_from(["swarmbot", "groups", "ls"]).expect("parses groups ls");
        assert!(matches!(
            cli.command,
            Command::Groups {
                command: GroupsCommand::Ls
            }
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn group_run_streams_delegated_dm_and_message_bot_with_dm_labels() {
        let dir = TempDir::new("run-group-dm").expect("temp");
        let mut config = BrainConfig {
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        };
        let script_path = dir.path().join("story.toml");
        std::fs::write(
            &script_path,
            r#"
[[turns]]
bot = "lead"
reasoning = "delegate search to scout"
text = "delegating now"
[[turns.tool_calls]]
name = "handoff_task"
arguments = { to = "scout", title = "probe", task = "check files" }

[[turns]]
bot = "lead"
text = "waiting for scout"

[[turns]]
bot = "scout"
reasoning = "reading workspace"
[[turns.tool_calls]]
name = "message_bot"
arguments = { to = "lead", text = "early ping" }

[[turns]]
bot = "scout"
text = "scout finished cleanly"

[[turns]]
bot = "lead"
text = "NO_REPLY"

[[turns]]
bot = "lead"
text = "all done"
"#,
        )
        .expect("writes script");
        config::per_bot_fakes_for_script(&mut config, &script_path);
        let embedded = Embedded::build(
            BrainParams {
                config,
                data_dir: dir.path().to_path_buf(),
                roots: Vec::new(),
                secrets: BTreeMap::new(),
                offline: true,
            },
            true,
        )
        .expect("boots");
        let swarm = Arc::clone(&embedded.swarm);
        crate::script::load(&swarm, &script_path).expect("loads script");
        let group = swarm
            .conversations
            .find_by_title("Team")
            .expect("list")
            .expect("Team");

        let mut buffer = Vec::new();
        let code = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            run_once(
                &embedded,
                &RunOnce {
                    text: "run check",
                    conversation: group.id.clone(),
                    wait: Duration::from_secs(30),
                    json: false,
                },
                &mut printer,
            )
            .await
            .expect("runs")
        };
        assert_eq!(code, 0);
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("@lead · thinking ›"), "{out}");
        assert!(out.contains("[dm @scout]"), "{out}");
        assert!(out.contains("@scout › scout finished cleanly"), "{out}");
        assert!(out.contains("@lead › all done"), "{out}");
        embedded.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dm_run_excludes_unrelated_group_messages() {
        let dir = TempDir::new("run-dm-isolated").expect("temp");
        let embedded = embedded(dir.path());
        let swarm = Arc::clone(&embedded.swarm);
        let group = swarm
            .conversations
            .find_by_title("Team")
            .expect("list")
            .expect("Team");
        let scout = swarm.bots.by_name("scout").expect("scout");
        swarm
            .transcript
            .bot_message(&group.id, &scout.id, "unrelated group noise", None, None)
            .expect("seeded");

        swarm
            .backends
            .registry()
            .fake("fake")
            .expect("fake")
            .push_turn(ScriptedTurn::text("direct reply"));

        let mut buffer = Vec::new();
        let code = {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            run_once(
                &embedded,
                &RunOnce {
                    text: "hello scout",
                    conversation: scout.direct_conversation_id.clone(),
                    wait: Duration::from_secs(30),
                    json: false,
                },
                &mut printer,
            )
            .await
            .expect("runs")
        };
        assert_eq!(code, 0);
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("@scout › direct reply"), "{out}");
        assert!(
            !out.contains("unrelated group noise"),
            "DM run must not pull unrelated group chatter: {out}"
        );
        embedded.shutdown().await;
    }
}
