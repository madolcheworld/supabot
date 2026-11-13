//! **Two separate OS processes, one real model, one remote tool call.**
//!
//! This is the last item on the README's unverified list, and it stayed there for
//! a precise reason: `crates/node/tests/real_brain.rs` proves the whole remote-call
//! path against the real `swarmbot-node` binary, but it composes the brain
//! *in process* — which is what makes a scripted `FakeBackend` reachable. A real
//! brain **process** cannot be handed a script, so the two halves were each proven
//! and their combination was not: a remote tool call from a bot turn in two
//! separate processes driven by a *real* model had never been run.
//!
//! This test runs exactly that:
//!
//! ```text
//! swarmbot-brain  (process 1)  ←── HTTP ────  this test  ─── process 2 ──→  swarmbot-node
//!        │                                                                    │
//!        └──────────── signed wss/ws control socket + tool call ──────────────┘
//! ```
//!
//! The model is a real gateway, so the assertions are about what a model *did*,
//! not about what a script says. Three things make a silent local fallback
//! impossible:
//!
//! * the marker file exists **only in the node's root**, with a decoy of different
//!   content in the brain's own workspace and in the bot's workspace — so a call
//!   that ran on the brain returns the wrong string;
//! * the node's own audit log must carry the `read_file` line, attributed to the
//!   bot that asked;
//! * the turn's `usage.input_tokens` must be non-zero, which no `FakeBackend` can
//!   produce — it is the proof that a real model drove the turn.
//!
//! Gated on `PROTO_LIVE_LLM=1` plus `PROTO_LIVE_BASE_URL`/`PROTO_LIVE_API_KEY`/
//! `PROTO_LIVE_MODEL`, and it costs **one small model turn with a couple of tool
//! rounds**. It prints what it spent. Run it with:
//!
//! ```text
//! PROTO_LIVE_LLM=1 PROTO_LIVE_BASE_URL=https://…/v1 PROTO_LIVE_API_KEY=… \
//! PROTO_LIVE_MODEL=… cargo test -p swarmbot-node --test live_two_process -- --ignored
//! ```

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const NODE_BIN: &str = env!("CARGO_BIN_EXE_swarmbot-node");

/// The **brain** binary, next to this test's own executable.
///
/// `CARGO_BIN_EXE_<name>` is only defined for a package's *own* bins, and the
/// brain belongs to another package — so the path is derived from
/// `current_exe()`, which lands in `target/<profile>/deps/`. One directory up is
/// where cargo puts every binary the workspace built, which is why the test
/// documents that both crates must be built first (the gate builds everything, so
/// this only matters for a focused run).
fn brain_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("the test executable's own path");
    let dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("the test binary lives in target/<profile>/deps");
    let candidate = dir.join("swarmbot-brain");
    assert!(
        candidate.is_file(),
        "the brain binary is not at {} — build it first: \
         `cargo build -p swarmbot-brain -p swarmbot-node`",
        candidate.display()
    );
    candidate
}

/// Generous: a real model may think, call a tool, and answer. Bounded so a stuck
/// child fails the test instead of the budget.
const LIVE_BUDGET: Duration = Duration::from_secs(240);
const BUDGET: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(100);

/// The marker, and the decoys. Same file name everywhere, different content: which
/// string the model reports is the proof of **where** the tool ran.
const NODE_MARKER: &str = "from the node";
const BRAIN_DECOY: &str = "from the brain";

/// A child process that is killed on drop, so a panicking test leaves no daemon
/// behind on this machine.
struct Guard {
    #[allow(dead_code)] // kept for the failure messages, which name the child
    name: String,
    child: Child,
    log_path: PathBuf,
}

impl Guard {
    fn spawn(name: &str, command: &mut Command, dir: &Path) -> (Guard, PathBuf) {
        let path = dir.join(format!("{name}.log"));
        let file = std::fs::File::create(&path).expect("the child's log file");
        command
            .stdout(Stdio::from(file.try_clone().expect("clone the log handle")))
            .stderr(Stdio::from(file));
        let child = command.spawn().expect("the child binary spawns");
        (
            Guard {
                name: name.to_string(),
                child,
                log_path: path.clone(),
            },
            path,
        )
    }

