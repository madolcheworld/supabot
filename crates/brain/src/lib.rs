//! SwarmBot brain: the composition root and the daemon.
//!
//! This crate is where everything else becomes a running process. It owns no
//! orchestration of its own — [`swarmbot_core::compose`] builds the swarm, and the
//! services in `core` do the work — so what is left here is the shape of a daemon:
//!
//! * [`config`] — the CLI, the TOML file, the environment, and the data dir
//!   layout, resolved into one [`config::Resolved`] value;
//! * [`auth`] — the owner token, the session cookie, the one-time login URL, the
//!   WebSocket `Origin` allow-list and the brain's Ed25519 identity;
//! * [`error`] — [`swarmbot_contracts::DomainError`] as the one error envelope;
//! * [`telemetry`] — tracing setup, and the rule that keeps a credential out of a
//!   log line;
//! * [`routes`], [`live`] — the REST API and the `/ws/live` socket;
//! * [`gateway`] — the node gateway: pairing, the signed control socket,
//!   `node.hello`, presence, node-initiated approvals, and the
//!   [`swarmbot_tools::NodeExecutor`] that puts a remote call on that socket;
//! * [`compose`] — the swarm, built with that executor in front of the local one;
//! * [`server`] — the two listeners, the middleware stack, the graceful stop;
//! * [`state`] — what a handler is handed.
//!
//! The node gateway speaks `swarmbot-node-protocol` v1: `POST /node/pair` redeems
//! a single-use code for a node identity, and `GET /node` upgrades to the control
//! socket only after a signature over a fresh nonce verifies. The MCP gateway on
//! the internal listener serves `/mcp` (the whole tool registry, behind the owner
//! gate) and per-turn `/mcp/t/{token}` endpoints (whose token is the credential).
//! The internal listener is loopback-only: `internal_addr` is refused at resolve
//! time unless it names `127.0.0.0/8` or `::1`.
//!
//! # The boot sequence
//!
//! [`run`] does, in this order and with a log line each: resolve the data dir →
//! load the config → load the secrets → compose the swarm, with the node gateway
//! wired into the machine side → boot it → start the expiry ticker → mint and
//! print the one-time login URL when the token file did not exist → bind the
//! listeners → serve. The order is not arbitrary: the data
//! dir decides where the config is, the config decides what the swarm is, and the
//! token file has to exist before the first listener accepts a request.
//!
//! # Shutdown
//!
//! SIGINT, SIGTERM and `POST /api/shutdown` all take the same path: stop
//! accepting, let the in-flight requests finish, then
//! [`swarmbot_core::Swarm::shutdown`] so no turn is left marked "running". A
//! second signal exits at once.

pub mod auth;
pub mod compose;
pub mod config;
pub mod error;
pub mod gateway;
pub mod live;
pub mod routes;
pub mod server;
pub mod state;
pub mod telemetry;
pub mod tls;

use std::collections::BTreeMap;
use std::sync::Arc;

use clap::Parser;
use tracing::{debug, info, warn};

use swarmbot_contracts::repo::SettingsStore as _;
use swarmbot_core::Swarm;

pub use auth::{Identity as BrainIdentity, OwnerAuth, Principal, identity_fingerprint};
pub use compose::{durable as compose_durable, memory as compose_memory};
pub use config::{Cli, Paths, Resolved};
pub use error::{ApiError, ApiResult};
pub use gateway::{Connected, NodeGateway, RecordedDecision};
pub use server::{ServeOptions, app, bind, internal_app, serve};
pub use state::AppState;
pub use telemetry::{SecretRegistry, init_tracing};
pub use tls::{TlsListener, TlsPaths};

use crate::config::Resolved as ResolvedConfig;

/// A composed, booted brain and the credentials around it.
///
/// Split from [`run`] so a test can build exactly what the daemon builds — a real
/// swarm on a real store — and then decide for itself whether to bind a socket.
pub struct Brain {
    pub swarm: Arc<Swarm>,
    pub state: AppState,
    pub resolved: Arc<ResolvedConfig>,
    /// The one-time login URL, when this boot minted the owner token. `None`
    /// afterwards: a URL printed on every start would not be one-time.
    pub login_url: Option<String>,
}

