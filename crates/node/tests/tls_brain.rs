//! A real node against a real **TLS** brain: `https://` pairing and a `wss://`
//! control socket (D9.25).
//!
//! This is the half the README called unverified for several rounds, and the
//! reason was precise: the node could already *be configured* for `wss://`, and
//! `tokio-tungstenite` could already dial it, but no brain ever listened on TLS —
//! so "the client half works" was a claim with nothing on the other end.
//!
//! What is proven here, with the real `swarmbot-node` binary as a child process:
//!
//! 1. `swarmbot-node pair https://…` posts to the TLS brain and stores the
//!    identity — so pairing works over `https://`, not only `http://`;
//! 2. `swarmbot-node run --brain wss://… --brain-ca <ca.pem>` completes the signed
//!    upgrade **and** `node.hello` over TLS, which the roster shows as an online
//!    node with its label and approved capabilities;
//! 3. a node that does **not** trust the CA is refused at the handshake — the
//!    proof that the connection is really encrypted rather than downgraded.
//!
//! The certificate chain is made by `openssl` at test time (a CA plus a leaf),
//! because a self-signed certificate is not a valid *server* certificate for a
//! verifying client — webpki rejects it with `CaUsedAsEndEntity`. Nothing here
//! touches the network: the brain binds `127.0.0.1:0`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::process::Command;

use swarmbot_brain::{Brain, Paths, Resolved, SecretRegistry, ServeOptions, server};
use swarmbot_contracts::config::{BackendSpec, BrainConfig, TlsConfig};
use swarmbot_contracts::id::NodeId;

/// The real node daemon under test, built by cargo.
const NODE_BIN: &str = env!("CARGO_BIN_EXE_swarmbot-node");

/// Bounded waits: generous enough for a child process on a loaded machine, small
/// enough that a hung child fails the test rather than the budget.
const BUDGET: Duration = Duration::from_secs(45);
const POLL: Duration = Duration::from_millis(40);

/// A CA, a leaf signed by it, and the leaf's key.
struct Chain {
    ca: PathBuf,
    cert: PathBuf,
    key: PathBuf,
}

/// Build a CA and a leaf for `localhost`/`127.0.0.1` with `openssl`.
///
/// Three files rather than one because a self-signed certificate carries
/// `CA:TRUE` and cannot be presented as a server certificate; a verifying client
/// rejects it with `CaUsedAsEndEntity`. This is the shape a private deployment
/// has, so it is the shape the test uses.
fn chain(dir: &Path) -> Chain {
    let ca = dir.join("ca.pem");
    let ca_key = dir.join("ca-key.pem");
    let cert = dir.join("leaf.pem");
    let key = dir.join("leaf-key.pem");
    let csr = dir.join("leaf.csr");
    let ext = dir.join("leaf.ext");

    let run = |args: &[&str]| {
        let output = std::process::Command::new("openssl")
            .args(args)
            .output()
            .expect("openssl runs (it is how a real certificate is made)");
        assert!(
            output.status.success(),
            "openssl {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&[
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-keyout",
        ca_key.to_str().expect("utf-8"),
        "-out",
        ca.to_str().expect("utf-8"),
        "-days",
        "1",
        "-nodes",
        "-subj",
        "/CN=SwarmBot Test CA",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
        "-addext",
        "keyUsage=critical,keyCertSign,cRLSign",
    ]);
    run(&[
        "req",
        "-newkey",
        "rsa:2048",
        "-keyout",
        key.to_str().expect("utf-8"),
        "-out",
        csr.to_str().expect("utf-8"),
        "-nodes",
        "-subj",
        "/CN=localhost",
    ]);
    std::fs::write(
        &ext,
        "basicConstraints=critical,CA:FALSE\n\
         keyUsage=critical,digitalSignature,keyEncipherment\n\
         extendedKeyUsage=serverAuth\n\
         subjectAltName=DNS:localhost,IP:127.0.0.1\n",
    )
    .expect("the extension file");
    run(&[
        "x509",
        "-req",
        "-in",
        csr.to_str().expect("utf-8"),
        "-CA",
        ca.to_str().expect("utf-8"),
        "-CAkey",
        ca_key.to_str().expect("utf-8"),
        "-CAcreateserial",
        "-out",
        cert.to_str().expect("utf-8"),
        "-days",
        "1",
        "-extfile",
        ext.to_str().expect("utf-8"),
    ]);
    Chain { ca, cert, key }
}