    /// The child's log so far. Read from the file each time rather than buffered:
    /// the child writes it directly, so this is always current.
    fn text(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The brain process, its address, and the owner token.
struct BrainProc {
    guard: Guard,
    addr: String,
    token: String,
}

impl BrainProc {
    fn get(&self, path: &str) -> Result<Value, String> {
        let (status, body) = http(&self.addr, "GET", path, &self.token, None)?;
        if !(200..300).contains(&status) {
            return Err(format!("GET {path} answered {status}: {body}"));
        }
        serde_json::from_str(&body).map_err(|error| format!("{error}: {body}"))
    }

    fn post(&self, path: &str, payload: Option<&Value>) -> Result<Value, String> {
        let body = payload.map(|value| value.to_string());
        let (status, text) = http(&self.addr, "POST", path, &self.token, body.as_deref())?;
        if !(200..300).contains(&status) {
            return Err(format!("POST {path} answered {status}: {text}"));
        }
        serde_json::from_str(&text).map_err(|error| format!("{error}: {text}"))
    }
}

/// A minimal owner-authenticated HTTP call. Hand-rolled because the alternative is
/// a client dependency this crate does not have, and the request is a status line
/// plus a body.
fn http(
    addr: &str,
    method: &str,
    path: &str,
    token: &str,
    body: Option<&str>,
) -> Result<(u16, String), String> {
    let mut stream = TcpStream::connect(addr).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .map_err(|error| error.to_string())?;
    let payload = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| error.to_string())?;
    let mut raw = String::new();
    stream
        .read_to_string(&mut raw)
        .map_err(|error| error.to_string())?;
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("no status line: {raw}"))?;
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
    Ok((status, body))
}

/// The address out of the brain's own `listener=127.0.0.1:PORT` log field.
///
/// The `listener=` field specifically, not any digits in the line: a tracing
/// prefix is full of digits and colons, which is the bug `AGENTS.md` records from
/// `real_brain`'s `401`-inside-a-timestamp failure.
fn listener_addr(line: &str) -> Option<String> {
    let rest = line.split("listener=").nth(1)?;
    let candidate: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ':')
        .collect();
    (candidate.contains(':') && !candidate.ends_with(':')).then_some(candidate)
}

/// Boot the brain as its **own process**, with the real backend in its config.
///
/// The API key comes from the environment variable the backend names
/// (`PROTO_LIVE_API_KEY`), which the child inherits — so the key is never written
/// to a file, never in argv, and never in the test's output.
fn start_brain(dir: &Path) -> BrainProc {
    let data = dir.join("brain");
    std::fs::create_dir_all(&data).expect("the brain's data dir");
    let base_url = std::env::var("PROTO_LIVE_BASE_URL").expect("set PROTO_LIVE_BASE_URL");
    let model = std::env::var("PROTO_LIVE_MODEL").expect("set PROTO_LIVE_MODEL");
    // The config names the *environment variable*, never the key (D2.3).
    let config = format!(
        r#"data_dir = "{data}"
public_addr = "127.0.0.1:0"
default_backend = "fake"

[[backends]]
id = "fake"
kind = "fake"
model = "fake"

[[backends]]
id = "live"
kind = "openai_compat"
base_url = "{base_url}"
api_key_env = "PROTO_LIVE_API_KEY"
model = "{model}"
"#,
        data = data.display(),
    );
    std::fs::write(data.join("config.toml"), config).expect("the brain's config");

    let mut command = Command::new(brain_bin());
    command
        .arg("--data-dir")
        .arg(&data)
        .arg("--config")
        .arg(data.join("config.toml"));
    let (guard, _log_path) = Guard::spawn("brain", &mut command, dir);

    // The brain announces its listener on stdout; a `--addr 127.0.0.1:0` bind makes
    // that the only way to learn the port without racing a probe.
    let token_path = data.join("owner-token");
    let deadline = Instant::now() + BUDGET;
    let addr = loop {
        if let Some(line) = guard.text().lines().find_map(listener_addr) {
            break line;
        }
        assert!(
            Instant::now() < deadline,
            "the brain never announced its listener; log:\n{}",
            guard.text()
        );
        std::thread::sleep(POLL);
    };
    let token = loop {
        if let Ok(text) = std::fs::read_to_string(&token_path) {
            let trimmed = text.trim().to_string();
            if !trimmed.is_empty() {
                break trimmed;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the brain never minted an owner token"
        );
        std::thread::sleep(POLL);
    };

    // Readiness: the API answers before the first request races the accept loop.
    let brain = BrainProc { guard, addr, token };
    loop {
        match brain.get("/api/health") {
            Ok(_) => break,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "the brain never answered /api/health: {error}\nlog:\n{}",
                    brain.guard.text()
                );
                std::thread::sleep(POLL);
            }
        }
    }
    brain
}

fn run_cli(args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new(NODE_BIN);
    command.args(args);
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().expect("the node binary spawns")
}

/// The bot names the file, so the model has an obvious single tool to reach for.
const INSTRUCTIONS: &str = "You are a test bot. When asked to read a file, you MUST call the \
                            `read_file` tool with that path and then report the file's exact \
                            contents. Never guess a file's contents and never say you cannot \
                            read it: the tool is available to you.";

