//! The command line: `pair`, `run`, `status`.
//!
//! One rule shapes the flag handling — **the command line wins over
//! `<data>/node.toml`**. The file is what the owner wrote down once; a flag is
//! what they mean this run. A flag that lost to a stale file would silently
//! change what a daemon is allowed to do, and neither direction of that mistake
//! is survivable.
//!
//! There is deliberately no flag that widens policy beyond a ceiling: no
//! `--dangerously-skip-permissions`, no `--trust-brain`, no auto-allow. The only
//! way to reach `always` is to type `--ceiling surface=always` or write it into
//! the file, and both are visible in `status` and in every audit line.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use swarmbot_contracts::{NodeRoot, RootMode};
use tokio_util::sync::CancellationToken;

use crate::config::{self, CeilingFlag, NodeConfig, NodePaths, parse_ceiling};
use crate::dial::{self, DialPlan, ServeOptions, StopReason};
use crate::handlers::NodeRuntime;
use crate::identity::{self, Identity, PairOutcome, dial_url};
use crate::state::{self, NodeState, RunStatus, StateWriter};
use crate::{NODE_VERSION, NodeError};

/// The daemon's command line.
#[derive(Parser, Clone, Debug)]
#[command(
    name = "swarmbot-node",
    version = NODE_VERSION,
    about = "Run a SwarmBot node: the hands that execute fs and shell calls under their own policy.",
    long_about = "A SwarmBot node pairs once with a brain, then dials out to it and serves \
                  `fs.*` and `shell.exec` calls. The node decides finally: its own roots, its own \
                  deny list and its own ceilings, with the effective ceiling always \
                  `min(brain, node)`. A brain can ask; it can never raise what this node allows.\n\n\
                  Everything lives under one data dir: `identity.json` (mode 0600), `node.toml`, \
                  `workspace/`, `audit/`, `spill/`, `node-state.json`. By default `workspace/` is a \
                  read-write root and `spill/` a read-only one, so a bot can page back the output a \
                  `shell.exec` had to truncate but cannot use that directory as scratch space."
)]
pub struct Cli {
    /// Where this node keeps its identity, config, workspace, audit log and spill.
    #[arg(
        long,
        global = true,
        env = "SWARMBOT_NODE_DATA_DIR",
        value_name = "DIR",
        help = "Data dir [default: $XDG_DATA_HOME/swarmbot]"
    )]
    pub data_dir: Option<PathBuf>,

    /// Log at debug level (also settable with SWARMBOT_NODE_LOG or RUST_LOG).
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// The three things a node operator does.
#[derive(Subcommand, Clone, Debug)]
pub enum Command {
    /// Pair with a brain using a one-time code, and pin the brain's key.
    Pair {
        /// The brain, e.g. `https://brain.example` or `http://127.0.0.1:8787`.
        #[arg(value_name = "BRAIN_URL")]
        brain_url: String,
        /// The 8-character code: `ABCD-EFGH`, `ABCDEFGH` and `abcd efgh` are one code.
        #[arg(value_name = "CODE")]
        code: String,
    },

    /// Dial the brain and serve calls until stopped.
    Run(RunOptions),

    /// Report the identity, the policy, and whether a daemon is live.
    Status,
}

/// `swarmbot-node run` flags. Every one of them overrides `<data>/node.toml`.
#[derive(Args, Clone, Debug)]
pub struct RunOptions {
    /// The brain to dial (`ws://`, `wss://`, `http://` or `https://`). Defaults
    /// to the origin pinned at pairing.
    #[arg(long, value_name = "URL")]
    pub brain: Option<String>,

    /// A read-write root this node will act inside. Repeatable. Replaces the
    /// file's roots **and** the default roots. Defaults to `<data>/workspace`,
    /// next to a read-only `<data>/spill` root — naming any root here drops
    /// that spill root too, so add `--read-only-root <data>/spill` to keep
    /// spilled `shell.exec` output readable back.
    #[arg(long = "root", value_name = "PATH")]
    pub roots: Vec<PathBuf>,

    /// A read-only root: reads resolve inside it, writes are refused with
    /// `PathDenied { reason: "read-only root" }`. Repeatable.
    #[arg(long = "read-only-root", value_name = "PATH")]
    pub read_only_roots: Vec<PathBuf>,

