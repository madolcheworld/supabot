//! End-to-end over a real WebSocket on `127.0.0.1:0` — the only test in this
//! crate that opens a socket, and it binds the loopback ephemeral port, uses
//! it once and closes it (the listener is dropped right after `accept`).
//!
//! What it proves, in one flow (§3 + §4 + §8 checklist):
//!
//! 1. a node with a generated identity signs its upgrade header;
//! 2. the brain verifies it during the handshake with a lookup closure
//!    (and answers `401` without upgrading when it fails — second test);
//! 3. `node.hello` completes with the brain's signature over the upgrade
//!    nonce, and only then does the gated node answer anything else;
//! 4. a unary `fs.stat` request/response round-trips;
//! 5. a 3-chunk `shell.exec` stream round-trips with per-chunk Acks.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::timeout;

use swarmbot_node_protocol::ed25519_dalek::{SigningKey, VerifyingKey};
use swarmbot_node_protocol::rpc::{
    FS_STAT, FileKind, FileStat, FsStat, HelloParams, HelloPolicy, NODE_HELLO, NodeHello,
    OutputStream, SHELL_EXEC, ShellChunk, ShellExec, decode_params, unimplemented,
};
use swarmbot_node_protocol::serde_json::{self, Value};
use swarmbot_node_protocol::swarmbot_contracts::{
    BotId, Capabilities, ConversationId, Invocation, NodeId, NodeRoot, PolicyCeiling, ProtocolRange,
};
use swarmbot_node_protocol::{
    BrainIdentity, ChunkSink, KeyLookup, NodeIdentity, Peer, PeerConfig, ReplayCache, RpcHandler,
    TypedError, WsRequest, WsResponse, accept_ws_hdr, brain_origin_from_url, connect_ws,
    make_upgrade_auth, new_nonce, sign_hello, unix_ms_now, upgrade_auth, verify_hello,
    verify_upgrade_auth,
};

/// Everything the node side needs to answer `node.hello` and friends.
struct NodeHandler {
    node_id: String,
    /// The **pinned** brain key (§2.3): the only key a hello signature may
    /// verify against.
    brain_key: VerifyingKey,
    /// The nonce from this connection's upgrade — hello is bound to it.
    upgrade_nonce: String,
    origin: String,
}

type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl RpcHandler for NodeHandler {
    fn handle<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        tx: Option<ChunkSink>,
    ) -> Boxed<'a, Result<Value, TypedError>> {
        Box::pin(async move {
            match method {
                NODE_HELLO => {
                    let hello: HelloParams = decode_params(NODE_HELLO, params)?;
                    // §3.2: verify with the pinned key over id+nonce+origin.
                    verify_hello(
                        &self.brain_key,
                        &self.node_id,
                        &self.upgrade_nonce,
                        &self.origin,
                        &hello.brain_sig,
                    )
                    .map_err(|e| TypedError::not_authenticated(e.to_string()))?;
                    let protocol = hello
                        .protocol
                        .negotiate(ProtocolRange::current())
                        .ok_or_else(|| {
                            TypedError::policy_refused(
                                NODE_HELLO,
                                "no common protocol version",
                                false,
                            )
                        })?;
                    let answer = NodeHello {
                        node_id: NodeId::parse(&self.node_id).map_err(|e| {
                            TypedError::policy_refused(NODE_HELLO, e.to_string(), false)
                        })?,
                        version: "0.1.0-test".into(),
                        hostname: "test-node".into(),
                        platform: std::env::consts::OS.into(),
                        arch: std::env::consts::ARCH.into(),
                        protocol,
                        roots: vec![NodeRoot::read_write("work", "/tmp/swarmbot-test")],
                        capabilities: Capabilities::new(["fs@1", "shell@1"]),
                        tunnels: vec![],
                        policy: HelloPolicy {
                            ceiling: PolicyCeiling::Ask,
                            remote_managed: false,
                        },
                        brain_verified: true,
                    };
                    Ok(serde_json::to_value(&answer).unwrap())
                }
                FS_STAT => {
                    let _request: FsStat = decode_params(FS_STAT, params)?;
                    Ok(serde_json::to_value(FileStat {
                        kind: FileKind::File,
                        size: 3,
                        mtime_ms: 1_700_000_000_000,
                    })
                    .unwrap())
                }
                SHELL_EXEC => {
                    let _request: ShellExec = decode_params(SHELL_EXEC, params)?;
                    let sink = tx.ok_or_else(|| {
                        TypedError::policy_refused(SHELL_EXEC, "no stream channel", false)
                    })?;
                    let chunks = [
                        ShellChunk::Started {
                            pid: 4242,
                            pgid: 4242,
                        },
                        ShellChunk::Output {
                            stream: OutputStream::Stdout,
                            data: "hello\n".into(),
                            seq: 0,
                        },
                        ShellChunk::Exit {
                            code: Some(0),
                            signal: None,
                            timed_out: false,
                            duration_ms: 7,
                            output_truncated: Some(false),
                            spill_path: None,
                        },
                    ];
                    for chunk in chunks {
                        sink.send(serde_json::to_value(&chunk).unwrap())
                            .await
                            .map_err(|e| {
                                TypedError::policy_refused(SHELL_EXEC, e.to_string(), false)
                            })?;
                    }
                    Ok(Value::Null)
                }
                // §5 subset: everything else the reference protocol defines
                // gets an honest typed refusal, unknown names too.
                other => Err(unimplemented(other)
                    .unwrap_or_else(|| TypedError::policy_refused(other, "unknown method", false))),
            }
        })
    }
}

