//! ACP adapter tests — no network and no real agents in the suite:
//!
//! * a **scripted in-memory agent** over a duplex pipe drives the whole
//!   protocol: handshake, session setup, a streamed turn, permission asks,
//!   the fs refusal, cancellation, idle timeout, process death, model
//!   selection;
//! * a **`/bin/sh` agent** over the real [`ProcessTransport`] proves NDJSON
//!   framing over a real child's stdio — and that dropping the backend kills
//!   the agent's whole process group;
//! * a live-agent test is `#[ignore]`d behind `PROTO_LIVE_ACP=1`: one turn,
//!   printing what it spent.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf, split,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use swarmbot_contracts::{BackendKind, BackendSpec};
use swarmbot_llm::{
    AcpAgent, AcpTransport, AgentHandle, AgentLaunch, BackendRegistry, ChatMessage, ChatRequest,
    FinishReason, LlmBackend, LlmError, LlmEvent, McpServerSpec, PermissionMode, ProcessTransport,
};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// How the scripted agent behaves. One behavior per protocol question, so a
/// failure names the behavior that broke.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Behavior {
    /// Thought + message chunks, a permission ask (id 900) and an
    /// `fs/read_text_file` call (id 901) mid-turn, then `end_turn`.
    Normal,
    /// Never answers the prompt until `session/cancel` arrives, then answers
    /// `cancelled`.
    AnswerAfterCancel,
    /// Never answers the prompt at all: the idle timeout must fire.
    NeverAnswer,
    /// Hangs up right after `session/new` — but only the first time (the
    /// transport's `died` flag makes the next spawn healthy, so the test also
    /// proves the connection respawns).
    DieAfterSession,
    /// `session/new` advertises models (current `m1`, available `m1`+`m2`);
    /// `session/set_model` works.
    ModelsWithSetModel,
    /// Same models, but `session/set_model` answers `-32601`: the unstable
    /// method is missing and the turn must continue with a warning.
    ModelsNoSetModelMethod,
    /// `session/new` advertises configOptions for "model" (current `m1`, available `m1`+`m2`);
    /// `session/set_config_option` works.
    ConfigOptionsWithSetConfigOption,
    /// `initialize` advertises `mcpCapabilities: { http: true, sse: false }`.
    McpHttpSupported,
    /// `initialize` advertises `mcpCapabilities: { http: false, sse: false }`.
    McpHttpUnsupported,
    /// The *first* `session/new` is never answered (so one turn's setup timeout
    /// fires); later ones answer normally, and the prompt streams a keepalive
    /// every 10 simulated seconds — resetting the idle clock — before answering
    /// at 50s, long after the first turn's setup budget ran out.
    SlowFirstSetup,
    /// `initialize` is never answered: connection establishment hangs until the
    /// setup budget expires.
    SilentHandshake,
    /// `session/new` is never answered, ever: every turn times out in setup, so
    /// the connection accumulates unanswered setup calls and is eventually
    /// replaced as wedged.
    NeverAnswerSessionNew,
}

type Log = Arc<Mutex<Vec<Value>>>;

/// The client's end of the agent's stdin, wrapped so the scripted agent can tell
/// when the client closed it. A real child sees EOF on stdin and exits; an
/// in-memory duplex does not, because the client still holds its own read half.
/// Modelling the EOF is what makes the isolation test able to catch a connection
/// being shut down: the agent exits, the pipe closes, and the reader on the
/// client side reports "gone" exactly as it does for a killed process.
struct ClosingStdin {
    inner: WriteHalf<DuplexStream>,
    closed: CancellationToken,
}

impl AsyncWrite for ClosingStdin {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl Drop for ClosingStdin {
    fn drop(&mut self) {
        // stdin dropped = EOF for the child.
        self.closed.cancel();
    }
}

struct ScriptedAgentTransport {
    behavior: Behavior,
    log: Log,
    /// Set once the first agent hung up (see [`Behavior::DieAfterSession`]).
    died: Arc<AtomicBool>,
    /// Whether `initialize` advertises `sessionCapabilities.close`. On by
    /// default (a modern ACP agent); one test turns it off to pin the honest
    /// "do not call an unadvertised optional method" behaviour.
    advertise_session_close: bool,
}

impl ScriptedAgentTransport {
    fn new(behavior: Behavior) -> (Self, Log) {
        Self::build(behavior, true)
    }

    /// An agent that does not advertise `session/close`.
    fn without_session_close(behavior: Behavior) -> (Self, Log) {
        Self::build(behavior, false)
    }

    fn build(behavior: Behavior, advertise_session_close: bool) -> (Self, Log) {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                behavior,
                log: Arc::clone(&log),
                died: Arc::new(AtomicBool::new(false)),
                advertise_session_close,
            },
            log,
        )
    }
}

#[async_trait::async_trait]
impl AcpTransport for ScriptedAgentTransport {
    async fn spawn(&self, _launch: &AgentLaunch) -> Result<AgentHandle, String> {
        let (connection_side, agent_side) = tokio::io::duplex(64 * 1024);
        let stdin_closed = CancellationToken::new();
        tokio::spawn(play_agent(
            agent_side,
            self.behavior,
            Arc::clone(&self.log),
            Arc::clone(&self.died),
            self.advertise_session_close,
            stdin_closed.clone(),
        ));
        let (agent_read, agent_write) = split(connection_side);
        Ok(AgentHandle {
            label: "scripted agent (in memory)".to_string(),
            stdin: Box::new(ClosingStdin {
                inner: agent_write,
                closed: stdin_closed,
            }),
            stdout: Box::new(agent_read),
            stderr: None,
            kill: Arc::new(|| {}),
            process: None,
        })
    }
}