    /// A per-surface ceiling: `--ceiling shell=never`, `--ceiling read_file=always`.
    /// Surfaces: `shell`, `read_file`, `write_file`, `edit_file`, `computer`.
    /// Levels: `never`, `ask`, `always`. A bare level sets the default for every
    /// surface not named. Repeatable.
    #[arg(long = "ceiling", value_name = "SURFACE=LEVEL")]
    pub ceilings: Vec<String>,

    /// How the owner refers to this node. Defaults to the hostname.
    #[arg(long, value_name = "NAME")]
    pub label: Option<String>,

    /// Withdraw the `shell@1` capability: no `shell.exec` will run, and the
    /// hello object stops advertising it.
    #[arg(long)]
    pub no_shell: bool,

    /// `node.heartbeat` period in seconds [default: 15].
    #[arg(long, value_name = "N")]
    pub heartbeat_secs: Option<u64>,

    /// Back off this long before the first redial, in milliseconds. Hidden: it
    /// exists so a test can compress the documented 250 ms…30 s curve, not so
    /// an operator can make a reconnect storm.
    #[arg(long, value_name = "MS", hide = true)]
    pub redial_initial_ms: Option<u64>,
}

/// Parse the command line. Split out so `main` stays three lines and a test can
/// build a [`Cli`] the same way the binary does.
pub fn parse() -> Cli {
    Cli::parse()
}

/// Run one parsed command. `Ok(())` is exit 0; `Err` is printed by `main` and
/// becomes exit 1 — including for a pairing the brain refused, whose message is
/// the brain's own reason.
pub async fn execute(cli: Cli) -> Result<(), NodeError> {
    let data_dir = cli.data_dir.clone();
    match cli.command {
        Command::Pair { brain_url, code } => {
            let outcome = pair_command(data_dir.as_deref(), &brain_url, &code).await?;
            // stdout rather than the log: this is the operator's answer, and the
            // log has its own formatting.
            println!("{}", outcome.node_id);
            println!(
                "paired with {} (brain key ed25519:{})",
                outcome.identity.brain_origin(),
                outcome.identity.brain_fingerprint()
            );
            println!("identity: {} (mode 0600)", outcome.identity_path.display());
            println!("start the daemon with: swarmbot-node run");
            Ok(())
        }
        Command::Run(options) => match run_command(data_dir.as_deref(), &options).await? {
            // A revoked node is a condition an operator must notice, not a clean
            // stop, so it is a non-zero exit with the reason as the message.
            StopReason::Revoked => Err(NodeError::auth(StopReason::Revoked.to_string())),
            StopReason::Shutdown => {
                println!("swarmbot-node stopped");
                Ok(())
            }
        },
        Command::Status => {
            println!("{}", status_command(data_dir.as_deref())?);
            Ok(())
        }
    }
}

/// The resolved data dir: `--data-dir` (with `~` expanded), else the platform
/// default.
pub fn data_dir(override_dir: Option<&Path>) -> PathBuf {
    match override_dir {
        Some(dir) => config::expand_tilde(dir.to_path_buf()),
        None => NodePaths::default_dir(),
    }
}

/// Resolve the configuration: the file first, then the command line over it.
pub fn resolve_config(
    override_dir: Option<&Path>,
    options: &RunOptions,
) -> Result<NodeConfig, NodeError> {
    let mut resolved = NodeConfig::load(data_dir(override_dir))?;
    if let Some(url) = &options.brain {
        resolved.brain_url = Some(url.clone());
    }
    if let Some(label) = &options.label {
        resolved.label = label.clone();
    }
    if let Some(secs) = options.heartbeat_secs {
        resolved.heartbeat_secs = secs.max(1);
    }
    if options.no_shell {
        // One-way on purpose: the file cannot re-enable what this run withdrew,
        // and there is no `--shell` flag to undo it.
        resolved.shell_enabled = false;
    }
    if !options.roots.is_empty() || !options.read_only_roots.is_empty() {
        let mut roots = Vec::new();
        for path in &options.roots {
            roots.push((config::expand_tilde(path.clone()), RootMode::ReadWrite));
        }
        for path in &options.read_only_roots {
            roots.push((config::expand_tilde(path.clone()), RootMode::ReadOnly));
        }
        resolved.roots = named_roots(roots);
    }
    for flag in &options.ceilings {
        match parse_ceiling(flag)? {
            CeilingFlag::Default(ceiling) => resolved.ceilings.default = ceiling,
            CeilingFlag::Surface { surface, ceiling } => resolved.ceilings.set(surface, ceiling),
        }
    }
    Ok(resolved)
}

