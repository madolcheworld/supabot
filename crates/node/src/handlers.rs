//! The node's RPC surface: the `node.hello` gate, method routing, the
//! payload → [`NodeCall`] bridge, error translation, approval round-trips and
//! the audit line.
//!
//! # The three rules this module exists to enforce
//!
//! 1. **Nothing runs before the brain proves itself.** [`NodeHandler`] answers
//!    every method except `node.hello` with `NotAuthenticated` until
//!    `brainSig` verified against the key *pinned at pairing* (decision D1.3).
//!    The mux enforces this too when built with `require_hello: true`; the
//!    check here is the second lock, so an embedder that forgets the flag does
//!    not get an unauthenticated node.
//! 2. **The node decides finally.** The ceiling comes from this node's own
//!    [`PolicyMap`](swarmbot_contracts::PolicyMap) and the path boundary from
//!    its own [`PathPolicy`]. A brain request can lower either, never raise
//!    one, and when the ceiling says `ask` and the brain is unreachable,
//!    refuses, or answers with something that is not an
//!    [`ApprovalAnswer`](swarmbot_node_protocol::rpc::ApprovalAnswer), the answer is
//!    **deny** (decision D1.4).
//! 3. **A capability this build lacks is a typed refusal.** Every method in
//!    [`swarmbot_node_protocol::rpc::UNIMPLEMENTED_METHODS`] answers
//!    [`swarmbot_node_protocol::rpc::unimplemented`], and a name nobody ever defined
//!    answers `PolicyRefused { reason: "unknown method" }`. Never a silent
//!    success.
//!
//! # The `rpc` ↔ `NodeCall` bridge
//!
//! The wire payloads and [`NodeCall`] are two shapes of the same intent and are
//! **not** field-for-field identical (protocol doc §5.1). [`to_node_call`] is
//! the one place that bridges them; every difference has a comment there. Where
//! the wire can express something `NodeCall` cannot (`fs.edit`'s
//! `expectedSha256`, `fs.write`'s base64 body), the call is **refused** rather
//! than silently downgraded: dropping a compare-and-set would turn a safety
//! check into a clobber.
//!
//! # Result shape
//!
//! A successful `fs.*` result is the serialized
//! [`ToolResult`](swarmbot_contracts::ToolResult) the
//! [`LocalExecutor`](swarmbot_tools::LocalExecutor) produced —
//! `{content:[{type:"text",text:"…"}], isError, meta}`. That is deliberate and
//! it is a **deviation from the protocol doc's §5 result column**, which names
//! `rpc::FsReadResult` & co.: those structs cannot be built from a
//! `ToolResult` without re-implementing the filesystem handlers here, and two
//! implementations of `fs.read` is exactly the drift this crate exists to
//! avoid. Returning the `ToolResult` means the brain's remote `NodeExecutor`
//! deserializes the value straight into the type its own port returns.
//! `fs.stat` of a missing path is the documented `null`, and `shell.exec` ends
//! with `null` after its chunks.

use std::path::Path;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use serde_json::Value;
use swarmbot_contracts::{
    Decision as ApprovalDecision, Invocation, NodeRoot, PolicyMap, ProtocolRange, Surface,
    ToolError, ToolResult,
};
use swarmbot_tools::handler::NodeCall;
use swarmbot_tools::handlers::{
    TOOL_EDIT_FILE, TOOL_GLOB, TOOL_GREP, TOOL_LIST_DIR, TOOL_READ_FILE, TOOL_SHELL,
    TOOL_WRITE_FILE,
};
use swarmbot_tools::{
    AuditDecision, AuditEntry, AuditLog, AuditOutcome, LocalExecutor, NodeExecutor as _,
    PathPolicy, PolicyEngine, destructive_label,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use swarmbot_node_protocol::mux::{BoxFuture, Peer, Socket};
use swarmbot_node_protocol::rpc::{
    self, ApprovalAnswer, Encoding, FsDownload, FsDownloadResult, FsEdit, FsGlob, FsGrep, FsList,
    FsMkdir, FsMove, FsRead, FsRemove, FsStat, FsWrite, GrepOutputMode, HelloParams, NodeHello,
    RequestApproval, ShellExec, decode_params,
};
use swarmbot_node_protocol::wire::TypedError;
use swarmbot_node_protocol::{
    CLOSE_HELLO_FAILED, CLOSE_NO_PROTOCOL, ChunkSink, HelloPolicy, ProtocolError, RpcHandler,
    verify_hello,
};

use crate::config::{self, NodeConfig};
use crate::identity::Identity;
use crate::shell::{self, ProcRegistry, ShellFailure, ShellOptions};
use crate::{NODE_VERSION, NodeError};
use swarmbot_tools::Decision as PolicyDecision;

/// The node's way back to the brain: `node.requestApproval`, and the socket
/// close that a failed `node.hello` must trigger.
///
/// A trait rather than a concrete [`Peer`] because the peer's type parameter
/// names the transport, and the handler must work over a WebSocket in the
/// daemon and over an in-memory duplex in tests without being generic itself.
pub trait BrainLink: Send + Sync + 'static {
    /// A unary RPC to the brain (§4.3).
    fn call<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, ProtocolError>>;

    /// Close the control socket with a WebSocket close code (§3.3).
    fn close<'a>(&'a self, code: u16, reason: &'a str) -> BoxFuture<'a, ()>;
}

/// The [`BrainLink`] of a live [`Peer`].
pub struct PeerLink<S>(Peer<S>);

impl<S: Socket> PeerLink<S> {
    /// A peer as the handler's [`BrainLink`]. Named `shared` rather than `new`
    /// because it returns the trait object the handler stores, not `Self`: the
    /// handler must not be generic over the transport.
    pub fn shared(peer: Peer<S>) -> Arc<dyn BrainLink> {
        Arc::new(PeerLink(peer))
    }
}

impl<S: Socket + Send + 'static> BrainLink for PeerLink<S> {
    fn call<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, ProtocolError>> {
        Box::pin(self.0.call(method, params))
    }

    fn close<'a>(&'a self, code: u16, reason: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(self.0.close(code, reason))
    }
}

/// Everything a handler needs that outlives one connection: policy, executor,
/// audit, and the shutdown token.
///
/// Built once per daemon; a fresh [`NodeHandler`] is built per connection
/// (because the `node.hello` gate and the upgrade nonce are per connection).
pub struct NodeRuntime {
    pub config: NodeConfig,
    pub identity: Arc<Identity>,
    pub paths: config::NodePaths,
    pub policy: Arc<PathPolicy>,
    pub engine: Arc<PolicyEngine>,
    pub executor: Arc<LocalExecutor>,
    pub audit: AuditLog,
    /// Live process groups, so a shutdown kills them all.
    pub registry: ProcRegistry,
    /// Tripped by SIGINT/SIGTERM, and by [`NodeRuntime::shutdown`]. Cancelling
    /// it kills running shell groups; the method also cancels the executor's
    /// own token, which is what stops an in-flight call that is not a shell.
    pub stopping: CancellationToken,
}

impl std::fmt::Debug for NodeRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeRuntime")
            .field("config", &self.config)
            .field("identity", &self.identity)
            .field("audit", &self.audit)
            .finish_non_exhaustive()
    }
}