// tungstenite's `Callback` returns `Result<Response, http::Response<Option<String>>>`;
// the large Err variant is fixed by the dependency's own signature.
#[allow(clippy::result_large_err)]
fn handshake_capture(
    captured: Arc<Mutex<Option<String>>>,
) -> impl FnOnce(&WsRequest, WsResponse) -> Result<WsResponse, swarmbot_node_protocol::WsErrorResponse>
{
    move |req: &WsRequest, res: WsResponse| {
        if let Some(value) = req
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
        {
            *captured.lock().unwrap() = Some(value.to_string());
        }
        Ok(res)
    }
}

#[tokio::test]
async fn signed_upgrade_hello_stat_and_stream_end_to_end() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{addr}/node");
    let origin = brain_origin_from_url(&url).unwrap();
    assert_eq!(origin, format!("http://{addr}"));

    // Identities as they would exist after pairing: the node pinned the
    // brain's key, the brain's roster knows the node's.
    let brain = BrainIdentity::generate(&origin).unwrap();
    let node = NodeIdentity::generate(brain.keypair.verifying_key(), &origin).unwrap();

    let known_id = node.node_id.to_string();
    let known_key = node.keypair.verifying_key();
    let replay = Arc::new(ReplayCache::new());
    let brain_task = {
        let replay = Arc::clone(&replay);
        let origin = origin.clone();
        let brain_keypair = brain.keypair.clone();
        // The task needs 'static copies; the originals stay with the test
        // for the replay assertion at the end.
        let task_id = known_id.clone();
        tokio::spawn(async move {
            let (tcp, _peer) = listener.accept().await.unwrap();
            // The test must not leave the ephemeral port bound.
            drop(listener);

            // Capture the Authorization header during the handshake and
            // verify it exactly as the brain's upgrade handler would (§3.1).
            let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let ws = accept_ws_hdr(tcp, handshake_capture(Arc::clone(&captured)))
                .await
                .unwrap();
            let header = captured
                .lock()
                .unwrap()
                .clone()
                .expect("an Authorization header");
            let verified = verify_upgrade_auth(
                &header,
                &origin,
                &|id: &str| {
                    if id == task_id {
                        KeyLookup::Active(known_key)
                    } else {
                        KeyLookup::Unknown
                    }
                },
                &replay,
                unix_ms_now(),
            )
            .expect("the signed upgrade must verify");

            let peer = Peer::new(ws, PeerConfig::default());

            // 1. node.hello, signed over the node id + the upgrade nonce.
            let hello_params = HelloParams {
                protocol: ProtocolRange::current(),
                brain_version: "0.1.0-test".into(),
                brain_sig: sign_hello(&brain_keypair, &verified.node_id, &verified.nonce, &origin),
            };
            let hello_value = timeout(
                Duration::from_secs(10),
                peer.call(NODE_HELLO, serde_json::to_value(&hello_params).unwrap()),
            )
            .await
            .expect("hello timed out")
            .expect("hello failed");
            let hello: NodeHello = serde_json::from_value(hello_value).unwrap();

            // 2. A unary fs.stat round-trip.
            let inv = Invocation::new(BotId::new(), ConversationId::new());
            let stat_value = timeout(
                Duration::from_secs(10),
                peer.call(
                    FS_STAT,
                    serde_json::to_value(FsStat {
                        inv: inv.clone(),
                        path: "/tmp/swarmbot-test/x".into(),
                    })
                    .unwrap(),
                ),
            )
            .await
            .expect("stat timed out")
            .expect("stat failed");
            let stat: FileStat = serde_json::from_value(stat_value).unwrap();

            // 3. A 3-chunk shell.exec stream with per-chunk Acks (§4.6).
            //    The call must run concurrently with the chunk collection,
            //    so it gets its own task.
            let (out_tx, mut out_rx) = mpsc::channel(8);
            let streaming = {
                let peer = peer.clone();
                tokio::spawn(async move {
                    peer.call_stream(
                        SHELL_EXEC,
                        serde_json::to_value(ShellExec {
                            inv,
                            command: "echo hello".into(),
                            cwd: "/tmp/swarmbot-test".into(),
                            timeout_ms: 5_000,
                            env: None,
                            output_limit_bytes: None,
                        })
                        .unwrap(),
                        out_tx,
                    )
                    .await
                })
            };
            let mut chunks = Vec::new();
            while let Some(value) = out_rx.recv().await {
                chunks.push(serde_json::from_value::<ShellChunk>(value).unwrap());
            }
            timeout(Duration::from_secs(10), streaming)
                .await
                .expect("stream timed out")
                .expect("stream task panicked")
                .expect("stream failed");

            peer.close(1000, "test done").await;
            (hello, stat, chunks)
        })
    };

    // The node dials out with a freshly signed upgrade header (§8.2).
    let nonce = new_nonce().unwrap();
    let (header, nonce_b64) = make_upgrade_auth(
        node.node_id.as_str(),
        &node.keypair,
        &origin,
        unix_ms_now(),
        &nonce,
    );
    let socket = timeout(Duration::from_secs(10), connect_ws(&url, &header))
        .await
        .expect("dial timed out")
        .expect("dial failed");
    let handler = Arc::new(NodeHandler {
        node_id: node.node_id.to_string(),
        brain_key: node.brain_public_key,
        upgrade_nonce: nonce_b64,
        origin: origin.clone(),
    });
    let peer = Peer::new(
        socket,
        PeerConfig {
            // Node side: nothing is served until hello verified (§3.2).
            require_hello: true,
            handler: Some(handler),
            ..Default::default()
        },
    );

    // The brain drives the whole conversation; join it first so a failure
    // there surfaces with its own message, then check the node observed the
    // brain's close (1000) as a clean shutdown.
    let (hello, stat, chunks) = timeout(Duration::from_secs(20), brain_task)
        .await
        .expect("brain task timed out")
        .expect("brain task panicked");

    timeout(Duration::from_secs(10), peer.closed())
        .await
        .expect("the node peer never saw the connection close");
    assert!(peer.is_closed());

    assert_eq!(hello.node_id, node.node_id);
    assert!(hello.brain_verified);
    assert_eq!(hello.protocol, swarmbot_node_protocol::PROTOCOL_VERSION);
    assert!(hello.capabilities.has("fs"));
    assert_eq!(
        stat,
        FileStat {
            kind: FileKind::File,
            size: 3,
            mtime_ms: 1_700_000_000_000,
        }
    );
    assert_eq!(chunks.len(), 3);
    assert!(matches!(
        chunks[0],
        ShellChunk::Started {
            pid: 4242,
            pgid: 4242
        }
    ));
    assert!(
        matches!(&chunks[1], ShellChunk::Output { stream: OutputStream::Stdout, data, seq: 0 } if data == "hello\n")
    );
    assert!(matches!(
        &chunks[2],
        ShellChunk::Exit {
            code: Some(0),
            signal: None,
            timed_out: false,
            ..
        }
    ));

    // The nonce is burned: a second dial replaying the same header must be
    // refused (checked here so the assertion belongs to the same scenario).
    assert!(
        verify_upgrade_auth(
            &header,
            &origin,
            &|id: &str| {
                if id == known_id {
                    KeyLookup::Active(known_key)
                } else {
                    KeyLookup::Unknown
                }
            },
            &replay,
            unix_ms_now(),
        )
        .is_err()
    );
}