impl Brain {
    /// A brain on disk: SQLite, the JSONL transcript mirror and the audit log.
    pub fn durable(
        resolved: ResolvedConfig,
        secrets: &BTreeMap<String, String>,
        registry: Arc<SecretRegistry>,
    ) -> anyhow::Result<Self> {
        Self::build(resolved, secrets, registry, true)
    }

    /// A brain in memory: tests, and `--offline` demos that must leave no file.
    pub fn memory(
        resolved: ResolvedConfig,
        secrets: &BTreeMap<String, String>,
        registry: Arc<SecretRegistry>,
    ) -> anyhow::Result<Self> {
        Self::build(resolved, secrets, registry, false)
    }

    fn build(
        resolved: ResolvedConfig,
        secrets: &BTreeMap<String, String>,
        registry: Arc<SecretRegistry>,
        on_disk: bool,
    ) -> anyhow::Result<Self> {
        // A fresh brain adopts the config's settings *before* composition, because
        // the safety ceilings are read once, while the tool policy engine is
        // built. Writing them afterwards would change what the runners do but not
        // what the tools allow — and a ceiling that half applies is worse than one
        // that is ignored.
        if on_disk && resolved.fresh {
            seed_settings(&resolved)?;
        }

        // The origin every node signature is bound to (§2.3). Normalized once,
        // here, because three things have to agree on it: the identity file, the
        // `brainOrigin` a node pins at pairing, and the string an upgrade
        // signature is verified against. A brain behind a reverse proxy is
        // reached at one address and signs as another, and the pinned one wins.
        let origin = auth::brain_origin(&resolved.public_origin())?;
        let gateway = NodeGateway::new(origin);

        info!(
            step = "compose",
            data_dir = %resolved.data_dir.display(),
            on_disk,
            "opening the store and wiring the swarm"
        );
        let composed = if on_disk {
            compose::durable(
                resolved.config.clone(),
                &resolved.data_dir,
                resolved.roots.clone(),
                secrets,
                Arc::clone(&gateway),
            )
        } else {
            compose::memory(
                resolved.config.clone(),
                &resolved.data_dir,
                resolved.roots.clone(),
                secrets,
                Arc::clone(&gateway),
            )
        };
        let swarm = composed.map_err(|err| anyhow::anyhow!("{err}"))?;
        Self::finish(swarm, resolved, secrets, registry, on_disk, gateway)
    }