impl NodeRuntime {
    /// Wire one node's policy, executor and audit log. The workspace directory
    /// is created if missing: a root that does not exist would make every
    /// relative path a denial the owner cannot explain.
    pub fn new(config: NodeConfig, identity: Arc<Identity>) -> Result<Arc<NodeRuntime>, NodeError> {
        let paths = config.paths();
        let workspace = config.workspace();
        paths.ensure_dirs(&workspace)?;
        let policy = Arc::new(config.path_policy());
        let engine = Arc::new(config.policy_engine());
        let executor = Arc::new(
            LocalExecutor::new(workspace, Arc::clone(&policy))
                .with_engine(Arc::clone(&engine))
                // This daemon is its own node: a call addressed to its paired id
                // is exactly what it must run, and `nod_local` is not that id.
                .for_node(identity.node_id().clone()),
        );
        let audit = AuditLog::for_node(&paths.data_dir, identity.node_id());
        Ok(Arc::new(NodeRuntime {
            config,
            identity,
            paths,
            policy,
            engine,
            executor,
            audit,
            registry: ProcRegistry::new(),
            stopping: CancellationToken::new(),
        }))
    }

    /// Stop everything: in-flight calls see the token, and any process group
    /// still registered is killed. Called by `run` on SIGINT/SIGTERM before
    /// the socket is closed, so a stopping node leaves nothing behind.
    pub fn shutdown(&self) {
        self.executor.cancel_token().cancel();
        self.registry.kill_all();
        self.stopping.cancel();
    }

    /// The roots as this node resolved them — what `node.hello` reports.
    pub fn roots(&self) -> Vec<NodeRoot> {
        self.policy.roots()
    }

    /// The node's own ceilings, for `status` and the state file.
    pub fn ceilings(&self) -> &PolicyMap {
        &self.config.ceilings
    }
}

/// One connection's handler: the hello gate, the upgrade nonce the brain signs
/// over, and the way back to the brain.
pub struct NodeHandler {
    runtime: Arc<NodeRuntime>,
    /// The nonce from *this* connection's upgrade header. `node.hello`'s
    /// `brainSig` is over `swarmbot-brain-v1|<nodeId>|<nonce>|<brainOrigin>`,
    /// so a signature captured on another connection does not verify here.
    nonce: String,
    /// Flipped once — and only once — `brainSig` verified.
    authenticated: watch::Sender<bool>,
    /// A receiver kept alive on purpose: a `watch::Sender::send` **fails and
    /// stores nothing** once every receiver has been dropped, so a handler that
    /// held only the sender would silently never open its own gate.
    authenticated_rx: watch::Receiver<bool>,
    /// Set after the peer exists; `None` means "no brain to ask", which is a
    /// **deny**, never an allow.
    link: RwLock<Option<Arc<dyn BrainLink>>>,
    active: AtomicU32,
}

impl std::fmt::Debug for NodeHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeHandler")
            .field("node_id", &self.runtime.identity.node_id().to_string())
            // The nonce is a one-time anti-replay value, not a credential, but
            // there is no reason to put it in a log either.
            .field("nonce", &format_args!("<{} bytes>", self.nonce.len()))
            .field("authenticated", &self.is_authenticated())
            .field("link", &self.link().map(|_| "dyn BrainLink"))
            .field("active", &self.active_invocations())
            .finish()
    }
}

impl NodeHandler {
    /// A handler for one connection. `nonce` is the base64url nonce this
    /// connection's upgrade header carried; pass an empty string only in tests
    /// that never verify a hello.
    pub fn new(runtime: Arc<NodeRuntime>, nonce: impl Into<String>) -> Arc<NodeHandler> {
        let (authenticated, authenticated_rx) = watch::channel(false);
        Arc::new(NodeHandler {
            runtime,
            nonce: nonce.into(),
            authenticated,
            authenticated_rx,
            link: RwLock::new(None),
            active: AtomicU32::new(0),
        })
    }

    pub fn runtime(&self) -> &Arc<NodeRuntime> {
        &self.runtime
    }

    /// Install the way back to the brain. Called once the peer exists.
    pub fn attach(&self, link: Arc<dyn BrainLink>) {
        *self.write_link() = Some(link);
    }

    /// Drop the link, e.g. when the socket this handler belonged to died. Any
    /// approval asked after this is a deny.
    pub fn detach(&self) {
        *self.write_link() = None;
    }

    pub fn link(&self) -> Option<Arc<dyn BrainLink>> {
        self.link
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn write_link(&self) -> std::sync::RwLockWriteGuard<'_, Option<Arc<dyn BrainLink>>> {
        self.link.write().unwrap_or_else(|error| error.into_inner())
    }

    /// Has `node.hello` verified on this connection?
    pub fn is_authenticated(&self) -> bool {
        *self.authenticated_rx.borrow()
    }

    /// Resolves when the gate flips. The dial loop waits on this to send
    /// `node.connected` — the event must follow a *verified* hello, not a
    /// socket that merely opened.
    pub async fn wait_authenticated(&self) {
        let mut rx = self.authenticated_rx.clone();
        while !*rx.borrow_and_update() && rx.changed().await.is_ok() {}
    }

    /// Invocations currently running, for `node.heartbeat`.
    pub fn active_invocations(&self) -> u32 {
        self.active.load(Ordering::SeqCst)
    }

    /// The truthful hello object for a negotiated protocol version.
    pub fn hello_object(&self, protocol: u32) -> NodeHello {
        NodeHello {
            node_id: self.runtime.identity.node_id().clone(),
            version: NODE_VERSION.to_string(),
            // The real hostname, not the owner's label: `node.hello` describes
            // the machine, and the label travels in the `node.connected` event.
            hostname: config::hostname(),
            platform: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            protocol,
            roots: self.runtime.roots(),
            capabilities: self.runtime.config.capabilities(),
            // No tunnels in this build, and none are pretended.
            tunnels: Vec::new(),
            policy: HelloPolicy {
                // The strictest of the per-surface ceilings: the brain may
                // lower it further with `min`, never raise it.
                ceiling: self.runtime.config.aggregate_ceiling(),
                // The owner did not hand policy management to the brain, and
                // no code path here would accept it if they had.
                remote_managed: false,
            },
            brain_verified: true,
        }
    }

    /// Close the socket with a code, from inside a handler.
    ///
    /// Spawned, and the caller **must not await anything afterwards**: the
    /// driver writes queued frames at the top of its loop and exits on
    /// `Cmd::Close` without draining them, so the error frame explaining the
    /// close has to reach the command channel first. `tokio::spawn` does not
    /// run the new task until the current one yields, which is exactly what
    /// makes "spawn, then return immediately" the correct order.
    fn schedule_close(&self, code: u16, reason: &'static str) {
        let Some(link) = self.link() else {
            return;
        };
        tokio::spawn(async move { link.close(code, reason).await });
    }