/// The agent side of the duplex pipe: a minimal ACP agent that logs every
/// line the client sends, so tests assert on the protocol itself.
///
/// Two exits, both faithful to a real child: EOF on stdin (the client closed
/// the pipe) and the client's `kill` (a real transport group-kills the child).
/// Either way the agent stops its turn tasks and shuts its write half, so the
/// client's reader sees the EOF a dead process produces — otherwise a leaked
/// task holding the write half would keep a shut-down connection looking alive
/// and hide exactly the bug the isolation test is about.
async fn play_agent(
    io: DuplexStream,
    behavior: Behavior,
    log: Log,
    died: Arc<AtomicBool>,
    advertise_session_close: bool,
    stdin_closed: CancellationToken,
) {
    let (read_half, write_half) = split(io);
    let writer = Arc::new(tokio::sync::Mutex::new(write_half));
    let closed = CancellationToken::new();
    let mut lines = BufReader::new(read_half).lines();
    let mut prompt_id: Option<u64> = None;
    let mut sessions_created = 0u32;

    while let Some(line) = next_line(&mut lines, &stdin_closed).await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        log.lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(value.clone());
        // Responses to our own agent→client requests (900/901) only get
        // logged; the tests assert on them.
        if value.get("method").is_none() {
            continue;
        }
        let Some(method) = value["method"].as_str() else {
            continue;
        };
        let id = value["id"].as_u64();
        match method {
            "initialize" if behavior == Behavior::SilentHandshake => {
                // The handshake is the setup call this agent never answers.
            }
            "initialize" => {
                let mut agent_capabilities = json!({});
                if advertise_session_close {
                    agent_capabilities["sessionCapabilities"] = json!({"close": {}});
                }
                match behavior {
                    Behavior::McpHttpSupported => {
                        agent_capabilities["mcpCapabilities"] = json!({"http": true, "sse": false});
                    }
                    Behavior::McpHttpUnsupported => {
                        agent_capabilities["mcpCapabilities"] =
                            json!({"http": false, "sse": false});
                    }
                    _ => {}
                }
                respond(
                    &writer,
                    id,
                    json!({
                        "protocolVersion": 1,
                        "authMethods": [],
                        "agentCapabilities": agent_capabilities,
                    }),
                )
                .await;
            }
            "session/new" => {
                sessions_created += 1;
                if behavior == Behavior::SlowFirstSetup && sessions_created == 1 {
                    // The first session setup is the slow one: it is never
                    // answered, so that turn's setup budget is what ends it.
                    continue;
                }
                if behavior == Behavior::NeverAnswerSessionNew {
                    continue;
                }
                let result = match behavior {
                    Behavior::ModelsWithSetModel | Behavior::ModelsNoSetModelMethod => json!({
                        "sessionId": "s-test",
                        "models": {
                            "availableModels": [
                                {"modelId": "m1", "name": "Model One"},
                                {"modelId": "m2", "name": "Model Two"},
                            ],
                            "currentModelId": "m1",
                        },
                    }),
                    Behavior::ConfigOptionsWithSetConfigOption => json!({
                        "sessionId": "s-test",
                        "configOptions": [
                            {
                                "id": "model",
                                "name": "Model",
                                "type": "select",
                                "currentValue": "m1",
                                "options": [
                                    {"value": "m1", "name": "Model One"},
                                    {"value": "m2", "name": "Model Two"},
                                ],
                            }
                        ],
                    }),
                    _ => json!({"sessionId": "s-test"}),
                };
                respond(&writer, id, result).await;
                if behavior == Behavior::DieAfterSession && !died.swap(true, Ordering::SeqCst) {
                    // Hang up: dropping both halves gives the client EOF.
                    break;
                }
            }
            "session/close" => {
                respond(&writer, id, json!({})).await;
            }
            "session/set_model" => match behavior {
                Behavior::ModelsNoSetModelMethod => {
                    respond_error(&writer, id, -32601, "no such method").await;
                }
                _ => respond(&writer, id, json!({})).await,
            },
            "session/set_config_option" => {
                respond(&writer, id, json!({})).await;
            }
            "session/prompt" => match behavior {
                Behavior::AnswerAfterCancel => prompt_id = id,
                Behavior::NeverAnswer => {}
                // Streams keepalives, then answers: the concurrent turn whose
                // setup times out must not take this one down with it, and the
                // keepalives keep this turn's *idle* clock from firing first
                // (D2.5: updates reset the idle timeout).
                Behavior::SlowFirstSetup => {
                    let Some(id) = id else { continue };
                    let sid = value["params"]["sessionId"]
                        .as_str()
                        .unwrap_or("s-test")
                        .to_string();
                    let agent_writer = Arc::clone(&writer);
                    let stopped = closed.clone();
                    tokio::spawn(async move {
                        for _ in 0..5 {
                            tokio::select! {
                                _ = stopped.cancelled() => return,
                                _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                            }
                            update(
                                &agent_writer,
                                &sid,
                                json!({"sessionUpdate": "tool_call_update",
                                       "toolCallId": "keepalive",
                                       "title": "working",
                                       "status": "in_progress"}),
                            )
                            .await;
                        }
                        update(
                            &agent_writer,
                            &sid,
                            json!({"sessionUpdate": "agent_message_chunk",
                                   "content": {"type": "text", "text": "still here"}}),
                        )
                        .await;
                        respond(&agent_writer, Some(id), json!({"stopReason": "end_turn"})).await;
                    });
                }
                // Only the *first* agent hangs up; after that the scenario
                // plays a normal turn so the respawn can be asserted.
                Behavior::DieAfterSession if !died.load(Ordering::SeqCst) => {}
                _ => {
                    let Some(id) = id else { continue };
                    let sid = value["params"]["sessionId"]
                        .as_str()
                        .unwrap_or("s-test")
                        .to_string();
                    let agent_writer = Arc::clone(&writer);
                    let stopped = closed.clone();
                    tokio::spawn(async move {
                        update(
                            &agent_writer,
                            &sid,
                            json!({"sessionUpdate": "agent_thought_chunk",
                                   "content": {"type": "text", "text": "thinking…"}}),
                        )
                        .await;
                        update(
                            &agent_writer,
                            &sid,
                            json!({"sessionUpdate": "agent_message_chunk",
                                   "content": {"type": "text", "text": "Hello "}}),
                        )
                        .await;
                        // A permission ask, the way real agents send it.
                        send_line(
                            &agent_writer,
                            json!({
                                "jsonrpc": "2.0",
                                "id": 900,
                                "method": "session/request_permission",
                                "params": {
                                    "sessionId": sid,
                                    "toolCall": {
                                        "toolCallId": "t1",
                                        "title": "Edit src/main.rs",
                                        "kind": "edit",
                                    },
                                    "options": [
                                        {"optionId": "allow-always", "name": "Always allow", "kind": "allow_always"},
                                        {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
                                        {"optionId": "reject-once", "name": "Reject", "kind": "reject_once"},
                                    ],
                                },
                            }),
                        )
                        .await;
                        // A client capability we never declared: must come
                        // back as a typed refusal, not silence and not a fake
                        // success.
                        send_line(
                            &agent_writer,
                            json!({
                                "jsonrpc": "2.0",
                                "id": 901,
                                "method": "fs/read_text_file",
                                "params": {"sessionId": sid, "path": "/etc/passwd"},
                            }),
                        )
                        .await;
                        // Let the client answer both before the turn ends, so
                        // the responses are on the wire deterministically.
                        tokio::select! {
                            _ = stopped.cancelled() => return,
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                        }
                        if stopped.is_cancelled() {
                            return;
                        }
                        update(
                            &agent_writer,
                            &sid,
                            json!({"sessionUpdate": "tool_call",
                                   "toolCallId": "t1", "title": "Edit src/main.rs",
                                   "kind": "edit", "status": "completed"}),
                        )
                        .await;
                        update(
                            &agent_writer,
                            &sid,
                            json!({"sessionUpdate": "agent_message_chunk",
                                   "content": {"type": "text", "text": "world"}}),
                        )
                        .await;
                        respond(&agent_writer, Some(id), json!({"stopReason": "end_turn"})).await;
                    });
                }
            },
            "session/cancel" => {
                if behavior == Behavior::AnswerAfterCancel
                    && let Some(id) = prompt_id.take()
                {
                    respond(&writer, Some(id), json!({"stopReason": "cancelled"})).await;
                }
            }
            other => respond_error(&writer, id, -32601, format!("unknown method {other}")).await,
        }
    }
    // EOF on stdin: the client closed the pipe, so this agent exits like a real
    // child would. Stop whatever was streaming, then shut the write half so the
    // client's reader really sees the end of the pipe.
    closed.cancel();
    let mut guard = writer.lock().await;
    let _ = guard.shutdown().await;
}

/// Read one line, or `None` when the client closed the agent's stdin.
async fn next_line(
    lines: &mut tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
    stdin_closed: &CancellationToken,
) -> Option<String> {
    tokio::select! {
        line = lines.next_line() => line.ok().flatten(),
        _ = stdin_closed.cancelled() => None,
    }
}

async fn send_line(writer: &Arc<tokio::sync::Mutex<WriteHalf<DuplexStream>>>, value: Value) {
    let mut guard = writer.lock().await;
    let line = value.to_string();
    if guard.write_all(line.as_bytes()).await.is_err()
        || guard.write_all(b"\n").await.is_err()
        || guard.flush().await.is_err()
    {
        // The client is gone; the reader loop notices EOF soon enough.
    }
}

