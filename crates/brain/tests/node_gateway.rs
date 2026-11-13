//! The node gateway end to end: a real brain, a real socket, and a node that
//! speaks the real protocol.
//!
//! The node side here is built from `swarmbot-node-protocol`'s own client pieces
//! (`connect_ws_with_headers`, `Peer`, `verify_hello`) rather than from the
//! `swarmbot-node` crate, which this crate may not depend on — the workspace
//! manifest has no entry for it and adding one is the owner's call. What that buys
//! is the part that matters: the frames on the wire are the frames the real daemon
//! sends, produced by the same library it uses, so a mismatch in a method name, a
//! field's casing or a close code fails here rather than in production.
//!
//! Two adapters live in this file because the protocol crate's own `WsSocket`
//! throws away the peer's close code — and 4401 ("you are revoked, stop
//! redialing") is only observable *from* that code. `RecordingSocket` keeps it, the
//! way `swarmbot-node`'s `NodeSocket` does.

mod common;

use common::*;

use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};

use swarmbot_contracts::Invocation;
use swarmbot_contracts::PROTOCOL_VERSION;
use swarmbot_contracts::id::{BotId, ConversationId, NodeId};
use swarmbot_contracts::node::{
    Capabilities, NodeRoot, PairRequest, PairResponse, PolicyCeiling, ProtocolRange,
};
use swarmbot_contracts::tool::{ToolError, ToolResult};
use swarmbot_node_protocol::auth::{upgrade_auth, verify_hello};
use swarmbot_node_protocol::identity::{NodeIdentity, parse_public_key_base64url};
use swarmbot_node_protocol::mux::{BoxFuture, ChunkSink, Peer, PeerConfig, RpcHandler, Socket};
use swarmbot_node_protocol::rpc::{
    self, FsRead, FsStat, HelloParams, HelloPolicy, NodeHello, OutputStream, RequestApproval,
    ShellChunk, ShellExec,
};
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::Message as WsMessage;
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::protocol::CloseFrame;
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes;
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use swarmbot_node_protocol::wire::{ProtocolError, TypedError};
use swarmbot_node_protocol::{CLOSE_REVOKED, PROTOCOL_VERSION as WIRE_VERSION};
use swarmbot_tools::NodeCall;

/// How long a test waits for something that is already on its way.
const BUDGET: Duration = Duration::from_secs(30);

/// The invocation a test acts as when it drives the [`swarmbot_tools::NodeExecutor`]
/// port itself rather than through a bot turn: these tests are the caller, so
/// they say who they are instead of leaving the brain to infer it.
fn as_caller() -> Invocation {
    Invocation::new(BotId::new(), ConversationId::new())
}

// --- the node side -----------------------------------------------------------

/// A `Socket` over tokio-tungstenite that remembers the peer's close code.
struct RecordingSocket {
    inner: swarmbot_node_protocol::tokio_tungstenite::WebSocketStream<
        swarmbot_node_protocol::tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    close_code: Arc<AtomicU16>,
}

impl Socket for RecordingSocket {
    async fn send_text(&mut self, text: &str) -> Result<(), ProtocolError> {
        self.inner
            .send(WsMessage::Text(Utf8Bytes::from(text)))
            .await
            .map_err(|error| ProtocolError::ws(error.to_string()))
    }

    async fn recv_text(&mut self) -> Result<Option<String>, ProtocolError> {
        loop {
            match self.inner.next().await {
                Some(Ok(WsMessage::Text(text))) => return Ok(Some(text.to_string())),
                Some(Ok(WsMessage::Ping(_)))
                | Some(Ok(WsMessage::Pong(_)))
                | Some(Ok(WsMessage::Binary(_)))
                | Some(Ok(WsMessage::Frame(_))) => continue,
                Some(Ok(WsMessage::Close(frame))) => {
                    if let Some(frame) = frame {
                        self.close_code.store(frame.code.into(), Ordering::SeqCst);
                    }
                    return Ok(None);
                }
                Some(Err(error)) => return Err(ProtocolError::ws(error.to_string())),
                None => return Ok(None),
            }
        }
    }

    async fn close(&mut self, code: u16, reason: &str) -> Result<(), ProtocolError> {
        let frame = CloseFrame {
            code: CloseCode::from(code),
            reason: Utf8Bytes::from(reason),
        };
        let _ = self.inner.send(WsMessage::Close(Some(frame))).await;
        let _ = self.inner.close(None).await;
        Ok(())
    }
}

type NodePeer = Peer<RecordingSocket>;

/// What the fake node was asked to do, in order.
#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    params: Value,
}

/// The node's answers, scripted by the test.
#[derive(Default)]
struct Script {
    /// `fs.stat` answers `null` for these paths (a missing file, not an error).
    missing: Mutex<Vec<String>>,
    /// `fs.read` answers with this text.
    read_body: Mutex<Option<String>>,
    /// `shell.exec` streams these lines on stdout, then this exit code.
    shell: Mutex<Option<(Vec<String>, i32)>>,
    /// Every method answers this typed error instead of running.
    refuse: Mutex<Option<TypedError>>,
}

/// The node's half of the protocol: verify the brain's hello, then serve.
struct FakeNode {
    identity: Arc<NodeIdentity>,
    /// The nonce from *this* connection's upgrade header, which the brain signs
    /// its hello over.
    nonce: String,
    script: Arc<Script>,
    calls: Arc<Mutex<Vec<Recorded>>>,
    hello_verified: Arc<AtomicBool>,
    authenticated: Arc<AtomicBool>,
}