    /// Ask the brain for a decision (§6). Every failure mode — no link, an
    /// unreachable brain, an answer that is not an `ApprovalAnswer` — is a
    /// **deny**. An approval that cannot be obtained is not an approval.
    async fn request_approval(
        &self,
        inv: &Invocation,
        surface: Surface,
        target: &str,
        reason: Option<String>,
    ) -> ApprovalDecision {
        let Some(link) = self.link() else {
            tracing::warn!(
                surface = %surface,
                target,
                "no control socket to ask for an approval on; denying (an unreachable brain is a deny, never an allow)"
            );
            return ApprovalDecision::Deny;
        };
        let ask = RequestApproval {
            invocation_id: inv.id.clone(),
            bot_id: inv.bot_id.clone(),
            action: surface.as_str().to_string(),
            target: target.to_string(),
            reason: reason.unwrap_or_else(|| format!("`{surface}` on {target}")),
        };
        let params = match serde_json::to_value(&ask) {
            Ok(params) => params,
            Err(error) => {
                tracing::error!(%error, "cannot serialize an approval request; denying");
                return ApprovalDecision::Deny;
            }
        };
        match link.call(rpc::NODE_REQUEST_APPROVAL, params).await {
            Ok(value) => match serde_json::from_value::<ApprovalAnswer>(value.clone()) {
                Ok(answer) => {
                    tracing::info!(
                        surface = %surface,
                        target,
                        decision = ?answer.decision,
                        by = %answer.by,
                        "the brain answered an approval request"
                    );
                    answer.decision
                }
                Err(error) => {
                    tracing::warn!(%error, "the brain's approval answer is not an ApprovalAnswer; denying");
                    ApprovalDecision::Deny
                }
            },
            Err(error) => {
                tracing::warn!(%error, "the brain could not be reached for an approval; denying");
                ApprovalDecision::Deny
            }
        }
    }

    /// One line in `<data>/audit/<node id>.jsonl`. `AuditLog::record` never
    /// fails the call and logs an append failure loudly, which is the whole
    /// contract (AGENTS.md).
    #[allow(clippy::too_many_arguments)]
    fn audit(
        &self,
        inv: &Invocation,
        tool: &str,
        surface: Surface,
        target: &str,
        decision: AuditDecision,
        outcome: AuditOutcome,
        started: Instant,
        result_bytes: usize,
    ) {
        let entry = AuditEntry::builder(inv, tool)
            .node(self.runtime.identity.node_id().clone())
            .surface(surface.as_str())
            .target(target)
            .decision(decision)
            .outcome(outcome)
            .duration_ms(started.elapsed().as_millis() as u64)
            .result_bytes(result_bytes)
            .build();
        self.runtime.audit.record(entry);
    }

    // -- node.hello ---------------------------------------------------------

    async fn hello(&self, params: Value) -> Result<Value, TypedError> {
        let params: HelloParams = decode_params(rpc::NODE_HELLO, params)?;
        let node_id = self.runtime.identity.node_id();
        let Some(protocol) = ProtocolRange::current().negotiate(params.protocol) else {
            // §3.3: no overlap is a close, not a shrug. The reason is returned
            // too, so a brain that reads the error frame learns something.
            self.schedule_close(CLOSE_NO_PROTOCOL, "no common protocol version");
            return Err(TypedError::policy_refused(
                rpc::NODE_HELLO,
                format!(
                    "no common protocol version: this node speaks {} and the brain offered {}-{}",
                    swarmbot_node_protocol::PROTOCOL_VERSION,
                    params.protocol.min,
                    params.protocol.max
                ),
                false,
            ));
        };
        if let Err(error) = verify_hello(
            self.runtime.identity.brain_public_key(),
            node_id.as_str(),
            &self.nonce,
            self.runtime.identity.brain_origin(),
            &params.brain_sig,
        ) {
            // The anti-hijack rule (D1.3): a brain that cannot sign over the
            // pinned key is not our brain, and the socket must not stay open —
            // leaving it up would let the peer keep probing.
            tracing::warn!(
                node_id = %node_id,
                brain = %self.runtime.identity.brain_fingerprint(),
                %error,
                "node.hello did not verify against the pinned brain key; closing with 4403"
            );
            self.schedule_close(CLOSE_HELLO_FAILED, "node.hello failed");
            return Err(TypedError::not_authenticated(format!(
                "the brain's signature does not verify against the key pinned at pairing: {error}"
            )));
        }
        if self.authenticated.send(true).is_err() {
            // Unreachable while `authenticated_rx` is held, and a closed gate
            // would fail every later call rather than open one, so logging is
            // enough — but it must not be silent.
            tracing::error!("the authentication watch channel is closed; the gate cannot open");
        }
        tracing::info!(
            node_id = %node_id,
            brain_version = %params.brain_version,
            protocol,
            brain = %self.runtime.identity.brain_fingerprint(),
            "node.hello verified; this connection may now drive the node"
        );
        serde_json::to_value(self.hello_object(protocol)).map_err(|error| {
            TypedError::policy_refused(
                rpc::NODE_HELLO,
                format!("cannot serialize the hello object: {error}"),
                false,
            )
        })
    }
}

impl RpcHandler for NodeHandler {
    fn handle<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        tx: Option<ChunkSink>,
    ) -> BoxFuture<'a, Result<Value, TypedError>> {
        Box::pin(self.route(method, params, tx))
    }
}

impl NodeHandler {
    async fn route(
        &self,
        method: &str,
        params: Value,
        tx: Option<ChunkSink>,
    ) -> Result<Value, TypedError> {
        // `node.hello` is the one method that runs before authentication — it
        // *is* the authentication (§3.2).
        if method == rpc::NODE_HELLO {
            return self.hello(params).await;
        }
        if !self.is_authenticated() {
            return Err(TypedError::not_authenticated(
                "node.hello has not been verified on this connection",
            ));
        }
        // Honesty before routing: a method this build does not implement is a
        // typed refusal, not a silent success and not an "unknown method" shrug.
        if let Some(refusal) = rpc::unimplemented(method) {
            tracing::debug!(method, "refusing a method this build does not implement");
            return Err(refusal);
        }
        match method {
            rpc::SHELL_EXEC => {
                let sink = tx.ok_or_else(|| {
                    TypedError::policy_refused(
                        method,
                        "shell.exec streams, but this request arrived without a chunk sink",
                        false,
                    )
                })?;
                self.shell(method, params, sink).await
            }
            // A download is not a `NodeCall`: the bytes ride a data stream, not
            // the control socket, so it has its own path (§8).
            rpc::FS_DOWNLOAD => self.download(method, params).await,
            rpc::FS_READ
            | rpc::FS_WRITE
            | rpc::FS_EDIT
            | rpc::FS_STAT
            | rpc::FS_LIST
            | rpc::FS_GLOB
            | rpc::FS_GREP
            | rpc::FS_MKDIR
            | rpc::FS_MOVE
            | rpc::FS_REMOVE => self.fs(method, params).await,
            // These are the *brain's* methods (§6). A brain that sends its own
            // method back to a node is a wiring bug, and saying so beats
            // pretending to serve it.
            rpc::NODE_REQUEST_APPROVAL | rpc::NODE_EVENT | rpc::NODE_HEARTBEAT => {
                Err(TypedError::policy_refused(
                    method,
                    format!("`{method}` is served by the brain, not by a node"),
                    false,
                ))
            }
            _ => Err(TypedError::policy_refused(method, "unknown method", false)),
        }
    }

