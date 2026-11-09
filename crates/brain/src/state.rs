//! What every handler is handed.
//!
//! One `AppState`, cloned by value into each request (it is six `Arc`s and two
//! timestamps), so a handler never has to know where a service came from. The
//! swarm is the whole orchestration; everything else is the daemon around it —
//! credentials, the resolved configuration, the moment the process started, and
//! the one signal `POST /api/shutdown` and SIGINT share.

use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use tokio::sync::watch;

use swarmbot_core::Swarm;

use crate::auth::{Identity as BrainIdentity, OwnerAuth};
use crate::config::Resolved;
use crate::gateway::NodeGateway;
use crate::telemetry::SecretRegistry;

/// The daemon's shared state.
#[derive(Clone)]
pub struct AppState {
    /// The wired orchestration: services, store, hub, runners.
    pub swarm: Arc<Swarm>,
    /// Owner token, sessions and the WebSocket Origin allow-list.
    pub auth: Arc<OwnerAuth>,
    /// The brain's Ed25519 identity, which signs `node.hello` (M6).
    pub identity: Arc<BrainIdentity>,
    /// The resolved configuration, for `/api/health` and the login URL.
    pub config: Arc<Resolved>,
    /// The credentials the log scrubber watches; a session minted at runtime is
    /// registered here so it can never be written to a log line.
    pub secrets: Arc<SecretRegistry>,
    /// The node gateway: which nodes are connected, the origin their signatures
    /// are bound to, and the nonce replay cache.
    pub gateway: Arc<NodeGateway>,
    /// When this process started, for the log and for `uptime_secs`.
    pub started_at: DateTime<Utc>,
    /// Monotonic, because a wall clock can be stepped by NTP.
    started: Instant,
    /// Set once by `POST /api/shutdown`; the listener watches the other end.
    shutdown: watch::Sender<bool>,
    shutdown_state: watch::Receiver<bool>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        swarm: Arc<Swarm>,
        auth: Arc<OwnerAuth>,
        identity: Arc<BrainIdentity>,
        config: Arc<Resolved>,
        secrets: Arc<SecretRegistry>,
        gateway: Arc<NodeGateway>,
    ) -> Self {
        let (shutdown, shutdown_state) = watch::channel(false);
        Self {
            swarm,
            auth,
            identity,
            config,
            secrets,
            gateway,
            started_at: Utc::now(),
            started: Instant::now(),
            shutdown,
            shutdown_state,
        }
    }

    /// A state for a swarm with no data dir behind it: tests and `--offline`.
    ///
    /// The token and the identity are random per call, so a test never
    /// accidentally shares a credential with another test. Fallible because both
    /// come from the operating system's RNG, and a brain that cannot get entropy
    /// must not invent a credential.
    pub fn ephemeral(
        swarm: Arc<Swarm>,
        config: Arc<Resolved>,
        gateway: Arc<NodeGateway>,
    ) -> anyhow::Result<Self> {
        let identity = BrainIdentity::generate(gateway.origin().to_string())
            .map_err(|err| anyhow::anyhow!("cannot create a brain identity: {err}"))?;
        Ok(Self::new(
            swarm,
            OwnerAuth::ephemeral(crate::auth::random_token()),
            Arc::new(identity),
            config,
            SecretRegistry::new(),
            gateway,
        ))
    }

    /// The owner token this state authenticates with.
    ///
    /// Exists so a test can log in the way an owner does; the API itself never
    /// hands a token out except through `POST /api/session`'s cookie.
    pub fn owner_token(&self) -> &str {
        self.auth.token()
    }

    pub fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// The live event log's epoch. A client whose cursor carries a different
    /// epoch must resync rather than resume, which is why `/api/health` reports
    /// it: it is the cheap "did the brain restart under me?" check.
    pub fn epoch(&self) -> u64 {
        self.swarm.hub.cursor().epoch
    }

    /// Ask the listeners to stop accepting and drain.
    ///
    /// Returns false only when the server task is already gone, which is not an
    /// error: a second `POST /api/shutdown` racing the first must still answer.
    pub fn request_shutdown(&self) -> bool {
        self.shutdown.send(true).is_ok()
    }

    pub fn shutdown_requested(&self) -> bool {
        *self.shutdown_state.borrow()
    }

    /// Resolves once [`Self::request_shutdown`] has been called.
    pub async fn wait_for_shutdown(&self) {
        let mut state = self.shutdown_state.clone();
        while !*state.borrow_and_update() {
            if state.changed().await.is_err() {
                // The sender is gone, so nobody can ever ask: stop waiting
                // rather than parking the listener task forever.
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use swarmbot_contracts::config::BrainConfig;
    use swarmbot_core::compose;

    fn config() -> Arc<Resolved> {
        Arc::new(Resolved {
            config: BrainConfig::default(),
            data_dir: std::path::PathBuf::from("data"),
            paths: crate::config::Paths::new("data"),
            config_path: None,
            team_path: None,
            roots: Vec::new(),
            expiry_ticker: false,
            offline: true,
            fresh: true,
        })
    }

    #[tokio::test]
    async fn a_shutdown_request_reaches_everyone_waiting_on_it() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let swarm = compose::memory(
            BrainConfig {
                team: Some(BrainConfig::demo_team("fake")),
                ..BrainConfig::default()
            },
            dir.path(),
            vec![],
            &BTreeMap::new(),
        )
        .expect("a swarm");
        swarm.boot().expect("boots");
        let state = AppState::ephemeral(
            Arc::clone(&swarm),
            config(),
            crate::gateway::NodeGateway::new("http://127.0.0.1:0"),
        )
        .expect("a state");

        assert!(!state.shutdown_requested());
        assert!(state.version() == env!("CARGO_PKG_VERSION"));
        assert_eq!(
            state.epoch(),
            1,
            "a memory store's event log starts at epoch 1"
        );

        let waiter = state.clone();
        let handle = tokio::spawn(async move { waiter.wait_for_shutdown().await });
        // Let the waiter park on the watch channel before the request arrives.
        tokio::task::yield_now().await;
        assert!(state.request_shutdown());
        assert!(state.shutdown_requested());
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("the waiter wakes")
            .expect("and did not panic");

        // A second request is harmless.
        assert!(state.request_shutdown());
        swarm.shutdown().await;
    }

    #[tokio::test]
    async fn a_request_that_arrived_before_the_wait_is_not_missed() {
        // The listeners register their graceful-shutdown future after the routes
        // are already serving, so a `POST /api/shutdown` that lands first must
        // still stop them. `watch` is what makes that safe; this is the test that
        // would fail with a `Notify`-style "signal once" primitive.
        let dir = tempfile::tempdir().expect("a temp dir");
        let swarm = compose::memory(BrainConfig::default(), dir.path(), vec![], &BTreeMap::new())
            .expect("a swarm");
        let state = AppState::ephemeral(
            swarm.clone(),
            config(),
            crate::gateway::NodeGateway::new("http://127.0.0.1:0"),
        )
        .expect("a state");
        assert!(state.request_shutdown());
        tokio::time::timeout(std::time::Duration::from_secs(5), state.wait_for_shutdown())
            .await
            .expect("an earlier request is still seen by a later waiter");
        // And every later waiter sees it too, which is why both listeners stop.
        tokio::time::timeout(std::time::Duration::from_secs(5), state.wait_for_shutdown())
            .await
            .expect("the flag is sticky");
        swarm.shutdown().await;
    }
}
