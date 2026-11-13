//! The test harness: a real brain on a real socket.
//!
//! Every integration test in this crate goes through here, and every one of them
//! talks to a `127.0.0.1:0` listener with `reqwest`, `tokio-tungstenite` or the
//! real `BrainClient` — never through `tower::ServiceExt::oneshot`. That is
//! deliberate: the middleware stack (auth, CORS, the error-envelope net), the
//! WebSocket upgrade and the graceful stop all live *outside* the router, so a
//! test that calls the router directly would pass while the daemon is broken.
//!
//! Two rules this harness exists to enforce:
//!
//! * nothing touches the network — the swarm runs on the scripted `FakeBackend`;
//! * nothing is left running — [`TestBrain::stop`] drains the listener and shuts
//!   the swarm down, and `Drop` does the same best effort if a test panics first.

#![allow(dead_code)] // a shared harness: no single test uses every helper

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::header::AUTHORIZATION;
use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use swarmbot_brain::{AppState, Brain, Paths, Resolved, SecretRegistry, ServeOptions, server};
use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::config::{BackendSpec, BrainConfig};
use swarmbot_contracts::id::{BotId, ConversationId};
use swarmbot_core::Swarm;
use swarmbot_llm::{FakeBackend, ScriptedTurn};

/// How long a test waits for the orchestration to go quiet. Generous, because a
/// slow machine must not turn into a flaky test.
pub const IDLE_BUDGET: Duration = Duration::from_secs(30);

/// A budget for anything that should be near-instant.
pub const QUICK: Duration = Duration::from_secs(15);

/// A running brain, its socket, and its owner token.
pub struct TestBrain {
    pub swarm: Arc<Swarm>,
    pub state: AppState,
    pub addr: SocketAddr,
    /// `http://127.0.0.1:<port>`
    pub base_url: String,
    /// The owner token: from the printed one-time login URL for a durable brain,
    /// generated for a memory one.
    pub token: String,
    /// The login URL, when this boot minted the token (durable, first boot).
    pub login_url: Option<String>,
    /// `None` when the caller owns the data dir, so it can outlive this brain.
    dir: Option<TempDir>,
    serve: Option<JoinHandle<anyhow::Result<()>>>,
}

impl TestBrain {
    /// A brain in memory with the four-bot demo team on the scripted fake.
    pub async fn start() -> Self {
        Self::start_with(|config| {
            config.team = Some(BrainConfig::demo_team("fake"));
        })
        .await
    }

    /// A brain in memory with no roster at all.
    pub async fn start_empty() -> Self {
        Self::start_with(|config| config.team = None).await
    }