    /// One `fs.*` call: bridge, gate, execute, translate, audit.
    async fn fs(&self, method: &str, params: Value) -> Result<Value, TypedError> {
        let started = Instant::now();
        // Two facts are read out of the raw params before they are consumed by
        // the bridge, because neither survives into a `NodeCall`:
        // `overwrite: false` (§5.1) and the path `fs.stat` asked about.
        let no_clobber = no_clobber_target(method, &params);
        let stat_path = (method == rpc::FS_STAT)
            .then(|| {
                params
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .flatten();
        let (inv, call) = to_node_call(method, params)?;
        let surface = call.surface();
        let read_only = call.read_only();
        let target = call.describe();
        let tool = tool_name(method);
        let _active = self.enter();

        // `NodeCall::FsMove` cannot say "do not clobber", so the wire's promise
        // is kept here instead of being dropped on the floor.
        if let Some(to) = &no_clobber
            && self.target_exists(method, to)?
        {
            self.audit(
                &inv,
                tool,
                surface,
                &target,
                AuditDecision::Refuse,
                AuditOutcome::Error,
                started,
                0,
            );
            return Err(TypedError::fs_error(
                method,
                to.clone(),
                "EEXIST",
                "`overwrite: false` was requested but the target already exists; nothing was moved",
            ));
        }

        let gate = self
            .gate(&inv, method, surface, read_only, &target, &call)
            .await?;
        let audit_decision = gate.audit_decision();
        if gate.blocked() {
            // A denial is recorded as an error outcome, matching the tools
            // registry: nothing ran, and the line has to say so.
            self.audit(
                &inv,
                tool,
                surface,
                &target,
                audit_decision,
                AuditOutcome::Error,
                started,
                0,
            );
            return Err(TypedError::policy_refused(
                method,
                gate.denial_reason(&target, surface),
                false,
            ));
        }

        let node_id = self.runtime.identity.node_id().clone();
        // `inv` is the invocation that arrived with the request, so the executor
        // runs the call under the caller's own attribution — the same one this
        // handler audits the line against above.
        let result = self.runtime.executor.exec(&node_id, &inv, call).await;
        match result {
            Ok(result) => {
                let bytes = result.text_only().len();
                self.audit(
                    &inv,
                    tool,
                    surface,
                    &target,
                    audit_decision,
                    AuditOutcome::Ok,
                    started,
                    bytes,
                );
                encode_result(method, &result)
            }
            // §5: `fs.stat` of a path that does not exist is `null`, not an
            // error — "is it there?" is a question and "no" is an answer. Only a
            // real `NotFound` becomes null; a permission problem stays an error.
            Err(ToolError::Failed { .. })
                if method == rpc::FS_STAT && self.is_missing(stat_path.as_deref()) =>
            {
                self.audit(
                    &inv,
                    tool,
                    surface,
                    &target,
                    audit_decision,
                    AuditOutcome::Ok,
                    started,
                    0,
                );
                Ok(Value::Null)
            }
            Err(error) => {
                self.audit(
                    &inv,
                    tool,
                    surface,
                    &target,
                    audit_decision,
                    AuditOutcome::Error,
                    started,
                    0,
                );
                Err(translate(method, &target, &error))
            }
        }
    }

    /// `fs.download` (§5.1 + §8): read one file under this node's own policy,
    /// then send its bytes over the data stream the brain minted for it.
    ///
    /// The order is deliberate: **the policy decision comes first, the dial
    /// second.** A node that dialled first and checked the path afterwards would
    /// have opened a stream (and spent the brain's one-time token) for a read it
    /// is about to refuse — and the brain's socket task would be waiting on a
    /// transfer that never comes.
    ///
    /// The RPC answers only after the bytes have been **sent**, with what was
    /// sent. Answering earlier would be a claim about a transfer still in flight;
    /// the whole point of `{size, sha256}` in the answer is that the brain can
    /// compare it against what actually arrived.
    async fn download(&self, method: &str, params: Value) -> Result<Value, TypedError> {
        let started = Instant::now();
        let payload: FsDownload = decode_params(method, params)?;
        let inv = payload.inv.clone();
        let target = payload.path.clone();
        let tool = TOOL_READ_FILE;
        let surface = Surface::ReadFile;
        let _active = self.enter();

        // Resolving is the path policy: outside the roots, or inside the deny
        // list, is a denial no approval can waive.
        let resolved = match self.resolve(&payload.path, false) {
            Ok(resolved) => resolved,
            Err(error) => {
                self.audit(
                    &inv,
                    tool,
                    surface,
                    &target,
                    AuditDecision::Refuse,
                    AuditOutcome::Error,
                    started,
                    0,
                );
                return Err(translate(method, &target, &error));
            }
        };

        // The gate decides on a read, exactly as `fs.read` would: a download is
        // not a new permission surface, it is a different transport for the same
        // permission.
        let call = NodeCall::FsRead {
            path: payload.path.clone(),
            offset: None,
            limit: None,
            encoding: None,
        };
        let gate = self
            .gate(&inv, method, surface, true, &target, &call)
            .await?;
        let audit_decision = gate.audit_decision();
        if gate.blocked() {
            self.audit(
                &inv,
                tool,
                surface,
                &target,
                audit_decision,
                AuditOutcome::Error,
                started,
                0,
            );
            return Err(TypedError::policy_refused(
                method,
                gate.denial_reason(&target, surface),
                false,
            ));
        }

        // The brain's one-time grant is spent by the dial below; from here on a
        // failure has to be reported *to the brain over the socket as well*, or
        // its ticket would wait out the timeout. `stream::send_file` closes, and
        // the brain treats a socket that ends without `Done` as a failure.
        let bytes = match std::fs::read(&resolved) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.audit(
                    &inv,
                    tool,
                    surface,
                    &target,
                    audit_decision,
                    AuditOutcome::Error,
                    started,
                    0,
                );
                return Err(TypedError::fs_error(
                    method,
                    target,
                    errno_code(&error.to_string()),
                    format!("cannot read {}: {error}", resolved.display()),
                ));
            }
        };

        // The URL is derived from the brain this node paired with, never from a
        // payload: a node pointed at an address inside a frame could be aimed at
        // a third machine.
        let brain = self
            .runtime
            .config
            .brain_url
            .clone()
            .unwrap_or_else(|| self.runtime.identity.brain_origin().to_string());
        let url = match crate::stream::stream_url(&brain, &payload.stream_id) {
            Ok(url) => url,
            Err(error) => {
                self.audit(
                    &inv,
                    tool,
                    surface,
                    &target,
                    audit_decision,
                    AuditOutcome::Error,
                    started,
                    0,
                );
                return Err(TypedError::policy_refused(method, error.to_string(), false));
            }
        };
        let sent = match crate::stream::send_file(&url, &payload.stream_token, &bytes).await {
            Ok(sent) => sent,
            Err(error) => {
                tracing::warn!(
                    stream = %payload.stream_id,
                    error = %error,
                    "a data stream could not be completed"
                );
                self.audit(
                    &inv,
                    tool,
                    surface,
                    &target,
                    audit_decision,
                    AuditOutcome::Error,
                    started,
                    0,
                );
                return Err(TypedError::policy_refused(
                    method,
                    format!("the stream to the brain failed: {error}"),
                    true,
                ));
            }
        };

        self.audit(
            &inv,
            tool,
            surface,
            &target,
            audit_decision,
            AuditOutcome::Ok,
            started,
            sent.size as usize,
        );
        serde_json::to_value(FsDownloadResult {
            size: sent.size,
            sha256: sent.sha256,
        })
        .map_err(|error| TypedError::policy_refused(method, error.to_string(), false))
    }