async fn respond(
    writer: &Arc<tokio::sync::Mutex<WriteHalf<DuplexStream>>>,
    id: Option<u64>,
    result: Value,
) {
    if let Some(id) = id {
        send_line(
            writer,
            json!({"jsonrpc": "2.0", "id": id, "result": result}),
        )
        .await;
    }
}

async fn respond_error(
    writer: &Arc<tokio::sync::Mutex<WriteHalf<DuplexStream>>>,
    id: Option<u64>,
    code: i64,
    message: impl Into<String>,
) {
    if let Some(id) = id {
        send_line(
            writer,
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}}),
        )
        .await;
    }
}

async fn update(
    writer: &Arc<tokio::sync::Mutex<WriteHalf<DuplexStream>>>,
    session_id: &str,
    update: Value,
) {
    send_line(
        writer,
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session_id, "update": update},
        }),
    )
    .await;
}

fn acp_spec(mut options: Map<String, Value>) -> BackendSpec {
    options
        .entry("command")
        .or_insert_with(|| json!("scripted-agent"));
    BackendSpec {
        id: "acp-test".into(),
        kind: BackendKind::Acp,
        base_url: None,
        api_key_env: None,
        model: None,
        extra_body: Map::new(),
        extra_headers: BTreeMap::new(),
        timeout_secs: 5,
        max_retries: 0,
        options,
    }
}

fn agent(behavior: Behavior) -> (AcpAgent, Log) {
    let (transport, log) = ScriptedAgentTransport::new(behavior);
    let backend = AcpAgent::with_transport(acp_spec(Map::new()), None, Arc::new(transport))
        .expect("a valid spec builds");
    (backend, log)
}

fn request_with_history() -> ChatRequest {
    let mut request = ChatRequest::new("", "You are @test, a terse bot.");
    request.messages.push(ChatMessage::user("hi there"));
    request
}

/// Collects every event until the sender drops (which `chat` does when it
/// returns), so tests can assert the whole stream and the Finished-once
/// invariant.
fn collect_events(rx: mpsc::Receiver<LlmEvent>) -> tokio::task::JoinHandle<Vec<LlmEvent>> {
    tokio::spawn(async move {
        let mut events = Vec::new();
        let mut rx = rx;
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    })
}

fn logged(log: &Log) -> Vec<Value> {
    log.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

fn find_request(entries: &[Value], method: &str) -> Option<Value> {
    entries
        .iter()
        .find(|entry| entry["method"].as_str() == Some(method))
        .cloned()
}

fn find_response(entries: &[Value], id: u64) -> Option<Value> {
    entries
        .iter()
        .find(|entry| entry["id"].as_u64() == Some(id) && entry.get("method").is_none())
        .cloned()
}

/// Bounded wait for a log condition — the agent task logs lines as it reads
/// them, which happens just after the turn ends.
async fn wait_for_log(log: &Log, what: &str, predicate: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let entries = logged(log);
        if predicate(&entries) {
            return entries;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}; log so far: {entries:#?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// the turn itself
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_full_turn_streams_reasoning_and_text_and_finishes_once() {
    let (backend, log) = agent(Behavior::Normal);
    let (tx, rx) = mpsc::channel(64);
    let events = collect_events(rx);

    let response = backend
        .chat(request_with_history(), tx, CancellationToken::new())
        .await
        .expect("the turn completes");

    assert_eq!(response.text, "Hello world");
    assert_eq!(response.reasoning, "thinking…");
    assert_eq!(response.finish_reason, FinishReason::Stop);
    // The agent ran its own tools; nothing may reach the runner's tool loop.
    assert!(response.tool_calls.is_empty());
    assert_eq!(response.usage.llm_calls, 1);

    let events = events.await.expect("joined");
    assert_eq!(
        events,
        vec![
            LlmEvent::ReasoningDelta("thinking…".into()),
            LlmEvent::TextDelta("Hello ".into()),
            LlmEvent::TextDelta("world".into()),
            LlmEvent::Finished {
                finish_reason: FinishReason::Stop,
                usage: response.usage,
                text: "Hello world".into(),
                reasoning: "thinking…".into(),
                tool_calls: vec![],
            },
        ]
    );
    // The Finished-once invariant, spelled out.
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, LlmEvent::Finished { .. }))
            .count(),
        1
    );

    // The prompt the agent received: instructions and transcript as text
    // blocks, in one fresh session.
    let entries = wait_for_log(&log, "session/prompt", |entries| {
        find_request(entries, "session/prompt").is_some()
    })
    .await;
    let initialize = find_request(&entries, "initialize").expect("handshake happened");
    assert_eq!(initialize["params"]["protocolVersion"], 1);
    // No fs, no terminal: the agent must use its own tools.
    assert_eq!(initialize["params"]["clientCapabilities"], json!({}));
    let new_session = find_request(&entries, "session/new").expect("a session was created");
    assert_eq!(new_session["params"]["mcpServers"], json!([]));
    assert!(
        new_session["params"]["cwd"]
            .as_str()
            .is_some_and(|cwd| !cwd.is_empty())
    );
    let prompt = find_request(&entries, "session/prompt").expect("the turn ran");
    assert_eq!(prompt["params"]["sessionId"], "s-test");
    let blocks = prompt["params"]["prompt"]
        .as_array()
        .expect("content blocks");
    let text: String = blocks
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("You are @test"), "{text}");
    assert!(text.contains("user: hi there"), "{text}");
}

#[tokio::test]
async fn permission_asks_get_the_configured_once_answer() {
    for (mode, expected_option) in [
        (PermissionMode::Reject, "reject-once"),
        (PermissionMode::Allow, "allow-once"),
    ] {
        let (transport, log) = ScriptedAgentTransport::new(Behavior::Normal);
        let mut options = Map::new();
        options.insert("permission_mode".into(), json!(mode.label()));
        let backend = AcpAgent::with_transport(acp_spec(options), None, Arc::new(transport))
            .expect("a valid spec builds");
        assert_eq!(backend.permission_mode(), Some(mode));

        let (tx, rx) = mpsc::channel(64);
        let events = collect_events(rx);
        backend
            .chat(request_with_history(), tx, CancellationToken::new())
            .await
            .expect("the turn completes");
        events.await.expect("joined");

        // The permission answer: the *_once option, never the blanket one.
        let entries = wait_for_log(&log, "the permission answer", |entries| {
            find_response(entries, 900).is_some()
        })
        .await;
        let answer = find_response(&entries, 900).expect("answered");
        assert_eq!(
            answer["result"]["outcome"],
            json!({"outcome": "selected", "optionId": expected_option}),
            "mode {mode:?}"
        );

        // The fs call we never advertised a capability for: a typed refusal.
        let entries = wait_for_log(&log, "the fs refusal", |entries| {
            find_response(entries, 901).is_some()
        })
        .await;
        let refusal = find_response(&entries, 901).expect("refused");
        assert_eq!(refusal["error"]["code"], -32601, "{refusal}");
        assert!(
            refusal["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("no fs capability")),
            "{refusal}"
        );
    }
}

#[tokio::test]
async fn the_owner_stop_cancels_the_turn_and_tells_the_agent() {
    let (backend, log) = agent(Behavior::AnswerAfterCancel);
    let cancel = CancellationToken::new();
    // A clone of the *same* token: cancelling a child would not cancel the
    // parent, and the turn would run out its idle timeout instead.
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        stopper.cancel();
    });

    let (tx, rx) = mpsc::channel(16);
    let events = collect_events(rx);
    let started = tokio::time::Instant::now();
    let error = backend
        .chat(request_with_history(), tx, cancel)
        .await
        .expect_err("a stopped turn is an error");
    events.await.expect("joined");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the stop must not wait out the idle timeout"
    );
    assert!(matches!(error, LlmError::Cancelled), "{error}");

    let entries = wait_for_log(&log, "session/cancel", |entries| {
        find_request(entries, "session/cancel").is_some()
    })
    .await;
    let cancel_notification = find_request(&entries, "session/cancel").expect("sent");
    assert_eq!(cancel_notification["params"]["sessionId"], "s-test");
    // It is a notification, not a request.
    assert!(cancel_notification.get("id").is_none());
}

