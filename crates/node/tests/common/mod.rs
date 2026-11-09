//! Shared scaffolding for the daemon's wire tests.
//!
//! Everything here runs over `node_protocol::testing::duplex_pair` — an
//! in-memory, frame-preserving socket pair — so no test binds a port except the
//! two that genuinely cannot avoid it (pairing is HTTP, and a WebSocket close
//! code cannot be produced by a scripted socket).
//!
//! The rig is deliberately built the way the daemon builds it: a real
//! [`NodeRuntime`] with a real [`PathPolicy`], a real [`PolicyEngine`] and a
//! real [`LocalExecutor`], in a `TempDir`. Only the brain is a double.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use swarmbot_contracts::{
    BotId, ConversationId, Invocation, NodeId, NodeRoot, PolicyCeiling, PolicyMap, ProtocolRange,
    RootMode, Surface,
};
use swarmbot_node_protocol::mux::{Peer, Socket};
use swarmbot_node_protocol::rpc::{ApprovalAnswer, RequestApproval};
use swarmbot_node_protocol::testing::LineSocket;
use swarmbot_node_protocol::testing::duplex_pair;
use swarmbot_node_protocol::{ProtocolError, sign_hello};
use tokio::io::DuplexStream;

use swarmbot_node::handlers::NodeHandler;
use swarmbot_node::{Identity, NodeConfig, NodePaths, NodeRuntime};

/// The node's half of an in-memory socket pair, wrapped twice so a test can see
/// the close code the daemon asked for (a `LineSocket` ignores it) and can make
/// the socket die on demand (see [`DeadSocket`]).
pub type NodeSocket = DeadSocket<CloseRecorder<LineSocket<DuplexStream>>>;
/// The brain's half of the same pair.
pub type BrainSocket = LineSocket<DuplexStream>;
pub type NodePeer = Peer<NodeSocket>;
pub type BrainPeer = Peer<BrainSocket>;

/// The brain origin every test identity is pinned to. It is an `http://` origin
/// because that is what `brain_origin_from_url` produces for a `ws://` dial.
pub const BRAIN_ORIGIN: &str = "http://127.0.0.1:8787";

/// The nonce this connection's upgrade header "carried". `node.hello`'s
/// signature is over it, so the test signs with exactly this value.
pub const NONCE: &str = "AAAAAAAAAAAAAAAAAAAAAQ";

/// A deterministic brain key. Deterministic so a failing test prints the same
/// signature every run.
pub fn brain_key() -> SigningKey {
    SigningKey::from_bytes(&[4u8; 32])
}

/// A key that is *not* the pinned brain key: the hijacker.
pub fn wrong_brain_key() -> SigningKey {
    SigningKey::from_bytes(&[5u8; 32])
}

pub fn temp_root() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).expect("root dir");
    (tmp, root)
}

/// A node config rooted at `root`, with every ceiling `always` so a test
/// exercises the behaviour it named rather than the approval round-trip.
pub fn permissive_config(dir: &Path, root: &Path) -> NodeConfig {
    let mut config = NodeConfig::new_in(dir);
    config.roots = vec![NodeRoot::read_write("root", root.display().to_string())];
    config.ceilings = PolicyMap::uniform(PolicyCeiling::Always);
    config.label = "test node".into();
    config
}

/// The identity of a node pinned to [`brain_key`].
pub fn identity(dir: &Path) -> Arc<Identity> {
    identity_with(dir, &brain_key())
}

pub fn identity_with(dir: &Path, brain: &SigningKey) -> Arc<Identity> {
    let paths = NodePaths::new(dir);
    std::fs::create_dir_all(&paths.data_dir).expect("data dir");
    Arc::new(
        Identity::from_parts(
            NodeId::new(),
            SigningKey::from_bytes(&[9u8; 32]),
            brain.verifying_key(),
            BRAIN_ORIGIN,
        )
        .expect("identity")
        .saved(&paths),
    )
}

trait Saved: Sized {
    fn saved(self, paths: &NodePaths) -> Self;
}

impl Saved for Identity {
    fn saved(self, paths: &NodePaths) -> Self {
        self.save(paths).expect("saves at 0600");
        self
    }
}

/// A connected node: the runtime, the handler for one connection, and the peer
/// the handler is attached to.
pub struct NodeSide {
    pub tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub config: NodeConfig,
    pub runtime: Arc<NodeRuntime>,
    pub handler: Arc<NodeHandler>,
    pub peer: NodePeer,
    /// Close codes the node asked its socket to send.
    pub closes: Arc<Mutex<Vec<(u16, String)>>>,
    /// Kills this connection's transport, simulating a dropped socket.
    pub kill: DeadSwitch,
}