    /// The streaming `shell.exec`.
    async fn shell(
        &self,
        method: &str,
        params: Value,
        sink: ChunkSink,
    ) -> Result<Value, TypedError> {
        let started = Instant::now();
        let _active = self.enter();
        let surface = Surface::Shell;

        if !self.runtime.config.shell_enabled {
            // The capability was withdrawn before the hello, so a brain that
            // read `capabilities` should not have sent this — but a brain that
            // did not must still get a typed refusal rather than a hang.
            let reason = "this node was started with `--no-shell`: the `shell@1` capability is \
                          withdrawn and no command will run";
            tracing::warn!(%reason, "refusing shell.exec");
            return Err(TypedError::policy_refused(method, reason, false));
        }

        let payload: ShellExec = decode_params(method, params)?;
        let env = payload.env.clone().unwrap_or_default();
        let call = NodeCall::ShellExec {
            command: payload.command.clone(),
            // An empty `cwd` means "the node's workspace", which is what
            // `LocalExecutor` and the runner both do with `None`.
            cwd: (!payload.cwd.trim().is_empty()).then(|| payload.cwd.clone()),
            timeout_ms: Some(payload.timeout_ms),
            env: Some(env.clone()),
        };
        let target = call.describe();
        let inv = payload.inv.clone();

        // Resolve the working directory *before* asking, matching the order the
        // local shell handler uses: a cwd outside the roots is a path denial no
        // approval can waive.
        let cwd = match self.resolve(&payload.cwd, false) {
            Ok(cwd) => cwd,
            Err(error) => {
                self.audit(
                    &inv,
                    TOOL_SHELL,
                    surface,
                    &target,
                    AuditDecision::Refuse,
                    AuditOutcome::Error,
                    started,
                    0,
                );
                return Err(translate(method, &target, &error));
            }
        };

        let gate = self
            .gate(&inv, method, surface, false, &target, &call)
            .await?;
        let audit_decision = gate.audit_decision();
        if gate.blocked() {
            self.audit(
                &inv,
                TOOL_SHELL,
                surface,
                &target,
                audit_decision,
                AuditOutcome::Error,
                started,
                0,
            );
            return Err(TypedError::policy_refused(
                method,
                gate.denial_reason(&target, surface),
                false,
            ));
        }

        let options = ShellOptions {
            spill_dir: self.runtime.paths.spill(),
            spill_name: inv.id.to_string(),
            output_limit_bytes: payload
                .output_limit_bytes
                .and_then(|limit| usize::try_from(limit).ok())
                .unwrap_or(0),
            shutdown: self.runtime.stopping.clone(),
            registry: self.runtime.registry.clone(),
        };
        let run = shell::run_streaming(
            &payload.command,
            &cwd,
            payload.timeout_ms,
            &env,
            &options,
            &sink,
        )
        .await;
        match run {
            Ok(run) => {
                // A non-zero exit is an honest `Exit` chunk, not an RPC error:
                // the command ran and reported its status, which is data.
                self.audit(
                    &inv,
                    TOOL_SHELL,
                    surface,
                    &target,
                    audit_decision,
                    AuditOutcome::Ok,
                    started,
                    usize::try_from(run.output_bytes).unwrap_or(usize::MAX),
                );
                Ok(Value::Null)
            }
            Err(failure) => {
                self.audit(
                    &inv,
                    TOOL_SHELL,
                    surface,
                    &target,
                    audit_decision,
                    AuditOutcome::Error,
                    started,
                    0,
                );
                Err(shell_failure(method, &failure))
            }
        }
    }

    /// Ceiling + approval for one call.
    async fn gate(
        &self,
        inv: &Invocation,
        method: &str,
        surface: Surface,
        read_only: bool,
        target: &str,
        call: &NodeCall,
    ) -> Result<Gate, TypedError> {
        let command = match call {
            NodeCall::ShellExec { command, .. } => Some(command.as_str()),
            _ => None,
        };
        match self.runtime.engine.decide(surface, read_only, command) {
            PolicyDecision::Run => Ok(Gate::Run),
            PolicyDecision::Refuse => {
                tracing::warn!(
                    method,
                    surface = %surface,
                    target,
                    "refused by this node's own ceiling (`never`)"
                );
                Ok(Gate::Refused)
            }
            PolicyDecision::Ask => {
                // D5.5: a destructive command always asks, and the reason says
                // so — the owner is being asked about the pattern, not the verb.
                let reason = command
                    .and_then(destructive_label)
                    .map(|label| {
                        format!(
                            "command matches a destructive pattern ({label}); destructive commands always ask"
                        )
                    })
                    .unwrap_or_else(|| {
                        format!("this node's `{surface}` ceiling is `ask`")
                    });
                Ok(
                    match self
                        .request_approval(inv, surface, target, Some(reason))
                        .await
                    {
                        ApprovalDecision::Deny => Gate::Denied,
                        answer => Gate::Allowed(answer),
                    },
                )
            }
        }
    }

    /// Resolve one caller-supplied path through this node's own policy. The
    /// caller's string is *unresolved* by design (§5.1): a remote node has its
    /// own filesystem, so resolution happens here and never on the brain.
    fn resolve(&self, path: &str, write: bool) -> Result<std::path::PathBuf, ToolError> {
        let raw = if path.trim().is_empty() { "." } else { path };
        self.runtime
            .policy
            .resolve(Path::new(raw), self.runtime.executor.workspace(), write)
    }

    /// Does the resolved target of a `fs.move` already exist?
    fn target_exists(&self, method: &str, to: &str) -> Result<bool, TypedError> {
        let resolved = self
            .resolve(to, true)
            .map_err(|error| translate(method, to, &error))?;
        Ok(resolved.exists())
    }

    /// Is the path a `fs.stat` asked about simply not there? Resolution runs
    /// again on purpose: a path outside the roots must stay a `PathDenied`
    /// rather than being reported as "missing", which would be a lie about why.
    fn is_missing(&self, path: Option<&str>) -> bool {
        let Some(path) = path else { return false };
        match self.resolve(path, false) {
            Ok(resolved) => std::fs::symlink_metadata(&resolved)
                .err()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            Err(_) => false,
        }
    }

    /// Count one running invocation, for `node.heartbeat` and `status`.
    fn enter(&self) -> ActiveGuard<'_> {
        self.active.fetch_add(1, Ordering::SeqCst);
        ActiveGuard {
            active: &self.active,
        }
    }
}

/// The approval outcome of one call, in the shape the audit line records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    /// The ceiling allowed it without asking.
    Run,
    /// Asked, and the brain said yes. Carries the decision so an
    /// `allow_always` stays distinguishable from an `allow_once` in a log.
    Allowed(ApprovalDecision),
    /// Asked, and the answer was no — or could not be obtained, which is the
    /// same thing (D1.4).
    Denied,
    /// A `never` ceiling refused it before anyone was asked.
    Refused,
}

impl Gate {
    fn blocked(&self) -> bool {
        matches!(self, Gate::Denied | Gate::Refused)
    }

    fn audit_decision(&self) -> AuditDecision {
        match self {
            Gate::Run => AuditDecision::Run,
            Gate::Allowed(_) => AuditDecision::Allow,
            Gate::Denied => AuditDecision::Deny,
            Gate::Refused => AuditDecision::Refuse,
        }
    }

    /// Why the call did not run, in words a model can act on. The two cases
    /// read differently on purpose: a `never` ceiling will refuse again, while
    /// a denied ask may succeed next time with a different target.
    fn denial_reason(&self, target: &str, surface: Surface) -> String {
        match self {
            Gate::Refused => format!(
                "this node never allows `{surface}` actions, so `{target}` is refused; retrying cannot help"
            ),
            _ => format!(
                "denied: `{surface}` on {target} was not approved (an unreachable or disagreeing brain is a deny, never an allow)"
            ),
        }
    }
}