    /// A brain in memory, with the config adjusted before composition.
    pub async fn start_with(mutate: impl FnOnce(&mut BrainConfig)) -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut config = BrainConfig {
            public_addr: "127.0.0.1:0".into(),
            internal_addr: None,
            default_backend: "fake".into(),
            backends: vec![BackendSpec::fake("fake")],
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        };
        mutate(&mut config);
        let resolved = Resolved {
            config,
            data_dir: dir.path().to_path_buf(),
            paths: Paths::new(dir.path()),
            config_path: None,
            team_path: None,
            roots: Vec::new(),
            // Off in tests: the ticker is a 15-second loop that a test would
            // otherwise have to outlive, and expiring requests is tested in core.
            expiry_ticker: false,
            offline: true,
            fresh: true,
        };
        let brain = Brain::memory(resolved, &BTreeMap::new(), SecretRegistry::new())
            .expect("a brain composes and boots");
        Self::listen(brain, Some(dir)).await
    }

    /// A brain on disk, the way the daemon builds one: SQLite, a minted owner
    /// token and the one-time login URL.
    pub async fn start_durable() -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        let brain = Self::boot_durable(dir.path());
        Self::listen(brain, Some(dir)).await
    }

    /// A brain on disk in a data dir the *caller* owns, so a test can stop it and
    /// boot it again — which is what "a second boot reuses the token" needs.
    pub async fn start_durable_in(data_dir: &std::path::Path) -> Self {
        let brain = Self::boot_durable(data_dir);
        Self::listen(brain, None).await
    }

    fn boot_durable(data_dir: &std::path::Path) -> Brain {
        let resolved = Resolved {
            config: BrainConfig {
                public_addr: "127.0.0.1:0".into(),
                internal_addr: None,
                default_backend: "fake".into(),
                backends: vec![BackendSpec::fake("fake")],
                team: Some(BrainConfig::demo_team("fake")),
                ..BrainConfig::default()
            },
            data_dir: data_dir.to_path_buf(),
            paths: Paths::new(data_dir),
            config_path: None,
            team_path: None,
            roots: Vec::new(),
            expiry_ticker: false,
            offline: true,
            fresh: true,
        };
        Brain::durable(resolved, &BTreeMap::new(), SecretRegistry::new())
            .expect("a durable brain composes and boots")
    }

    async fn listen(brain: Brain, dir: Option<TempDir>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback socket");
        let addr = listener.local_addr().expect("the bound address");
        let login_url = brain.login_url.clone();
        let token = brain.state.owner_token().to_string();
        let serve = tokio::spawn(server::serve(
            brain.state.clone(),
            listener,
            None,
            ServeOptions::in_process(),
        ));
        // Give the listener a moment to be polling, so the first request in a
        // test does not race the `accept` loop.
        wait_for_port(addr).await;
        Self {
            swarm: Arc::clone(&brain.swarm),
            state: brain.state.clone(),
            addr,
            base_url: format!("http://{addr}"),
            token,
            login_url,
            dir,
            serve: Some(serve),
        }
    }

    /// Boot a brain whose public listener terminates TLS with `cert`/`key`.
    ///
    /// The certificate is loaded by the **same** `TlsPaths::load` the daemon uses,
    /// so a test that passes here passes on the real start-up path; what differs is
    /// only who called `bind`.
    pub async fn start_tls(cert: &std::path::Path, key: &std::path::Path) -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        let brain = Self::boot_durable(dir.path());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback socket");
        let addr = listener.local_addr().expect("the bound address");
        let login_url = brain.login_url.clone();
        let token = brain.state.owner_token().to_string();
        let paths = swarmbot_brain::tls::TlsPaths {
            cert: cert.to_path_buf(),
            key: key.to_path_buf(),
        };
        let serve = tokio::spawn(server::serve_tls(
            brain.state.clone(),
            listener,
            paths,
            None,
            ServeOptions::in_process(),
        ));
        // `wait_for_port` cannot be used here: a TLS listener completes the TCP
        // handshake and then waits for a ClientHello, so a bare connect succeeds
        // before anything is ready. A real TLS handshake is the readiness probe.
        wait_for_tls(addr).await;
        Self {
            swarm: Arc::clone(&brain.swarm),
            state: brain.state.clone(),
            addr,
            base_url: format!("https://{addr}"),
            token,
            login_url,
            dir: Some(dir),
            serve: Some(serve),
        }
    }

    pub fn ws_url(&self) -> String {
        format!("ws://{}/ws/live", self.addr)
    }

    /// An HTTP client that authenticates as the owner.
    pub fn client(&self) -> reqwest::Client {
        Self::client_with(self.token())
    }

    /// An HTTP client with an explicit bearer token (or none, for `""`).
    pub fn client_with(token: &str) -> reqwest::Client {
        let mut headers = axum::http::HeaderMap::new();
        if !token.is_empty() {
            headers.insert(
                AUTHORIZATION,
                format!("Bearer {token}").parse().expect("an ascii token"),
            );
        }
        reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .expect("a reqwest client builds")
    }

    /// The owner token, from the one-time login URL when there was one.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The token as it appears in the printed login URL.
    pub fn printed_token(&self) -> String {
        match &self.login_url {
            Some(url) => url
                .split_once("#token=")
                .map(|(_, token)| token.to_string())
                .unwrap_or_else(|| self.token.clone()),
            None => self.token.clone(),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// The scripted fake backend every bot in this brain runs on.
    pub fn fake(&self) -> Arc<FakeBackend> {
        self.swarm
            .backends
            .registry()
            .fake("fake")
            .expect("the fake backend is always scriptable")
    }

    /// Queue a turn for the next bot that asks the model something.
    pub fn script(&self, turn: ScriptedTurn) {
        self.fake().push_turn(turn);
    }

    /// Queue one plain text reply.
    pub fn script_text(&self, text: &str) {
        self.script(ScriptedTurn::text(text.to_string()));
    }

    pub fn bot(&self, name: &str) -> Bot {
        self.swarm.bots.by_name(name).expect("the bot exists")
    }

    pub fn bot_id(&self, name: &str) -> BotId {
        self.bot(name).id
    }

    /// A conversation by title, for the demo team's group.
    pub fn conversation_titled(&self, title: &str) -> ConversationId {
        self.swarm
            .conversations
            .find_by_title(title)
            .expect("the lookup works")
            .unwrap_or_else(|| panic!("there is a conversation called {title}"))
            .id
    }

    /// Wait until no bot is busy, nothing is queued and no task is pending.
    ///
    /// A timeout explains itself: "did not go quiet" without saying *what* is
    /// still working is the kind of failure that costs an hour to diagnose.
    /// It reads the state **twice** — before and after the wait — because the
    /// two answers are different failures: work that is still running, and work
    /// that finished while `wait_for_idle` was watching but whose bookkeeping
    /// (a turn, a delivery, a task) was left behind. A single reading cannot
    /// tell those apart, and the second one is a bug in the product rather than
    /// a slow machine.
    pub async fn settle(&self) {
        let before = self.work_report();
        if self.swarm.wait_for_idle(IDLE_BUDGET).await {
            return;
        }
        let after = self.work_report();
        panic!(
            "the swarm did not go quiet within {IDLE_BUDGET:?}\n  before: {before}\n  after:  {after}"
        );
    }

    /// Every signal `wait_for_idle` consults, in one line.
    ///
    /// `running_turns` is included on purpose: it is the one signal that is
    /// **not** in memory, so a turn the store still calls `Running` is invisible
    /// in `busy`/`queued`/`parked` — and invisible is how a stuck turn survived
    /// a whole debugging session (AGENTS.md, "make the failure print the state").
    pub fn work_report(&self) -> String {
        let bots = self.swarm.store.list_bots().unwrap_or_default();
        let busy: Vec<&str> = bots
            .iter()
            .filter(|bot| self.swarm.runners.is_busy(&bot.id))
            .map(|bot| bot.name.as_str())
            .collect();
        let queued: Vec<String> = bots
            .iter()
            .filter_map(|bot| {
                let waiting = self
                    .swarm
                    .store
                    .queued_deliveries(&bot.id)
                    .map(|deliveries| deliveries.len())
                    .unwrap_or(0);
                (waiting > 0).then(|| format!("{}:{waiting}", bot.name))
            })
            .collect();
        let tasks = self
            .swarm
            .store
            .queued_tasks()
            .map(|tasks| tasks.len())
            .unwrap_or(0);
        let mut running: Vec<String> = self
            .swarm
            .store
            .running_turns()
            .unwrap_or_default()
            .into_iter()
            .map(|turn| {
                let name = self
                    .swarm
                    .store
                    .get_bot(&turn.bot_id)
                    .ok()
                    .flatten()
                    .map(|bot| bot.name)
                    .unwrap_or_else(|| turn.bot_id.as_str().to_string());
                format!(
                    "{name}={} started={} llm_calls={}",
                    turn.id.as_str(),
                    turn.started_at.to_rfc3339(),
                    turn.usage.llm_calls
                )
            })
            .collect();
        running.sort();
        format!(
            "busy={busy:?} queued={queued:?} parked={} queued_tasks={tasks} running_turns={running:?}",
            self.swarm.runners.parked_count()
        )
    }

    /// The entries of one conversation, ascending.
    pub fn entries(&self, conversation: &ConversationId) -> Vec<swarmbot_contracts::entry::Entry> {
        self.swarm
            .conversations
            .tail(conversation, 1000)
            .expect("the transcript reads")
    }

    /// Stop the listener and the swarm. Called by every test that finishes
    /// normally; `Drop` covers the ones that panic.
    pub async fn stop(mut self) {
        self.state.request_shutdown();
        if let Some(serve) = self.serve.take() {
            let stopped = tokio::time::timeout(QUICK, serve).await;
            assert!(
                stopped.is_ok(),
                "the listener did not stop after a shutdown request"
            );
            if let Ok(Err(err)) = stopped {
                panic!("the listener failed: {err:#}");
            }
        }
        self.swarm.shutdown().await;
        assert_eq!(
            self.swarm.runners.len(),
            0,
            "no runner may outlive the test"
        );
        // `self` drops here: `Drop` finds nothing left to stop, and the `TempDir`
        // takes the data dir with it.
    }
}

impl Drop for TestBrain {
    /// A test that panicked still must not leave a listener task or a bot runner
    /// spinning. `shutdown` is synchronous (it only sends the command), so this is
    /// the best effort available without a runtime.
    fn drop(&mut self) {
        self.state.request_shutdown();
        if let Some(serve) = self.serve.take() {
            serve.abort();
        }
        for id in self.swarm.runners.bot_ids() {
            if let Some(handle) = self.swarm.runners.get(&id) {
                handle.shutdown();
            }
        }
    }
}

/// Block until something accepts a connection on `addr`.
/// Wait until a TLS listener completes handshakes, not merely accepts TCP.
///
/// A TLS listener accepts the socket and *then* waits for a ClientHello, so
/// `TcpStream::connect` succeeding says nothing about readiness — a plaintext
/// readiness probe would race the accept loop and make the first request in a
/// test flaky in a way that looks like a TLS bug.
async fn wait_for_tls(addr: std::net::SocketAddr) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        // A handshake that fails for any reason (including an untrusted cert)
        // still proves the server answered, which is all readiness means.
        let attempt = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::net::TcpStream::connect(addr),
        )
        .await;
        if let Ok(Ok(stream)) = attempt {
            // Send a byte that is not a TLS record: the server drops the
            // connection, and the drop is what says "the handler is running".
            use tokio::io::AsyncWriteExt as _;
            let mut stream = stream;
            let _ = stream.write_all(b"not a tls hello").await;
            let _ = stream.shutdown().await;
            // One accept is enough: the listener loop is running.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the TLS listener on {addr} never accepted a connection"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn wait_for_port(addr: SocketAddr) {
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{addr} never accepted a connection");
}

