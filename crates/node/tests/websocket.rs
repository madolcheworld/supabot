//! The one end-to-end test that runs over a **real WebSocket**: a hand-written
//! brain on `127.0.0.1:0` and `dial::serve` in a task, with nothing mocked below
//! the socket.
//!
//! The in-memory tests in the other files cover the policy and the wire shapes;
//! this one covers the pieces only a real transport exercises — [`NodeSocket`]'s
//! framing and close-code capture, the signed `Authorization` header on the way
//! out, and `dial::serve`'s whole loop (dial → hello → serve → redial/stop).
//!
//! The brain here is hand-written rather than `swarmbot-brain` because that
//! crate's node gateway is still a 501/1011 stub; `tests/brain_end_to_end.rs`
//! carries the recipe for replacing this when it lands.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use swarmbot_contracts::{NodeId, PolicyCeiling, PolicyMap};
use swarmbot_node::NodeRuntime;
use swarmbot_node::dial::{self, DialPlan, ServeOptions, StopReason};
use swarmbot_node_protocol::mux::{Peer, PeerConfig};
use swarmbot_node_protocol::rpc;
use swarmbot_node_protocol::{BoxFuture, ChunkSink, RpcHandler, TypedError};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

/// Everything the scripted brain observed, so the test can assert on it after
/// the connection is over.
#[derive(Default)]
struct Observed {
    upgrade: Option<String>,
    protocol_header: Option<String>,
    hello: Option<Value>,
    read: Option<Value>,
    chunks: Vec<Value>,
    /// Params of every `node.event` / `node.heartbeat` the node sent.
    inbound: Vec<(String, Value)>,
    note: Option<String>,
}

/// Records what the node calls *it* with, which is the brain-served half of the
/// protocol (§6).
struct Recorder(Arc<Mutex<Observed>>);

impl RpcHandler for Recorder {
    fn handle<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        _tx: Option<ChunkSink>,
    ) -> BoxFuture<'a, Result<Value, TypedError>> {
        Box::pin(async move {
            match method {
                rpc::NODE_EVENT | rpc::NODE_HEARTBEAT => {
                    self.0
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .inbound
                        .push((method.to_string(), params));
                    Ok(Value::Null)
                }
                other => Err(TypedError::policy_refused(
                    other,
                    "the scripted brain does not serve this",
                    false,
                )),
            }
        })
    }
}

/// Captures the upgrade headers, which is where the signed `Authorization` and
/// the nonce the brain must sign `node.hello` over arrive.
#[derive(Clone, Default)]
struct HeaderCapture(Arc<Mutex<(Option<String>, Option<String>)>>);