/// Roots with ids derived from their directory names and de-duplicated: two
/// roots sharing an id would make the roster and the hello object ambiguous.
fn named_roots(roots: Vec<(PathBuf, RootMode)>) -> Vec<NodeRoot> {
    let mut out: Vec<NodeRoot> = Vec::new();
    for (index, (path, mode)) in roots.into_iter().enumerate() {
        let derived = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("root{index}"));
        let id = if out.iter().any(|root| root.id == derived) {
            format!("{derived}-{index}")
        } else {
            derived
        };
        out.push(NodeRoot {
            id,
            path: path.display().to_string(),
            mode,
        });
    }
    out
}

/// `swarmbot-node pair <brain-url> <code>`.
pub async fn pair_command(
    override_dir: Option<&Path>,
    brain_url: &str,
    code: &str,
) -> Result<PairOutcome, NodeError> {
    let config = NodeConfig::load(data_dir(override_dir))?;
    identity::pair(brain_url, code, &config).await
}

/// `swarmbot-node run`: dial, serve, redial, and stop cleanly on a signal.
pub async fn run_command(
    override_dir: Option<&Path>,
    options: &RunOptions,
) -> Result<StopReason, NodeError> {
    let config = resolve_config(override_dir, options)?;
    let paths = config.paths();
    let identity = Arc::new(identity::ensure_paired(&paths)?);
    let runtime = NodeRuntime::new(config.clone(), Arc::clone(&identity))?;

    tracing::info!(
        node_id = %identity.node_id(),
        label = %config.label,
        version = NODE_VERSION,
        brain = %identity.brain_origin(),
        workspace = %runtime.executor.workspace().display(),
        roots = config.roots.len(),
        shell = config.shell_enabled,
        ceilings = %ceiling_summary(&config),
        "swarmbot-node starting"
    );
    if !config.shell_enabled {
        tracing::warn!(
            "`--no-shell`: the `shell@1` capability is withdrawn and every shell.exec will be refused"
        );
    }

    let state = StateWriter::start(
        &paths,
        &NodeState::starting(
            std::process::id(),
            identity.node_id().as_str(),
            &config.label,
            identity.brain_origin(),
            dial_url(&config, &identity).ok(),
            &paths.data_dir,
            runtime.roots(),
            config.ceilings.clone(),
            config.shell_enabled,
            config.capabilities().as_slice().to_vec(),
        ),
    )?;

    let shutdown = CancellationToken::new();
    let signals = install_signals(shutdown.clone(), Arc::clone(&runtime));
    let mut plan = DialPlan::default();
    if let Some(ms) = options.redial_initial_ms {
        plan.initial = std::time::Duration::from_millis(ms);
    }
    let outcome = dial::serve(
        Arc::clone(&runtime),
        shutdown,
        ServeOptions {
            plan,
            state: Some(state.clone()),
        },
    )
    .await;
    signals.abort();
    // Whatever happened, this daemon is stopping: kill anything still running,
    // and remove the state file so `status` does not report a node that is gone.
    runtime.shutdown();
    state.remove();
    outcome
}