impl RpcHandler for FakeNode {
    fn handle<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        tx: Option<ChunkSink>,
    ) -> BoxFuture<'a, Result<Value, TypedError>> {
        Box::pin(self.route(method, params, tx))
    }
}

impl FakeNode {
    async fn route(
        &self,
        method: &str,
        params: Value,
        tx: Option<ChunkSink>,
    ) -> Result<Value, TypedError> {
        // `node.hello` is the authentication itself, so it runs before the gate.
        if method == rpc::NODE_HELLO {
            return self.hello(params).await;
        }
        if !self.authenticated.load(Ordering::SeqCst) {
            return Err(TypedError::not_authenticated(
                "node.hello has not been verified on this connection",
            ));
        }
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Recorded {
                method: method.to_string(),
                params: params.clone(),
            });
        if let Some(refusal) = self
            .script
            .refuse
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Err(refusal);
        }
        match method {
            rpc::FS_STAT => {
                let payload: FsStat = rpc::decode_params(method, params)?;
                if self
                    .script
                    .missing
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&payload.path)
                {
                    // §5: a stat of a path that is not there is `null`, an answer.
                    return Ok(Value::Null);
                }
                Ok(serde_json::to_value(ToolResult::text(
                    "kind: file\nsize: 12\nmtime_ms: 1700000000000",
                ))
                .expect("a result serializes"))
            }
            rpc::FS_READ => {
                let payload: FsRead = rpc::decode_params(method, params)?;
                let body = self
                    .script
                    .read_body
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .unwrap_or_else(|| format!("the contents of {}", payload.path));
                Ok(serde_json::to_value(ToolResult::text(body)).expect("a result serializes"))
            }
            rpc::SHELL_EXEC => {
                let payload: ShellExec = rpc::decode_params(method, params)?;
                let sink = tx.ok_or_else(|| {
                    TypedError::policy_refused(method, "shell.exec needs a chunk sink", false)
                })?;
                let (lines, code) = self
                    .script
                    .shell
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .unwrap_or_else(|| (vec!["ran".to_string()], 0));
                sink.send(
                    serde_json::to_value(ShellChunk::Started {
                        pid: 4242,
                        pgid: 4242,
                    })
                    .expect("a chunk"),
                )
                .await
                .map_err(|error| TypedError::policy_refused(method, error.to_string(), false))?;
                for (seq, line) in lines.iter().enumerate() {
                    sink.send(
                        serde_json::to_value(ShellChunk::Output {
                            stream: OutputStream::Stdout,
                            data: format!("{line}\n"),
                            seq: seq as u64 + 1,
                        })
                        .expect("a chunk"),
                    )
                    .await
                    .map_err(|error| {
                        TypedError::policy_refused(method, error.to_string(), false)
                    })?;
                }
                sink.send(
                    serde_json::to_value(ShellChunk::Exit {
                        code: Some(code),
                        signal: None,
                        timed_out: false,
                        duration_ms: 7,
                        output_truncated: None,
                        spill_path: None,
                    })
                    .expect("a chunk"),
                )
                .await
                .map_err(|error| TypedError::policy_refused(method, error.to_string(), false))?;
                // A non-zero exit is data, not an RPC error: the stream ends null.
                let _ = payload;
                Ok(Value::Null)
            }
            other => Err(TypedError::policy_refused(
                other,
                "this fake node does not serve that method",
                false,
            )),
        }
    }

    async fn hello(&self, params: Value) -> Result<Value, TypedError> {
        let payload: HelloParams = rpc::decode_params(rpc::NODE_HELLO, params)?;
        let Some(protocol) = ProtocolRange::current().negotiate(payload.protocol) else {
            return Err(TypedError::policy_refused(
                rpc::NODE_HELLO,
                "no common protocol version",
                false,
            ));
        };
        let _ = protocol;
        // The anti-hijack check: the brain must sign over the key this node pinned
        // at pairing, the id it dialled as, this connection's nonce and the pinned
        // origin — not the host it happens to have connected to.
        if let Err(error) = verify_hello(
            &self.identity.brain_public_key,
            self.identity.node_id.as_str(),
            &self.nonce,
            &self.identity.brain_origin,
            &payload.brain_sig,
        ) {
            return Err(TypedError::not_authenticated(error.to_string()));
        }
        self.hello_verified.store(true, Ordering::SeqCst);
        self.authenticated.store(true, Ordering::SeqCst);
        let answer = NodeHello {
            node_id: self.identity.node_id.clone(),
            version: "0.1.0-fake".to_string(),
            hostname: "fake-node".to_string(),
            platform: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            protocol: PROTOCOL_VERSION,
            roots: vec![NodeRoot::read_write("work", "/tmp/fake-node/work")],
            capabilities: Capabilities::new(["fs@1", "shell@1"]),
            tunnels: Vec::new(),
            policy: HelloPolicy {
                ceiling: PolicyCeiling::Ask,
                remote_managed: false,
            },
            brain_verified: true,
        };
        serde_json::to_value(&answer)
            .map_err(|error| TypedError::policy_refused(rpc::NODE_HELLO, error.to_string(), false))
    }
}

/// A paired, dialled, hello-verified node.
struct Paired {
    identity: Arc<NodeIdentity>,
    peer: NodePeer,
    script: Arc<Script>,
    calls: Arc<Mutex<Vec<Recorded>>>,
    close_code: Arc<AtomicU16>,
    hello_verified: Arc<AtomicBool>,
}

impl Paired {
    fn node_id(&self) -> NodeId {
        self.identity.node_id.clone()
    }