impl swarmbot_node_protocol::WsCallback for HeaderCapture {
    // The trait takes the callback by value; the capture lives in an `Arc`, so
    // the handle the test kept still sees what was recorded.
    fn on_request(
        self,
        request: &swarmbot_node_protocol::WsRequest,
        response: swarmbot_node_protocol::WsResponse,
    ) -> Result<swarmbot_node_protocol::WsResponse, swarmbot_node_protocol::WsErrorResponse> {
        let headers = request.headers();
        let authorization = headers
            .get("Authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let protocol = headers
            .get("x-swarmbot-protocol")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *slot = (authorization, protocol);
        // The upgrade is accepted unconditionally here: the signature is checked
        // by `node.hello`, which is the part this crate implements. A real brain
        // answers 401 without upgrading instead (protocol doc §3.1, §12).
        Ok(response)
    }
}

impl HeaderCapture {
    fn get(&self) -> (Option<String>, Option<String>) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn nonce_of(authorization: &str) -> Option<String> {
    authorization
        .split(',')
        .find_map(|part| part.strip_prefix("nonce="))
        .map(str::to_string)
}

#[tokio::test]
async fn a_node_dials_signs_hellos_and_serves_over_a_real_websocket() {
    let (tmp, root) = temp_root();
    let brain_key = SigningKey::from_bytes(&[4u8; 32]);

    // A node configured for this test: one root, every ceiling `always` so the
    // exchange needs no approval round-trip, and a 1 s heartbeat so the test
    // does not have to wait 15 s to see one.
    let mut config = permissive_config(tmp.path(), &root);
    config.ceilings = PolicyMap::uniform(PolicyCeiling::Always);
    config.heartbeat_secs = 1;
    config
        .paths()
        .ensure_dirs(&config.workspace())
        .expect("directories");
    let identity = identity_with(tmp.path(), &brain_key);
    let node_id: NodeId = (*identity.node_id()).clone();
    let signed_for: NodeId = node_id.clone();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    config.brain_url = Some(format!("ws://{addr}"));
    let runtime = NodeRuntime::new(config, Arc::clone(&identity)).expect("runtime");
    runtime.paths.ensure_dirs(&root).expect("directories");
    std::fs::write(root.join("hello.txt"), "over a real socket\n").expect("seed");

    let observed = Arc::new(Mutex::new(Observed::default()));
    let done = Arc::new(Notify::new());
    let server = {
        let observed = Arc::clone(&observed);
        let done = Arc::clone(&done);
        let node_id = signed_for;
        tokio::spawn(async move {
            let mut served = false;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                if served {
                    // A redial after the script finished: hang up. The test's
                    // shutdown token ends `serve` before this can loop for long.
                    drop(stream);
                    continue;
                }
                served = true;
                let capture = HeaderCapture::default();
                let Ok(socket) =
                    swarmbot_node_protocol::accept_ws_hdr(stream, capture.clone()).await
                else {
                    continue;
                };
                let (authorization, protocol_header) = capture.get();
                let Some(nonce) = authorization.as_deref().and_then(nonce_of) else {
                    observed.lock().unwrap_or_else(|e| e.into_inner()).note =
                        Some("the upgrade carried no nonce".into());
                    done.notify_one();
                    return;
                };
                let recorder = Arc::new(Recorder(Arc::clone(&observed)));
                let peer = Peer::new(
                    socket,
                    PeerConfig {
                        // The brain side of the gate: it *calls* node.hello.
                        require_hello: false,
                        handler: Some(recorder as swarmbot_node_protocol::Handler),
                        ping_interval: Duration::from_secs(3600),
                        liveness_timeout: Duration::from_secs(3600),
                    },
                );
                {
                    let mut slot = observed.lock().unwrap_or_else(|e| e.into_inner());
                    slot.upgrade = authorization;
                    slot.protocol_header = protocol_header;
                }

                // 1. Mutual authentication: the brain signs over *this*
                //    connection's nonce and the origin the node pinned.
                let hello = peer
                    .call(rpc::NODE_HELLO, hello_params(&brain_key, &node_id, &nonce))
                    .await;
                // 2. A real fs.read.
                let read = peer.call(rpc::FS_READ, fs_read("hello.txt")).await;
                // 3. A real streaming shell.exec.
                let (tx, mut rx) = mpsc::channel(64);
                let chunks = {
                    let peer = peer.clone();
                    tokio::spawn(async move {
                        let call = peer
                            .call_stream(rpc::SHELL_EXEC, shell("echo streamed", 30_000), tx)
                            .await;
                        let _ = call;
                    })
                };
                let mut collected = Vec::new();
                while let Some(value) = rx.recv().await {
                    collected.push(value);
                }
                let _ = chunks.await;
                // 4. Long enough for one heartbeat at a 1 s period.
                tokio::time::sleep(Duration::from_millis(1500)).await;
                {
                    let mut slot = observed.lock().unwrap_or_else(|e| e.into_inner());
                    slot.hello = hello.ok();
                    slot.read = read.ok();
                    slot.chunks = collected;
                }
                done.notify_one();
                peer.closed().await;
                return;
            }
        })
    };