    /// Boot, start the ticker, load the credentials. Shared by both stores so the
    /// order — and therefore the log — is the same.
    fn finish(
        swarm: Arc<Swarm>,
        resolved: ResolvedConfig,
        secrets: &BTreeMap<String, String>,
        registry: Arc<SecretRegistry>,
        on_disk: bool,
        gateway: Arc<NodeGateway>,
    ) -> anyhow::Result<Self> {
        let resolved = Arc::new(resolved);

        info!(step = "boot", "recovering what a restart cut off");
        let report = swarm.boot().map_err(|err| anyhow::anyhow!("{err}"))?;
        if report.is_empty() {
            debug!("nothing to recover");
        } else {
            info!(summary = %report.summary(), "boot recovery");
        }

        // Settings: a fresh brain takes the config's, an existing one keeps what
        // its owner last set through the API — a restart must not silently undo
        // a ceiling somebody tightened at runtime.
        if resolved.fresh {
            let configured = resolved.config.settings.clone();
            let stored = swarm.settings.get();
            if configured != stored {
                swarm
                    .settings
                    .update(|slot| *slot = configured.clone())
                    .map_err(|err| anyhow::anyhow!("{err}"))?;
                info!(
                    step = "settings",
                    "a fresh brain adopted the configured settings"
                );
            }
        } else if resolved.config.settings != swarm.settings.get() {
            debug!(
                "the config's [settings] differ from the stored ones; the store wins, because \
                 it is what the owner last set"
            );
        }

        let bots = swarm.bots.list().map_err(|err| anyhow::anyhow!("{err}"))?;
        info!(
            step = "team",
            bots = bots.len(),
            runners = swarm.runners.len(),
            conversations = swarm.conversations.list().map(|c| c.len()).unwrap_or(0),
            "the roster is ready"
        );

        if resolved.expiry_ticker {
            swarm.start_expiry_ticker();
            info!(step = "ticker", "unattended approval requests will expire");
            // Routines and housekeeping ride the same switch: `--no-ticker` is
            // the operator saying "nothing in this brain wakes itself up", and
            // a scheduled routine that fires anyway would be exactly the
            // surprise that flag exists to prevent.
            swarm.start_routine_ticker();
            swarm.start_housekeeping_ticker();
            info!(
                step = "ticker",
                "routines fire on schedule; the live-event log is bounded"
            );
        } else {
            warn!(
                step = "ticker",
                "--no-ticker: unattended approval requests will NOT expire, \
                 scheduled routines will NOT fire, and the live-event log will grow unbounded"
            );
        }

        // Credentials. Every one of them is registered with the log scrubber
        // before the first listener opens, so there is no window in which a key
        // could be logged in the clear.
        for (name, key) in secrets {
            registry.register(key.clone());
            debug!(key = %name, "an API key is being watched by the log scrubber");
        }
        // The login URL and the CORS/Origin allow-list use the *dialable* origin,
        // which is not necessarily the one nodes sign over (a proxy, or a
        // wildcard bind): one is for a human's browser, the other for a machine's
        // signature.
        let origin = resolved.public_origin();
        let (auth, minted) = if on_disk {
            info!(step = "token", path = %resolved.paths.owner_token().display(), "loading the owner token");
            OwnerAuth::load_or_create(&resolved.paths, &origin, &resolved.config.public_addr)?
        } else {
            (OwnerAuth::ephemeral(auth::random_token()), false)
        };
        // Sessions minted later register themselves in `POST /api/session`; this
        // one call covers the credential that exists from now on.
        registry.register(auth.token().to_string());
        // The identity is bound to the *gateway* origin: that is the string a node
        // pins at pairing and the string `node.hello` is signed over.
        let node_origin = gateway.origin().to_string();
        let identity = if on_disk {
            auth::load_or_create_identity(&resolved.paths, &node_origin)?
        } else {
            // A memory brain has no data dir to keep a key in, so it generates one
            // and drops it when the process ends.
            auth::Identity::generate(&node_origin)
                .map_err(|err| anyhow::anyhow!("cannot create a brain identity: {err}"))?
        };
        info!(
            step = "identity",
            fingerprint = %auth::identity_fingerprint(&identity),
            origin = %node_origin,
            "the brain's node identity is ready; it signs node.hello for every dial"
        );

        let login_url = minted.then(|| auth.login_url(&origin));
        let state = AppState::new(
            Arc::clone(&swarm),
            Arc::clone(&auth),
            Arc::new(identity),
            Arc::clone(&resolved),
            registry,
            gateway,
        );
        Ok(Self {
            swarm,
            state,
            resolved,
            login_url,
        })
    }

    /// Bind both listeners, serve until asked to stop, then shut the swarm down.
    ///
    /// The swarm is stopped even when a listener failed: leaving runners alive
    /// because a port was busy would be a worse outcome than the error itself.
    pub async fn serve(self, options: ServeOptions) -> anyhow::Result<()> {
        let public = bind(&self.resolved.config.public_addr).await?;
        let bound = public.local_addr()?;
        if self.login_url.is_some() && bound.port() != configured_port(&self.resolved) {
            // The token was minted before the bind, so the URL that was printed
            // carries the configured port. Saying it twice is cheaper than an
            // owner opening a dead address.
            warn!(
                listener = %bound,
                "the port differs from the configured one; the login URL below is the live one"
            );
        }
        let internal = match self.resolved.config.internal_addr.as_deref() {
            Some(addr) if !addr.trim().is_empty() => Some(bind(addr).await?),
            _ => None,
        };
        // TLS is decided by the resolved config, not by a separate entry point: a
        // brain either serves `https://`/`wss://` for every client or plaintext,
        // and which one it is should not depend on which function the caller
        // picked.
        let outcome = match self.resolved.config.tls.as_ref() {
            Some(tls) => {
                let paths = crate::tls::TlsPaths {
                    cert: tls.cert.clone(),
                    key: tls.key.clone(),
                };
                // A bad certificate must fail *before* the swarm is told it is
                // serving; `serve_tls` loads it first and returns the typed error
                // naming the file.
                server::serve_tls(self.state.clone(), public, paths, internal, options).await
            }
            None => server::serve(self.state.clone(), public, internal, options).await,
        };
        info!(
            step = "shutdown",
            "stopping the swarm: turns finish, nothing is left running"
        );
        self.swarm.shutdown().await;
        outcome
    }
}