/// A TLS brain, its data dir, and the node's data dir — one world per test.
struct Rig {
    /// Held, not read: dropping it drops the swarm the listener serves from, and a
    /// `Rig` that let it go early would leave the test querying a brain whose
    /// runners had stopped.
    _brain: Brain,
    serve: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    /// `https://<addr>` — what a node pairs against.
    base_url: String,
    /// `wss://<addr>` — what a node dials for the control socket.
    ws_url: String,
    token: String,
    http: reqwest::Client,
    chain: Chain,
    tmp: TempDir,
    node_dir: PathBuf,
}

impl Rig {
    async fn boot() -> Rig {
        let tmp = TempDir::new().expect("a temp dir");
        let brain_dir = tmp.path().join("brain");
        std::fs::create_dir_all(&brain_dir).expect("the brain's data dir");
        let chain = chain(tmp.path());
        let node_dir = tmp.path().join("node");

        let config = BrainConfig {
            public_addr: "127.0.0.1:0".into(),
            internal_addr: None,
            tls: Some(TlsConfig {
                cert: chain.cert.clone(),
                key: chain.key.clone(),
            }),
            default_backend: "fake".into(),
            backends: vec![BackendSpec::fake("fake")],
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        };
        let config_path = brain_dir.join("config.toml");
        std::fs::write(&config_path, "# written by the TLS node test\n").expect("config file");
        let resolved = Resolved {
            config,
            data_dir: brain_dir.clone(),
            paths: Paths::new(&brain_dir),
            config_path: Some(config_path),
            team_path: None,
            roots: Vec::new(),
            expiry_ticker: false,
            offline: true,
            fresh: true,
        };
        let brain = Brain::durable(resolved, &BTreeMap::new(), SecretRegistry::new())
            .expect("a durable brain composes and boots");

        // Bind, then serve over TLS with the same `TlsPaths::load` the daemon uses.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let token = brain.state.owner_token().to_string();
        let paths = swarmbot_brain::tls::TlsPaths {
            cert: chain.cert.clone(),
            key: chain.key.clone(),
        };
        let serve = tokio::spawn(server::serve_tls(
            brain.state.clone(),
            listener,
            paths,
            None,
            ServeOptions::in_process(),
        ));
        // Readiness: a TLS client that trusts the CA must get an answer. A bare
        // TCP connect says nothing — the listener accepts and then waits for a
        // ClientHello.
        let http = reqwest::Client::builder()
            .add_root_certificate(
                reqwest::Certificate::from_pem(&std::fs::read(&chain.ca).expect("read the CA"))
                    .expect("the CA parses"),
            )
            .build()
            .expect("a TLS http client");
        let health_url = format!("https://127.0.0.1:{}/api/health", addr.port());
        let deadline = tokio::time::Instant::now() + BUDGET;
        loop {
            match http.get(&health_url).send().await {
                Ok(_) => break,
                Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(POLL).await;
                }
                Err(error) => panic!("the TLS brain never answered {health_url}: {error}"),
            }
        }

        Rig {
            _brain: brain,
            serve: Some(serve),
            base_url: format!("https://127.0.0.1:{}", addr.port()),
            ws_url: format!("wss://127.0.0.1:{}", addr.port()),
            token,
            http,
            chain,
            tmp,
            node_dir,
        }
    }

    async fn get(&self, path: &str) -> Value {
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap_or_else(|error| panic!("GET {path} failed in transport: {error}"));
        assert!(
            response.status().is_success(),
            "GET {path} answered {}",
            response.status()
        );
        response.json().await.expect("a JSON body")
    }

    async fn post_empty(&self, path: &str) -> Value {
        let response = self
            .http
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap_or_else(|error| panic!("POST {path} failed in transport: {error}"));
        assert!(
            response.status().is_success(),
            "POST {path} answered {}",
            response.status()
        );
        response.json().await.expect("a JSON body")
    }

    /// `swarmbot-node pair https://… <code>` — the real binary, over TLS.
    async fn pair(&self) -> NodeId {
        let code = self.post_empty("/api/nodes/pairing-codes").await;
        let code = code["code"]
            .as_str()
            .unwrap_or_else(|| panic!("the pairing-code response carries a code: {code}"));
        // `reqwest` is used by the node for pairing over `https://`; the system
        // roots do not include this test CA, so the node is told to trust it with
        // the same `--brain-ca` flag an operator would use.
        let output = tokio::time::timeout(
            BUDGET,
            Command::new(NODE_BIN)
                .arg("--data-dir")
                .arg(&self.node_dir)
                .arg("pair")
                .arg(&self.base_url)
                .arg(code)
                .env("SSL_CERT_FILE", &self.chain.ca)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("`swarmbot-node pair` finishes")
        .expect("the node binary spawns");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "pairing over https:// failed\nexit {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        );
        let printed = stdout
            .lines()
            .next()
            .unwrap_or_else(|| panic!("pair prints the node id: {stdout}{stderr}"));
        NodeId::parse(printed.trim())
            .unwrap_or_else(|error| panic!("pair printed `{printed}`: {error}"))
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(serve) = self.serve.take() {
            serve.abort();
        }
        let _ = &self.tmp;
    }
}