/// The `swarmbot-node status` report, as text. Returning a string keeps the
/// shape testable and lets `execute` decide where it goes.
pub fn status_command(override_dir: Option<&Path>) -> Result<String, NodeError> {
    let dir = data_dir(override_dir);
    let paths = NodePaths::new(dir.clone());
    let mut out = String::new();
    out.push_str(&format!("data dir:      {}\n", dir.display()));

    let identity = match Identity::load(&paths) {
        Ok(identity) => Some(identity),
        Err(NodeError::NotPaired { path }) => {
            out.push_str("identity:      not paired\n");
            out.push_str(&format!(
                "               {} does not exist; run `swarmbot-node pair <brain-url> <code>`\n",
                path.display()
            ));
            None
        }
        Err(error) => {
            out.push_str(&format!("identity:      unreadable: {error}\n"));
            None
        }
    };
    if let Some(identity) = &identity {
        out.push_str("identity:      paired\n");
        out.push_str(&format!("node id:       {}\n", identity.node_id()));
        out.push_str(&format!("brain origin:  {}\n", identity.brain_origin()));
        out.push_str(&format!(
            "brain key:     ed25519:{} (pinned at pairing; never re-fetched)\n",
            identity.brain_fingerprint()
        ));
        out.push_str(&format!(
            "node key:      ed25519:{} (the private half never leaves the file)\n",
            identity.fingerprint()
        ));
    }

    let config = NodeConfig::load(&dir)?;
    out.push_str(&format!("label:         {}\n", config.label));
    out.push_str(&format!("version:       {NODE_VERSION}\n"));
    out.push_str(&format!(
        "brain url:     {}\n",
        config
            .brain_url
            .as_deref()
            .unwrap_or("(derived from the pinned origin)")
    ));
    out.push_str("roots:\n");
    if config.roots.is_empty() {
        out.push_str("               (none — every path would be refused)\n");
    }
    for root in &config.roots {
        let mode = match root.mode {
            RootMode::ReadOnly => "read_only",
            RootMode::ReadWrite => "read_write",
        };
        out.push_str(&format!("  {:<12} {} ({mode})\n", root.id, root.path));
    }
    out.push_str("ceilings:\n");
    out.push_str(&format!(
        "  {:<12} {}\n",
        "default",
        config::ceiling_label(config.ceilings.default)
    ));
    for (surface, ceiling) in &config.ceilings.surfaces {
        out.push_str(&format!(
            "  {:<12} {}\n",
            surface.as_str(),
            config::ceiling_label(*ceiling)
        ));
    }
    out.push_str(
        "               (effective is min(brain, node): a brain can lower these, never raise them)\n",
    );
    out.push_str(&format!(
        "capabilities:  {}\n",
        config.capabilities().as_slice().join(", ")
    ));
    out.push_str(&format!(
        "shell:         {}\n",
        if config.shell_enabled {
            "enabled"
        } else {
            "withdrawn (--no-shell)"
        }
    ));
    out.push_str(&format!(
        "heartbeat:     every {} s\n",
        config.heartbeat_secs
    ));
    let node_id = identity
        .as_ref()
        .map(|identity| identity.node_id().to_string())
        .unwrap_or_else(|| "<node id>".to_string());
    out.push_str(&format!(
        "audit log:     {}\n",
        paths.audit().join(format!("{node_id}.jsonl")).display()
    ));

    match state::read(&paths)? {
        RunStatus::NotRunning => {
            out.push_str("daemon:        not running\n");
        }
        RunStatus::Crashed { state } => {
            out.push_str(&format!(
                "daemon:        not running — a state file from pid {} is left behind, so it did not exit cleanly\n",
                state.pid
            ));
            if let Some(error) = &state.last_error {
                out.push_str(&format!("last error:    {error}\n"));
            }
        }
        RunStatus::Running { state } => {
            let connection = if state.connected {
                match &state.connected_at {
                    Some(at) => format!("connected since {}", at.to_rfc3339()),
                    None => "connected".to_string(),
                }
            } else {
                "dialing (no verified node.hello yet)".to_string()
            };
            out.push_str(&format!(
                "daemon:        running (pid {}), {connection}\n",
                state.pid
            ));
            out.push_str(&format!(
                "invocations:   {} active\n",
                state.active_invocations
            ));
            if let Some(error) = &state.last_error {
                out.push_str(&format!("last error:    {error}\n"));
            }
        }
    }
    Ok(out.trim_end().to_string())
}

/// One line naming every ceiling, for the start-up log: what this node allows
/// should be in the first thing it says about itself.
fn ceiling_summary(config: &NodeConfig) -> String {
    let mut parts = vec![format!(
        "default={}",
        config::ceiling_label(config.ceilings.default)
    )];
    for (surface, ceiling) in &config.ceilings.surfaces {
        parts.push(format!(
            "{}={}",
            surface.as_str(),
            config::ceiling_label(*ceiling)
        ));
    }
    parts.join(" ")
}