impl NodeSide {
    pub fn node_id(&self) -> &NodeId {
        self.runtime.identity.node_id()
    }

    pub fn write(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent dir");
        }
        std::fs::write(&path, content).expect("test file write");
        path
    }
}

/// Build the node half of a connection over an in-memory socket pair, and
/// return it together with the socket the test drives as the brain.
///
/// `require_hello: true` is the daemon's setting; the handler also refuses on
/// its own, and both halves are tested.
pub fn node_pair(config: impl FnOnce(&Path, &Path) -> NodeConfig) -> (NodeSide, BrainSocket) {
    let (tmp, root) = temp_root();
    let resolved = config(tmp.path(), &root);
    let paths = resolved.paths();
    paths
        .ensure_dirs(&resolved.workspace())
        .expect("directories");
    let identity = identity(tmp.path());
    let runtime = NodeRuntime::new(resolved.clone(), Arc::clone(&identity)).expect("runtime");
    let handler = NodeHandler::new(Arc::clone(&runtime), NONCE);
    let (node_socket, brain_socket) = duplex_pair(256 * 1024);
    let closes = Arc::new(Mutex::new(Vec::new()));
    let (socket, kill) = DeadSocket::new(CloseRecorder {
        inner: node_socket,
        closes: Arc::clone(&closes),
    });
    let peer = Peer::new(
        socket,
        swarmbot_node_protocol::PeerConfig {
            require_hello: true,
            handler: Some(Arc::clone(&handler) as swarmbot_node_protocol::Handler),
            // Long timers: a test ends when its assertions are done, not when
            // a liveness deadline happens to fire.
            ping_interval: std::time::Duration::from_secs(3600),
            liveness_timeout: std::time::Duration::from_secs(3600),
        },
    );
    handler.attach(swarmbot_node::handlers::PeerLink::shared(peer.clone()));
    (
        NodeSide {
            tmp,
            root,
            config: resolved,
            runtime,
            handler,
            peer,
            closes,
            kill,
        },
        brain_socket,
    )
}