#[tokio::test]
async fn idle_silence_times_out_and_cancels_the_session() {
    let (transport, log) = ScriptedAgentTransport::new(Behavior::NeverAnswer);
    let mut spec = acp_spec(Map::new());
    spec.timeout_secs = 1; // the idle timeout: one second of silence is enough
    let backend =
        AcpAgent::with_transport(spec, None, Arc::new(transport)).expect("a valid spec builds");

    let (tx, rx) = mpsc::channel(16);
    let events = collect_events(rx);
    let error = backend
        .chat(request_with_history(), tx, CancellationToken::new())
        .await
        .expect_err("a silent agent times out");
    events.await.expect("joined");
    assert!(matches!(error, LlmError::Timeout { secs: 1 }), "{error}");

    let entries = wait_for_log(&log, "session/cancel after the timeout", |entries| {
        find_request(entries, "session/cancel").is_some()
    })
    .await;
    assert!(find_request(&entries, "session/cancel").is_some());
}

#[tokio::test]
async fn a_dead_agent_fails_the_turn_honestly_and_respawns_for_the_next_one() {
    let (backend, _log) = agent(Behavior::DieAfterSession);

    let (tx, rx) = mpsc::channel(16);
    drop(rx); // this turn's events go nowhere; the error is the assertion
    let error = backend
        .chat(request_with_history(), tx, CancellationToken::new())
        .await
        .expect_err("the agent hangs up mid-turn");
    match &error {
        LlmError::Unavailable { reason, .. } => {
            assert!(reason.contains("died mid-turn"), "{reason}");
            // The half-run warning: tools may have executed, so no silent
            // automatic retry.
            assert!(reason.contains("not retried automatically"), "{reason}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert!(
        error.retryable(),
        "a dead process may come back on a respawn"
    );

    // The next turn spawns a healthy agent (the scripted transport's flag).
    let (tx, rx) = mpsc::channel(16);
    let events = collect_events(rx);
    let response = backend
        .chat(request_with_history(), tx, CancellationToken::new())
        .await
        .expect("the connection respawns");
    events.await.expect("joined");
    assert_eq!(response.text, "Hello world");
}

// ---------------------------------------------------------------------------
// one shared backend, one slow turn: isolation between concurrent turns
// ---------------------------------------------------------------------------

/// The headline isolation test: a setup timeout is *that turn's* failure.
///
/// Both turns share one agent process (same connection key: no `model_env`, no
/// per-bot cwd). The first turn's `session/new` is never answered, so its setup
/// budget expires. The second turn is still in flight at that moment — it
/// streams keepalives and answers at 50s, past the first turn's 30s setup
/// budget — and it must survive to finish. With the old code the timeout shut
/// the shared process down and this turn died with "agent died mid-turn".
///
/// The clock is paused and advanced by tokio's auto-advance: the test spends no
/// real time, and the ordering (slow turn owns the first `session/new`) is
/// pinned by the log rather than by sleeps.
#[tokio::test(start_paused = true)]
async fn one_slow_setup_fails_only_its_own_turn() {
    let (transport, log) = ScriptedAgentTransport::new(Behavior::SlowFirstSetup);
    let mut spec = acp_spec(Map::new());
    // setup_timeout and the idle timeout are both 30s here, so the keepalives
    // are what keep the surviving turn alive past the other turn's setup
    // budget — which is exactly the production shape: a working agent streams.
    spec.timeout_secs = 30;
    let backend = Arc::new(
        AcpAgent::with_transport(spec, None, Arc::new(transport)).expect("a valid spec builds"),
    );

    let (tx_slow, rx_slow) = mpsc::channel(8);
    let events_slow = collect_events(rx_slow);
    let slow = {
        let backend = Arc::clone(&backend);
        tokio::spawn(async move {
            backend
                .chat(request_with_history(), tx_slow, CancellationToken::new())
                .await
        })
    };
    // Wait until the slow turn's `session/new` is on the wire before starting
    // the other turn: the scripted agent hangs the *first* session setup, so
    // this is what makes the scenario ordered instead of raced.
    wait_for_log(&log, "the first session/new", |entries| {
        entries
            .iter()
            .filter(|entry| entry["method"].as_str() == Some("session/new"))
            .count()
            == 1
    })
    .await;

    let (tx_live, rx_live) = mpsc::channel(8);
    let events_live = collect_events(rx_live);
    let live = {
        let backend = Arc::clone(&backend);
        tokio::spawn(async move {
            backend
                .chat(request_with_history(), tx_live, CancellationToken::new())
                .await
        })
    };
    // The second turn is now in flight: session created, prompt sent, waiting.
    wait_for_log(&log, "the second turn's prompt", |entries| {
        entries
            .iter()
            .filter(|entry| entry["method"].as_str() == Some("session/prompt"))
            .count()
            == 1
    })
    .await;

    let slow_result = slow.await.expect("joined");
    match slow_result {
        Err(LlmError::Timeout { .. }) => {}
        other => panic!("the slow turn must fail with its own timeout, got {other:?}"),
    }

    let live_result = live.await.expect("joined");
    events_live.await.expect("joined");
    let response = match live_result {
        Ok(response) => response,
        Err(error) => panic!("another turn's setup timeout killed this turn: {error}"),
    };
    assert_eq!(response.text, "still here");
    events_slow.await.expect("joined");

    // One process, one handshake: the timeout never respawned or shut down the
    // agent the surviving turn was using.
    let entries = logged(&log);
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry["method"].as_str() == Some("initialize"))
            .count(),
        1,
        "the shared agent must not be respawned by one turn's timeout: {entries:#?}"
    );
}

/// A stopped turn must not wait out somebody else's connect.
///
/// A cold agent that never answers `initialize` holds establishment for the
/// whole setup budget (30s floor). A second turn on the same backend that is
/// stopped must leave at once — with the old code it queued behind the first
/// turn's connect, and the stop only landed when that connect timed out.
#[tokio::test(start_paused = true)]
async fn a_stop_during_another_turns_handshake_returns_promptly() {
    let (transport, log) = ScriptedAgentTransport::new(Behavior::SilentHandshake);
    let backend = Arc::new(
        AcpAgent::with_transport(acp_spec(Map::new()), None, Arc::new(transport))
            .expect("a valid spec builds"),
    );

    let (tx_first, rx_first) = mpsc::channel(4);
    drop(rx_first);
    let first = {
        let backend = Arc::clone(&backend);
        tokio::spawn(async move {
            backend
                .chat(request_with_history(), tx_first, CancellationToken::new())
                .await
        })
    };
    wait_for_log(&log, "the handshake", |entries| {
        find_request(entries, "initialize").is_some()
    })
    .await;

    let cancel = CancellationToken::new();
    let (tx_second, rx_second) = mpsc::channel(4);
    drop(rx_second);
    let second = {
        let backend = Arc::clone(&backend);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            backend
                .chat(request_with_history(), tx_second, cancel)
                .await
        })
    };
    // Let the second turn reach the establishment gate, then stop it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stopped_at = tokio::time::Instant::now();
    cancel.cancel();
    let error = second.await.expect("joined");
    let waited = stopped_at.elapsed();
    assert!(
        waited < Duration::from_secs(2),
        "a stopped turn waited {waited:?} for another turn's handshake"
    );
    match error {
        Err(LlmError::Cancelled) => {}
        other => panic!("a stopped turn must report cancellation, got {other:?}"),
    }

    // The first turn is still establishing: let it run out its budget so the
    // test leaves nothing detached behind.
    let first_result = first.await.expect("joined");
    match first_result {
        Err(LlmError::Timeout { .. }) => {}
        other => panic!("the silent handshake must time out, got {other:?}"),
    }
}

