//! M6's last unverified piece: a **real brain** driving the **real
//! `swarmbot-node` binary** through an actual `fs`/`shell` call.
//!
//! The two halves were each proven separately. `crates/brain/tests/node_gateway.rs`
//! drives the whole remote-call path (pairing, signed upgrade, `node.hello`,
//! `fs.*`, streamed `shell.exec`, revoke → 4401) against the real protocol
//! library on a real loopback socket — but with a *fake* node. And a
//! two-process run of the two real binaries (`target/two-process/run.sh`) proved
//! pairing → hello → heartbeats → revoke → 4401 → the node stops redialing —
//! but a separate brain *process* cannot be handed a scripted model turn, so no
//! remote tool call ever happened there.
//!
//! This file combines the halves: the brain is composed **in process**
//! (`Brain::durable` + `server::serve` on `127.0.0.1:0`, with
//! `ServeOptions::in_process()` so nothing touches signals), which is what makes
//! its scripted `Fake` backend reachable; the node is the real daemon binary
//! cargo built (`CARGO_BIN_EXE_swarmbot-node`), spawned as a child process. No
//! test here invokes `cargo` — cargo holds the target-dir lock while tests run,
//! so a test that shells out to it would deadlock; `CARGO_BIN_EXE_*` exists
//! precisely to avoid that.
//!
//! What is proven here that neither half proves alone:
//!
//! 1. a bot bound to the paired node issues `read_file` and `shell` calls that
//!    cross a real WebSocket, and the text that comes back is the marker file in
//!    **the node's root** — a decoy marker with different text sits in the bot's
//!    *brain-side* workspace, so a silent local fallback (the D9.11 bug) would
//!    hand back the wrong string and fail;
//! 2. the streamed shell result carries the exit code and stdout the node's
//!    `Started`/`Output`/`Exit` chunks delivered, and the node's own audit JSONL
//!    gained both invocations, attributed to the bot that made them;
//! 3. with `--ceiling shell=never`, the same scripted `shell` call is refused by
//!    **the node** even though this brain's own (owner-configured) `shell`
//!    ceiling is `always`, and the refusal reaches the model as `ERROR: …`:
//!    the node decides finally (D1.4), and the brain never ran it locally;
//! 4. `DELETE /api/nodes/{id}` closes the real child with 4401: it exits
//!    non-zero telling its operator to re-pair, and a redial attempt is refused
//!    at the handshake, before any upgrade.
//!
//! Every child process is held by a [`ChildGuard`] whose `Drop` kills it, so
//! even a panicking test leaves no daemon running; every temp dir is a
//! `TempDir`; every wait is bounded by a deadline that says what it was waiting
//! for; and the only listener is the brain's, on `127.0.0.1:0`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use swarmbot_brain::{Brain, Paths, Resolved, SecretRegistry, ServeOptions, server};
use swarmbot_contracts::config::{BackendSpec, BrainConfig};
use swarmbot_contracts::entry::{Entry, EntryKind, ToolCallEntry, ToolCallStatus};
use swarmbot_contracts::id::{BotId, ConversationId, NodeId};
use swarmbot_contracts::node::{PolicyCeiling, Surface};
use swarmbot_llm::{FakeBackend, ScriptedTurn};

/// The real node daemon under test, built by cargo.
const NODE_BIN: &str = env!("CARGO_BIN_EXE_swarmbot-node");

/// A budget for waits that involve a real child process and a real socket:
/// generous so a loaded machine is not a flaky test, bounded so a hung child
/// fails the test instead of the CI budget.
const BUDGET: Duration = Duration::from_secs(45);
/// A budget for things that should be near-instant.
const QUICK: Duration = Duration::from_secs(15);
/// The polling interval every bounded wait uses.
const POLL: Duration = Duration::from_millis(25);

/// The marker texts. Same file name, one per machine, different content: which
/// string comes back is the proof of *where* the call ran.
/// The backend id of the bot under test, so its script queue is its own.
const NODE_BACKEND: &str = "fake-node";

const NODE_MARKER: &str = "from the node";
const BRAIN_MARKER: &str = "from the brain";

// --- the child process guard -------------------------------------------------

/// A spawned `swarmbot-node` child whose log is captured and whose life the
/// test owns. `Drop` kills it, so a panic between spawn and the explicit
/// `kill`/`wait` still cannot leave a daemon running.
struct ChildGuard {
    label: String,
    child: Option<Child>,
    lines: Arc<Mutex<Vec<String>>>,
}

impl ChildGuard {
    /// Spawn with piped stdout/stderr (both drained into one log) and
    /// `kill_on_drop` as the belt to this guard's braces.
    fn spawn(label: &str, command: &mut Command) -> ChildGuard {
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            // Deterministic log level regardless of the developer's shell:
            // `init_tracing` checks this variable before `RUST_LOG`.
            .env("SWARMBOT_NODE_LOG", "info");
        let mut child = command
            .spawn()
            .unwrap_or_else(|error| panic!("spawning `{label}` failed: {error}"));
        let lines = Arc::new(Mutex::new(Vec::new()));
        drain(
            child.stdout.take().expect("stdout is piped"),
            Arc::clone(&lines),
        );
        drain(
            child.stderr.take().expect("stderr is piped"),
            Arc::clone(&lines),
        );
        ChildGuard {
            label: label.to_string(),
            child: Some(child),
            lines,
        }
    }

    /// The whole captured log (stdout and stderr, in arrival order).
    fn log(&self) -> String {
        self.lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .join("\n")
    }