/// Wait for SIGINT or SIGTERM and cancel `shutdown`.
///
/// A second signal forces the kill path again: an operator who presses Ctrl-C
/// twice means it, and a node that will not stop is worse than one that drops a
/// call. The process still exits 0, because "stopped when asked" is the
/// successful outcome of `run`.
fn install_signals(
    shutdown: CancellationToken,
    runtime: Arc<NodeRuntime>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut terminate = match tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        ) {
            Ok(stream) => Some(stream),
            Err(error) => {
                // Not fatal: SIGINT still works, and a platform without SIGTERM
                // has nothing to install.
                tracing::debug!(%error, "cannot listen for SIGTERM; SIGINT still stops the node");
                None
            }
        };
        if !wait_for_signal(&mut terminate).await {
            tracing::warn!(
                "cannot listen for stop signals; this daemon can only be stopped by killing it"
            );
            return;
        }
        tracing::info!("received a stop signal; finishing in-flight work and closing the socket");
        runtime.shutdown();
        shutdown.cancel();
        if wait_for_signal(&mut terminate).await {
            tracing::warn!(
                "a second stop signal: killing the remaining process groups and exiting now"
            );
            runtime.registry.kill_all();
            std::process::exit(0);
        }
    })
}

/// Resolve on the next SIGINT or SIGTERM. `false` means neither can be waited
/// for, which the caller reports rather than silently ignoring.
async fn wait_for_signal(terminate: &mut Option<tokio::signal::unix::Signal>) -> bool {
    loop {
        tokio::select! {
            result = tokio::signal::ctrl_c() => match result {
                Ok(()) => return true,
                // A ctrl-c listener that fails at once would spin; the loop
                // gives the SIGTERM branch its chance instead.
                Err(error) => {
                    tracing::debug!(%error, "cannot listen for SIGINT");
                    if terminate.is_none() {
                        return false;
                    }
                }
            },
            _ = async {
                match terminate {
                    Some(stream) => while stream.recv().await.is_none() {},
                    None => std::future::pending::<()>().await,
                }
            } => return true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use clap::CommandFactory as _;
    use swarmbot_contracts::{PolicyCeiling, PolicyMap, Surface};

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("parses")
    }

    fn run_options() -> RunOptions {
        RunOptions {
            brain: None,
            roots: Vec::new(),
            read_only_roots: Vec::new(),
            ceilings: Vec::new(),
            label: None,
            no_shell: false,
            heartbeat_secs: None,
            redial_initial_ms: None,
        }
    }

    /// A paired identity in a temp dir, with a brain key that is not this
    /// machine's: the pin is what makes a hello meaningful.
    fn fake_identity(dir: &Path) -> Identity {
        use ed25519_dalek::SigningKey;
        use swarmbot_contracts::NodeId;
        Identity::from_parts(
            NodeId::new(),
            SigningKey::from_bytes(&[9u8; 32]),
            SigningKey::from_bytes(&[4u8; 32]).verifying_key(),
            "https://brain.example",
        )
        .expect("identity")
        .saved_in(dir)
    }

    trait SavedIn: Sized {
        fn saved_in(self, dir: &Path) -> Self;
    }

    impl SavedIn for Identity {
        fn saved_in(self, dir: &Path) -> Self {
            self.save(&NodePaths::new(dir)).expect("saves");
            self
        }
    }

    #[test]
    fn the_top_level_help_names_every_subcommand() {
        // Not a clap smoke test: `--help` is what the report quotes, and a
        // rename that breaks it would otherwise only show up in a terminal.
        let help = Cli::command().render_help().to_string();
        for expected in ["pair", "run", "status", "--data-dir", "--verbose"] {
            assert!(
                help.contains(expected),
                "the help is missing {expected}:\n{help}"
            );
        }
    }

    #[test]
    fn the_run_help_lists_every_policy_flag() {
        let mut command = Cli::command();
        let sub = command
            .find_subcommand_mut("run")
            .expect("run is a subcommand");
        let help = sub.render_help().to_string();
        for flag in [
            "--brain",
            "--root",
            "--read-only-root",
            "--ceiling",
            "--label",
            "--no-shell",
            "--heartbeat-secs",
        ] {
            assert!(
                help.contains(flag),
                "the run help is missing {flag}:\n{help}"
            );
        }
        assert!(
            !help.contains("--redial-initial-ms"),
            "the testing knob stays hidden:\n{help}"
        );
        assert!(
            !help.contains("skip-permissions") && !help.contains("--trust"),
            "there is no bypass flag:\n{help}"
        );
    }

    #[test]
    fn pair_takes_a_url_and_a_code() {
        match cli(&["swarmbot-node", "pair", "http://127.0.0.1:1", "ABCD-EFGH"]).command {
            Command::Pair { brain_url, code } => {
                assert_eq!(brain_url, "http://127.0.0.1:1");
                assert_eq!(code, "ABCD-EFGH");
            }
            other => panic!("expected Pair, got {other:?}"),
        }
    }

    #[test]
    fn the_command_line_wins_over_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("node.toml"),
            r#"label = "from the file"
heartbeat_secs = 3
shell = true

[[roots]]
path = "/srv/from-file"

[ceilings]
default = "always"
"#,
        )
        .expect("write");
        let root = dir.path().join("cli-root");
        std::fs::create_dir_all(&root).expect("mkdir");
        let options = RunOptions {
            brain: Some("ws://127.0.0.1:9".into()),
            roots: vec![root.clone()],
            ceilings: vec!["shell=never".into()],
            label: Some("from the cli".into()),
            no_shell: true,
            heartbeat_secs: Some(30),
            ..run_options()
        };
        let config = resolve_config(Some(dir.path()), &options).expect("resolves");
        assert_eq!(config.label, "from the cli");
        assert_eq!(config.brain_url.as_deref(), Some("ws://127.0.0.1:9"));
        assert_eq!(config.heartbeat_secs, 30);
        assert!(!config.shell_enabled, "--no-shell wins over `shell = true`");
        assert_eq!(config.roots.len(), 1, "--root replaces the file's roots");
        assert_eq!(config.roots[0].path, root.display().to_string());
        assert_eq!(
            config.ceilings.default,
            PolicyCeiling::Always,
            "an unnamed surface keeps the file's default"
        );
        assert_eq!(config.ceilings.get(Surface::Shell), PolicyCeiling::Never);
    }

    #[test]
    fn absent_flags_leave_the_file_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("node.toml"),
            "label = \"from the file\"\nshell = false\nheartbeat_secs = 42\n",
        )
        .expect("write");
        let config = resolve_config(Some(dir.path()), &run_options()).expect("resolves");
        assert_eq!(config.label, "from the file");
        assert!(!config.shell_enabled);
        assert_eq!(config.heartbeat_secs, 42);
    }

    #[test]
    fn read_only_roots_refuse_a_write_at_the_policy_level() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rw = dir.path().join("rw");
        let ro = dir.path().join("ro");
        let options = RunOptions {
            roots: vec![rw.clone()],
            read_only_roots: vec![ro.clone()],
            ..run_options()
        };
        let config = resolve_config(Some(dir.path()), &options).expect("resolves");
        assert_eq!(config.roots.len(), 2);
        assert_eq!(config.roots[0].mode, RootMode::ReadWrite);
        assert_eq!(config.roots[0].id, "rw");
        assert_eq!(config.roots[1].mode, RootMode::ReadOnly);
        assert_eq!(config.roots[1].id, "ro");
        let policy = config.path_policy();
        let denied = policy
            .resolve(&ro.join("x"), dir.path(), true)
            .expect_err("a read-only root refuses a write");
        assert!(
            matches!(&denied, swarmbot_contracts::ToolError::PathDenied { reason, .. } if reason == "read-only root"),
            "{denied:?}"
        );
        // ...and still allows the read.
        assert!(policy.resolve(&ro.join("x"), dir.path(), false).is_ok());
    }

    #[test]
    fn a_bad_ceiling_flag_is_refused_with_the_accepted_shapes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let options = RunOptions {
            ceilings: vec!["shell=whenever".into()],
            ..run_options()
        };
        let error = resolve_config(Some(dir.path()), &options).expect_err("refused");
        assert!(
            error.to_string().contains("surface=never|ask|always"),
            "{error}"
        );
    }

    #[test]
    fn a_zero_heartbeat_period_is_raised_to_one() {
        // `interval(0)` panics in tokio; a nonsense value must degrade rather
        // than take the daemon down.
        let dir = tempfile::tempdir().expect("tempdir");
        let options = RunOptions {
            heartbeat_secs: Some(0),
            ..run_options()
        };
        let config = resolve_config(Some(dir.path()), &options).expect("resolves");
        assert_eq!(config.heartbeat_secs, 1);
    }

    #[test]
    fn status_of_an_unpaired_node_says_how_to_pair() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = status_command(Some(dir.path())).expect("reports");
        assert!(report.contains("identity:      not paired"), "{report}");
        assert!(report.contains("swarmbot-node pair"), "{report}");
        assert!(report.contains("daemon:        not running"), "{report}");
        assert!(report.contains("min(brain, node)"), "{report}");
        assert!(report.contains("read_file"), "{report}");
    }

    #[test]
    fn status_of_a_paired_node_reports_the_pin_and_the_policy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = fake_identity(dir.path());
        let mut config = NodeConfig::new_in(dir.path());
        config.brain_url = Some("ws://127.0.0.1:1".into());
        config.ceilings.set(Surface::Shell, PolicyCeiling::Never);
        config.save().expect("saves");

        let report = status_command(Some(dir.path())).expect("reports");
        assert!(report.contains("identity:      paired"), "{report}");
        assert!(report.contains(identity.node_id().as_str()), "{report}");
        assert!(
            report.contains("brain origin:  https://brain.example"),
            "{report}"
        );
        assert!(
            report.contains(&format!("ed25519:{}", identity.brain_fingerprint())),
            "{report}"
        );
        assert!(
            report.lines().any(|line| {
                let line = line.trim();
                line.starts_with("shell") && line.ends_with("never")
            }),
            "the `shell = never` ceiling must be reported: {report}"
        );
        assert!(report.contains("daemon:        not running"), "{report}");
        // The private key is in the report in no spelling.
        let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(identity.signing_key().to_bytes());
        assert!(!report.contains(&secret), "{report}");
        assert!(
            !report.contains(&hex::encode(identity.signing_key().to_bytes())),
            "{report}"
        );
    }

    #[test]
    fn status_reports_a_live_daemon_from_its_state_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(dir.path());
        fake_identity(dir.path());
        let snapshot = NodeState::starting(
            std::process::id(),
            "nod_0189fbd3a4d37f0e9c0e6a3f8f1c9d2e",
            "bench",
            "http://127.0.0.1:1",
            Some("ws://127.0.0.1:1/node".into()),
            &paths.data_dir,
            Vec::new(),
            PolicyMap::default(),
            true,
            vec!["fs@1".into()],
        );
        let writer = StateWriter::start(&paths, &snapshot).expect("starts");
        writer
            .update(|state| {
                state.connected = true;
                state.connected_at = Some(chrono::Utc::now());
                state.active_invocations = 2;
            })
            .expect("updates");
        let report = status_command(Some(dir.path())).expect("reports");
        assert!(report.contains("daemon:        running"), "{report}");
        assert!(report.contains("connected since"), "{report}");
        assert!(report.contains("2 active"), "{report}");

        writer.remove();
        let report = status_command(Some(dir.path())).expect("reports");
        assert!(report.contains("daemon:        not running"), "{report}");
    }

    #[test]
    fn status_calls_a_dead_daemon_crashed_rather_than_running() {
        // The point of the pid check: a leftover file must not be reported as a
        // live connection.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(dir.path());
        let snapshot = NodeState::starting(
            i32::MAX as u32,
            "nod_0189fbd3a4d37f0e9c0e6a3f8f1c9d2e",
            "bench",
            "http://127.0.0.1:1",
            None,
            &paths.data_dir,
            Vec::new(),
            PolicyMap::default(),
            true,
            vec!["fs@1".into()],
        );
        StateWriter::start(&paths, &snapshot).expect("starts");
        let report = status_command(Some(dir.path())).expect("reports");
        assert!(report.contains("did not exit cleanly"), "{report}");
        assert!(!report.contains("daemon:        running"), "{report}");
    }
}