/// Write the config's settings into a store that has never held any.
fn seed_settings(resolved: &ResolvedConfig) -> anyhow::Result<()> {
    let store = swarmbot_store::SqliteStore::open(&resolved.paths.database()).map_err(|err| {
        anyhow::anyhow!("cannot open {}: {err}", resolved.paths.database().display())
    })?;
    store
        .put_settings(&resolved.config.settings)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    info!(
        step = "settings",
        max_hops = resolved.config.settings.max_hops,
        "a fresh brain's settings were written before composition"
    );
    Ok(())
}

/// The port the config asked for, or 0 when it did not name one.
fn configured_port(resolved: &ResolvedConfig) -> u16 {
    resolved
        .config
        .public_addr
        .rsplit_once(':')
        .and_then(|(_, port)| port.trim_end_matches(']').parse().ok())
        .unwrap_or(0)
}

/// Run the daemon: parse nothing (the caller did), build everything, serve.
///
/// This is what `main` calls, and the whole boot sequence in the module docs
/// happens here in order.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let registry = SecretRegistry::new();
    init_tracing(Arc::clone(&registry));
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "swarmbot-brain starting"
    );

    // 1–2. The data dir, then the configuration that lives in it.
    let resolved = ResolvedConfig::load(&cli)?;
    if cli.print_config {
        // stdout, not the log: this is the operator's answer, and the log has its
        // own formatting.
        println!(
            "{}",
            serde_json::to_string_pretty(&resolved.redacted())
                .map_err(|err| anyhow::anyhow!("cannot render the configuration: {err}"))?
        );
        return Ok(());
    }

    // 3. Secrets, before anything that could ask for a key.
    let secrets = config::load_secrets(&resolved.paths, &resolved.config)?;
    for key in secrets.values() {
        registry.register(key.clone());
    }

    if cli.check {
        resolved
            .config
            .validate()
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        auth::check_store(&resolved.paths)?;
        println!(
            "configuration is valid and {} opens; {} backend(s), {} extra root(s)",
            resolved.paths.database().display(),
            resolved.config.backends.len(),
            resolved.roots.len()
        );
        return Ok(());
    }

    // 4–6. Compose, boot, ticker.
    let brain = Brain::durable(resolved, &secrets, Arc::clone(&registry))?;

    // 7. The one-time login URL. Printed to stdout rather than through tracing on
    // purpose: the scrubber would (correctly) redact the token it contains, and
    // this is the one place the token is meant to be shown.
    if let Some(url) = &brain.login_url {
        println!();
        println!("  SwarmBot is running. Open this URL once to log in:");
        println!();
        println!("    {url}");
        println!();
        println!(
            "  The owner token lives in {} (mode 0600) and is not printed again.",
            brain.resolved.paths.owner_token().display()
        );
        println!();
    }

    // 8–9. Bind, then serve until stopped.
    brain.serve(ServeOptions::default()).await?;
    info!("swarmbot-brain stopped");
    Ok(())
}