#[tokio::test]
// The 401 ErrorResponse is `http::Response<Option<String>>` — its size is
// tungstenite's choice, not ours.
#[allow(clippy::result_large_err)]
async fn a_bad_upgrade_signature_is_rejected_with_401_before_upgrading() {
    use swarmbot_node_protocol::tokio_tungstenite::tungstenite::http::StatusCode;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{addr}/node");
    let origin = brain_origin_from_url(&url).unwrap();

    // The roster knows one node; the impostor signs with a different key.
    let known = SigningKey::from_bytes(&[1u8; 32]);
    let known_id = NodeId::new();
    let known_key = known.verifying_key();
    let replay = Arc::new(ReplayCache::new());

    let server = {
        let replay = Arc::clone(&replay);
        let origin = origin.clone();
        let known_id = known_id.to_string();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            drop(listener);
            // §3.1: verification runs *in the handshake callback*; a failure
            // answers 401 without upgrading.
            accept_ws_hdr(tcp, move |req: &WsRequest, res: WsResponse| {
                let header = req
                    .headers()
                    .get("Authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                match verify_upgrade_auth(
                    &header,
                    &origin,
                    &|id: &str| {
                        if id == known_id {
                            KeyLookup::Active(known_key)
                        } else {
                            KeyLookup::Unknown
                        }
                    },
                    &replay,
                    unix_ms_now(),
                ) {
                    Ok(_) => Ok(res),
                    Err(_) => {
                        let mut refusal = swarmbot_node_protocol::WsErrorResponse::new(Some(
                            "unauthorized".to_string(),
                        ));
                        *refusal.status_mut() = StatusCode::UNAUTHORIZED;
                        Err(refusal)
                    }
                }
            })
            .await
            .is_ok()
        })
    };

    let impostor_key = SigningKey::from_bytes(&[66u8; 32]);
    let (header, _) = upgrade_auth(known_id.as_str(), &impostor_key, &origin).unwrap();
    let dial = timeout(Duration::from_secs(10), connect_ws(&url, &header))
        .await
        .expect("dial timed out");
    let err = dial.expect_err("an impostor must not get a control socket");
    assert!(
        err.to_string().contains("401"),
        "expected an HTTP 401 refusal, got: {err}"
    );

    // The server side saw the handshake fail too — nothing was upgraded.
    let upgraded = timeout(Duration::from_secs(10), server)
        .await
        .expect("server task timed out")
        .expect("server task panicked");
    assert!(
        !upgraded,
        "the handshake must not succeed for a bad signature"
    );

    // The failed attempt burned nothing: the real node's nonce policy is
    // untouched (the replay cache is still empty).
    assert!(replay.is_empty());
}