/// The `to` path of a `fs.move` that asked not to clobber, if any. Read from the
/// raw params because `NodeCall::FsMove` has no field for it (§5.1).
fn no_clobber_target(method: &str, params: &Value) -> Option<String> {
    if method != rpc::FS_MOVE {
        return None;
    }
    let payload: FsMove = serde_json::from_value(params.clone()).ok()?;
    (payload.overwrite == Some(false)).then_some(payload.to)
}

/// Decrements the active-invocation count when a call finishes, however it
/// finished — including by having its future dropped on an `Interrupt`.
struct ActiveGuard<'a> {
    active: &'a AtomicU32,
}

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The audit `tool` label for a method. The seven that exist as bot tools use
/// the tools crate's own constants, so an audit line from a node and one from
/// the brain's local executor say the same thing; the fs-only RPCs (`stat`,
/// `mkdir`, `move`, `remove`) have no tool and keep their verb.
pub fn tool_name(method: &str) -> &'static str {
    match method {
        rpc::FS_READ => TOOL_READ_FILE,
        rpc::FS_WRITE => TOOL_WRITE_FILE,
        rpc::FS_EDIT => TOOL_EDIT_FILE,
        rpc::FS_LIST => TOOL_LIST_DIR,
        rpc::FS_GLOB => TOOL_GLOB,
        rpc::FS_GREP => TOOL_GREP,
        rpc::SHELL_EXEC => TOOL_SHELL,
        rpc::FS_STAT => "stat",
        rpc::FS_MKDIR => "mkdir",
        rpc::FS_MOVE => "move",
        rpc::FS_REMOVE => "remove",
        _ => "unknown",
    }
}

/// Encode a [`ToolResult`] as the RPC's success value. See the module docs:
/// this is the deliberate deviation from the doc's `Fs*Result` column, made so
/// the brain's remote `NodeExecutor` can deserialize straight into the type its
/// own port returns.
fn encode_result(method: &str, result: &ToolResult) -> Result<Value, TypedError> {
    serde_json::to_value(result).map_err(|error| {
        TypedError::policy_refused(
            method,
            format!("cannot serialize the result: {error}"),
            false,
        )
    })
}

/// A `u64` limit from the wire into the `u32` one `NodeCall` carries. Saturating
/// rather than refusing: a caller asking for "a lot" means "a lot", and an
/// error here would be about our types, not their call.
fn limit_u32(limit: Option<u64>) -> Option<u32> {
    limit.map(|limit| u32::try_from(limit).unwrap_or(u32::MAX))
}

fn encoding_label(encoding: Option<Encoding>) -> Option<String> {
    encoding.map(|encoding| match encoding {
        Encoding::Utf8 => "utf8".to_string(),
        Encoding::Base64 => "base64".to_string(),
    })
}

fn grep_mode_label(mode: Option<GrepOutputMode>) -> Option<String> {
    mode.map(|mode| match mode {
        GrepOutputMode::Content => "content".to_string(),
        GrepOutputMode::FilesWithMatches => "files_with_matches".to_string(),
        GrepOutputMode::Count => "count".to_string(),
    })
}

/// The wire payload → [`NodeCall`] bridge. The **only** place the two shapes
/// meet; every difference is commented because each one is a place a future
/// edit could silently drop a guarantee.
fn to_node_call(method: &str, params: Value) -> Result<(Invocation, NodeCall), TypedError> {
    let refuse = |reason: String| TypedError::policy_refused(method, reason, false);
    match method {
        rpc::FS_READ => {
            let p: FsRead = decode_params(method, params)?;
            // `base64` is accepted by the wire and refused by the reader: the
            // local implementation reads UTF-8 text only, and saying so is
            // better than returning replacement characters for a binary file.
            Ok((
                p.inv,
                NodeCall::FsRead {
                    path: p.path,
                    offset: p.offset,
                    limit: p.limit,
                    encoding: encoding_label(p.encoding),
                },
            ))
        }
        rpc::FS_WRITE => {
            let p: FsWrite = decode_params(method, params)?;
            if matches!(p.encoding, Some(Encoding::Base64)) {
                // `NodeCall::FsWrite` carries `content: String`, so a base64
                // body cannot cross the bridge. Refusing keeps the caller from
                // believing binary content was written.
                return Err(refuse(
                    "`encoding: base64` cannot be executed by this node: the write body is \
                     carried as text, so a binary write is not expressible on this path"
                        .to_string(),
                ));
            }
            // `expectedSha256` maps straight through: `NodeCall::FsWrite` has
            // the field, so the compare-and-set is enforced by the executor.
            Ok((
                p.inv,
                NodeCall::FsWrite {
                    path: p.path,
                    content: p.content,
                    expected_sha256: p.expected_sha256,
                    create_parents: p.create_parents,
                },
            ))
        }
        rpc::FS_EDIT => {
            let p: FsEdit = decode_params(method, params)?;
            if p.expected_sha256.is_some() {
                // `NodeCall::FsEdit` has no `expected_sha256`, so the condition
                // could only be honoured by dropping it — which would turn a
                // stale-write check into a clobber. Refuse instead (§5.1).
                return Err(refuse(
                    "a conditional edit (`expectedSha256`) cannot be expressed as a local \
                     `NodeCall` yet, so this node refuses it rather than dropping the check"
                        .to_string(),
                ));
            }
            Ok((
                p.inv,
                NodeCall::FsEdit {
                    path: p.path,
                    old_string: p.old_string,
                    new_string: p.new_string,
                    replace_all: p.replace_all,
                },
            ))
        }
        rpc::FS_STAT => {
            let p: FsStat = decode_params(method, params)?;
            Ok((p.inv, NodeCall::FsStat { path: p.path }))
        }
        rpc::FS_LIST => {
            let p: FsList = decode_params(method, params)?;
            Ok((
                p.inv,
                NodeCall::FsList {
                    path: p.path,
                    depth: p.depth,
                    include_hidden: p.include_hidden,
                    limit: limit_u32(p.limit),
                },
            ))
        }
        rpc::FS_GLOB => {
            let p: FsGlob = decode_params(method, params)?;
            // The wire's `cwd` is required; `NodeCall`'s `path` is optional.
            // An empty `cwd` means "the node's workspace", which is what
            // `None` also means — so the two line up without a guess.
            let path = (!p.cwd.trim().is_empty()).then(|| p.cwd.clone());
            Ok((
                p.inv,
                NodeCall::FsGlob {
                    pattern: p.pattern,
                    path,
                    limit: limit_u32(p.limit),
                },
            ))
        }
        rpc::FS_GREP => {
            let p: FsGrep = decode_params(method, params)?;
            let path = (!p.path.trim().is_empty()).then(|| p.path.clone());
            Ok((
                p.inv,
                NodeCall::FsGrep {
                    pattern: p.pattern,
                    path,
                    glob: p.glob,
                    ignore_case: p.ignore_case,
                    literal: p.literal,
                    context_lines: p.context_lines,
                    output_mode: grep_mode_label(p.output_mode),
                    limit: limit_u32(p.limit),
                },
            ))
        }
        rpc::FS_MKDIR => {
            let p: FsMkdir = decode_params(method, params)?;
            // `recursive` on the wire and `parents` in `NodeCall` are the same
            // flag under two names — not two different behaviours.
            Ok((
                p.inv,
                NodeCall::FsMkdir {
                    path: p.path,
                    parents: p.recursive,
                },
            ))
        }
        rpc::FS_MOVE => {
            let p: FsMove = decode_params(method, params)?;
            // `overwrite` has no `NodeCall` counterpart; `overwrite: false` is
            // enforced by the caller (`refuses_to_clobber`) and `true`/absent
            // is what `rename` does anyway.
            Ok((
                p.inv,
                NodeCall::FsMove {
                    from: p.from,
                    to: p.to,
                },
            ))
        }
        rpc::FS_REMOVE => {
            let p: FsRemove = decode_params(method, params)?;
            Ok((
                p.inv,
                NodeCall::FsRemove {
                    path: p.path,
                    recursive: p.recursive,
                },
            ))
        }
        other => Err(refuse(format!("`{other}` carries no `NodeCall`"))),
    }
}