/// Parse the command line. Split out so `main` stays three lines and the tests can
/// build a [`Cli`] the same way the binary does.
pub fn cli() -> Cli {
    Cli::parse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;
    use swarmbot_contracts::config::BrainConfig;

    fn resolved(dir: &std::path::Path, offline: bool) -> ResolvedConfig {
        ResolvedConfig {
            config: BrainConfig {
                team: Some(BrainConfig::demo_team("fake")),
                public_addr: "127.0.0.1:0".into(),
                internal_addr: None,
                ..BrainConfig::default()
            },
            data_dir: dir.to_path_buf(),
            paths: Paths::new(dir),
            config_path: None,
            team_path: None,
            roots: Vec::new(),
            expiry_ticker: false,
            offline,
            fresh: true,
        }
    }

    #[tokio::test]
    async fn a_memory_brain_boots_the_demo_team_and_needs_no_token_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let brain = Brain::memory(
            resolved(dir.path(), true),
            &BTreeMap::new(),
            SecretRegistry::new(),
        )
        .expect("a brain");
        assert!(brain.login_url.is_none(), "no data dir, no login URL");
        assert!(!brain.state.owner_token().is_empty());
        assert_eq!(brain.swarm.bots.list().expect("bots").len(), 4);
        assert_eq!(brain.swarm.runners.len(), 4, "every bot has a runner");
        assert_eq!(brain.state.epoch(), 1);
        // No test may leave a runner alive.
        brain.swarm.shutdown().await;
    }

    #[tokio::test]
    async fn a_durable_brain_mints_a_token_once_and_prints_the_url_once() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let secrets = BTreeMap::new();
        let first = Brain::durable(resolved(dir.path(), true), &secrets, SecretRegistry::new())
            .expect("a brain");
        let url = first.login_url.clone().expect("a first-boot login URL");
        assert!(url.contains(first.state.owner_token()));
        assert!(url.contains("#token="), "{url}");
        assert!(config::is_owner_only(&first.resolved.paths.owner_token()));
        first.swarm.shutdown().await;

        // The same data dir again: the token is reused, and the URL is not
        // printed a second time — that is what makes it one-time.
        let second = Brain::durable(resolved(dir.path(), true), &secrets, SecretRegistry::new())
            .expect("a brain");
        assert!(second.login_url.is_none());
        assert_eq!(second.state.owner_token(), first.state.owner_token());
        assert!(config::is_owner_only(
            &second.resolved.paths.brain_identity()
        ));
        second.swarm.shutdown().await;
    }

    #[tokio::test]
    async fn a_fresh_durable_brain_adopts_the_configured_settings() {
        use swarmbot_contracts::node::{PolicyCeiling, Surface};
        use swarmbot_contracts::{BotId, ConversationId, Invocation, ToolCallId, ToolCallRequest};
        use swarmbot_tools::{ApprovalGate, ToolContext};

        let dir = tempfile::tempdir().expect("a temp dir");
        let mut resolved = resolved(dir.path(), true);
        resolved.config.settings.max_hops = 2;
        resolved.config.settings.bot_messaging = false;
        resolved
            .config
            .settings
            .safety
            .set(Surface::Shell, PolicyCeiling::Never);
        // Even without a config file on disk, pre-composition seeding ensures
        // both `SettingsHandle` and `PolicyEngine` start from configured values.
        assert!(resolved.config_path.is_none());

        let brain =
            Brain::durable(resolved, &BTreeMap::new(), SecretRegistry::new()).expect("a brain");
        assert_eq!(brain.swarm.settings.get().max_hops, 2);
        assert!(!brain.swarm.settings.get().bot_messaging);
        assert_eq!(
            brain.swarm.store.settings().expect("settings").max_hops,
            2,
            "the store holds them before composition reads them"
        );

        let workspace = dir.path().join("workspaces/lead");
        let ctx = ToolContext::local(
            Invocation::new(BotId::from_suffix("lead"), ConversationId::from_suffix("c")),
            workspace.clone(),
            Arc::clone(&brain.swarm.gate) as Arc<dyn ApprovalGate>,
            Arc::clone(&brain.swarm.nodes),
            workspace.join(".spool"),
        );
        let result = brain
            .swarm
            .tools
            .dispatch(
                &ctx,
                ToolCallRequest {
                    id: ToolCallId::new(),
                    name: "shell".into(),
                    args: serde_json::json!({"command": "echo should-not-run"}),
                },
            )
            .await;
        assert!(result.is_error, "shell must respect PolicyCeiling::Never");
        assert!(
            brain
                .swarm
                .store
                .pending_requests()
                .expect("requests")
                .is_empty(),
            "PolicyCeiling::Never refuses before queuing approval"
        );
        brain.swarm.shutdown().await;
    }

    #[tokio::test]
    async fn a_memory_brain_adopts_the_configured_settings_and_ceilings() {
        use swarmbot_contracts::node::{PolicyCeiling, Surface};
        use swarmbot_contracts::{BotId, ConversationId, Invocation, ToolCallId, ToolCallRequest};
        use swarmbot_tools::{ApprovalGate, ToolContext};

        let dir = tempfile::tempdir().expect("a temp dir");
        let mut resolved = resolved(dir.path(), true);
        resolved.config.settings.max_hops = 3;
        resolved.config.settings.bot_messaging = false;
        resolved
            .config
            .settings
            .safety
            .set(Surface::Shell, PolicyCeiling::Never);

        let brain =
            Brain::memory(resolved, &BTreeMap::new(), SecretRegistry::new()).expect("a brain");
        let settings = brain.swarm.settings.get();
        assert_eq!(settings.max_hops, 3);
        assert!(!settings.bot_messaging);
        assert_eq!(brain.swarm.store.settings().expect("settings"), settings);

        let workspace = dir.path().join("workspaces/lead");
        let ctx = ToolContext::local(
            Invocation::new(BotId::from_suffix("lead"), ConversationId::from_suffix("c")),
            workspace.clone(),
            Arc::clone(&brain.swarm.gate) as Arc<dyn ApprovalGate>,
            Arc::clone(&brain.swarm.nodes),
            workspace.join(".spool"),
        );
        let result = brain
            .swarm
            .tools
            .dispatch(
                &ctx,
                ToolCallRequest {
                    id: ToolCallId::new(),
                    name: "shell".into(),
                    args: serde_json::json!({"command": "echo should-not-run"}),
                },
            )
            .await;
        assert!(result.is_error, "shell must respect PolicyCeiling::Never");
        assert!(
            brain
                .swarm
                .store
                .pending_requests()
                .expect("requests")
                .is_empty()
        );
        brain.swarm.shutdown().await;
    }

    #[tokio::test]
    async fn an_existing_brain_keeps_the_settings_its_owner_set() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut resolved = resolved(dir.path(), true);
        resolved.config.settings.max_hops = 2;
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "# written by the test\n").expect("write");
        resolved.config_path = Some(config_path);
        {
            let brain = Brain::durable(resolved.clone(), &BTreeMap::new(), SecretRegistry::new())
                .expect("a brain");
            brain
                .swarm
                .settings
                .update(|slot| slot.max_hops = 5)
                .expect("updated");
            brain.swarm.shutdown().await;
        }
        // A restart is not a rollback of a runtime change.
        let mut reopened = resolved;
        reopened.fresh = false;
        let brain =
            Brain::durable(reopened, &BTreeMap::new(), SecretRegistry::new()).expect("a brain");
        assert_eq!(brain.swarm.settings.get().max_hops, 5);
        brain.swarm.shutdown().await;
    }

    #[test]
    fn a_configured_port_is_read_back_out_of_the_address() {
        let dir = std::path::PathBuf::from(".");
        let mut resolved = resolved(&dir, true);
        resolved.config.public_addr = "127.0.0.1:8787".into();
        assert_eq!(configured_port(&resolved), 8787);
        resolved.config.public_addr = "[::1]:9000".into();
        assert_eq!(configured_port(&resolved), 9000);
        resolved.config.public_addr = "127.0.0.1:0".into();
        assert_eq!(configured_port(&resolved), 0);
    }

    #[test]
    fn the_cli_parses_its_own_help_and_version() {
        // Not a smoke test of clap: `--help` is what the report quotes, and a
        // rename that breaks it would otherwise only show up in a terminal.
        let help = Cli::command().render_help().to_string();
        for flag in [
            "--config",
            "--data-dir",
            "--addr",
            "--port",
            "--internal-addr",
            "--public-origin",
            "--team",
            "--backend",
            "--root",
            "--no-ticker",
            "--offline",
            "--print-config",
            "--check",
        ] {
            assert!(help.contains(flag), "the help is missing {flag}:\n{help}");
        }
    }
}