    /// Wait for the child to exit by itself, with a deadline. The child is
    /// consumed by the wait: `Drop` afterwards is a no-op.
    async fn wait(&mut self, budget: Duration) -> ExitStatus {
        let Some(mut child) = self.child.take() else {
            panic!("`{}` was already waited on", self.label);
        };
        tokio::time::timeout(budget, child.wait())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "`{}` did not exit within {budget:?}; its log:\n{}",
                    self.label,
                    self.log()
                )
            })
            .unwrap_or_else(|error| {
                panic!(
                    "waiting on `{}` failed: {error}; its log:\n{}",
                    self.label,
                    self.log()
                )
            })
    }

    /// Wait until one logged line's *message* contains `needle`, and return the
    /// whole line. The failure message is the child's own log: a daemon that
    /// died early should say so in the test output, not just "timed out".
    ///
    /// The match runs against [`message_of`] rather than the raw line on
    /// purpose. The needles here are short numbers (`401`, `4401`) and a
    /// tracing timestamp is arbitrary digits, so `2026-11-13T01:33:44.929401Z`
    /// matched a needle of `401` and this helper returned the daemon's startup
    /// line instead of waiting for the refusal it was asked about. That is the
    /// whole of the "flaky under load" story: more log lines meant more chances
    /// for the clock to answer the question.
    async fn wait_for_line(&self, needle: &str, budget: Duration) -> String {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            {
                let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(line) = lines.iter().find(|line| message_of(line).contains(needle)) {
                    return line.clone();
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "`{}` never logged a line containing {needle:?}; its log so far:\n{}",
                self.label,
                self.log()
            );
            tokio::time::sleep(POLL).await;
        }
    }

    /// Kill and reap the child. Used on the happy path so nothing is still
    /// dialing when the brain's listener drains.
    async fn kill(mut self) {
        self.kill_inner().await;
    }

    async fn kill_inner(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.start_kill();
        let _ = tokio::time::timeout(QUICK, child.wait()).await;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // No test may leave a daemon running, even a panicking one.
        if self.child.is_some() {
            // `kill_inner` is async and `Drop` is not; a blocking reap of an
            // already-SIGKILLed child is bounded and rare (panic path only).
            let _ = self.child.as_mut().expect("checked above").start_kill();
            self.child = None;
        }
    }
}

/// A captured log line without its tracing prefix: the part a test may match
/// on without matching the clock.
///
/// `tracing` writes `<RFC 3339 timestamp> LEVEL target: message`, and the
/// fractional seconds are arbitrary digits — `.929401` contains `401` and
/// `4401` both. Matching the raw line therefore let a timestamp answer a
/// question about a close code, which is a false positive that looks exactly
/// like a flake when the machine is busy enough to emit more lines. The prefix
/// ends at the first `Z` of a line that starts with a digit; a line that does
/// not look like that is returned untouched.
fn message_of(line: &str) -> &str {
    if !line.starts_with(|c: char| c.is_ascii_digit()) {
        return line;
    }
    match line.find('Z') {
        Some(z) if z < 40 => line.get(z + 1..).map_or(line, str::trim_start),
        _ => line,
    }
}

/// Pump one piped stream into the shared log until EOF.
fn drain(stream: impl AsyncRead + Unpin + Send + 'static, lines: Arc<Mutex<Vec<String>>>) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stream).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            lines.lock().unwrap_or_else(|e| e.into_inner()).push(line);
        }
    });
}

// --- the brain side ----------------------------------------------------------

/// A real brain, composed and served in process on `127.0.0.1:0`, plus the
/// temp-dir world around it: the node's data dir and the node's root.
struct Rig {
    brain: Brain,
    serve: Option<JoinHandle<anyhow::Result<()>>>,
    base_url: String,
    token: String,
    http: reqwest::Client,
    /// The whole world of one test; dropping it removes the brain's data dir,
    /// the node's data dir and the node's root.
    tmp: TempDir,
    node_dir: PathBuf,
    node_root: PathBuf,
}