/// Translate a [`ToolError`] into the typed wire error (§7).
///
/// The split that matters: a **denial** keeps its own tag and reads on its own
/// (`PathDenied`, `PolicyRefused`), while an **infrastructure failure** becomes
/// `FsError`/`EditFailed` with an errno-style code. Mixing them would teach a
/// model to retry a refusal, or to accept a broken disk as a policy answer.
pub fn translate(method: &str, target: &str, error: &ToolError) -> TypedError {
    match error {
        ToolError::PathDenied { path, reason } => {
            TypedError::path_denied(path.clone(), reason.clone())
        }
        ToolError::PolicyRefused { reason, retry } => {
            TypedError::policy_refused(method, reason.clone(), *retry)
        }
        ToolError::Denied { reason } => TypedError::policy_refused(method, reason.clone(), false),
        ToolError::InvalidArgs { message } => {
            TypedError::policy_refused(method, message.clone(), false)
        }
        ToolError::UnknownTool { tool } => {
            TypedError::policy_refused(method, format!("unknown tool `{tool}`"), false)
        }
        ToolError::Cancelled => TypedError::policy_refused(
            method,
            "the node stopped this call because it is shutting down",
            true,
        ),
        ToolError::Failed { message } => failed(method, target, message),
    }
}

/// A `Failed` is either an edit that could not be applied or a filesystem error;
/// the method decides which tag the caller gets, and the errno code is pulled
/// out of the message the tools layer formatted (it embeds `(os error N)`).
fn failed(method: &str, target: &str, message: &str) -> TypedError {
    match method {
        rpc::FS_WRITE | rpc::FS_EDIT => TypedError::EditFailed {
            path: target.to_string(),
            reason: message.to_string(),
        },
        _ => TypedError::fs_error(method, target, errno_code(message), message),
    }
}

/// The errno name behind an `(os error N)` suffix, or `EIO` when the message
/// carries none. Named from `libc` so the mapping cannot drift from the
/// platform's own numbers.
pub fn errno_code(message: &str) -> &'static str {
    let Some(number) = message
        .rsplit_once("(os error ")
        .and_then(|(_, tail)| tail.split(')').next())
        .and_then(|digits| digits.trim().parse::<i32>().ok())
    else {
        return "EIO";
    };
    match number {
        libc::ENOENT => "ENOENT",
        libc::EEXIST => "EEXIST",
        libc::EACCES => "EACCES",
        libc::EPERM => "EPERM",
        libc::EISDIR => "EISDIR",
        libc::ENOTDIR => "ENOTDIR",
        libc::ENOTEMPTY => "ENOTEMPTY",
        libc::EXDEV => "EXDEV",
        libc::ELOOP => "ELOOP",
        libc::ENAMETOOLONG => "ENAMETOOLONG",
        libc::ENOSPC => "ENOSPC",
        libc::EROFS => "EROFS",
        libc::EBUSY => "EBUSY",
        libc::EMFILE | libc::ENFILE => "EMFILE",
        _ => "EIO",
    }
}