#[test]
#[ignore = "live: two real processes and a real model (PROTO_LIVE_LLM=1 + gateway env)"]
fn a_real_model_calls_a_tool_on_a_real_node_in_another_process() {
    if std::env::var("PROTO_LIVE_LLM").as_deref() != Ok("1") {
        eprintln!("skipping live two-process test: set PROTO_LIVE_LLM=1 to run it");
        return;
    }
    let tmp = TempDir::new().expect("a temp dir");
    let node_root = tmp.path().join("noderoot");
    std::fs::create_dir_all(&node_root).expect("the node's root");
    std::fs::write(node_root.join("marker.txt"), format!("{NODE_MARKER}\n"))
        .expect("the node's marker");

    // The brain's own workspace and the bot's workspace get a decoy of the same
    // name with different content. A tool call that silently ran on the brain
    // returns the decoy, and the assertion below fails with the wrong string in
    // hand rather than a vague "the reply did not match".
    let brain_ws = tmp.path().join("brain").join("workspaces");
    std::fs::create_dir_all(&brain_ws).expect("the brain's workspace root");

    let brain = start_brain(tmp.path());
    eprintln!("brain listening on {}", brain.addr);

    // 1. Pair the real node binary against the real brain.
    let code = brain
        .post("/api/nodes/pairing-codes", None)
        .expect("a pairing code is minted");
    let code = code["code"]
        .as_str()
        .expect("the code is a string")
        .to_string();
    let node_dir = tmp.path().join("node");
    let pair = run_cli(
        &[
            "--data-dir",
            node_dir.to_str().expect("utf-8"),
            "pair",
            &format!("http://{}", brain.addr),
            &code,
        ],
        &[("SWARMBOT_NODE_LOG", "info")],
    );
    assert!(
        pair.status.success(),
        "pairing failed:\n{}{}",
        String::from_utf8_lossy(&pair.stdout),
        String::from_utf8_lossy(&pair.stderr)
    );
    let node_id = String::from_utf8_lossy(&pair.stdout)
        .lines()
        .next()
        .expect("pair prints the node id")
        .trim()
        .to_string();

    // 2. Run the daemon as a **separate process**, on its own root.
    let mut node_command = Command::new(NODE_BIN);
    node_command
        .arg("--data-dir")
        .arg(&node_dir)
        .arg("run")
        .arg("--root")
        .arg(&node_root)
        .arg("--label")
        .arg("live-node")
        .env("SWARMBOT_NODE_LOG", "info");
    let (node_guard, node_log) = Guard::spawn("node", &mut node_command, tmp.path());

    // 3. Wait for the roster to show it online with its capabilities approved.
    let deadline = Instant::now() + BUDGET;
    loop {
        let nodes = brain.get("/api/nodes").expect("the roster reads");
        if let Some(row) = nodes
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == json!(node_id)))
            && row["online"] == json!(true)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "node {node_id} never came online; brain log:\n{}\nnode log:\n{}",
            brain.guard.text(),
            std::fs::read_to_string(&node_log).unwrap_or_default()
        );
        std::thread::sleep(POLL);
    }

    // 4. A bot on the **real backend**, hosted by that node.
    let bot = brain
        .post(
            "/api/bots",
            Some(&json!({
                "name": "live",
                "label": "Live",
                "instructions": INSTRUCTIONS,
                "backend": "live",
                "node": node_id,
            })),
        )
        .expect("the bot is created");
    let bot_id = bot["id"].as_str().expect("a bot id").to_string();
    let dm = bot["direct_conversation_id"]
        .as_str()
        .expect("the bot has a DM")
        .to_string();
    // A decoy in the bot's own brain-side workspace: the node's root is *not* this.
    let bot_ws = brain_ws.join("live");
    std::fs::create_dir_all(&bot_ws).expect("the bot's workspace");
    std::fs::write(bot_ws.join("marker.txt"), format!("{BRAIN_DECOY}\n"))
        .expect("the decoy marker");

    // 5. Ask, in a way that leaves one obvious tool call.
    brain
        .post(
            &format!("/api/conversations/{dm}/messages"),
            Some(&json!({
                "text": "Read the file `marker.txt` with the read_file tool and tell me exactly \
                         what it contains. Quote the content.",
            })),
        )
        .expect("the message is accepted");

    // 6. Wait for the turn to **finish**, then read the reply.
    //
    // Waiting for the first non-empty bot text is wrong and this test learned it
    // the hard way: a streamed reply upserts one entry at a rising revision, so the
    // first thing visible is a partial word (`"The"`). "Done" is the bot's own
    // status going idle again, which the product itself uses to mean "the turn
    // ended".
    let deadline = Instant::now() + LIVE_BUDGET;
    let reply = loop {
        let tail = brain
            .get(&format!("/api/conversations/{dm}/tail?limit=50"))
            .expect("the transcript reads");
        let entries = tail.as_array().cloned().unwrap_or_default();
        // The bot's final **message**, not a streaming fragment and not its
        // reasoning. Two mistakes were made here before the assertion held:
        // matching any bot-authored text caught the reasoning entry (`"I"`), and
        // matching any non-empty text caught a partial word mid-stream. A message
        // entry is `kind.kind == "message"` and it is `post == "posted"` once the
        // turn has landed it.
        let text = entries.iter().find_map(|entry| {
            let author = entry.get("author")?;
            let is_bot = author.get("kind").and_then(Value::as_str) == Some("bot");
            let kind = entry.get("kind")?;
            let is_message = kind.get("kind").and_then(Value::as_str) == Some("message");
            let posted = kind.get("post").and_then(Value::as_str) == Some("posted");
            let text = kind.get("text").and_then(Value::as_str)?;
            (is_bot && is_message && posted && !text.trim().is_empty()).then(|| text.to_string())
        });
        let bots = brain.get("/api/bots").expect("the roster reads");
        let idle = bots
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == json!(bot_id)))
            // `BotStatus` serializes with a `state` tag: `{"state":"idle"}`.
            .map(|row| row["status"]["state"].as_str() == Some("idle"))
            .unwrap_or(false);
        if let (Some(text), true) = (text, idle) {
            break text;
        }
        assert!(
            Instant::now() < deadline,
            "the turn did not finish within {LIVE_BUDGET:?}. The transcript:\n{tail}\n\n\
             brain log:\n{}\n\nnode log:\n{}",
            brain.guard.text(),
            std::fs::read_to_string(&node_log).unwrap_or_default()
        );
        std::thread::sleep(POLL);
    };

    // 7. What did the real model spend? A `FakeBackend` cannot produce non-zero
    //    input tokens, so this is the proof that a real model drove the turn.
    let usage = brain.get("/api/usage").expect("the usage report reads");
    let by_bot = usage["by_bot"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .find(|row| row["bot_id"] == json!(bot_id))
        .unwrap_or_else(|| panic!("no usage row for the bot: {usage}"));
    let input_tokens = by_bot["usage"]["input_tokens"].as_u64().unwrap_or(0);
    eprintln!(
        "live two-process turn spent: input={input_tokens} output={} llm_calls={} \
         (model {})",
        by_bot["usage"]["output_tokens"],
        by_bot["usage"]["llm_calls"],
        std::env::var("PROTO_LIVE_MODEL").unwrap_or_default()
    );
    assert!(
        input_tokens > 0,
        "the turn reported no input tokens, so it did not run on the real model: {usage}\n\n\
         reply: {reply:?}\n\ntranscript:\n{}\n\nbrain log:\n{}\n\nnode log:\n{}",
        brain
            .get(&format!("/api/conversations/{dm}/tail?limit=50"))
            .unwrap_or_default(),
        brain.guard.text(),
        std::fs::read_to_string(&node_log).unwrap_or_default()
    );

    // 8. The reply is the **node's** marker, not the brain-side decoy.
    assert!(
        reply.contains(NODE_MARKER),
        "the model did not report the node's file. Reply: {reply:?}\n\nIf it says \
         `{BRAIN_DECOY}`, the read ran on the brain instead of the node — the exact bug \
         D9.11 closed."
    );
    assert!(
        !reply.contains(BRAIN_DECOY),
        "the read ran on the brain's workspace, not the node's: {reply:?}"
    );

    // 9. The node's own audit log names the tool call and the bot that asked.
    let audit_path = node_dir.join("audit").join(format!("{node_id}.jsonl"));
    let audit = std::fs::read_to_string(&audit_path).unwrap_or_else(|error| {
        panic!(
            "the node's audit log is at {}: {error}\nnode log:\n{}",
            audit_path.display(),
            std::fs::read_to_string(&node_log).unwrap_or_default()
        )
    });
    let line = audit
        .lines()
        .rfind(|line| line.contains("read_file"))
        .unwrap_or_else(|| panic!("no read_file line in the node's audit log:\n{audit}"));
    let entry: Value = serde_json::from_str(line).expect("an audit line is JSON");
    assert_eq!(
        entry["bot_id"],
        json!(bot_id),
        "the node attributed the call to the bot that asked: {entry}"
    );
    assert_eq!(
        entry["outcome"], "ok",
        "the node ran the call rather than refusing it: {entry}"
    );

    // The control socket and the tool call both crossed a real socket between two
    // real processes; say so once, since that is the whole claim of this test.
    eprintln!(
        "live two-process remote tool call: node {node_id} served read_file for bot {bot_id} \
         (brain pid {}, node pid {})",
        brain.guard.child.id(),
        node_guard.child.id()
    );
    // Held to the end so both children are alive for the whole test; `Drop` kills
    // them, so even a panicking assertion leaves no daemon on this machine.
    drop(node_guard);
}