// --- response helpers --------------------------------------------------------

/// The decoded JSON body, with a message that says which call it came from.
pub async fn body(response: reqwest::Response) -> Value {
    let url = response.url().to_string();
    let text = response.text().await.expect("a body");
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{url} answered `{text}`, which is not JSON: {err}"))
}

/// `(status, code, message)` of an error envelope.
pub async fn envelope(response: reqwest::Response) -> (u16, String, String) {
    let status = response.status().as_u16();
    let body = body(response).await;
    let error = &body["error"];
    assert!(
        error.is_object(),
        "a {status} must carry the envelope, got {body}"
    );
    (
        status,
        error["code"].as_str().unwrap_or_default().to_string(),
        error["message"].as_str().unwrap_or_default().to_string(),
    )
}

/// Assert the documented envelope for one failure, and return its message.
pub async fn expect_error(response: reqwest::Response, status: u16, code: &str) -> String {
    let (got_status, got_code, message) = envelope(response).await;
    assert_eq!(got_status, status, "status for `{code}`: {message}");
    assert_eq!(got_code, code, "code: {message}");
    message
}

/// POST a JSON body and decode the answer.
pub async fn post(client: &reqwest::Client, url: &str, body: &Value) -> reqwest::Response {
    client
        .post(url)
        .json(body)
        .send()
        .await
        .unwrap_or_else(|err| panic!("POST {url} failed in transport: {err}"))
}