/// A streaming failure as a typed error. The wire has no `ShellFailed` tag, so
/// a spawn failure is reported as a non-retryable `PolicyRefused` carrying the
/// real reason — typed, and honest about not being a policy decision.
fn shell_failure(method: &str, failure: &ShellFailure) -> TypedError {
    match failure {
        ShellFailure::Spawn { reason } => TypedError::policy_refused(method, reason.clone(), false),
        ShellFailure::Transport { reason } => {
            TypedError::policy_refused(method, reason.clone(), true)
        }
        ShellFailure::Interrupted { reason } => {
            TypedError::policy_refused(method, reason.clone(), true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use swarmbot_contracts::{BotId, ConversationId, PolicyCeiling};

    pub(crate) fn test_invocation() -> Invocation {
        Invocation::new(
            BotId::from_suffix(&"b".repeat(32)),
            ConversationId::from_suffix(&"c".repeat(32)),
        )
    }

    fn inv_value() -> Value {
        serde_json::to_value(test_invocation()).expect("serializes")
    }

    #[test]
    fn every_fs_payload_bridges_to_its_node_call() {
        let cases: Vec<(Value, &str)> = vec![
            (
                json!({"inv": inv_value(), "path": "a.txt", "offset": 2, "limit": 3, "encoding": "utf8"}),
                "fs_read",
            ),
            (
                json!({"inv": inv_value(), "path": "a.txt", "content": "hi", "createParents": true}),
                "fs_write",
            ),
            (
                json!({"inv": inv_value(), "path": "a.txt", "oldString": "a", "newString": "b", "replaceAll": true}),
                "fs_edit",
            ),
            (json!({"inv": inv_value(), "path": "a.txt"}), "fs_stat"),
            (
                json!({"inv": inv_value(), "path": ".", "depth": 2, "includeHidden": true, "limit": 5}),
                "fs_list",
            ),
            (
                json!({"inv": inv_value(), "pattern": "*.rs", "cwd": "src", "limit": 7}),
                "fs_glob",
            ),
            (
                json!({"inv": inv_value(), "pattern": "TODO", "path": "src", "outputMode": "count", "limit": 9}),
                "fs_grep",
            ),
            (
                json!({"inv": inv_value(), "path": "d", "recursive": false}),
                "fs_mkdir",
            ),
            (
                json!({"inv": inv_value(), "from": "a", "to": "b"}),
                "fs_move",
            ),
            (
                json!({"inv": inv_value(), "path": "a", "recursive": true}),
                "fs_remove",
            ),
        ];
        for (params, expected) in cases {
            let (inv, call) = to_node_call(expected_method(expected), params)
                .unwrap_or_else(|error| panic!("{expected}: {error}"));
            assert_eq!(serde_json::to_value(&call).expect("json")["call"], expected);
            assert_eq!(inv.bot_id, test_invocation().bot_id);
        }
    }

    fn expected_method(call_tag: &str) -> &'static str {
        match call_tag {
            "fs_read" => rpc::FS_READ,
            "fs_write" => rpc::FS_WRITE,
            "fs_edit" => rpc::FS_EDIT,
            "fs_stat" => rpc::FS_STAT,
            "fs_list" => rpc::FS_LIST,
            "fs_glob" => rpc::FS_GLOB,
            "fs_grep" => rpc::FS_GREP,
            "fs_mkdir" => rpc::FS_MKDIR,
            "fs_move" => rpc::FS_MOVE,
            "fs_remove" => rpc::FS_REMOVE,
            _ => unreachable!("test table"),
        }
    }

    #[test]
    fn glob_and_grep_rename_their_base_directory() {
        // `FsGlob.cwd` is required on the wire and optional in `NodeCall`.
        let (_, call) = to_node_call(
            rpc::FS_GLOB,
            json!({"inv": inv_value(), "pattern": "*.rs", "cwd": "src"}),
        )
        .expect("bridges");
        assert_eq!(
            call,
            NodeCall::FsGlob {
                pattern: "*.rs".into(),
                path: Some("src".into()),
                limit: None
            }
        );
        // An empty cwd means "the workspace", which is what `None` means too.
        let (_, call) = to_node_call(
            rpc::FS_GLOB,
            json!({"inv": inv_value(), "pattern": "*.rs", "cwd": "  "}),
        )
        .expect("bridges");
        match call {
            NodeCall::FsGlob { path, .. } => assert_eq!(path, None),
            other => panic!("expected FsGlob, got {other:?}"),
        }
    }

    #[test]
    fn mkdirs_recursive_is_node_calls_parents() {
        let (_, call) = to_node_call(
            rpc::FS_MKDIR,
            json!({"inv": inv_value(), "path": "a/b", "recursive": true}),
        )
        .expect("bridges");
        assert_eq!(
            call,
            NodeCall::FsMkdir {
                path: "a/b".into(),
                parents: Some(true)
            }
        );
    }

    #[test]
    fn a_conditional_edit_is_refused_not_silently_dropped() {
        let error = to_node_call(
            rpc::FS_EDIT,
            json!({"inv": inv_value(), "path": "a", "oldString": "x", "newString": "y",
                   "expectedSha256": "abc"}),
        )
        .expect_err("must refuse");
        match error {
            TypedError::PolicyRefused { reason, retry, .. } => {
                assert!(reason.contains("expectedSha256"), "{reason}");
                assert!(!retry);
            }
            other => panic!("expected PolicyRefused, got {other:?}"),
        }
        // Without the condition it bridges normally.
        assert!(
            to_node_call(
                rpc::FS_EDIT,
                json!({"inv": inv_value(), "path": "a", "oldString": "x", "newString": "y"}),
            )
            .is_ok()
        );
    }

    #[test]
    fn a_conditional_write_is_enforced_because_node_call_can_carry_it() {
        let (_, call) = to_node_call(
            rpc::FS_WRITE,
            json!({"inv": inv_value(), "path": "a", "content": "x", "expectedSha256": ""}),
        )
        .expect("bridges");
        match call {
            NodeCall::FsWrite {
                expected_sha256, ..
            } => assert_eq!(expected_sha256.as_deref(), Some("")),
            other => panic!("expected FsWrite, got {other:?}"),
        }
    }

    #[test]
    fn a_base64_write_body_is_refused() {
        let error = to_node_call(
            rpc::FS_WRITE,
            json!({"inv": inv_value(), "path": "a", "content": "AA==", "encoding": "base64"}),
        )
        .expect_err("must refuse");
        assert!(matches!(
            error,
            TypedError::PolicyRefused { retry: false, .. }
        ));
        assert!(error.to_string().contains("base64"), "{error}");
    }

    #[test]
    fn limits_are_widened_without_losing_their_meaning() {
        assert_eq!(limit_u32(Some(5)), Some(5));
        assert_eq!(limit_u32(None), None);
        assert_eq!(limit_u32(Some(u64::MAX)), Some(u32::MAX));
    }

    #[test]
    fn tool_errors_keep_denial_and_failure_apart() {
        let denied = translate(
            rpc::FS_READ,
            "/etc/passwd",
            &ToolError::PathDenied {
                path: "/etc/passwd".into(),
                reason: "outside roots".into(),
            },
        );
        assert_eq!(
            denied,
            TypedError::PathDenied {
                path: "/etc/passwd".into(),
                reason: "outside roots".into()
            }
        );

        let refused = translate(
            rpc::SHELL_EXEC,
            "rm -rf /",
            &ToolError::PolicyRefused {
                reason: "never".into(),
                retry: false,
            },
        );
        match refused {
            TypedError::PolicyRefused { action, retry, .. } => {
                assert_eq!(action, rpc::SHELL_EXEC);
                assert!(!retry);
            }
            other => panic!("expected PolicyRefused, got {other:?}"),
        }

        let write_failed = translate(
            rpc::FS_WRITE,
            "a.txt",
            &ToolError::failed("expected_sha256 mismatch for a.txt"),
        );
        assert!(
            matches!(write_failed, TypedError::EditFailed { .. }),
            "{write_failed:?}"
        );

        let fs_failed = translate(
            rpc::FS_REMOVE,
            "a.txt",
            &ToolError::failed("cannot remove a.txt: No such file or directory (os error 2)"),
        );
        match fs_failed {
            TypedError::FsError { op, code, .. } => {
                assert_eq!(op, rpc::FS_REMOVE);
                assert_eq!(code, "ENOENT");
            }
            other => panic!("expected FsError, got {other:?}"),
        }

        let cancelled = translate(rpc::FS_READ, "a", &ToolError::Cancelled);
        match cancelled {
            TypedError::PolicyRefused { retry, reason, .. } => {
                assert!(retry, "a shutdown is retryable on the next connection");
                assert!(reason.contains("shutting down"), "{reason}");
            }
            other => panic!("expected PolicyRefused, got {other:?}"),
        }
    }

    #[test]
    fn errno_codes_are_read_out_of_the_tools_message() {
        assert_eq!(errno_code("x (os error 2)"), "ENOENT");
        assert_eq!(errno_code("x (os error 17)"), "EEXIST");
        assert_eq!(errno_code("x (os error 13)"), "EACCES");
        assert_eq!(errno_code("x (os error 20)"), "ENOTDIR");
        assert_eq!(errno_code("x (os error 21)"), "EISDIR");
        assert_eq!(errno_code("x (os error 18)"), "EXDEV");
        assert_eq!(errno_code("no errno here"), "EIO");
        assert_eq!(errno_code("x (os error 4242)"), "EIO");
        // The names are the platform's own numbers.
        assert_eq!(libc::ENOENT, 2);
        assert_eq!(libc::EEXIST, 17);
    }

    #[test]
    fn tool_names_match_the_tools_crate_for_the_seven_real_tools() {
        assert_eq!(tool_name(rpc::FS_READ), TOOL_READ_FILE);
        assert_eq!(tool_name(rpc::FS_WRITE), TOOL_WRITE_FILE);
        assert_eq!(tool_name(rpc::FS_EDIT), TOOL_EDIT_FILE);
        assert_eq!(tool_name(rpc::FS_LIST), TOOL_LIST_DIR);
        assert_eq!(tool_name(rpc::FS_GLOB), TOOL_GLOB);
        assert_eq!(tool_name(rpc::FS_GREP), TOOL_GREP);
        assert_eq!(tool_name(rpc::SHELL_EXEC), TOOL_SHELL);
        assert_eq!(tool_name(rpc::FS_STAT), "stat");
        assert_eq!(tool_name("nonsense"), "unknown");
    }

    #[test]
    fn the_hello_ceiling_is_the_strictest_surface() {
        let mut config = NodeConfig::new_in(std::env::temp_dir());
        config.ceilings = PolicyMap::uniform(PolicyCeiling::Always);
        config.ceilings.set(Surface::Shell, PolicyCeiling::Never);
        assert_eq!(config.aggregate_ceiling(), PolicyCeiling::Never);
    }
}