    let shutdown = CancellationToken::new();
    let guard = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            shutdown.cancel();
        })
    };
    let serve = {
        let runtime = Arc::clone(&runtime);
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            dial::serve(
                runtime,
                shutdown,
                ServeOptions {
                    plan: DialPlan {
                        initial: Duration::from_millis(50),
                        ..DialPlan::default()
                    },
                    state: None,
                },
            )
            .await
        })
    };

    tokio::time::timeout(Duration::from_secs(20), done.notified())
        .await
        .expect("the scripted brain finished its exchange");
    shutdown.cancel();
    let reason = tokio::time::timeout(Duration::from_secs(20), serve)
        .await
        .expect("serve returns")
        .expect("the task does not panic")
        .expect("serve reports rather than fails");
    guard.abort();
    server.abort();
    assert_eq!(reason, StopReason::Shutdown);

    let observed = observed.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        observed.note.is_none(),
        "the scripted brain complained: {:?}",
        observed.note
    );

    // The upgrade carried the signed header and the informational version.
    let upgrade = observed.upgrade.clone().expect("an Authorization header");
    assert!(upgrade.starts_with("SwarmBot-Node v1 id="), "{upgrade}");
    assert!(upgrade.contains(&format!("id={node_id}")), "{upgrade}");
    assert!(
        upgrade.contains(",ts=") && upgrade.contains(",sig="),
        "{upgrade}"
    );
    assert_eq!(observed.protocol_header.as_deref(), Some("1-1"));
    // The header carries a signature, never the private key.
    assert!(
        !upgrade.contains(&hex::encode(identity.signing_key().to_bytes())),
        "{upgrade}"
    );

    // The hello answer is the truthful one, over a real socket.
    let hello = observed.hello.clone().expect("node.hello succeeded");
    assert_eq!(hello["nodeId"], json!(node_id.to_string()));
    assert_eq!(hello["brainVerified"], json!(true));
    assert_eq!(hello["capabilities"], json!(["fs@1", "shell@1"]));
    assert_eq!(hello["roots"][0]["mode"], json!("read_write"));

    // fs.read returned the file's contents through the real transport.
    let read = observed.read.clone().expect("fs.read succeeded");
    assert!(
        read["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("over a real socket")),
        "{read}"
    );

    // shell.exec streamed Started → Output → Exit, with Acks in between.
    let tags: Vec<&str> = observed
        .chunks
        .iter()
        .map(|chunk| chunk["_tag"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(tags.first().copied(), Some("Started"), "{tags:?}");
    assert_eq!(tags.last().copied(), Some("Exit"), "{tags:?}");
    assert!(tags.contains(&"Output"), "{tags:?}");
    let stdout: String = observed
        .chunks
        .iter()
        .filter(|chunk| chunk["_tag"] == "Output" && chunk["stream"] == "stdout")
        .map(|chunk| chunk["data"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(stdout, "streamed\n");
    let exit = observed.chunks.last().cloned().expect("an Exit chunk");
    assert_eq!(exit["code"], json!(0), "{exit}");

    // And the node called the brain's methods: `node.connected` right after the
    // hello, then heartbeats.
    let events: Vec<&str> = observed
        .inbound
        .iter()
        .filter(|(method, _)| method == rpc::NODE_EVENT)
        .map(|(_, params)| params["type"].as_str().unwrap_or_default())
        .collect();
    assert!(
        events.contains(&"node.connected"),
        "the node announced itself: {events:?}"
    );
    let beats: Vec<&Value> = observed
        .inbound
        .iter()
        .filter(|(method, _)| method == rpc::NODE_HEARTBEAT)
        .map(|(_, params)| params)
        .collect();
    assert!(
        !beats.is_empty(),
        "a 1 s heartbeat period must produce at least one beat"
    );
    assert!(
        beats[0]["at"].as_i64().is_some_and(|at| at > 0),
        "{:?}",
        beats[0]
    );
    assert!(
        beats[0]["activeInvocations"].as_u64().is_some(),
        "the heartbeat carries the active-invocation count: {:?}",
        beats[0]
    );
    let connected = observed
        .inbound
        .iter()
        .find(|(method, _)| method == rpc::NODE_EVENT)
        .map(|(_, params)| params.clone())
        .expect("an event");
    assert_eq!(connected["data"]["label"], json!("test node"));
    assert_eq!(
        connected["data"]["capabilities"],
        json!(["fs@1", "shell@1"]),
        "{connected}"
    );
}