/// POST with no body, the way the client crate's `post_empty` does.
pub async fn post_empty(client: &reqwest::Client, url: &str) -> reqwest::Response {
    client
        .post(url)
        .send()
        .await
        .unwrap_or_else(|err| panic!("POST {url} failed in transport: {err}"))
}

pub async fn get(client: &reqwest::Client, url: &str) -> reqwest::Response {
    client
        .get(url)
        .send()
        .await
        .unwrap_or_else(|err| panic!("GET {url} failed in transport: {err}"))
}

pub async fn patch(client: &reqwest::Client, url: &str, body: &Value) -> reqwest::Response {
    client
        .patch(url)
        .json(body)
        .send()
        .await
        .unwrap_or_else(|err| panic!("PATCH {url} failed in transport: {err}"))
}

pub async fn delete(client: &reqwest::Client, url: &str) -> reqwest::Response {
    client
        .delete(url)
        .send()
        .await
        .unwrap_or_else(|err| panic!("DELETE {url} failed in transport: {err}"))
}

/// Run a blocking client (the real `BrainClient` is `ureq`-based) off the async
/// workers, so a blocking read cannot stall the server it is talking to.
pub async fn blocking<F, T>(work: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => value,
        // An assertion that failed on the blocking thread has to fail the test
        // with its own message, not with "the task did not finish".
        Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
        Err(err) => panic!("the blocking task failed: {err}"),
    }
}

/// A JSON array body, decoded.
pub async fn array(response: reqwest::Response) -> Vec<Value> {
    let body = body(response).await;
    body.as_array()
        .unwrap_or_else(|| panic!("expected a JSON array, got {body}"))
        .clone()
}

/// Convenience for building a small object.
pub fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut map = serde_json::Map::new();
    for (key, value) in pairs {
        map.insert(key.to_string(), value);
    }
    Value::Object(map)
}

/// The `id` field of a JSON object, as a string.
pub fn id_of(value: &Value) -> String {
    value["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no id in {value}"))
        .to_string()
}