/// Make a [`DeadSocket`] report end-of-stream, the way a dropped TCP connection
/// does.
#[derive(Clone, Debug)]
pub struct DeadSwitch {
    dead: Arc<std::sync::atomic::AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl DeadSwitch {
    /// The connection is gone: the next `recv_text` reports EOF and the next
    /// `send_text` fails.
    pub fn kill(&self) {
        self.dead.store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_waiters();
        // A permit as well, so a socket that is not currently parked in
        // `recv_text` still notices the first time it goes back there.
        self.notify.notify_one();
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// A socket wrapper that reports end-of-stream once its [`DeadSwitch`] fires.
///
/// This exists because dropping every `Peer` handle does **not** close a
/// `node_protocol` peer: the driver keeps its own command-sender clone, so
/// `cmd_rx` never closes and the socket is never dropped. A test therefore has
/// to make the transport itself die, which is what a real disconnect does to the
/// node's `recv_text` anyway.
pub struct DeadSocket<S> {
    inner: S,
    switch: DeadSwitch,
}

impl<S> DeadSocket<S> {
    pub fn new(inner: S) -> (DeadSocket<S>, DeadSwitch) {
        let switch = DeadSwitch {
            dead: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            notify: Arc::new(tokio::sync::Notify::new()),
        };
        (
            DeadSocket {
                inner,
                switch: switch.clone(),
            },
            switch,
        )
    }

    pub fn switch(&self) -> DeadSwitch {
        self.switch.clone()
    }
}

impl<S: Socket> Socket for DeadSocket<S> {
    async fn send_text(&mut self, text: &str) -> Result<(), ProtocolError> {
        if self.switch.is_dead() {
            return Err(ProtocolError::SocketClosed);
        }
        self.inner.send_text(text).await
    }

    async fn recv_text(&mut self) -> Result<Option<String>, ProtocolError> {
        if self.switch.is_dead() {
            return Ok(None);
        }
        // `LineSocket::recv_text` is documented cancellation-safe, so losing the
        // race against the switch drops nothing.
        tokio::select! {
            biased;
            _ = self.switch.notify.notified() => Ok(None),
            frame = self.inner.recv_text() => frame,
        }
    }

    async fn close(&mut self, code: u16, reason: &str) -> Result<(), ProtocolError> {
        self.inner.close(code, reason).await
    }
}

/// The close codes a node's socket was asked to send.
pub fn recorded_closes(closes: &Arc<Mutex<Vec<(u16, String)>>>) -> Vec<(u16, String)> {
    closes.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// A [`Socket`] that records the close code, which is the only way to assert
/// 4403/4426 over an in-memory transport (`LineSocket` ignores the code).
pub struct CloseRecorder<S> {
    pub inner: S,
    pub closes: Arc<Mutex<Vec<(u16, String)>>>,
}

impl<S: Socket> Socket for CloseRecorder<S> {
    async fn send_text(&mut self, text: &str) -> Result<(), ProtocolError> {
        self.inner.send_text(text).await
    }

    async fn recv_text(&mut self) -> Result<Option<String>, ProtocolError> {
        self.inner.recv_text().await
    }

    async fn close(&mut self, code: u16, reason: &str) -> Result<(), ProtocolError> {
        self.closes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((code, reason.to_string()));
        self.inner.close(code, reason).await
    }
}

/// One invocation, as a bot would send it.
pub fn invocation() -> Invocation {
    Invocation::new(
        BotId::from_suffix(&"b".repeat(32)),
        ConversationId::from_suffix(&"c".repeat(32)),
    )
}

pub fn inv_value() -> Value {
    serde_json::to_value(invocation()).expect("serializes")
}

/// One invocation in both shapes, for a test that has to compare what it sent
/// with what the node forwarded. `invocation()` mints a fresh id every call, so
/// a test that builds the two separately is comparing two different invocations.
pub fn inv_pair() -> (Invocation, Value) {
    let inv = invocation();
    let value = serde_json::to_value(&inv).expect("serializes");
    (inv, value)
}

/// A `node.hello` challenge signed by `key`.
pub fn hello_params(key: &SigningKey, node_id: &NodeId, nonce: &str) -> Value {
    hello_params_ranged(key, node_id, nonce, ProtocolRange::current())
}

pub fn hello_params_ranged(
    key: &SigningKey,
    node_id: &NodeId,
    nonce: &str,
    protocol: ProtocolRange,
) -> Value {
    json!({
        "protocol": {"min": protocol.min, "max": protocol.max},
        "brainVersion": "0.1.0",
        "brainSig": sign_hello(key, node_id.as_str(), nonce, BRAIN_ORIGIN),
    })
}

/// The params a correctly signed `node.hello` for this rig.
pub fn good_hello(side: &NodeSide) -> Value {
    hello_params(&brain_key(), side.node_id(), NONCE)
}

/// Ask the node for `fs.read` of `path`.
pub fn fs_read(path: &str) -> Value {
    json!({"inv": inv_value(), "path": path})
}

pub fn fs_write(path: &str, content: &str) -> Value {
    json!({"inv": inv_value(), "path": path, "content": content})
}

pub fn shell(command: &str, timeout_ms: u64) -> Value {
    json!({
        "inv": inv_value(),
        "command": command,
        "cwd": "",
        "timeoutMs": timeout_ms,
    })
}

/// The typed error inside a `ProtocolError::Remote`, or a panic naming what
/// arrived instead.
pub fn typed(error: &ProtocolError) -> swarmbot_node_protocol::TypedError {
    match error.typed() {
        Some(error) => error.clone(),
        None => panic!("expected a typed error, got {error:?}"),
    }
}

/// A brain handler that answers `node.requestApproval` with a fixed decision
/// and records every request it received, so a test can assert the exact
/// payload the node sent.
pub struct ApprovalBrain {
    pub answer: ApprovalAnswer,
    pub requests: Arc<Mutex<Vec<RequestApproval>>>,
    /// The same requests as they arrived, so a test can assert the wire shape
    /// (`invocationId`, `botId`) and not just the decoded struct.
    pub raw: Arc<Mutex<Vec<Value>>>,
    /// Answer with a malformed body instead of an `ApprovalAnswer`.
    pub malformed: bool,
    /// Never answer at all: the call hangs until the socket dies, which is how
    /// an unreachable brain looks to a node.
    pub silent: bool,
}

impl ApprovalBrain {
    pub fn allowing() -> ApprovalBrain {
        ApprovalBrain::new(ApprovalAnswer {
            decision: swarmbot_contracts::Decision::AllowOnce,
            by: "owner".into(),
        })
    }

    pub fn denying() -> ApprovalBrain {
        ApprovalBrain::new(ApprovalAnswer {
            decision: swarmbot_contracts::Decision::Deny,
            by: "owner".into(),
        })
    }

    pub fn new(answer: ApprovalAnswer) -> ApprovalBrain {
        ApprovalBrain {
            answer,
            requests: Arc::new(Mutex::new(Vec::new())),
            raw: Arc::new(Mutex::new(Vec::new())),
            malformed: false,
            silent: false,
        }
    }

    pub fn malformed() -> ApprovalBrain {
        ApprovalBrain {
            malformed: true,
            ..ApprovalBrain::denying()
        }
    }

    pub fn silent() -> ApprovalBrain {
        ApprovalBrain {
            silent: true,
            ..ApprovalBrain::denying()
        }
    }

    /// An `Arc` copy sharing the same request log, for installing as a peer's
    /// handler while the test keeps a handle it can inspect.
    pub fn shared(&self) -> Arc<ApprovalBrain> {
        Arc::new(ApprovalBrain {
            answer: self.answer.clone(),
            requests: Arc::clone(&self.requests),
            raw: Arc::clone(&self.raw),
            malformed: self.malformed,
            silent: self.silent,
        })
    }

    pub fn received(&self) -> Vec<RequestApproval> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn received_raw(&self) -> Vec<Value> {
        self.raw.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl swarmbot_node_protocol::RpcHandler for ApprovalBrain {
    fn handle<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        _tx: Option<swarmbot_node_protocol::ChunkSink>,
    ) -> swarmbot_node_protocol::BoxFuture<'a, Result<Value, swarmbot_node_protocol::TypedError>>
    {
        Box::pin(async move {
            match method {
                "node.requestApproval" => {
                    self.raw
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(params.clone());
                    if self.silent {
                        // Hang: the node must treat an unanswered approval as a
                        // deny rather than waiting forever, and a socket that
                        // dies mid-ask must also deny.
                        std::future::pending::<()>().await;
                        Ok(Value::Null)
                    } else {
                        if let Ok(request) = serde_json::from_value::<RequestApproval>(params) {
                            self.requests
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(request);
                        }
                        if self.malformed {
                            Ok(json!({"decision": "maybe", "by": "nobody"}))
                        } else {
                            serde_json::to_value(&self.answer).map_err(|error| {
                                swarmbot_node_protocol::TypedError::policy_refused(
                                    method,
                                    error.to_string(),
                                    false,
                                )
                            })
                        }
                    }
                }
                "node.event" | "node.heartbeat" => Ok(Value::Null),
                other => Err(swarmbot_node_protocol::TypedError::policy_refused(
                    other,
                    "the test brain does not serve this",
                    false,
                )),
            }
        })
    }
}

/// The brain half of a connection, with an approval handler installed.
pub fn brain_peer(socket: BrainSocket, brain: Arc<ApprovalBrain>) -> BrainPeer {
    brain_peer_with(socket, brain)
}

/// The same, for a hand-written brain handler.
pub fn brain_peer_with(
    socket: BrainSocket,
    handler: Arc<dyn swarmbot_node_protocol::RpcHandler>,
) -> BrainPeer {
    Peer::new(
        socket,
        swarmbot_node_protocol::PeerConfig {
            handler: Some(handler),
            ping_interval: std::time::Duration::from_secs(3600),
            liveness_timeout: std::time::Duration::from_secs(3600),
            ..Default::default()
        },
    )
}

/// Complete a verified `node.hello` on a rig. Panics with the node's own error
/// if it does not succeed, because every later assertion depends on it.
pub async fn authenticate(brain: &BrainPeer, side: &NodeSide) -> Value {
    let hello = brain
        .call("node.hello", good_hello(side))
        .await
        .expect("node.hello succeeds");
    assert_eq!(hello["brainVerified"], json!(true), "{hello}");
    hello
}

/// A config whose ceilings are `ask` on one surface.
pub fn asking_config(surface: Surface) -> impl FnOnce(&Path, &Path) -> NodeConfig {
    move |dir: &Path, root: &Path| {
        let mut config = permissive_config(dir, root);
        config.auto_approve_read_only = false;
        config.ceilings.set(surface, PolicyCeiling::Ask);
        config
    }
}

/// A config that never allows one surface.
pub fn never_config(surface: Surface) -> impl FnOnce(&Path, &Path) -> NodeConfig {
    move |dir: &Path, root: &Path| {
        let mut config = permissive_config(dir, root);
        config.ceilings.set(surface, PolicyCeiling::Never);
        config
    }
}

/// A runtime that dials `brain_url`, with no socket attached: what `dial::serve`
/// needs, and what a redial test drives against a loopback listener.
pub fn runtime_in(dir: &Path, root: &Path, brain_url: &str) -> Arc<NodeRuntime> {
    let mut config = permissive_config(dir, root);
    config.brain_url = Some(brain_url.to_string());
    config
        .paths()
        .ensure_dirs(&config.workspace())
        .expect("directories");
    let identity = identity(dir);
    NodeRuntime::new(config, identity).expect("runtime")
}

/// A config whose `ask` ceilings are asked even for read-only surfaces.
pub fn asking_everything() -> impl FnOnce(&Path, &Path) -> NodeConfig {
    |dir: &Path, root: &Path| {
        let mut config = permissive_config(dir, root);
        config.ceilings = PolicyMap::uniform(PolicyCeiling::Ask);
        config.auto_approve_read_only = false;
        config
    }
}

/// The root of a rig, as a string a caller would send.
pub fn root_str(side: &NodeSide) -> String {
    side.root.display().to_string()
}

/// Read a root's mode out of a hello object.
pub fn root_mode(hello: &Value, id: &str) -> Option<String> {
    hello["roots"]
        .as_array()?
        .iter()
        .find(|root| root["id"] == json!(id))?["mode"]
        .as_str()
        .map(str::to_string)
}

pub fn read_only_root(dir: &Path, root: &Path) -> NodeConfig {
    let mut config = permissive_config(dir, root);
    let read_only = dir.join("reference");
    std::fs::create_dir_all(&read_only).expect("read-only root");
    config.roots.push(NodeRoot {
        id: "reference".into(),
        path: read_only.display().to_string(),
        mode: RootMode::ReadOnly,
    });
    config
}

// --- a hand-written fake brain over HTTP ------------------------------------
//
// Pairing is `POST /node/pair`, and there is no in-memory transport for HTTP, so
// this is the one place the tests bind a port (`127.0.0.1:0`). It answers every
// request with a canned status and body and records what it was asked for.

use std::net::SocketAddr;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// How a [`FakeBrain`] builds its response body, given the address it bound.
pub type BodyFn = Arc<dyn Fn(SocketAddr) -> String + Send + Sync + 'static>;

/// One recorded request: the request line plus headers, and the body.
#[derive(Clone, Debug)]
pub struct Recorded {
    head: Vec<String>,
    body: String,
}

impl Recorded {
    pub fn request_line(&self) -> &str {
        self.head.first().map(String::as_str).unwrap_or_default()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.head.iter().skip(1).find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.trim().eq_ignore_ascii_case(name))
                .map(|(_, value)| value.trim())
        })
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn pair_request(&self) -> swarmbot_contracts::PairRequest {
        serde_json::from_str(&self.body).expect("the body is a PairRequest")
    }
}

/// A canned HTTP responder on `127.0.0.1:0`.
pub struct FakeBrain {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
    task: JoinHandle<()>,
}

impl FakeBrain {
    pub async fn start(status: u16, body: String) -> FakeBrain {
        FakeBrain::start_with(status, Arc::new(move |_| body.clone())).await
    }

    /// A responder whose body is computed *after* the port is known. A pairing
    /// response has to name the origin the node dialed, and rebinding a port to
    /// learn it first is racy (the listener lingers), so the body is a function
    /// of the bound address instead.
    pub async fn start_with(status: u16, body: BodyFn) -> FakeBrain {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        FakeBrain::serve(listener, addr, status, body(addr)).await
    }

    async fn serve(
        listener: TcpListener,
        addr: SocketAddr,
        status: u16,
        body: String,
    ) -> FakeBrain {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let task = {
            let requests = Arc::clone(&requests);
            tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            let requests = Arc::clone(&requests);
                            let body = body.clone();
                            tokio::spawn(async move {
                                if let Some(recorded) = answer(stream, &body, status).await {
                                    requests
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .push(recorded);
                                }
                            });
                        }
                        Err(_) => return,
                    }
                }
            })
        };
        FakeBrain {
            addr,
            requests,
            task,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn received(&self) -> Vec<Recorded> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for FakeBrain {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Read one request, answer it, and return what was asked for.
async fn answer(stream: TcpStream, body: &str, status: u16) -> Option<Recorded> {
    // Owned halves, so the read side keeps the connection alive while the write
    // side answers it.
    let (reader, mut writer) = tokio::io::split(stream);
    let mut buf = BufReader::new(reader);
    let mut head = Vec::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        match buf.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
                if trimmed.is_empty() {
                    break;
                }
                if let Some((name, value)) = trimmed.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    content_length = value.trim().parse().unwrap_or(0);
                }
                head.push(trimmed);
            }
        }
    }
    let mut payload = vec![0u8; content_length];
    if content_length > 0 && buf.read_exact(&mut payload).await.is_err() {
        return None;
    }
    let response = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        if status < 400 { "OK" } else { "Error" },
        body.len()
    );
    let _ = writer.write_all(response.as_bytes()).await;
    let _ = writer.flush().await;
    let _ = writer.shutdown().await;
    Some(Recorded {
        head,
        body: String::from_utf8_lossy(&payload).into_owned(),
    })
}