/// Wait until the roster shows `node` online with `label`.
async fn wait_for_online(rig: &Rig, node: &NodeId, label: &str, log: &Path) -> Value {
    let deadline = tokio::time::Instant::now() + BUDGET;
    loop {
        let nodes = rig.get("/api/nodes").await;
        let row = nodes
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row["id"] == serde_json::json!(node.to_string()))
            })
            .cloned()
            .unwrap_or_else(|| panic!("the roster lost node {node}: {nodes}"));
        if row["online"] == serde_json::json!(true) && row["label"] == serde_json::json!(label) {
            return row;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "node {node} never came online as `{label}` over wss://; last row: {row}\nlog:\n{}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        tokio::time::sleep(POLL).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_node_pairs_and_speaks_to_a_tls_brain() {
    let rig = Rig::boot().await;
    let node = rig.pair().await;

    // `run --brain wss://…` with the CA the brain's certificate chains to.
    let log = rig.tmp.path().join("node.log");
    let log_file = std::fs::File::create(&log).expect("the node's log file");
    let mut child = Command::new(NODE_BIN)
        .arg("--data-dir")
        .arg(&rig.node_dir)
        .arg("run")
        .arg("--brain")
        .arg(&rig.ws_url)
        .arg("--brain-ca")
        .arg(&rig.chain.ca)
        .arg("--label")
        .arg("tls-node")
        .env("SWARMBOT_NODE_LOG", "info")
        .stdout(Stdio::from(
            log_file.try_clone().expect("clone the log handle"),
        ))
        .stderr(Stdio::from(log_file))
        .kill_on_drop(true)
        .spawn()
        .expect("the node binary spawns");

    let row = wait_for_online(&rig, &node, "tls-node", &log).await;
    // The capabilities were approved over a TLS upgrade **and** a TLS `node.hello`
    // — both signatures verified against the key pinned at pairing, which crossed
    // the same encrypted socket.
    assert_eq!(
        row["approved"],
        serde_json::json!(["fs@1", "shell@1"]),
        "the hello over wss:// was accepted: {row}"
    );

    let _ = child.kill().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_does_not_trust_the_ca_is_refused_at_the_handshake() {
    // The proof that the socket is genuinely encrypted: the *same* node, pointed
    // at the same brain, with no CA — must fail. Without this, a brain that served
    // plaintext while the node believed it was safe would pass the test above.
    let rig = Rig::boot().await;
    let node = rig.pair().await;
    // Pairing sets `brain_url` from the URL it was given, so the node would dial
    // `wss://` even without the flag; the point is that it has no CA to verify the
    // certificate against.
    let log = rig.tmp.path().join("untrusting.log");
    let log_file = std::fs::File::create(&log).expect("the node's log file");
    let mut child = Command::new(NODE_BIN)
        .arg("--data-dir")
        .arg(&rig.node_dir)
        .arg("run")
        .arg("--brain")
        .arg(&rig.ws_url)
        .arg("--label")
        .arg("untrusting")
        .env("SWARMBOT_NODE_LOG", "info")
        .stdout(Stdio::from(
            log_file.try_clone().expect("clone the log handle"),
        ))
        .stderr(Stdio::from(log_file))
        .kill_on_drop(true)
        .spawn()
        .expect("the node binary spawns");

    // It must never come online. Wait long enough that the redial loop has tried
    // more than once, then assert the roster still says offline.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    while tokio::time::Instant::now() < deadline {
        let nodes = rig.get("/api/nodes").await;
        let row = nodes
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row["id"] == serde_json::json!(node.to_string()))
            })
            .cloned()
            .unwrap_or_else(|| panic!("the roster lost node {node}"));
        assert!(
            row["online"] != serde_json::json!(true),
            "a node with no trust anchor must never complete the upgrade: {row}"
        );
        tokio::time::sleep(POLL).await;
    }
    let _ = child.kill().await;

    // And its own log says the failure was at the certificate, not at the API: a
    // node that got a 401 would be a *different* bug (the token), and this
    // assertion is what distinguishes them.
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let lower = text.to_lowercase();
    assert!(
        lower.contains("certificate") || lower.contains("tls") || lower.contains("handshake"),
        "the failure is a TLS one, not an HTTP one:\n{text}"
    );
}