/// The lazy reap, and its bound: a *run* of unanswered setup calls is a wedged
/// process, so the next turn replaces it — once. One turn's timeout is never
/// allowed to respawn anything (that is the previous test); a process that has
/// stopped answering setup calls is not kept forever either.
#[tokio::test(start_paused = true)]
async fn a_run_of_setup_timeouts_replaces_the_wedged_agent_once() {
    let (transport, log) = ScriptedAgentTransport::new(Behavior::NeverAnswerSessionNew);
    let backend = Arc::new(
        AcpAgent::with_transport(acp_spec(Map::new()), None, Arc::new(transport))
            .expect("a valid spec builds"),
    );

    // Each turn times out in setup; the handshake count is what shows whether
    // the process was replaced.
    for turn in 1..=4 {
        let (tx, rx) = mpsc::channel(4);
        drop(rx);
        let error = backend
            .chat(request_with_history(), tx, CancellationToken::new())
            .await
            .expect_err("an agent that never answers session/new times out");
        assert!(
            matches!(error, LlmError::Timeout { .. }),
            "turn {turn}: {error}"
        );
        let handshakes = logged(&log)
            .iter()
            .filter(|entry| entry["method"].as_str() == Some("initialize"))
            .count();
        match turn {
            // Three timeouts in a row are still the same process: replacing on
            // the first one would be exactly the shared-shutdown bug again.
            1..=3 => assert_eq!(handshakes, 1, "turn {turn} must reuse the process"),
            // The fourth turn sees the run and starts a fresh agent, once.
            _ => assert_eq!(handshakes, 2, "the wedged agent must be replaced once"),
        }
    }
}

// ---------------------------------------------------------------------------
// the session a turn opens is released when the turn ends
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_completed_turn_closes_its_acp_session() {
    let (backend, log) = agent(Behavior::Normal);
    let (tx, rx) = mpsc::channel(32);
    let events = collect_events(rx);
    backend
        .chat(request_with_history(), tx, CancellationToken::new())
        .await
        .expect("the turn completes");
    events.await.expect("joined");

    let entries = wait_for_log(&log, "session/close", |entries| {
        find_request(entries, "session/close").is_some()
    })
    .await;
    let close = find_request(&entries, "session/close").expect("the turn released its session");
    assert_eq!(close["params"]["sessionId"], "s-test");
    // A request (the agent acknowledges it), not a fire-and-forget notification.
    assert!(
        close.get("id").is_some(),
        "session/close must be a request: {close}"
    );
}

#[tokio::test]
async fn an_agent_without_session_close_is_not_sent_one() {
    // `session/close` is optional in ACP v1 and gated on the agent advertising
    // `sessionCapabilities.close`. An agent that stayed silent must not receive
    // a guess — its per-turn sessions live until the process exits, which is
    // the limit stated in the module docs.
    let (transport, log) = ScriptedAgentTransport::without_session_close(Behavior::Normal);
    let backend = AcpAgent::with_transport(acp_spec(Map::new()), None, Arc::new(transport))
        .expect("a valid spec builds");
    let (tx, rx) = mpsc::channel(32);
    let events = collect_events(rx);
    backend
        .chat(request_with_history(), tx, CancellationToken::new())
        .await
        .expect("the turn completes");
    events.await.expect("joined");

    // The turn is over, and a close would have been acknowledged before `chat`
    // returned (it is a request the adapter awaits), so its absence here is
    // deterministic.
    let entries = logged(&log);
    assert!(
        find_request(&entries, "session/prompt").is_some(),
        "the turn really ran: {entries:#?}"
    );
    assert!(
        find_request(&entries, "session/close").is_none(),
        "an unadvertised optional method must not be called: {entries:#?}"
    );
}

// ---------------------------------------------------------------------------
// model selection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_wished_model_is_set_when_the_agent_offers_it() {
    let (transport, log) = ScriptedAgentTransport::new(Behavior::ModelsWithSetModel);
    let backend = AcpAgent::with_transport(acp_spec(Map::new()), None, Arc::new(transport))
        .expect("a valid spec builds");

    let (tx, rx) = mpsc::channel(32);
    let events = collect_events(rx);
    let response = backend
        .chat(
            ChatRequest::new("m2", "instructions"),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("the turn completes");
    events.await.expect("joined");
    assert_eq!(response.text, "Hello world");

    let entries = wait_for_log(&log, "session/set_model", |entries| {
        find_request(entries, "session/set_model").is_some()
    })
    .await;
    let set_model = find_request(&entries, "session/set_model").expect("sent");
    assert_eq!(set_model["params"]["modelId"], "m2");
    assert_eq!(set_model["params"]["sessionId"], "s-test");
}