    fn calls(&self) -> Vec<Recorded> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Wait until the brain has recorded `n` calls, and return them.
    async fn wait_for_calls(&self, count: usize) -> Vec<Recorded> {
        let deadline = tokio::time::Instant::now() + BUDGET;
        loop {
            let calls = self.calls();
            if calls.len() >= count {
                return calls;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the node only saw {} call(s), wanted {count}: {calls:?}",
                calls.len()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn close_code(&self) -> Option<u16> {
        let deadline = tokio::time::Instant::now() + BUDGET;
        loop {
            let code = self.close_code.load(Ordering::SeqCst);
            if code != 0 {
                return Some(code);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// Pair over HTTP exactly the way `swarmbot-node pair` does, then dial.
async fn pair_and_dial(brain: &TestBrain, code: &str) -> Paired {
    let keypair = NodeIdentity::generate_keypair().expect("a keypair");
    let public_key =
        swarmbot_node_protocol::identity::public_key_base64url(&keypair.verifying_key());
    let request = PairRequest {
        code: code.to_string(),
        public_key: public_key.clone(),
        hostname: "fake-node".to_string(),
        platform: std::env::consts::OS.to_string(),
        arch: Some(std::env::consts::ARCH.to_string()),
        node_version: Some("0.1.0-fake".to_string()),
        protocol: Some(ProtocolRange::current()),
    };
    let response = post(
        &TestBrain::client_with(""),
        &brain.url("/node/pair"),
        &serde_json::to_value(&request).expect("a request serializes"),
    )
    .await;
    assert_eq!(response.status().as_u16(), 200, "pairing is accepted");
    let paired: PairResponse =
        serde_json::from_value(body(response).await).expect("a PairResponse");
    assert_eq!(paired.brain_public_key.len(), 43, "a raw 32-byte key");
    let identity = Arc::new(
        NodeIdentity::from_pair_response(keypair, &paired).expect("the response pins a brain"),
    );
    assert_eq!(
        parse_public_key_base64url(&paired.brain_public_key)
            .expect("a parseable key")
            .to_bytes(),
        brain.state.identity.keypair.verifying_key().to_bytes(),
        "the node pinned this brain's key"
    );
    dial(brain, &identity, None).await
}

/// Dial the control socket with a signed upgrade header.
///
/// `header` overrides the computed one, for the tests that must be refused.
async fn dial(brain: &TestBrain, identity: &NodeIdentity, header: Option<String>) -> Paired {
    let (auth, nonce) = upgrade_auth(
        identity.node_id.as_str(),
        &identity.keypair,
        &identity.brain_origin,
    )
    .expect("a signed upgrade header");
    let url = format!("ws://{}/node", brain.addr);
    let (stream, response) = connect(&url, header.unwrap_or(auth)).await;
    assert_eq!(
        response.status().as_u16(),
        101,
        "the upgrade is accepted for a valid signature"
    );
    let close_code = Arc::new(AtomicU16::new(0));
    let script = Arc::new(Script::default());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hello_verified = Arc::new(AtomicBool::new(false));
    let handler = Arc::new(FakeNode {
        identity: Arc::new(identity.clone()),
        nonce: nonce.clone(),
        script: Arc::clone(&script),
        calls: Arc::clone(&calls),
        hello_verified: Arc::clone(&hello_verified),
        authenticated: Arc::new(AtomicBool::new(false)),
    });
    let peer = Peer::new(
        RecordingSocket {
            inner: stream,
            close_code: Arc::clone(&close_code),
        },
        PeerConfig {
            // The node gates every method until the brain's hello verified.
            require_hello: true,
            handler: Some(handler),
            ..PeerConfig::default()
        },
    );
    Paired {
        identity: Arc::new(identity.clone()),
        peer,
        script,
        calls,
        close_code,
        hello_verified,
    }
}

/// One WebSocket handshake, returning the stream and the HTTP response.
async fn connect(
    url: &str,
    authorization: String,
) -> (
    swarmbot_node_protocol::tokio_tungstenite::WebSocketStream<
        swarmbot_node_protocol::tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    swarmbot_node_protocol::tokio_tungstenite::tungstenite::http::Response<Option<Vec<u8>>>,
) {
    use swarmbot_node_protocol::tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use swarmbot_node_protocol::tokio_tungstenite::tungstenite::http::header::{
        HeaderName, HeaderValue,
    };

    let mut request = url.into_client_request().expect("a dialable URL");
    request.headers_mut().insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_str(&authorization).expect("an ascii header"),
    );
    request.headers_mut().insert(
        HeaderName::from_static("x-swarmbot-protocol"),
        HeaderValue::from_str(&format!("{WIRE_VERSION}-{WIRE_VERSION}")).expect("an ascii header"),
    );
    swarmbot_node_protocol::tokio_tungstenite::connect_async(request)
        .await
        .unwrap_or_else(|error| panic!("the handshake failed for {url}: {error}"))
}

/// Mint a pairing code through the owner's API.
async fn mint_code(brain: &TestBrain) -> String {
    let value =
        body(post_empty(&brain.client(), &brain.url("/api/nodes/pairing-codes")).await).await;
    value["code"].as_str().expect("a code").to_string()
}

/// Wait until the roster says the node is online.
async fn wait_for_online(brain: &TestBrain, node: &NodeId, online: bool) -> Value {
    let deadline = tokio::time::Instant::now() + BUDGET;
    loop {
        let nodes = array(get(&brain.client(), &brain.url("/api/nodes")).await).await;
        if let Some(found) = nodes
            .iter()
            .find(|value| value["id"] == json!(node.to_string()))
            && found["online"] == json!(online)
        {
            return found.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "node {node} never became online={online}: {nodes:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// --- the tests ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_pairs_dials_and_appears_in_the_roster() {
    let brain = TestBrain::start_durable().await;
    let code = mint_code(&brain).await;
    let node = pair_and_dial(&brain, &code).await;
    let id = node.node_id();

    // The brain called `node.hello` first, and the node verified its signature
    // against the key it pinned — the anti-hijack rule.
    let deadline = tokio::time::Instant::now() + BUDGET;
    while !node.hello_verified.load(Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the brain never sent a verifiable node.hello"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // The roster row is the hello's facts, and the node is online.
    let row = wait_for_online(&brain, &id, true).await;
    assert_eq!(row["kind"], json!("remote"));
    assert_eq!(row["hostname"], json!("fake-node"));
    assert_eq!(row["version"], json!("0.1.0-fake"));
    assert_eq!(row["protocol"], json!(PROTOCOL_VERSION));
    assert_eq!(row["revoked"], json!(false));
    let roots = row["roots"].as_array().expect("roots");
    assert_eq!(roots.len(), 1, "{roots:?}");
    assert_eq!(roots[0]["path"], json!("/tmp/fake-node/work"));
    assert_eq!(roots[0]["mode"], json!("read_write"));
    // Declared ∩ approved: the pairing redeemed the code, so the first hello's
    // declarations are what the owner approved.
    assert!(
        row["declared"].as_array().expect("caps").len() >= 2,
        "{row}"
    );
    assert_eq!(row["declared"], row["approved"], "{row}");
    // The hello carries one aggregate ceiling — the strictest the node will run
    // under — and the brain stores it as a floor it must not exceed.
    assert_eq!(row["policy"]["default"], json!("ask"), "{row}");

    // A heartbeat keeps `last_seen_at` honest.
    node.peer
        .call(
            rpc::NODE_HEARTBEAT,
            json!({"at": swarmbot_node_protocol::unix_ms_now(), "activeInvocations": 1u32, "load": null, "freeMemBytes": null}),
        )
        .await
        .expect("the heartbeat is answered");
    let row = wait_for_online(&brain, &id, true).await;
    assert!(row["last_seen_at"].is_string(), "{row}");

    // `node.connected` brings the label its operator gave it.
    node.peer
        .call(
            rpc::NODE_EVENT,
            json!({"type": rpc::EVENT_NODE_CONNECTED, "data": {"label": "the big one", "version": "0.1.0-fake", "capabilities": ["fs@1", "shell@1"], "roots": []}}),
        )
        .await
        .expect("the event is answered");
    let deadline = tokio::time::Instant::now() + BUDGET;
    loop {
        let nodes = array(get(&brain.client(), &brain.url("/api/nodes")).await).await;
        let row = nodes
            .iter()
            .find(|value| value["id"] == json!(id.to_string()))
            .expect("the node is listed");
        if row["label"] == json!("the big one") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the label never arrived: {row}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // An unknown event type is ignored, not refused (§5.2): a node newer than this
    // brain must not lose its connection by telling it something new.
    node.peer
        .call(
            rpc::NODE_EVENT,
            json!({"type": "browser.opened", "data": {}}),
        )
        .await
        .expect("an unknown event is ignored");

    // The methods the *node* serves are refused by name when asked of the brain.
    let error = node
        .peer
        .call(rpc::FS_READ, json!({"inv": null, "path": "x"}))
        .await
        .expect_err("the brain does not serve fs.read");
    assert!(
        error.to_string().contains("not served by the brain"),
        "{error}"
    );

    node.peer.close(1000, "test done").await;
    let row = wait_for_online(&brain, &id, false).await;
    assert_eq!(
        row["online"],
        json!(false),
        "a node that left is offline, not gone"
    );
    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_code_is_single_use_and_a_bad_signature_never_upgrades() {
    let brain = TestBrain::start_durable().await;
    let code = mint_code(&brain).await;
    let node = pair_and_dial(&brain, &code).await;
    let identity = Arc::clone(&node.identity);
    node.peer.close(1000, "done").await;

    // The same code again: burned.
    let keypair = NodeIdentity::generate_keypair().expect("a keypair");
    let request = PairRequest {
        code: code.clone(),
        public_key: swarmbot_node_protocol::identity::public_key_base64url(
            &keypair.verifying_key(),
        ),
        hostname: "second".to_string(),
        platform: "linux".to_string(),
        arch: None,
        node_version: None,
        protocol: None,
    };
    let response = post(
        &TestBrain::client_with(""),
        &brain.url("/node/pair"),
        &serde_json::to_value(&request).expect("serializes"),
    )
    .await;
    // The message does not say which of "never minted", "expired" and "used" it
    // was: telling a caller that would be an oracle for guessing codes.
    let burned = expect_error(response, 409, "conflict").await;
    assert!(burned.contains("not valid here"), "{burned}");

    // A code nobody ever minted must be refused with the *same* words, or the
    // difference between the two answers is exactly the oracle.
    let bogus = PairRequest {
        code: "ZZZZ-ZZZZ".to_string(),
        ..request.clone()
    };
    let response = post(
        &TestBrain::client_with(""),
        &brain.url("/node/pair"),
        &serde_json::to_value(&bogus).expect("serializes"),
    )
    .await;
    let never_minted = expect_error(response, 409, "conflict").await;
    assert_eq!(
        burned, never_minted,
        "a used code and an unknown one must be indistinguishable"
    );

    // A malformed code is a 400, and says so.
    let malformed = PairRequest {
        code: "not a code!".to_string(),
        ..request.clone()
    };
    let response = post(
        &TestBrain::client_with(""),
        &brain.url("/node/pair"),
        &serde_json::to_value(&malformed).expect("serializes"),
    )
    .await;
    expect_error(response, 400, "invalid_input").await;

    // A public key that is not one.
    let bad_key = PairRequest {
        code: mint_code(&brain).await,
        public_key: "not base64url at all!!".to_string(),
        ..request.clone()
    };
    let response = post(
        &TestBrain::client_with(""),
        &brain.url("/node/pair"),
        &serde_json::to_value(&bad_key).expect("serializes"),
    )
    .await;
    expect_error(response, 400, "invalid_input").await;

    // --- the upgrade --------------------------------------------------------
    let url = format!("ws://{}/node", brain.addr);

    // No header at all: an HTTP 401 with the envelope, and no upgrade. A node
    // reports "the brain refused me", not "the socket died".
    let error = handshake(&url, "Bearer not-the-node-scheme").await;
    assert_eq!(error.0, 401, "{error:?}");

    // A valid shape, signed by a key this brain never saw.
    let stranger = NodeIdentity::generate(
        parse_public_key_base64url(&identity_brain_key(&brain)).expect("a key"),
        identity.brain_origin.clone(),
    )
    .expect("an identity");
    let (header, _) = upgrade_auth(
        stranger.node_id.as_str(),
        &stranger.keypair,
        &stranger.brain_origin,
    )
    .expect("a header");
    let error = handshake(&url, &header).await;
    assert_eq!(error.0, 401);
    assert!(error.1.contains("not paired"), "{error:?}");

    // The right node, the wrong origin in the signature: the pin is what makes a
    // proxy or DNS hijack useless.
    let (header, _) = swarmbot_node_protocol::auth::make_upgrade_auth(
        identity.node_id.as_str(),
        &identity.keypair,
        "http://evil.example",
        swarmbot_node_protocol::unix_ms_now(),
        &swarmbot_node_protocol::new_nonce().expect("a nonce"),
    );
    let error = handshake(&url, &header).await;
    assert_eq!(error.0, 401);
    assert!(error.1.contains("does not verify"), "{error:?}");

    // A clock that is too far off.
    let (header, _) = swarmbot_node_protocol::auth::make_upgrade_auth(
        identity.node_id.as_str(),
        &identity.keypair,
        &identity.brain_origin,
        swarmbot_node_protocol::unix_ms_now() - 10 * 60 * 1000,
        &swarmbot_node_protocol::new_nonce().expect("a nonce"),
    );
    let error = handshake(&url, &header).await;
    assert_eq!(error.0, 401);
    assert!(error.1.contains("too far from the clock"), "{error:?}");

    // A replayed header — the same nonce twice. The first dial burns it.
    let (header, nonce) = upgrade_auth(
        identity.node_id.as_str(),
        &identity.keypair,
        &identity.brain_origin,
    )
    .expect("a header");
    let first = dial_with(&url, &header).await;
    assert_eq!(first.0, 101, "a fresh nonce upgrades");
    drop(first);
    let second = dial_with(&url, &header).await;
    assert_eq!(second.0, 401, "a replayed nonce does not");
    assert!(second.1.contains("already used"), "{second:?}");
    assert!(!nonce.is_empty());

    brain.stop().await;
}

/// The brain's public key, as the roster's owner would read it.
fn identity_brain_key(brain: &TestBrain) -> String {
    brain.state.identity.public_key_base64url()
}

/// A handshake that is expected to fail: `(status, body)`.
async fn handshake(url: &str, authorization: &str) -> (u16, String) {
    use swarmbot_node_protocol::tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use swarmbot_node_protocol::tokio_tungstenite::tungstenite::http::header::{
        HeaderName, HeaderValue,
    };

    let mut request = url.into_client_request().expect("a dialable URL");
    request.headers_mut().insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_str(authorization).expect("an ascii header"),
    );
    match swarmbot_node_protocol::tokio_tungstenite::connect_async(request).await {
        Ok(_) => panic!("{url} upgraded when it should have been refused"),
        Err(swarmbot_node_protocol::tokio_tungstenite::tungstenite::Error::Http(response)) => {
            let status = response.status().as_u16();
            let body = response.body().as_ref().map_or_else(String::new, |bytes| {
                String::from_utf8_lossy(bytes).to_string()
            });
            (status, body)
        }
        Err(error) => (0, error.to_string()),
    }
}

/// A handshake with an explicit header: `(status, body or empty)`.
async fn dial_with(url: &str, authorization: &str) -> (u16, String) {
    use swarmbot_node_protocol::tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use swarmbot_node_protocol::tokio_tungstenite::tungstenite::http::header::{
        HeaderName, HeaderValue,
    };

    let mut request = url.into_client_request().expect("a dialable URL");
    request.headers_mut().insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_str(authorization).expect("an ascii header"),
    );
    match swarmbot_node_protocol::tokio_tungstenite::connect_async(request).await {
        Ok((stream, response)) => {
            drop(stream);
            (response.status().as_u16(), String::new())
        }
        Err(swarmbot_node_protocol::tokio_tungstenite::tungstenite::Error::Http(response)) => {
            let status = response.status().as_u16();
            let body = response.body().as_ref().map_or_else(String::new, |bytes| {
                String::from_utf8_lossy(bytes).to_string()
            });
            (status, body)
        }
        Err(error) => (0, error.to_string()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_call_crosses_the_socket_and_comes_back() {
    let brain = TestBrain::start_durable().await;
    let code = mint_code(&brain).await;
    let node = pair_and_dial(&brain, &code).await;
    let id = node.node_id();
    wait_for_online(&brain, &id, true).await;

    // `fs.stat`: a ToolResult on the wire, decoded straight into the port's type.
    let result = brain
        .swarm
        .nodes
        .exec(
            &id,
            &as_caller(),
            NodeCall::FsStat {
                path: "notes/todo.md".to_string(),
            },
        )
        .await
        .expect("the call succeeds");
    assert!(!result.is_error);
    assert!(
        result.text_only().contains("kind: file"),
        "{}",
        result.text_only()
    );
    let calls = node.wait_for_calls(1).await;
    assert_eq!(calls[0].method, rpc::FS_STAT);
    assert_eq!(calls[0].params["path"], json!("notes/todo.md"));
    // The invocation travels with the call: it is what the node audits against and
    // what it echoes back in an approval ask.
    assert!(calls[0].params["inv"]["id"].is_string(), "{:?}", calls[0]);

    // A missing path is `null`, which the brain renders the way the local
    // executor renders a missing file: a bot must not get two stories.
    node.script
        .missing
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push("gone.txt".to_string());
    let error = brain
        .swarm
        .nodes
        .exec(
            &id,
            &as_caller(),
            NodeCall::FsStat {
                path: "gone.txt".to_string(),
            },
        )
        .await
        .expect_err("a missing path is an error here, as it is locally");
    assert!(error.to_string().contains("gone.txt"), "{error}");

    // `fs.read`.
    *node
        .script
        .read_body
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some("the file's contents".to_string());
    let result = brain
        .swarm
        .nodes
        .exec(
            &id,
            &as_caller(),
            NodeCall::FsRead {
                path: "notes/todo.md".to_string(),
                offset: None,
                limit: None,
                encoding: None,
            },
        )
        .await
        .expect("the read succeeds");
    assert_eq!(result.text_only(), "the file's contents");

    // `shell.exec` streams, and the brain renders it in the local shape.
    *node.script.shell.lock().unwrap_or_else(|e| e.into_inner()) =
        Some((vec!["hello".to_string(), "world".to_string()], 0));
    let result = brain
        .swarm
        .nodes
        .exec(
            &id,
            &as_caller(),
            NodeCall::ShellExec {
                command: "echo hello".to_string(),
                cwd: None,
                timeout_ms: None,
                env: None,
            },
        )
        .await
        .expect("the command runs");
    assert!(!result.is_error, "{}", result.text_only());
    assert_eq!(
        result.text_only(),
        "exit=0\n--- stdout ---\nhello\nworld",
        "a remote shell result reads like a local one"
    );
    assert_eq!(result.meta["exit_code"], json!(0));
    assert_eq!(result.meta["duration_ms"], json!(7));

    // A non-zero exit is data, not an RPC error.
    *node.script.shell.lock().unwrap_or_else(|e| e.into_inner()) =
        Some((vec!["boom".to_string()], 3));
    let result = brain
        .swarm
        .nodes
        .exec(
            &id,
            &as_caller(),
            NodeCall::ShellExec {
                command: "false".to_string(),
                cwd: None,
                timeout_ms: None,
                env: None,
            },
        )
        .await
        .expect("a non-zero exit is not a transport failure");
    assert!(result.is_error, "the model has to see it as a failure");
    assert!(
        result.text_only().starts_with("exit=3"),
        "{}",
        result.text_only()
    );

    // A typed refusal stays a refusal — a denial is data, not an error.
    *node.script.refuse.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(TypedError::path_denied("/etc/shadow", "outside roots"));
    let error = brain
        .swarm
        .nodes
        .exec(
            &id,
            &as_caller(),
            NodeCall::FsStat {
                path: "/etc/shadow".to_string(),
            },
        )
        .await
        .expect_err("the node refused");
    match error {
        ToolError::PathDenied { path, reason } => {
            assert_eq!(path, "/etc/shadow");
            assert_eq!(reason, "outside roots");
        }
        other => panic!("expected PathDenied, got {other:?}"),
    }
    *node.script.refuse.lock().unwrap_or_else(|e| e.into_inner()) = None;

    // A capability the owner has not approved is refused brain-side, without a
    // round trip: `declared ∩ approved` is what a node may be asked to do.
    let mut row = brain
        .swarm
        .store
        .get_node(&id)
        .expect("the roster reads")
        .expect("the node is registered");
    row.approved = Capabilities::new(["fs@1"]);
    brain
        .swarm
        .store
        .update_node(&row)
        .expect("the roster writes");
    let error = brain
        .swarm
        .nodes
        .exec(
            &id,
            &as_caller(),
            NodeCall::ShellExec {
                command: "true".to_string(),
                cwd: None,
                timeout_ms: None,
                env: None,
            },
        )
        .await
        .expect_err("shell is not approved for this node");
    assert!(error.to_string().contains("`shell` capability"), "{error}");
    // Reading is still approved, so the same node still serves `fs`.
    let result = brain
        .swarm
        .nodes
        .exec(
            &id,
            &as_caller(),
            NodeCall::FsStat {
                path: ".".to_string(),
            },
        )
        .await
        .expect("fs is approved");
    assert!(!result.is_error, "{}", result.text_only());
    row.approved = Capabilities::new(["fs@1", "shell@1"]);
    brain
        .swarm
        .store
        .update_node(&row)
        .expect("the roster writes");

    // A call to a node that is not connected fails rather than running locally —
    // the whole reason the dispatcher exists.
    let error = brain
        .swarm
        .nodes
        .exec(
            &NodeId::new(),
            &as_caller(),
            NodeCall::FsStat {
                path: ".".to_string(),
            },
        )
        .await
        .expect_err("no such node");
    assert!(error.to_string().contains("not connected"), "{error}");

    node.peer.close(1000, "done").await;
    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoking_a_connected_node_closes_it_with_4401() {
    let brain = TestBrain::start_durable().await;
    let code = mint_code(&brain).await;
    let node = pair_and_dial(&brain, &code).await;
    let id = node.node_id();
    wait_for_online(&brain, &id, true).await;

    // The owner revokes it through the API.
    let response = delete(&brain.client(), &brain.url(&format!("/api/nodes/{id}"))).await;
    assert_eq!(response.status().as_u16(), 204);

    // 4401 is the one code that makes a node stop redialing for good and tell its
    // operator to re-pair; anything else and it knocks every 30 s forever.
    assert_eq!(
        node.close_code().await,
        Some(CLOSE_REVOKED),
        "the node was not told it is revoked"
    );
    let row = wait_for_online(&brain, &id, false).await;
    assert_eq!(row["revoked"], json!(true));
    assert!(
        row["approved"].as_array().expect("caps").is_empty(),
        "a revoked node is approved for nothing: {row}"
    );

    // Work aimed at it is refused brain-side, and says it was revoked rather than
    // merely unreachable: the operator has to know a re-pair is the only fix.
    let error = brain
        .swarm
        .nodes
        .exec(
            &id,
            &as_caller(),
            NodeCall::FsStat {
                path: ".".to_string(),
            },
        )
        .await
        .expect_err("a revoked node runs nothing");
    assert!(error.to_string().contains("revoked"), "{error}");

    // And it cannot come back with the key it has: a redial is refused at the
    // handshake, before any upgrade.
    let (status, body) = dial_with(
        &format!("ws://{}/node", brain.addr),
        &upgrade_auth(
            id.as_str(),
            &node.identity.keypair,
            &node.identity.brain_origin,
        )
        .expect("a header")
        .0,
    )
    .await;
    assert_eq!(status, 401);
    assert!(body.contains("revoked"), "{body}");

    // The local node still cannot be revoked: it is this process.
    let message = expect_error(
        delete(&brain.client(), &brain.url("/api/nodes/nod_local")).await,
        409,
        "conflict",
    )
    .await;
    assert!(message.contains("local node"), "{message}");

    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_asking_for_approval_reaches_the_owner_and_back() {
    let brain = TestBrain::start_durable().await;
    let code = mint_code(&brain).await;
    let node = pair_and_dial(&brain, &code).await;
    let id = node.node_id();
    wait_for_online(&brain, &id, true).await;
    let lead = brain.bot("lead");

    // The node asks, as it does when its own ceiling says `ask`. The call blocks
    // until the owner answers, so it runs on its own task.
    let peer = node.peer.clone();
    let bot_id = lead.id.clone();
    let asked = tokio::spawn(async move {
        peer.call(
            rpc::NODE_REQUEST_APPROVAL,
            serde_json::to_value(RequestApproval {
                invocation_id: swarmbot_contracts::id::InvocationId::new(),
                bot_id,
                action: "shell".to_string(),
                target: "cargo test --workspace".to_string(),
                reason: "this node's `shell` ceiling is `ask`".to_string(),
            })
            .expect("serializes"),
        )
        .await
    });

    // The card appears in the bot's conversation, next to the work (D5.4).
    let deadline = tokio::time::Instant::now() + BUDGET;
    let request_id = loop {
        let pending = array(get(&brain.client(), &brain.url("/api/requests")).await).await;
        if let Some(card) = pending
            .iter()
            .find(|value| value["target"] == json!("cargo test --workspace"))
        {
            assert_eq!(card["surface"], json!("shell"));
            assert_eq!(card["bot_id"], json!(lead.id.to_string()));
            assert_eq!(
                card["conversation_id"],
                json!(lead.direct_conversation_id.to_string()),
                "the card sits in the bot's conversation, not in a corner"
            );
            assert_eq!(
                card["reason"].as_str().expect("a reason"),
                "this node's `shell` ceiling is `ask`"
            );
            break card["id"].as_str().expect("an id").to_string();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no card arrived for the node's ask: {pending:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    // The owner allows it, and the node hears "allow_once": an `allow_always`
    // belongs in the brain's rule list, and telling the node "always" would hand
    // it a standing permission the owner only gave this brain.
    let answered = body(
        post(
            &brain.client(),
            &brain.url(&format!("/api/requests/{request_id}/respond")),
            &json!({"decision": "allow_always", "answer": null}),
        )
        .await,
    )
    .await;
    assert_eq!(answered["decision"], json!("allow_always"));
    let answer = tokio::time::timeout(BUDGET, asked)
        .await
        .expect("the node is answered")
        .expect("the call did not panic")
        .expect("the brain answered without an error");
    assert_eq!(answer["decision"], json!("allow_once"), "{answer}");
    assert_eq!(answer["by"], json!("owner"), "{answer}");

    // "Always allow" wrote a rule, so the *same* action is answered at once now —
    // by the rule, with no second card.
    let peer = node.peer.clone();
    let bot_id = lead.id.clone();
    let second = peer
        .call(
            rpc::NODE_REQUEST_APPROVAL,
            serde_json::to_value(RequestApproval {
                invocation_id: swarmbot_contracts::id::InvocationId::new(),
                bot_id,
                action: "shell".to_string(),
                target: "cargo test --workspace".to_string(),
                reason: "asked again".to_string(),
            })
            .expect("serializes"),
        )
        .await
        .expect("answered");
    assert_eq!(second["decision"], json!("allow_once"), "{second}");
    assert_eq!(second["by"], json!("rule"), "{second}");
    let pending = array(get(&brain.client(), &brain.url("/api/requests")).await).await;
    assert!(
        pending.is_empty(),
        "a rule answered it, so no card: {pending:?}"
    );

    // A surface this brain does not know is refused rather than guessed at.
    let unknown = node
        .peer
        .call(
            rpc::NODE_REQUEST_APPROVAL,
            json!({"invocationId": "inv_x", "botId": "bot_x", "action": "desktop", "target": "x", "reason": "y"}),
        )
        .await
        .expect("answered");
    assert_eq!(unknown["decision"], json!("deny"), "{unknown}");
    assert!(
        unknown["by"]
            .as_str()
            .expect("a reason")
            .contains("desktop"),
        "{unknown}"
    );

    node.peer.close(1000, "done").await;
    brain.settle().await;
    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bot_bound_to_a_remote_node_runs_its_tools_there() {
    let brain = TestBrain::start_durable().await;
    let code = mint_code(&brain).await;
    let node = pair_and_dial(&brain, &code).await;
    let id = node.node_id();
    wait_for_online(&brain, &id, true).await;

    // A bot whose tools run on that node.
    let created = body(
        post(
            &brain.client(),
            &brain.url("/api/bots"),
            &json!({
                "name": "remote",
                "label": "Remote",
                "instructions": "you work on another machine",
                "backend": "fake",
                "node": id.to_string(),
            }),
        )
        .await,
    )
    .await;
    assert_eq!(created["node"], json!(id.to_string()));
    let bot: BotId = created["id"]
        .as_str()
        .expect("an id")
        .parse()
        .expect("a bot id");
    let dm: swarmbot_contracts::id::ConversationId = created["direct_conversation_id"]
        .as_str()
        .expect("a conversation")
        .parse()
        .expect("a conversation id");

    // The model asks for a file read; reading inside a root is `always`, so the
    // brain's own gate waves it through and the call crosses the socket.
    *node
        .script
        .read_body
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some("the remote file's contents".to_string());
    brain.script(
        swarmbot_llm::ScriptedTurn::text("read it")
            .with_tool_call("read_file", json!({"path": "notes/todo.md"})),
    );
    let sent = body(
        post(
            &brain.client(),
            &brain.url(&format!("/api/conversations/{dm}/messages")),
            &json!({"text": "what is in notes/todo.md?", "addressed": []}),
        )
        .await,
    )
    .await;
    assert_eq!(sent["targets"], json!([bot.to_string()]));
    brain.settle().await;

    let calls = node.wait_for_calls(1).await;
    assert_eq!(calls[0].method, rpc::FS_READ, "{calls:?}");
    assert_eq!(calls[0].params["path"], json!("notes/todo.md"));
    // The invocation the call travelled with names the bot that made it, so the
    // node's audit line — and any approval card — says who asked.
    assert_eq!(
        calls[0].params["inv"]["bot_id"],
        json!(bot.to_string()),
        "the remote call is attributed to the bot that made it: {calls:?}"
    );
    assert_eq!(
        calls[0].params["inv"]["conversation_id"],
        json!(dm.to_string())
    );

    // And the tool result reached the model: the transcript shows the call.
    let entries = brain.entries(&dm);
    assert!(
        entries.iter().any(|entry| matches!(
            &entry.kind,
            swarmbot_contracts::entry::EntryKind::ToolCall(call) if call.tool == "read_file"
        )),
        "the tool call is in the transcript: {entries:?}"
    );

    node.peer.close(1000, "done").await;
    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_brain_stops_cleanly_with_a_node_still_connected() {
    // A graceful shutdown waits for upgraded connections, so a control socket
    // nobody told about would keep the listener draining forever — `stop` asserts
    // the listener ends, which is the part that would hang.
    let brain = TestBrain::start_durable().await;
    let code = mint_code(&brain).await;
    let node = pair_and_dial(&brain, &code).await;
    let id = node.node_id();
    wait_for_online(&brain, &id, true).await;

    brain.stop().await;

    // And the node was told, rather than dropped: 1001 "going away", so it redials
    // a brain that comes back instead of concluding it was revoked.
    assert_eq!(node.close_code().await, Some(1001));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_dial_replaces_the_first_connection() {
    let brain = TestBrain::start_durable().await;
    let code = mint_code(&brain).await;
    let first = pair_and_dial(&brain, &code).await;
    let id = first.node_id();
    wait_for_online(&brain, &id, true).await;

    // A node that redials before its old socket was torn down must not leave two
    // peers answering for one machine.
    let second = dial(&brain, &first.identity, None).await;
    assert_eq!(
        first.close_code().await,
        Some(swarmbot_node_protocol::CLOSE_REPLACED),
        "the replaced connection is told it was replaced"
    );
    wait_for_online(&brain, &id, true).await;

    // The new connection is the one the roster serves.
    let result = brain
        .swarm
        .nodes
        .exec(
            &id,
            &as_caller(),
            NodeCall::FsStat {
                path: ".".to_string(),
            },
        )
        .await
        .expect("the live connection answers");
    assert!(!result.is_error, "{}", result.text_only());
    second.peer.close(1000, "done").await;
    brain.stop().await;
}