impl Rig {
    /// Boot the brain the way the daemon builds one — `Brain::durable` with the
    /// demo team on the scripted fake backend — and serve it in process.
    ///
    /// Durable (rather than `memory`) on purpose: the safety ceilings are read
    /// once, while the tool policy engine is composed, and only a durable brain
    /// seeds the store from `resolved.config.settings` before that happens. The
    /// test *is* the owner, and the owner sets the brain-side `shell` ceiling
    /// to `always` through the ordinary settings path — not a bypass flag, but
    /// one half of the documented `min(brain, node)` pair. Making the brain as
    /// permissive as it can be is what makes every refusal observed below
    /// attributable to the node's *own* policy (D1.4) instead of to a
    /// brain-side card nobody is around to answer.
    async fn boot(tmp: TempDir) -> Rig {
        let brain_dir = tmp.path().join("brain");
        std::fs::create_dir_all(&brain_dir).expect("the brain's data dir");
        let node_dir = tmp.path().join("node");
        let node_root = tmp.path().join("noderoot");
        std::fs::create_dir_all(&node_root).expect("the node's root dir");
        std::fs::write(node_root.join("marker.txt"), format!("{NODE_MARKER}\n"))
            .expect("the node's marker file");

        let mut config = BrainConfig {
            public_addr: "127.0.0.1:0".into(),
            internal_addr: None,
            default_backend: "fake".into(),
            // Two scripted backends, not one: the demo team keeps `"fake"`, and
            // the node-bound bot gets its own queue so a demo bot cannot pop a
            // script meant for it. See `Rig::fake`.
            backends: vec![BackendSpec::fake("fake"), BackendSpec::fake(NODE_BACKEND)],
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        };
        config
            .settings
            .safety
            .set(Surface::Shell, PolicyCeiling::Always);
        // `Brain::durable` seeds the store from the config's settings only when
        // a config file exists; the content is irrelevant (the settings travel
        // in `resolved.config`), but the file has to look real.
        let config_path = brain_dir.join("config.toml");
        std::fs::write(&config_path, "# written by the real_brain test\n")
            .expect("the brain's config file");
        let resolved = Resolved {
            config,
            data_dir: brain_dir.clone(),
            paths: Paths::new(&brain_dir),
            config_path: Some(config_path),
            team_path: None,
            roots: Vec::new(),
            // Off in tests: a 15-second loop no test here needs to outlive.
            expiry_ticker: false,
            offline: true,
            fresh: true,
        };
        let brain = Brain::durable(resolved, &BTreeMap::new(), SecretRegistry::new())
            .expect("a durable brain composes and boots");
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the brain binds a loopback socket");
        let addr = listener.local_addr().expect("the bound address");
        let token = brain.state.owner_token().to_string();
        // `in_process()`: no signal handling — only `request_shutdown` stops it,
        // so a developer's Ctrl-C during `cargo test` cannot tear this down.
        let serve = tokio::spawn(server::serve(
            brain.state.clone(),
            listener,
            None,
            ServeOptions::in_process(),
        ));
        // Let the listener be polling before the first request races the accept
        // loop.
        let deadline = tokio::time::Instant::now() + QUICK;
        while TcpStream::connect(addr).await.is_err() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the brain's listener on {addr} never accepted a connection"
            );
            tokio::time::sleep(POLL).await;
        }
        Rig {
            brain,
            serve: Some(serve),
            base_url: format!("http://{addr}"),
            token,
            http: reqwest::Client::builder()
                .build()
                .expect("an http client builds"),
            tmp,
            node_dir,
            node_root,
        }
    }

    // -- HTTP helpers (owner-authenticated) -----------------------------------

    async fn get(&self, path: &str) -> Value {
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap_or_else(|error| panic!("GET {path} failed in transport: {error}"));
        body_of(response).await
    }

    async fn post(&self, path: &str, body: &Value) -> Value {
        let response = self
            .http
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .unwrap_or_else(|error| panic!("POST {path} failed in transport: {error}"));
        body_of(response).await
    }

    async fn post_empty(&self, path: &str) -> Value {
        let response = self
            .http
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap_or_else(|error| panic!("POST {path} failed in transport: {error}"));
        body_of(response).await
    }

    async fn delete(&self, path: &str) -> u16 {
        let response = self
            .http
            .delete(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap_or_else(|error| panic!("DELETE {path} failed in transport: {error}"));
        response.status().as_u16()
    }

    // -- the node child -------------------------------------------------------

    /// `swarmbot-node pair <brain-url> <code>` as the real binary, with a code
    /// minted through the brain's own API. Asserts exit 0 and an
    /// `identity.json` at mode 0600, and returns the node id it printed.
    async fn pair(&self) -> NodeId {
        let code = self.post_empty("/api/nodes/pairing-codes").await;
        let code = code["code"]
            .as_str()
            .unwrap_or_else(|| panic!("the pairing-code response carries a code: {code}"));
        let output = tokio::time::timeout(
            QUICK,
            Command::new(NODE_BIN)
                .arg("--data-dir")
                .arg(&self.node_dir)
                .arg("pair")
                .arg(&self.base_url)
                .arg(code)
                .env("SWARMBOT_NODE_LOG", "info")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("`swarmbot-node pair` did not finish in time")
        .expect("the real node binary spawns (CARGO_BIN_EXE_swarmbot-node)");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "`swarmbot-node pair` exited {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        );
        let printed = stdout
            .lines()
            .next()
            .expect("pair prints the node id on stdout");
        let node_id = NodeId::parse(printed.trim()).unwrap_or_else(|error| {
            panic!("pair printed `{printed}`, whose first line is not a node id: {error}")
        });
        let identity = self.node_dir.join("identity.json");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&identity)
                .expect("pairing wrote <data>/identity.json")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "identity.json must be 0600");
        }
        #[cfg(not(unix))]
        assert!(
            identity.exists(),
            "pairing wrote <data>/identity.json (no mode bits on this platform)"
        );
        node_id
    }

    /// `swarmbot-node run --root <tmp>/noderoot --label <label> [--ceiling …]`
    /// as a guarded child process.
    fn run_daemon(&self, label: &str, ceilings: &[&str]) -> ChildGuard {
        let mut command = Command::new(NODE_BIN);
        command
            .arg("--data-dir")
            .arg(&self.node_dir)
            .arg("run")
            .arg("--root")
            .arg(&self.node_root)
            .arg("--label")
            .arg(label);
        for ceiling in ceilings {
            command.arg("--ceiling").arg(ceiling);
        }
        ChildGuard::spawn(&format!("swarmbot-node run ({label})"), &mut command)
    }

    // -- roster waits ---------------------------------------------------------

    /// This node's row in `GET /api/nodes`. Pairing inserts it, so it exists
    /// from the moment `pair` returned.
    async fn node_row(&self, node: &NodeId) -> Value {
        let nodes = self.get("/api/nodes").await;
        nodes
            .as_array()
            .unwrap_or_else(|| panic!("GET /api/nodes answered {nodes}, not an array"))
            .iter()
            .find(|row| row["id"] == json!(node.to_string()))
            .cloned()
            .unwrap_or_else(|| panic!("the roster lost node {node}: {nodes}"))
    }

    /// Wait until the roster shows the node online, carrying the label its
    /// operator gave it, with both capabilities approved.
    ///
    /// `online` flips only after `node.hello` verified in *both* directions —
    /// the brain's signature over the key the node pinned at pairing, and the
    /// node's facts written into the roster — and the label only after the
    /// node's `node.connected` event arrived, so this wait is the real-binary
    /// form of the mutual-authentication check.
    async fn wait_for_online(&self, node: &NodeId, label: &str, daemon: &ChildGuard) -> Value {
        let deadline = tokio::time::Instant::now() + BUDGET;
        loop {
            let row = self.node_row(node).await;
            if row["online"] == json!(true)
                && row["label"] == json!(label)
                && row["approved"] == json!(["fs@1", "shell@1"])
            {
                return row;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "node {node} never showed up online as `{label}` with caps [fs@1, shell@1]; \
                 last row: {row}; the child's log:\n{}",
                daemon.log()
            );
            tokio::time::sleep(POLL).await;
        }
    }

    /// Wait until this node's roster row satisfies `ready`.
    async fn wait_for_row(
        &self,
        node: &NodeId,
        what: &str,
        ready: impl Fn(&Value) -> bool,
    ) -> Value {
        let deadline = tokio::time::Instant::now() + BUDGET;
        loop {
            let row = self.node_row(node).await;
            if ready(&row) {
                return row;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "node {node} never became {what}; last row: {row}"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    // -- the bot side ---------------------------------------------------------

    /// Create a bot bound to `node` through the API, and return its id and DM.
    async fn create_bound_bot(&self, node: &NodeId, name: &str) -> (BotId, ConversationId) {
        let created = self
            .post(
                "/api/bots",
                &json!({
                    "name": name,
                    "label": "Remote",
                    "instructions": "your tools run on another machine",
                    "backend": NODE_BACKEND,
                    "node": node.to_string(),
                }),
            )
            .await;
        assert_eq!(
            created["node"],
            json!(node.to_string()),
            "the bot is bound to the remote node: {created}"
        );
        let bot: BotId = created["id"]
            .as_str()
            .expect("the created bot has an id")
            .parse()
            .expect("that id parses as a BotId");
        let dm: ConversationId = created["direct_conversation_id"]
            .as_str()
            .expect("the created bot has a DM")
            .parse()
            .expect("that id parses as a ConversationId");
        (bot, dm)
    }

    /// Put a same-named marker with *different* text inside the bot's
    /// brain-side workspace: a call that silently ran on the brain's machine
    /// (the D9.11 bug this repo fixed) would resolve `marker.txt` against this
    /// directory and hand the model the wrong string.
    fn plant_brain_decoy(&self, bot_name: &str) {
        let workspace = self
            .tmp
            .path()
            .join("brain")
            .join("workspaces")
            .join(bot_name);
        std::fs::create_dir_all(&workspace).expect("the bot's brain-side workspace");
        std::fs::write(workspace.join("marker.txt"), format!("{BRAIN_MARKER}\n"))
            .expect("the brain's decoy marker");
    }

    /// The scripted backend the *node-bound* bot runs on.
    ///
    /// Deliberately not the demo team's `"fake"`. A scripted backend holds one
    /// queue, and a queue shared with the four demo bots is a race: under load
    /// another bot's turn can pop a script written for this one, and the test
    /// then fails with a transcript that holds only the owner's message and no
    /// tool call at all — a silent wrong-script failure with exit code 0. That
    /// is the same shape D9.15 fixed for the CLI's `--script`, and this rig
    /// builds its brain directly, so it never went through that fix. Giving the
    /// bot under test its own backend removes the sharing instead of widening a
    /// wait around it.
    fn fake(&self) -> Arc<FakeBackend> {
        self.brain
            .swarm
            .backends
            .registry()
            .fake(NODE_BACKEND)
            .expect("the fake backend is always scriptable")
    }

    /// Send the owner's message into the bot's DM and assert the router woke
    /// exactly that bot.
    async fn send_message(&self, dm: &ConversationId, bot: &BotId, text: &str) {
        let sent = self
            .post(
                &format!("/api/conversations/{dm}/messages"),
                &json!({"text": text, "addressed": []}),
            )
            .await;
        assert_eq!(
            sent["targets"],
            json!([bot.to_string()]),
            "the message wakes the bound bot: {sent}"
        );
    }

    /// Wait until the swarm is quiet, with the diagnostic that names what is
    /// still working when it is not.
    async fn settle(&self) {
        if self.brain.swarm.wait_for_idle(BUDGET).await {
            return;
        }
        let mut busy = Vec::new();
        let mut queued = Vec::new();
        for bot in self.brain.swarm.store.list_bots().unwrap_or_default() {
            if self.brain.swarm.runners.is_busy(&bot.id) {
                busy.push(bot.name.clone());
            }
            let waiting = self
                .brain
                .swarm
                .store
                .queued_deliveries(&bot.id)
                .map(|deliveries| deliveries.len())
                .unwrap_or(0);
            if waiting > 0 {
                queued.push(format!("{}:{waiting}", bot.name));
            }
        }
        let tasks = self
            .brain
            .swarm
            .store
            .queued_tasks()
            .map(|tasks| tasks.len())
            .unwrap_or(0);
        panic!(
            "the swarm did not go quiet within {BUDGET:?}: busy={busy:?} queued={queued:?} \
             queued_tasks={tasks}"
        );
    }

    /// What the bot's turns did, for a failure message that says *why* a
    /// transcript is missing a tool call.
    ///
    /// "the transcript has no `read_file` call" alone cannot distinguish the
    /// three things that actually produce it: the turn never ran, the turn
    /// failed (and the reason is on the turn), or the model was served a
    /// different script than the one this test pushed. Guessing between those
    /// costs a debugging session each time, so the report is part of the
    /// assertion instead of something a future reader has to go and add.
    fn turn_report(&self, dm: &ConversationId) -> String {
        let turns = match self.brain.swarm.store.turns_in(dm, 50) {
            Ok(turns) => turns,
            Err(error) => return format!("<cannot read turns: {error}>"),
        };
        if turns.is_empty() {
            return "<no turn ran in this conversation at all>".to_string();
        }
        turns
            .iter()
            .map(|turn| {
                format!(
                    "{:?} status={:?} error={:?} llm_calls={} tool_calls={}",
                    turn.id, turn.status, turn.error, turn.usage.llm_calls, turn.usage.tool_calls
                )
            })
            .collect::<Vec<_>>()
            .join("\n  ")
    }

    /// The transcript of one conversation, ascending.
    fn entries(&self, dm: &ConversationId) -> Vec<Entry> {
        self.brain
            .swarm
            .conversations
            .tail(dm, 1000)
            .expect("the transcript reads")
    }

    /// Wait until the conversation transcript matches a predicate, or panic with `desc`.
    async fn wait_for_transcript<F>(
        &self,
        dm: &ConversationId,
        desc: &str,
        predicate: F,
    ) -> Vec<Entry>
    where
        F: Fn(&[Entry]) -> bool,
    {
        let deadline = tokio::time::Instant::now() + BUDGET;
        loop {
            let entries = self.entries(dm);
            if predicate(&entries) {
                return entries;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("transcript never satisfied `{desc}` within {BUDGET:?}: {entries:?}");
            }
            tokio::time::sleep(POLL).await;
        }
    }

    // -- the node's audit log -------------------------------------------------

    /// `<tmp>/node/audit/<node id>.jsonl`, once it holds at least `count`
    /// lines. Written by the *node child*, so the wait also proves the daemon
    /// flushed what it decided.
    async fn audit_lines(&self, node: &NodeId, count: usize) -> Vec<Value> {
        let path = self.node_dir.join("audit").join(format!("{node}.jsonl"));
        let deadline = tokio::time::Instant::now() + QUICK;
        loop {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let lines: Vec<Value> = text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| {
                    serde_json::from_str(line).unwrap_or_else(|error| {
                        panic!("`{line}` is not a JSON audit line: {error}")
                    })
                })
                .collect();
            if lines.len() >= count {
                return lines;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{} never gained {count} audit line(s); it holds {}:\n{text}",
                path.display(),
                lines.len()
            );
            tokio::time::sleep(POLL).await;
        }
    }

    // -- shutdown -------------------------------------------------------------

    /// Stop the listener and the swarm. Called by every test that finishes
    /// normally; `Drop` covers the ones that panic.
    async fn stop(mut self) {
        self.brain.state.request_shutdown();
        if let Some(serve) = self.serve.take() {
            match tokio::time::timeout(QUICK, serve).await {
                Err(_) => panic!("the brain's listener did not stop after a shutdown request"),
                Ok(Err(error)) => panic!("the brain's listener task panicked: {error}"),
                Ok(Ok(Err(error))) => panic!("the brain's listener failed: {error:#}"),
                Ok(Ok(Ok(()))) => {}
            }
        }
        self.brain.swarm.shutdown().await;
        assert_eq!(
            self.brain.swarm.runners.len(),
            0,
            "no runner may outlive the test"
        );
        // `self` (and the TempDir world) drops here.
    }
}

impl Drop for Rig {
    /// A panicking test still must not leave a listener task or a bot runner
    /// spinning. `Swarm::shutdown` is async, so this is the synchronous best
    /// effort — the same shape as the brain crate's own `TestBrain::drop`.
    fn drop(&mut self) {
        self.brain.state.request_shutdown();
        if let Some(serve) = self.serve.take() {
            serve.abort();
        }
        for id in self.brain.swarm.runners.bot_ids() {
            if let Some(handle) = self.brain.swarm.runners.get(&id) {
                handle.shutdown();
            }
        }
    }
}

/// The decoded JSON body of a successful response, with a message that says
/// which call it came from.
async fn body_of(response: reqwest::Response) -> Value {
    let url = response.url().to_string();
    let status = response.status();
    let text = response.text().await.expect("a response body");
    assert!(status.is_success(), "{url} answered {status}: {text}");
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{url} answered `{text}`, which is not JSON: {error}"))
}

/// The `tool` entry of one tool call in a transcript, or a panic naming what
/// the transcript does hold.
fn tool_call<'a>(entries: &'a [Entry], tool: &str) -> &'a ToolCallEntry {
    entries
        .iter()
        .find_map(|entry| match &entry.kind {
            EntryKind::ToolCall(call) if call.tool == tool => Some(call.as_ref()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the transcript has no `{tool}` call: {entries:?}"))
}

// --- the tests ---------------------------------------------------------------

/// The whole round trip: pair the real binary, run it, bind a bot to it, and
/// let that bot's scripted model call `read_file` and `shell` — both served by
/// the real node process, over a real WebSocket, under the node's own policy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_node_serves_a_bound_bots_read_and_shell_calls() {
    let tmp = tempfile::tempdir().expect("a temp dir");
    let rig = Rig::boot(tmp).await;

    // The real binary pairs: exit 0, `identity.json` at 0600, the node id on
    // stdout (asserted inside `pair`).
    let node_id = rig.pair().await;

    // The real binary runs, rooted at the node's own temp root, and shows up
    // in the roster online, labelled, with both capabilities approved.
    let daemon = rig.run_daemon("it-node", &["read_file=always", "shell=always"]);
    let row = rig.wait_for_online(&node_id, "it-node", &daemon).await;
    assert_eq!(row["kind"], json!("remote"), "{row}");
    assert_eq!(
        row["declared"], row["approved"],
        "redeeming the pairing code approved the first hello's declarations: {row}"
    );
    assert_eq!(row["revoked"], json!(false), "{row}");

    // A bot bound to that node — and a decoy marker in the bot's brain-side
    // workspace, so a silent local fallback would come back with the wrong text.
    let (bot, dm) = rig.create_bound_bot(&node_id, "remote").await;
    rig.plant_brain_decoy("remote");

    // The model asks for the marker twice: once through `fs.read`, once through
    // a streamed `shell.exec` (`cat` inside the node's root — a harmless
    // command, inside a TempDir).
    let fake = rig.fake();
    fake.push_turn(
        ScriptedTurn::text("let me read it")
            .with_tool_call("read_file", json!({"path": "marker.txt"})),
    );
    fake.push_turn(
        ScriptedTurn::text("let me cat it")
            .with_tool_call("shell", json!({"command": "cat marker.txt"})),
    );
    fake.push_turn(ScriptedTurn::text("done"));
    rig.send_message(&dm, &bot, "what does marker.txt say?")
        .await;
    rig.settle().await;

    // The transcript: both calls completed, both attributed to the remote
    // node, and the text that came back is the NODE's marker — the assertion
    // that proves the call really crossed the socket.
    let entries = rig.entries(&dm);
    // Named here rather than left to `tool_call`'s generic panic: the turn
    // report is what distinguishes "never ran" from "failed" from "ran a
    // different script", and those have different fixes.
    assert!(
        entries.iter().any(|entry| {
            matches!(&entry.kind, EntryKind::ToolCall(call) if call.tool == "read_file")
        }),
        "the bound bot called `read_file`; transcript={} entries\n  turns:\n  {}",
        entries.len(),
        rig.turn_report(&dm)
    );
    let read = tool_call(&entries, "read_file");
    assert_eq!(
        read.status,
        ToolCallStatus::Completed,
        "the remote read completed: {read:?}"
    );
    assert_eq!(
        read.node_id.as_ref(),
        Some(&node_id),
        "the tool-call entry names the remote node, not the local one: {read:?}"
    );
    let read_text = read.result_text.as_deref().unwrap_or_default();
    assert!(
        read_text.contains(NODE_MARKER),
        "the bytes came back from the node's root: {read_text:?}"
    );
    assert!(
        !read_text.contains(BRAIN_MARKER),
        "a silent local fallback would have read the brain's decoy: {read_text:?}"
    );

    let shell = tool_call(&entries, "shell");
    assert_eq!(
        shell.status,
        ToolCallStatus::Completed,
        "the remote shell call completed: {shell:?}"
    );
    assert_eq!(
        shell.node_id.as_ref(),
        Some(&node_id),
        "the tool-call entry names the remote node: {shell:?}"
    );
    let shell_text = shell.result_text.as_deref().unwrap_or_default();
    // The exit code and the stdout section are assembled from the node's
    // `Started`/`Output`/`Exit` chunks; a stream that ended without an `Exit`
    // chunk is a hard error brain-side, so this exact text *is* the proof that
    // the whole chunk stream arrived.
    assert_eq!(
        shell_text,
        format!("exit=0\n--- stdout ---\n{NODE_MARKER}"),
        "the exit code and stdout arrived through the chunk stream"
    );

    // The results also reached the model: a later scripted round saw them in
    // its own conversation history.
    let requests = fake.requests();
    let saw_node_marker = requests
        .iter()
        .flat_map(|request| request.messages.iter())
        .any(|message| message.content.contains(NODE_MARKER));
    assert!(
        saw_node_marker,
        "the remote results were fed back to the model"
    );

    // The node's own audit JSONL gained both invocations, attributed to the
    // bot that made them: the brain derived the attribution, the node recorded
    // it on its own disk.
    let lines = rig.audit_lines(&node_id, 2).await;
    let read_line = lines
        .iter()
        .find(|line| line["tool"] == json!("read_file"))
        .unwrap_or_else(|| panic!("the node audited the read: {lines:?}"));
    assert_eq!(read_line["decision"], json!("run"), "{read_line}");
    assert_eq!(read_line["outcome"], json!("ok"), "{read_line}");
    assert_eq!(read_line["surface"], json!("read_file"), "{read_line}");
    assert_eq!(
        read_line["bot_id"],
        json!(bot.to_string()),
        "the audit line names the bot that asked: {read_line}"
    );
    assert_eq!(
        read_line["node_id"],
        json!(node_id.to_string()),
        "{read_line}"
    );
    let shell_line = lines
        .iter()
        .find(|line| line["tool"] == json!("shell"))
        .unwrap_or_else(|| panic!("the node audited the shell call: {lines:?}"));
    assert_eq!(shell_line["decision"], json!("run"), "{shell_line}");
    assert_eq!(shell_line["outcome"], json!("ok"), "{shell_line}");
    assert_eq!(
        shell_line["target"],
        json!("cat marker.txt"),
        "{shell_line}"
    );
    assert_eq!(shell_line["bot_id"], json!(bot.to_string()), "{shell_line}");

    daemon.kill().await;
    rig.stop().await;
}

/// The node decides finally (D1.4). This brain's owner set the brain-side
/// `shell` ceiling to `always`, but the node was started with
/// `--ceiling shell=never` — so the same scripted call is refused *by the node
/// process*, with the node's own words, and the refusal reaches the model as
/// `ERROR: …`. Nothing ran on either machine.
/// `fs.download` end to end over a **real data stream**: the real brain mints a
/// grant, the real `swarmbot-node` child reads a file under its own policy and
/// dials the brain's `/node/stream/:id`, and the brain assembles the bytes and
/// checks the digest the node put in its RPC answer.
///
/// This is the piece neither half's own tests can cover. `brain/tests/streams.rs`
/// proves the endpoint against a scripted dialer; the node's own tests prove the
/// framing; only this one proves the two halves **agree** — that the
/// `streamId`/`streamToken` the brain mints is the one the node dials with, that
/// the file comes back byte-for-byte, and that the digest crosses both machines.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_node_streams_a_file_to_the_brain_over_a_data_stream() {
    use swarmbot_contracts::Invocation;
    use swarmbot_node_protocol::rpc::FsDownload;
    use swarmbot_node_protocol::streams::StreamKind;

    let rig = Rig::boot(TempDir::new().expect("a temp dir")).await;
    let node = rig.pair().await;
    let daemon = rig.run_daemon("stream-node", &[]);
    rig.wait_for_online(&node, "stream-node", &daemon).await;

    // Bytes that would not survive a text round trip: a NUL, a lone high byte,
    // and a line break. A transfer that quietly decoded as UTF-8 would corrupt
    // every one of them, and the digest is what would catch it.
    let content: Vec<u8> = b"binary-ish payload\n\x00\xff\x01end".to_vec();
    std::fs::write(rig.node_root.join("payload.bin"), &content).expect("the node's file");

    // The brain's half: mint and register **before** the RPC, so an immediate
    // dial finds a ticket.
    let hub = rig.brain.state.gateway.streams();
    let ticket = hub.open(StreamKind::Transfer).expect("a grant is minted");
    let (stream_id, token) = (ticket.stream_id.clone(), ticket.token.clone());

    let peer = rig
        .brain
        .state
        .gateway
        .peer(&node)
        .expect("the node's control socket is connected");
    let inv = Invocation::new(
        BotId::from_suffix("lead"),
        ConversationId::from_suffix("dm-lead"),
    );
    let call = FsDownload {
        inv: inv.clone(),
        path: "payload.bin".into(),
        stream_id: stream_id.clone(),
        stream_token: token,
    };
    // The RPC and the stream run concurrently: the node answers only *after* it
    // has finished sending, so awaiting the call first would deadlock on the
    // very acks the stream is delivering.
    let answer = tokio::spawn(async move {
        peer.call(
            swarmbot_node_protocol::rpc::FS_DOWNLOAD,
            serde_json::to_value(call).expect("the payload serializes"),
        )
        .await
    });

    let received = tokio::time::timeout(BUDGET, hub.receive(ticket))
        .await
        .expect("the transfer finishes within the budget")
        .expect("the transfer completes");
    assert_eq!(
        received.bytes, content,
        "the bytes crossed the stream unchanged"
    );
    assert_eq!(
        received.sha256,
        swarmbot_node_protocol::chunk_sha256(&content)
    );

    // The node's RPC answer names what it sent, and it agrees with what
    // arrived: the claim and the payload are checked against each other.
    let answer = tokio::time::timeout(BUDGET, answer)
        .await
        .expect("the RPC answers")
        .expect("the call task did not panic")
        .expect("fs.download succeeded");
    assert_eq!(answer["size"].as_u64(), Some(content.len() as u64));
    assert_eq!(
        answer["sha256"].as_str(),
        Some(received.sha256.as_str()),
        "the node's claim and the arriving bytes hash to the same thing: {answer}"
    );

    // And the node's own audit log recorded the read, attributed to the bot that
    // asked — the stream is not a way around attribution.
    let audit = std::fs::read_to_string(rig.node_dir.join("audit").join(format!("{node}.jsonl")))
        .unwrap_or_else(|error| panic!("the node's audit log is readable: {error}"));
    let line = audit
        .lines()
        .rfind(|line| line.contains("read_file"))
        .unwrap_or_else(|| panic!("no read_file line in the node's audit log:\n{audit}"));
    let entry: Value = serde_json::from_str(line).expect("an audit line is JSON");
    assert_eq!(
        entry["bot_id"],
        json!(inv.bot_id.to_string()),
        "the audit line names the bot that asked: {entry}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_nodes_own_never_ceiling_refuses_what_the_brain_allowed() {
    let tmp = tempfile::tempdir().expect("a temp dir");
    let rig = Rig::boot(tmp).await;
    let node_id = rig.pair().await;
    let daemon = rig.run_daemon("it-node", &["read_file=always", "shell=never"]);
    let row = rig
        .wait_for_row(&node_id, "online with policy default `never`", |row| {
            row["online"] == json!(true)
                && row["label"] == json!("it-node")
                && row["policy"]["default"] == json!("never")
        })
        .await;
    assert_eq!(row["policy"]["default"], json!("never"), "{row}");

    let (bot, dm) = rig.create_bound_bot(&node_id, "remote").await;
    rig.plant_brain_decoy("remote");

    let fake = rig.fake();
    fake.push_turn(
        ScriptedTurn::text("reading is fine")
            .with_tool_call("read_file", json!({"path": "marker.txt"})),
    );
    fake.push_turn(
        ScriptedTurn::text("shelling is not")
            .with_tool_call("shell", json!({"command": "cat marker.txt"})),
    );
    fake.push_turn(ScriptedTurn::text("done"));
    rig.send_message(&dm, &bot, "read marker.txt, then cat it")
        .await;
    let entries = rig
        .wait_for_transcript(&dm, "read_file completed and shell finished", |entries| {
            let read_ok = entries.iter().any(|e| {
                matches!(&e.kind, EntryKind::ToolCall(c) if c.tool == "read_file" && c.status == ToolCallStatus::Completed)
            });
            let shell_done = entries.iter().any(|e| {
                matches!(&e.kind, EntryKind::ToolCall(c) if c.tool == "shell" && c.status != ToolCallStatus::Running)
            });
            read_ok && shell_done
        })
        .await;
    rig.settle().await;

    // `read_file` still crosses and completes: the refusal is per-surface, on
    // the same socket, in the same turn.
    let read = tool_call(&entries, "read_file");
    assert_eq!(read.status, ToolCallStatus::Completed, "{read:?}");
    assert_eq!(read.node_id.as_ref(), Some(&node_id), "{read:?}");
    assert!(
        read.result_text
            .as_deref()
            .unwrap_or_default()
            .contains(NODE_MARKER),
        "{read:?}"
    );

    // `shell` is refused by the node's own ceiling, and the words are the
    // node's: "this node never allows `<surface>` actions … retrying cannot
    // help" exists only in `crates/node/src/handlers.rs` (`Gate::denial_reason`).
    // The brain's own refusal reads differently — and could not happen here at
    // all, since its owner-configured ceiling is `always` — so this text proves
    // which process decided.
    let shell = tool_call(&entries, "shell");
    assert_eq!(
        shell.status,
        ToolCallStatus::Failed,
        "a refusal is a failed call, not a completed one: {shell:?}"
    );
    assert_eq!(shell.node_id.as_ref(), Some(&node_id), "{shell:?}");
    let text = shell.result_text.as_deref().unwrap_or_default();
    assert!(
        text.starts_with("ERROR: refused by policy:"),
        "the typed refusal reaches the transcript as an ERROR result: {text:?}"
    );
    assert!(
        text.contains("this node never allows `shell` actions"),
        "the refusal is the node's own sentence: {text:?}"
    );
    assert!(text.contains("retrying cannot help"), "{text:?}");
    assert!(
        !text.contains(NODE_MARKER) && !text.contains(BRAIN_MARKER),
        "nothing ran on the node and nothing ran locally on the brain: {text:?}"
    );

    // The same text is what the model saw in its next round.
    let requests = fake.requests();
    let saw_refusal = requests
        .iter()
        .flat_map(|request| request.messages.iter())
        .any(|message| {
            message.content.starts_with("ERROR: refused by policy")
                && message.content.contains("this node never allows")
        });
    assert!(saw_refusal, "the refusal reached the model as `ERROR: …`");

    // The node audited both calls: the read as `run`, the shell as `refuse` —
    // the decision was made and recorded *on the node*.
    let lines = rig.audit_lines(&node_id, 2).await;
    let read_line = lines
        .iter()
        .find(|line| line["tool"] == json!("read_file"))
        .unwrap_or_else(|| panic!("the node audited the read: {lines:?}"));
    assert_eq!(read_line["decision"], json!("run"), "{read_line}");
    assert_eq!(read_line["outcome"], json!("ok"), "{read_line}");
    let shell_line = lines
        .iter()
        .find(|line| line["tool"] == json!("shell"))
        .unwrap_or_else(|| panic!("the node audited the refused shell call: {lines:?}"));
    assert_eq!(
        shell_line["decision"],
        json!("refuse"),
        "the node's own ceiling refused it before any approval was asked: {shell_line}"
    );
    assert_eq!(shell_line["outcome"], json!("error"), "{shell_line}");
    assert_eq!(
        shell_line["target"],
        json!("cat marker.txt"),
        "{shell_line}"
    );
    assert_eq!(shell_line["bot_id"], json!(bot.to_string()), "{shell_line}");

    daemon.kill().await;
    rig.stop().await;
}

/// Revoke kills the real child: `DELETE /api/nodes/{id}` closes the control
/// socket with 4401, the daemon stops redialing for good and exits non-zero
/// telling its operator to re-pair, and a redial attempt afterwards is refused
/// at the handshake, before any upgrade.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoking_closes_the_real_child_with_4401_and_a_redial_is_refused() {
    let tmp = tempfile::tempdir().expect("a temp dir");
    let rig = Rig::boot(tmp).await;
    let node_id = rig.pair().await;
    let mut daemon = rig.run_daemon("it-node", &["read_file=always", "shell=always"]);
    rig.wait_for_online(&node_id, "it-node", &daemon).await;

    // The owner revokes through the API.
    assert_eq!(
        rig.delete(&format!("/api/nodes/{node_id}")).await,
        204,
        "the revoke is accepted"
    );
    let row = rig
        .wait_for_row(&node_id, "revoked and offline", |row| {
            row["revoked"] == json!(true) && row["online"] == json!(false)
        })
        .await;
    assert!(
        row["approved"].as_array().expect("caps").is_empty(),
        "a revoked node is approved for nothing: {row}"
    );

    // The child exits *by itself* — nobody killed it — non-zero, naming 4401
    // and the re-pair that is the only fix.
    let exit = daemon.wait(QUICK).await;
    assert_eq!(
        exit.code(),
        Some(1),
        "a revoked node exits non-zero; its log:\n{}",
        daemon.log()
    );
    daemon.wait_for_line("4401", QUICK).await;
    daemon.wait_for_line("revoked", QUICK).await;

    // A redial with the same pinned identity is refused at the handshake:
    // HTTP 401 before any upgrade, which the node reports as an unreachable
    // brain and retries with backoff — it never comes back online.
    let redial = rig.run_daemon("it-node", &[]);
    // Wait for the sentence, not the number. A bare `401` also matches the
    // `4401` close code and — less obviously — a random hex id: this line is
    // `cannot reach the brain at <url>: HTTP error: 401 Unauthorized`, and the
    // URL or a node id in the same message can contain those three digits. The
    // assertion below then reads the status *out of* the line it waited for.
    let line = redial.wait_for_line("cannot reach the brain", QUICK).await;
    assert!(
        line.contains("401"),
        "the dial failure carries the status the brain answered with: {line}"
    );
    let row = rig.node_row(&node_id).await;
    assert_eq!(
        row["online"],
        json!(false),
        "the refused redial never came online: {row}"
    );
    assert_eq!(row["revoked"], json!(true), "{row}");

    redial.kill().await;
    rig.stop().await;
}

/// The matcher behind `wait_for_line`, pinned.
///
/// Without `message_of`, the first assertion below fails: `.929401` contains
/// `401`, so a needle of `401` matched the startup line's timestamp and the
/// test above reported "the refusal is a dial failure…" about a line that was
/// not a refusal at all. This is the reproduction, kept cheap and deterministic.
#[test]
fn a_timestamp_is_never_mistaken_for_the_message() {
    let startup = "2026-11-13T01:33:44.929401Z  INFO swarmbot-node starting node_id=nod_x";
    assert!(
        !message_of(startup).contains("401"),
        "the timestamp is not part of the message"
    );
    assert!(message_of(startup).contains("starting node_id"));

    let refusal = "2026-11-13T01:33:45.000001Z  INFO swarmbot-node redialing \
                   last_error=\"cannot reach the brain at http://127.0.0.1:1: \
                   HTTP error: 401 Unauthorized\"";
    let message = message_of(refusal);
    assert!(message.contains("401"), "{message}");
    assert!(message.contains("cannot reach the brain"), "{message}");

    // A line with no tracing prefix is its own message, not an empty string.
    assert_eq!(message_of("plain output"), "plain output");
    assert_eq!(message_of(""), "");
}