#[tokio::test]
async fn a_model_the_agent_does_not_offer_is_an_error_not_a_silent_swap() {
    let (backend, _log) = agent(Behavior::ModelsWithSetModel);
    let (tx, rx) = mpsc::channel(16);
    drop(rx);
    let error = backend
        .chat(
            ChatRequest::new("m9", "instructions"),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect_err("m9 is not on offer");
    match &error {
        LlmError::Unavailable { reason, .. } => {
            assert!(reason.contains("does not offer model `m9`"), "{reason}");
            assert!(reason.contains("m1, m2"), "{reason}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[tokio::test]
async fn a_missing_set_model_method_keeps_the_agents_own_choice_with_a_warning() {
    let (transport, log) = ScriptedAgentTransport::new(Behavior::ModelsNoSetModelMethod);
    let backend = AcpAgent::with_transport(acp_spec(Map::new()), None, Arc::new(transport))
        .expect("a valid spec builds");
    let (tx, rx) = mpsc::channel(32);
    let events = collect_events(rx);
    let response = backend
        .chat(
            ChatRequest::new("m2", "instructions"),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("an unstable missing method must not fail the turn");
    events.await.expect("joined");
    assert_eq!(response.text, "Hello world");
    // The attempt is visible on the wire even though it was refused.
    let entries = wait_for_log(&log, "the refused session/set_model", |entries| {
        find_request(entries, "session/set_model").is_some()
    })
    .await;
    assert!(find_request(&entries, "session/set_model").is_some());
}

#[tokio::test]
async fn a_turn_can_switch_models_via_set_config_option() {
    let (transport, log) = ScriptedAgentTransport::new(Behavior::ConfigOptionsWithSetConfigOption);
    let backend = AcpAgent::with_transport(acp_spec(Map::new()), None, Arc::new(transport))
        .expect("a valid spec builds");
    let (tx, rx) = mpsc::channel(32);
    let events = collect_events(rx);
    let response = backend
        .chat(
            ChatRequest::new("m2", "instructions"),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("the turn completes");
    events.await.expect("joined");
    assert_eq!(response.text, "Hello world");

    let entries = wait_for_log(&log, "session/set_config_option", |entries| {
        find_request(entries, "session/set_config_option").is_some()
    })
    .await;
    let set_config = find_request(&entries, "session/set_config_option").expect("sent");
    assert_eq!(set_config["params"]["configId"], "model");
    assert_eq!(set_config["params"]["value"], "m2");
    assert_eq!(set_config["params"]["sessionId"], "s-test");
}

#[tokio::test]
async fn a_model_the_agent_does_not_offer_in_config_options_fails_honestly() {
    let (backend, _log) = agent(Behavior::ConfigOptionsWithSetConfigOption);
    let (tx, rx) = mpsc::channel(16);
    drop(rx);
    let error = backend
        .chat(
            ChatRequest::new("m9", "instructions"),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect_err("m9 is not on offer in configOptions");
    match &error {
        LlmError::Unavailable { reason, .. } => {
            assert!(reason.contains("does not offer model `m9`"), "{reason}");
            assert!(reason.contains("m1, m2"), "{reason}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// probe, keys, registry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn probe_reports_a_ready_agent_without_spending_a_turn() {
    let (backend, _log) = agent(Behavior::Normal);
    let status = backend.probe().await.expect("probe reports, never throws");
    assert!(status.ok, "{status:#?}");
    assert!(status.detail.contains("ACP v1"), "{status:#?}");
    assert!(status.detail.contains("permissions: reject"), "{status:#?}");
    assert_eq!(status.version.as_deref(), Some("acp v1"));
}

#[tokio::test]
async fn probe_reports_a_missing_binary_honestly() {
    let mut options = Map::new();
    options.insert(
        "command".into(),
        json!("/nonexistent/swarmbot-test/acp-agent"),
    );
    let backend = AcpAgent::with_transport(
        acp_spec(options),
        None,
        Arc::new(ProcessTransport) as Arc<dyn AcpTransport>,
    )
    .expect("a valid spec builds");
    let status = backend.probe().await.expect("probe reports, never throws");
    assert!(!status.ok);
    assert!(status.detail.contains("cannot run"), "{status:#?}");
}

#[tokio::test]
async fn a_named_key_env_without_a_key_fails_before_any_spawn() {
    let mut spec = acp_spec(Map::new());
    spec.api_key_env = Some("SWARMBOT_TEST_ACP_KEY_DEFINITELY_UNSET".into());
    let (transport, log) = ScriptedAgentTransport::new(Behavior::Normal);
    let backend =
        AcpAgent::with_transport(spec, None, Arc::new(transport)).expect("a valid spec builds");

    let (tx, rx) = mpsc::channel(4);
    drop(rx);
    let error = backend
        .chat(request_with_history(), tx, CancellationToken::new())
        .await
        .expect_err("no key, no turn");
    assert!(
        matches!(&error, LlmError::MissingKey { env }
            if env == "SWARMBOT_TEST_ACP_KEY_DEFINITELY_UNSET"),
        "{error}"
    );
    // Nothing was ever spawned or said.
    assert!(logged(&log).is_empty());

    let status = backend.probe().await.expect("probe reports");
    assert!(!status.ok);
    assert!(status.detail.contains("no api key"), "{status:#?}");
}

#[tokio::test]
async fn the_key_reaches_the_child_environment_under_its_own_name() {
    // The scripted transport ignores the launch, so capture it here: this is
    // the assertion that the resolved key travels as an env value — never in
    // argv, never in the label.
    #[derive(Clone)]
    struct CapturingTransport {
        launches: Arc<Mutex<Vec<AgentLaunchSnapshot>>>,
    }
    #[derive(Clone, Debug)]
    struct AgentLaunchSnapshot {
        label: String,
        env: BTreeMap<String, String>,
        argv: Vec<String>,
    }
    #[async_trait::async_trait]
    impl AcpTransport for CapturingTransport {
        async fn spawn(&self, launch: &AgentLaunch) -> Result<AgentHandle, String> {
            self.launches.lock().unwrap().push(AgentLaunchSnapshot {
                label: launch.label(),
                env: launch.env.clone(),
                argv: std::iter::once(launch.command.clone())
                    .chain(launch.args.iter().cloned())
                    .collect(),
            });
            ScriptedAgentTransport::new(Behavior::Normal)
                .0
                .spawn(launch)
                .await
        }
    }

    let mut spec = acp_spec(Map::new());
    spec.api_key_env = Some("ANTHROPIC_API_KEY".into());
    let transport = CapturingTransport {
        launches: Arc::new(Mutex::new(Vec::new())),
    };
    let launches = Arc::clone(&transport.launches);
    let backend = AcpAgent::with_transport(
        spec,
        Some("sk-secret-DO-NOT-LEAK".into()),
        Arc::new(transport),
    )
    .expect("a valid spec builds");

    let (tx, rx) = mpsc::channel(32);
    let events = collect_events(rx);
    backend
        .chat(request_with_history(), tx, CancellationToken::new())
        .await
        .expect("the turn completes");
    events.await.expect("joined");

    let snapshot = &launches.lock().unwrap()[0];
    assert_eq!(
        snapshot.env.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some("sk-secret-DO-NOT-LEAK")
    );
    let argv = snapshot.argv.join(" ");
    assert!(!argv.contains("sk-secret"), "{argv}");
    assert!(
        !snapshot.label.contains("sk-secret"),
        "{:#?}",
        snapshot.label
    );
}

#[tokio::test]
async fn the_registry_builds_probes_and_lists_acp_backends() {
    let mut options = Map::new();
    options.insert("command".into(), json!("/bin/false"));
    let mut spec = acp_spec(options);
    spec.id = "opencode".into();
    let registry = BackendRegistry::new("fake", vec![spec], &BTreeMap::new()).expect("builds");

    let backend = registry.get("opencode").expect("configured");
    assert_eq!(backend.kind(), BackendKind::Acp);
    // No api_key_env → ready, like the CLI harnesses: the agent's own login
    // is the probe's job to verify.
    assert!(
        registry
            .list()
            .iter()
            .find(|info| info.id == "opencode")
            .is_some_and(|info| info.ready)
    );
    // /bin/false runs but never answers the handshake: probe must report the
    // failure, not hang and not lie.
    let statuses = registry.probe_all().await;
    let status = statuses
        .iter()
        .find(|status| status.backend == "opencode")
        .expect("one row per backend");
    assert!(!status.ok, "{status:#?}");
}

#[tokio::test]
async fn a_spec_without_a_command_fails_at_registry_build_time() {
    let mut spec = acp_spec(Map::new());
    spec.options.clear();
    let error = BackendRegistry::new("fake", vec![spec], &BTreeMap::new())
        .expect_err("validate() runs before any adapter is built");
    assert!(error.to_string().contains("options.command"), "{error}");
}

// ---------------------------------------------------------------------------
// a real child process over the real transport
// ---------------------------------------------------------------------------

/// `/bin/sh` as an ACP agent: proves NDJSON framing, ids and shutdown against
/// a real process — and that dropping the backend kills the agent's whole
/// process group (the script parks in a `sleep` loop with its pid on disk).
#[tokio::test]
async fn a_real_sh_agent_speaks_ndjson_and_dies_with_its_group() {
    if !std::path::Path::new("/proc").exists() {
        eprintln!("skipping: this test asserts on /proc and needs Linux");
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "swarmbot-acp-sh-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("a temp dir is creatable");
    let pidfile = dir.join("agent.pid");

    let script = format!(
        r#"
        echo $$ > {pidfile}
        read -r _initialize
        printf '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"authMethods":[]}}}}\n'
        read -r _new_session
        printf '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"sh1"}}}}\n'
        read -r _prompt
        printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"sh1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"hello "}}}}}}}}\n'
        printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"sh1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"from sh"}}}}}}}}\n'
        printf '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}\n'
        while :; do sleep 0.2; done
        "#,
        pidfile = pidfile.display()
    );

    let mut options = Map::new();
    options.insert("command".into(), json!("/bin/sh"));
    options.insert("args".into(), json!(["-c", script, "sh-agent"]));
    options.insert("cwd".into(), json!(dir.to_string_lossy()));
    let backend = AcpAgent::from_spec(acp_spec(options), None).expect("a valid spec builds");

    let (tx, rx) = mpsc::channel(32);
    let events = collect_events(rx);
    let response = backend
        .chat(request_with_history(), tx, CancellationToken::new())
        .await
        .expect("the sh agent completes a turn");
    let events = events.await.expect("joined");
    assert_eq!(response.text, "hello from sh");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::Finished { .. }))
    );

    // The agent is still alive (the connection is kept for the next turn)…
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("the agent wrote its pid")
        .trim()
        .parse()
        .expect("a pid");
    assert!(
        std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "the agent should still be running between turns"
    );

    // …and dropping the backend kills its whole process group: no orphan
    // `sleep` loop survives (D5.6).
    drop(backend);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "agent pid {pid} survived the dropped backend"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    std::fs::remove_dir_all(&dir).expect("the temp dir is removable");
}

// ---------------------------------------------------------------------------
// live, gated, one turn
// ---------------------------------------------------------------------------

/// One turn against a real ACP agent, for manual verification:
///
/// ```text
/// PROTO_LIVE_ACP=1 PROTO_LIVE_ACP_CMD=opencode PROTO_LIVE_ACP_ARGS=acp \
/// PROTO_LIVE_ACP_PROMPT=1 \
/// cargo test -p swarmbot-llm --test acp -- --ignored --nocapture
/// ```
///
/// Without `PROTO_LIVE_ACP_PROMPT=1` it stops after `probe` (handshake plus
/// `session/new`), which spends nothing. The prompt turn spends one small
/// model call; the test prints its duration. `PROTO_LIVE_ACP_MODEL` and
/// `PROTO_LIVE_ACP_PERMISSIONS` (`reject`/`allow`) map to the backend spec.
#[tokio::test]
#[ignore = "live: needs PROTO_LIVE_ACP=1 plus PROTO_LIVE_ACP_CMD (and optionally _ARGS/_MODEL/_PROMPT)"]
async fn live_acp_agent_handshake_and_one_cheap_turn() {
    if std::env::var("PROTO_LIVE_ACP").as_deref() != Ok("1") {
        eprintln!("skipping live test: set PROTO_LIVE_ACP=1 to run it");
        return;
    }
    let command = std::env::var("PROTO_LIVE_ACP_CMD").expect("set PROTO_LIVE_ACP_CMD");
    let args: Vec<String> = std::env::var("PROTO_LIVE_ACP_ARGS")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let mut options = Map::new();
    options.insert("command".into(), json!(command));
    options.insert("args".into(), json!(args));
    if let Ok(mode) = std::env::var("PROTO_LIVE_ACP_PERMISSIONS") {
        options.insert("permission_mode".into(), json!(mode));
    }
    let mut spec = acp_spec(options);
    spec.timeout_secs = 120;
    if let Ok(model) = std::env::var("PROTO_LIVE_ACP_MODEL") {
        spec.model = Some(model);
    }
    let backend = AcpAgent::from_spec(spec, None).expect("a valid spec builds");

    let started = std::time::Instant::now();
    let status = backend.probe().await.expect("probe reports");
    println!("probe after {started:?}: {status:#?}");
    assert!(status.ok, "the live agent must be reachable: {status:#?}");

    if std::env::var("PROTO_LIVE_ACP_PROMPT").as_deref() != Ok("1") {
        println!("PROTO_LIVE_ACP_PROMPT not set: stopped after the (free) handshake");
        return;
    }
    let mut request = ChatRequest::new("", "You answer in one word.");
    request
        .messages
        .push(ChatMessage::user("Reply with exactly one word: PONG"));
    let (tx, rx) = mpsc::channel(256);
    let events = collect_events(rx);
    let started = std::time::Instant::now();
    let response = backend
        .chat(request, tx, CancellationToken::new())
        .await
        .expect("the live turn completes");
    let event_count = events.await.expect("joined").len();
    println!(
        "live turn in {:?} ({} events): text={:?} reasoning_len={} — this spent one small model call",
        started.elapsed(),
        event_count,
        response.text,
        response.reasoning.chars().count()
    );
    assert!(
        response.text.to_uppercase().contains("PONG"),
        "unexpected reply: {:?}",
        response.text
    );
}

// ---------------------------------------------------------------------------
// the model list behind the provider menu's right column
// ---------------------------------------------------------------------------

#[tokio::test]
async fn available_models_come_from_the_agents_advertised_session_state() {
    let (transport, _log) = ScriptedAgentTransport::new(Behavior::ModelsWithSetModel);
    let backend = AcpAgent::with_transport(acp_spec(Map::new()), None, Arc::new(transport))
        .expect("a valid spec builds");
    let list = backend
        .available_models()
        .await
        .expect("the agent advertises");
    assert!(
        list.authoritative,
        "advertised models accept session/set_model"
    );
    assert_eq!(
        list.models
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m1", "m2"]
    );
}

#[tokio::test]
async fn available_models_come_from_agents_config_options() {
    let (transport, _log) = ScriptedAgentTransport::new(Behavior::ConfigOptionsWithSetConfigOption);
    let backend = AcpAgent::with_transport(acp_spec(Map::new()), None, Arc::new(transport))
        .expect("a valid spec builds");
    let list = backend
        .available_models()
        .await
        .expect("the agent advertises configOptions");
    assert!(
        list.authoritative,
        "configOptions models accept session/set_config_option"
    );
    assert_eq!(
        list.models
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m1", "m2"]
    );
}

#[tokio::test]
async fn an_agent_without_model_state_lists_none_instead_of_a_fabricated_menu() {
    let (transport, _log) = ScriptedAgentTransport::new(Behavior::Normal);
    let backend = AcpAgent::with_transport(acp_spec(Map::new()), None, Arc::new(transport))
        .expect("a valid spec builds");
    let list = backend
        .available_models()
        .await
        .expect("an honest empty list");
    assert!(
        list.models.is_empty(),
        "no advertised models, no fabricated menu"
    );
    assert!(
        !list.authoritative,
        "without advertised model state a pick could not reach the agent"
    );
}

// ---------------------------------------------------------------------------
// the models_command fallback: agents whose ACP server stays silent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_models_command_lists_models_the_agent_does_not_advertise() {
    let (transport, _log) = ScriptedAgentTransport::new(Behavior::Normal);
    let mut options = Map::new();
    options.insert(
        "models_command".into(),
        json!(["/bin/sh", "-c", "printf 'm1\\nm2\\nm1\\n'"]),
    );
    let backend = AcpAgent::with_transport(acp_spec(options), None, Arc::new(transport))
        .expect("a valid spec builds");
    let list = backend
        .available_models()
        .await
        .expect("the lister answers where the wire stayed silent");
    assert!(!list.authoritative, "a CLI-read list is informational only");
    assert_eq!(
        list.models
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m1", "m2"]
    );
}

#[tokio::test]
async fn a_failing_models_command_is_an_error_not_an_empty_list() {
    let (transport, _log) = ScriptedAgentTransport::new(Behavior::Normal);
    let mut options = Map::new();
    options.insert(
        "models_command".into(),
        json!(["/bin/sh", "-c", "echo boom >&2; exit 3"]),
    );
    let backend = AcpAgent::with_transport(acp_spec(options), None, Arc::new(transport))
        .expect("a valid spec builds");
    let error = backend
        .available_models()
        .await
        .expect_err("a broken lister is a failure, not 'no models'");
    match &error {
        LlmError::Unavailable { reason, .. } => {
            assert!(reason.contains("exited"), "{reason}");
            assert!(reason.contains("boom"), "{reason}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// model_env: one pinned connection per model (opencode-style agents)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn each_pinned_model_gets_its_own_connection_with_the_substituted_env() {
    struct Capturing {
        launches: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
    }
    #[async_trait::async_trait]
    impl AcpTransport for Capturing {
        async fn spawn(&self, launch: &AgentLaunch) -> Result<AgentHandle, String> {
            self.launches
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(launch.env.clone());
            ScriptedAgentTransport::new(Behavior::Normal)
                .0
                .spawn(launch)
                .await
        }
    }
    let launches = Arc::new(Mutex::new(Vec::new()));
    let mut options = Map::new();
    options.insert("model_env".into(), json!({"PIN": "model={model}"}));
    let backend = AcpAgent::with_transport(
        acp_spec(options),
        None,
        Arc::new(Capturing {
            launches: Arc::clone(&launches),
        }),
    )
    .expect("a valid spec builds");

    for model in ["mA", "mA", "mB"] {
        let (tx, rx) = mpsc::channel(32);
        let events = collect_events(rx);
        backend
            .chat(ChatRequest::new(model, "i"), tx, CancellationToken::new())
            .await
            .expect("turn completes");
        events.await.expect("joined");
    }
    let launches = launches.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(
        launches.len(),
        2,
        "mA reuses its pinned connection; mB spawns a second"
    );
    assert_eq!(launches[0].get("PIN").map(String::as_str), Some("model=mA"));
    assert_eq!(launches[1].get("PIN").map(String::as_str), Some("model=mB"));
}

#[tokio::test]
async fn model_env_makes_a_silent_agent_authoritative() {
    let (transport, _log) = ScriptedAgentTransport::new(Behavior::Normal);
    let mut options = Map::new();
    options.insert("model_env".into(), json!({"PIN": "{model}"}));
    let backend = AcpAgent::with_transport(acp_spec(options), None, Arc::new(transport))
        .expect("a valid spec builds");
    let list = backend.available_models().await.expect("lists");
    assert!(
        list.authoritative,
        "with model_env a pick reaches the agent via its child env"
    );
}

#[tokio::test]
async fn each_bot_cwd_gets_its_own_connection_and_session_cwd() {
    struct CapturingCwd {
        launches: Arc<Mutex<Vec<Option<std::path::PathBuf>>>>,
        transport: ScriptedAgentTransport,
    }
    #[async_trait::async_trait]
    impl AcpTransport for CapturingCwd {
        async fn spawn(&self, launch: &AgentLaunch) -> Result<AgentHandle, String> {
            self.launches
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(launch.cwd.clone());
            self.transport.spawn(launch).await
        }
    }

    let (transport, log) = ScriptedAgentTransport::new(Behavior::Normal);
    let launches = Arc::new(Mutex::new(Vec::new()));
    let backend = AcpAgent::with_transport(
        acp_spec(Map::new()),
        None,
        Arc::new(CapturingCwd {
            launches: Arc::clone(&launches),
            transport,
        }),
    )
    .expect("a valid spec builds");

    let ws_a = std::env::temp_dir().join("swarmbot-test-ws-alpha");
    let ws_b = std::env::temp_dir().join("swarmbot-test-ws-beta");
    for ws in [&ws_a, &ws_b, &ws_a] {
        let (tx, rx) = mpsc::channel(32);
        let events = collect_events(rx);
        let mut req = ChatRequest::new("", "You are a bot.");
        req.messages.push(ChatMessage::user("hi"));
        req.cwd = Some(ws.clone());
        backend
            .chat(req, tx, CancellationToken::new())
            .await
            .expect("turn completes");
        events.await.expect("joined");
    }

    let launches = launches.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(
        launches.len(),
        2,
        "two workspaces reuse their own agent processes"
    );
    assert_eq!(launches[0].as_ref(), Some(&ws_a));
    assert_eq!(launches[1].as_ref(), Some(&ws_b));

    let cwds: Vec<String> = logged(&log)
        .into_iter()
        .filter(|entry| entry.get("method").and_then(Value::as_str) == Some("session/new"))
        .filter_map(|entry| entry["params"]["cwd"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        cwds,
        vec![
            ws_a.to_string_lossy().to_string(),
            ws_b.to_string_lossy().to_string(),
            ws_a.to_string_lossy().to_string(),
        ]
    );
}

// ---------------------------------------------------------------------------
// per-turn MCP servers on session/new
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_serializes_http_and_stdio_mcp_servers_in_session_new() {
    for behavior in [Behavior::McpHttpSupported, Behavior::Normal] {
        let (backend, log) = agent(behavior);
        let (tx, rx) = mpsc::channel(32);
        let events = collect_events(rx);

        let mut headers = BTreeMap::new();
        headers.insert("Authorization".into(), "Bearer tok-123".into());
        let mut env = BTreeMap::new();
        env.insert("RUST_LOG".into(), "info".into());
        let mut req = request_with_history();
        req.mcp_servers = vec![
            McpServerSpec::Http {
                name: "swarmbot".into(),
                url: "http://127.0.0.1:8788/mcp/t/turn-1".into(),
                headers,
            },
            McpServerSpec::Stdio {
                name: "helper".into(),
                command: "/usr/bin/helper-mcp".into(),
                args: vec!["--stdio".into()],
                env,
            },
        ];

        backend
            .chat(req, tx, CancellationToken::new())
            .await
            .expect("turn completes");
        events.await.expect("joined");

        let entries = wait_for_log(&log, "session/new", |entries| {
            find_request(entries, "session/new").is_some()
        })
        .await;
        let new_session = find_request(&entries, "session/new").expect("session/new sent");
        assert_eq!(
            new_session["params"]["mcpServers"],
            json!([
                {
                    "type": "http",
                    "name": "swarmbot",
                    "url": "http://127.0.0.1:8788/mcp/t/turn-1",
                    "headers": [{"name": "Authorization", "value": "Bearer tok-123"}],
                },
                {
                    "name": "helper",
                    "command": "/usr/bin/helper-mcp",
                    "args": ["--stdio"],
                    "env": [{"name": "RUST_LOG", "value": "info"}],
                }
            ]),
            "behavior {behavior:?}"
        );
    }
}

#[tokio::test]
async fn chat_skips_http_mcp_server_when_agent_advertises_http_false_but_keeps_stdio() {
    let (backend, log) = agent(Behavior::McpHttpUnsupported);
    let (tx, rx) = mpsc::channel(32);
    let events = collect_events(rx);

    let mut req = request_with_history();
    req.mcp_servers = vec![
        McpServerSpec::http("swarmbot", "http://127.0.0.1:8788/mcp/t/turn-2"),
        McpServerSpec::stdio("helper", "helper-mcp"),
    ];

    backend
        .chat(req, tx, CancellationToken::new())
        .await
        .expect("turn completes cleanly even when HTTP MCP is skipped");
    events.await.expect("joined");

    let entries = wait_for_log(&log, "session/new", |entries| {
        find_request(entries, "session/new").is_some()
    })
    .await;
    let new_session = find_request(&entries, "session/new").expect("session/new sent");
    assert_eq!(
        new_session["params"]["mcpServers"],
        json!([
            {
                "name": "helper",
                "command": "helper-mcp",
                "args": [],
                "env": [],
            }
        ])
    );
}
