//! What every handler is handed.
//!
//! One `AppState`, cloned by value into each request (a bag of `Arc`s and two
//! timestamps), so a handler never has to know where a service came from. The
//! swarm is the whole orchestration; everything else is the daemon around it —
//! credentials, the resolved configuration, the moment the process started, and
//! the one signal `POST /api/shutdown` and SIGINT share.

use std::net::SocketAddr;
use std::sync::{Arc, RwLock, Weak};
use std::time::Instant;

use chrono::{DateTime, Utc};
use tokio::sync::watch;

use swarmbot_contracts::{DomainError, Invocation, NodeId};
use swarmbot_core::{McpTurnProvider, McpTurnRegistration, McpTurnRequest, Swarm};
use swarmbot_llm::McpServerSpec;
use swarmbot_mcp::{SwarmMcpServer, ToolFilter, TurnMcpRegistry};
use swarmbot_tools::{ApprovalGate, ToolContext};

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
    /// Per-turn MCP HTTP services mounted at `/mcp/t/{token}` on the internal listener.
    pub turn_mcp: Arc<TurnMcpRegistry>,
    /// Bound internal listener address; `None` when disabled or not yet listening.
    internal_addr: Arc<RwLock<Option<SocketAddr>>>,
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
        let turn_mcp = Arc::new(TurnMcpRegistry::new());
        let internal_addr = Arc::new(RwLock::new(None));
        swarm.set_mcp_provider(Arc::new(BrainMcpProvider {
            swarm: Arc::downgrade(&swarm),
            turns: Arc::clone(&turn_mcp),
            internal_addr: Arc::clone(&internal_addr),
        }));
        Self {
            swarm,
            auth,
            identity,
            config,
            secrets,
            gateway,
            turn_mcp,
            internal_addr,
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

    /// Record the internal listener's bound address so per-turn MCP specs can
    /// advertise reachable `http://{addr}/mcp/t/{token}` endpoints.
    pub fn set_internal_addr(&self, addr: SocketAddr) {
        *self
            .internal_addr
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(addr);
    }

    /// The bound internal listener address, or `None` if disabled or unbound.
    pub fn internal_addr(&self) -> Option<SocketAddr> {
        *self.internal_addr.read().unwrap_or_else(|e| e.into_inner())
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

/// Registers turn-scoped [`SwarmMcpServer`] instances into [`TurnMcpRegistry`]
/// when the internal HTTP gateway is listening.
///
/// Holds a [`Weak`] back-reference to [`Swarm`] rather than an [`Arc`] because
/// [`Swarm`] owns this provider via `mcp_provider`, avoiding a reference cycle.
struct BrainMcpProvider {
    swarm: Weak<Swarm>,
    turns: Arc<TurnMcpRegistry>,
    internal_addr: Arc<RwLock<Option<SocketAddr>>>,
}

impl McpTurnProvider for BrainMcpProvider {
    fn register_turn(&self, req: McpTurnRequest) -> Result<McpTurnRegistration, DomainError> {
        let Some(addr) = *self.internal_addr.read().unwrap_or_else(|e| e.into_inner()) else {
            return Ok(McpTurnRegistration::empty());
        };
        let Some(swarm) = self.swarm.upgrade() else {
            return Ok(McpTurnRegistration::empty());
        };

        let host_node = swarm
            .store
            .get_bot(&req.bot_id)?
            .map(|bot| bot.host_node())
            .unwrap_or_else(NodeId::local);
        let mut invocation = Invocation::new(req.bot_id, req.conversation_id);
        invocation.turn_id = Some(req.turn_id);
        invocation.task_id = req.task_id;
        invocation.node_id = Some(host_node.clone());

        let mut ctx = ToolContext::local(
            invocation,
            req.workspace,
            Arc::clone(&swarm.gate) as Arc<dyn ApprovalGate>,
            Arc::clone(&swarm.nodes),
            req.spool_dir,
        );
        if !host_node.is_local() {
            ctx = ctx.with_node_id(host_node);
        }

        let server = SwarmMcpServer::new(Arc::clone(&swarm.tools), ctx).with_filter(ToolFilter {
            toolsets: Some(req.toolsets),
            node_caps: req.node_caps,
            in_task: req.in_task,
        });
        let handle = self.turns.register_handle(server);
        let url = format!("http://{addr}/mcp/t/{}", handle.token());
        let specs = vec![McpServerSpec::http("swarmbot", url)];
        Ok(McpTurnRegistration::new(specs, move || {
            let _ = handle.unregister();
        }))
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

    #[tokio::test]
    async fn mcp_provider_requires_bound_internal_addr_and_cleans_up_on_drop() {
        use swarmbot_contracts::{BotId, ConversationId, TurnId};

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

        let provider = state
            .swarm
            .runner_env()
            .mcp_provider
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("AppState wires McpTurnProvider onto Swarm");

        let req = McpTurnRequest {
            bot_id: BotId::from_suffix("lead"),
            conversation_id: ConversationId::from_suffix("conv"),
            turn_id: TurnId::new(),
            task_id: None,
            workspace: dir.path().to_path_buf(),
            spool_dir: dir.path().join(".spool"),
            toolsets: vec![swarmbot_contracts::Toolset::Bot],
            node_caps: None,
            in_task: false,
        };

        let unbound = provider
            .register_turn(req.clone())
            .expect("unbound registration");
        assert!(
            unbound.servers.is_empty(),
            "without a bound internal listener, no MCP server URL can be advertised"
        );
        assert_eq!(state.turn_mcp.len(), 0);

        let bound_addr: SocketAddr = "127.0.0.1:43210".parse().expect("valid addr");
        state.set_internal_addr(bound_addr);
        assert_eq!(state.internal_addr(), Some(bound_addr));

        let reg = provider.register_turn(req).expect("bound registration");
        assert_eq!(reg.servers.len(), 1);
        let url = match &reg.servers[0] {
            McpServerSpec::Http { name, url, .. } => {
                assert_eq!(name, "swarmbot");
                url.clone()
            }
            other => panic!("expected http MCP spec, got {other:?}"),
        };
        let token = url
            .strip_prefix("http://127.0.0.1:43210/mcp/t/")
            .expect("url includes /mcp/t/{token}");
        assert!(state.turn_mcp.get_service(token).is_some());

        drop(reg);
        assert!(
            state.turn_mcp.get_service(token).is_none(),
            "dropping McpTurnRegistration must unregister the turn endpoint"
        );
        swarm.shutdown().await;
    }
}
