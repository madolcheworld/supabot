//! A **stub** brain for tests: canned JSON on loopback, not a server.
//!
//! Compiled only under `cfg(any(test, feature = "test-support"))`. It binds
//! `127.0.0.1:0`, speaks just enough hand-written HTTP/1.1 (`content-length`,
//! `connection: close`, no keep-alive) to exercise [`crate::BrainClient`],
//! records every request so tests can assert on what the client *sent*, and
//! serves WebSocket paths under `/ws/`: `/ws/live` writes three canned
//! [`LiveEvent`] frames (snapshot, entry upsert, `synchronized`) and closes,
//! `/ws/garbage` writes one non-JSON frame, `/ws/denied` refuses the upgrade
//! with a 401 envelope, anything else closes immediately. Routes exist only
//! where a test needs them; everything else is a documented 404 envelope.
//!
//! Panic policy: this module is test scaffolding, so [`StubBrain::start`]
//! panics if it cannot bind a loopback socket or spawn its accept thread —
//! there is nothing to stub without them. No other code path panics.

use std::io::{Cursor, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use swarmbot_contracts::{
    ApprovalRule, BackendSpec, Bot, BotId, Conversation, ConversationId, ConversationKind,
    Decision, Entry, EntryId, EntryKind, LiveCursor, LiveEvent, NodeInfo, PolicyMap, RequestId,
    RequestStatus, Responder, RuleEffect, RuleScope, RuntimeRequest, Settings, Snapshot, Surface,
    Task, TaskId, TaskStatus, TaskStep,
};

/// One request the stub received, as tests see it.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedRequest {
    pub method: String,
    /// Path without the query, e.g. `/api/bots`.
    pub path: String,
    /// The raw (still percent-encoded) query string, empty when none.
    pub query: String,
    /// The `Authorization` header value, e.g. `Bearer tok`.
    pub auth: Option<String>,
    /// The `Cookie` header value ureq's jar sent, e.g. after `login()`.
    pub cookie: Option<String>,
    /// The request body as text (empty when there was none).
    pub body: String,
}

/// The running stub. Dropping it stops the accept thread and closes the
/// listener; per-connection threads end on their own (all reads are bounded
/// by short timeouts).
pub struct StubBrain {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl StubBrain {
    /// Bind `127.0.0.1:0` and start serving in the background.
    pub fn start() -> Self {
        // Test scaffolding: see the panic policy in the module docs.
        let listener = TcpListener::bind("127.0.0.1:0").expect("stub brain: bind loopback");
        listener
            .set_nonblocking(true)
            .expect("stub brain: nonblocking accept");
        let addr = listener.local_addr().expect("stub brain: local addr");
        let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let accept_thread = {
            let (requests, shutdown) = (Arc::clone(&requests), Arc::clone(&shutdown));
            std::thread::Builder::new()
                .name("stub-brain".to_string())
                .spawn(move || accept_loop(listener, requests, shutdown))
                .expect("stub brain: spawn accept thread")
        };
        Self {
            addr,
            requests,
            shutdown,
            accept_thread: Some(accept_thread),
        }
    }

    /// The HTTP origin, e.g. `http://127.0.0.1:54321`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The WebSocket origin, e.g. `ws://127.0.0.1:54321`.
    pub fn ws_url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// Every request received so far, in arrival order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock(&self.requests).clone()
    }

    /// The most recent request matching `method` and exact `path`.
    pub fn last_request(&self, method: &str, path: &str) -> Option<RecordedRequest> {
        lock(&self.requests)
            .iter()
            .rev()
            .find(|r| r.method == method && r.path == path)
            .cloned()
    }
}

impl Drop for StubBrain {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
    }
}

fn accept_loop(
    listener: TcpListener,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                let _ = stream.set_nonblocking(false);
                let requests = Arc::clone(&requests);
                // One thread per connection so a held-open WebSocket never
                // blocks later HTTP calls in the same test.
                let _ =
                    std::thread::Builder::new().spawn(move || handle_connection(stream, &requests));
            }
            Err(ref err) if err.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

// --- HTTP ----------------------------------------------------------------

struct RawRequest {
    method: String,
    path: String,
    query: String,
    /// The verbatim request head bytes: the WebSocket handshake replays them
    /// into tungstenite, which insists on parsing the upgrade request itself.
    head: Vec<u8>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RawRequest {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }

    fn is_websocket(&self) -> bool {
        // Path-based on purpose: an `Upgrade` header on a plain REST route is
        // ignored, exactly like a well-behaved HTTP server would.
        self.path.starts_with("/ws/")
            && self
                .header("upgrade")
                .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
    }
}

/// One canned answer: status, extra headers, body.
struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Reply {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.to_string(),
        }
    }

    /// 200 with a serialized contract value — the stub answers with the real
    /// wire shapes, not hand-typed JSON, so a schema drift breaks the tests.
    fn value<T: Serialize>(value: &T) -> Self {
        // Contract types always serialize; the fallback keeps the stub
        // panic-free without pretending to succeed.
        let body = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
        Self {
            status: 200,
            headers: Vec::new(),
            body,
        }
    }

    fn ok(body: Value) -> Self {
        Self::json(200, body)
    }

    fn no_content() -> Self {
        Self {
            status: 204,
            headers: Vec::new(),
            body: String::new(),
        }
    }

    fn error(status: u16, code: &str, message: &str) -> Self {
        Self::json(status, json!({"error": {"code": code, "message": message}}))
    }

    fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

fn handle_connection(mut stream: TcpStream, requests: &Mutex<Vec<RecordedRequest>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let Ok(request) = read_request(&mut stream) else {
        return;
    };
    lock(requests).push(RecordedRequest {
        method: request.method.clone(),
        path: request.path.clone(),
        query: request.query.clone(),
        auth: request.header("authorization"),
        cookie: request.header("cookie"),
        body: String::from_utf8_lossy(&request.body).to_string(),
    });
    if request.is_websocket() {
        serve_websocket(stream, &request);
        return;
    }
    let reply = route(&request);
    write_response(&mut stream, &reply);
    // Clean FIN, never a RST: the full request was consumed above, so nothing
    // unread is left in the socket when it is dropped.
    let _ = stream.shutdown(Shutdown::Write);
}

/// Read the request head one byte at a time: a buffered reader could swallow
/// bytes that belong to the WebSocket frames tungstenite must see itself.
fn read_request(stream: &mut TcpStream) -> std::io::Result<RawRequest> {
    let invalid = |what: &str| std::io::Error::new(ErrorKind::InvalidData, what.to_string());
    let mut head = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte)?;
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() > 16 * 1024 {
            return Err(invalid("request head is absurdly large"));
        }
    }
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.lines();
    let request_line = lines.next().ok_or_else(|| invalid("empty request"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| invalid("no method"))?
        .to_string();
    let target = parts.next().ok_or_else(|| invalid("no path"))?;
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.to_string(), String::new()),
    };
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        stream.read_exact(&mut body)?;
    }
    Ok(RawRequest {
        method,
        path,
        query,
        head,
        headers,
        body,
    })
}

fn write_response(stream: &mut TcpStream, reply: &Reply) {
    let reason = match reply.status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        502 => "Bad Gateway",
        _ => "Unknown",
    };
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\nconnection: close\r\n",
        reply.status
    );
    if reply.status != 204 {
        head.push_str(&format!(
            "content-type: application/json\r\ncontent-length: {}\r\n",
            reply.body.len()
        ));
    }
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    if reply.status != 204 {
        let _ = stream.write_all(reply.body.as_bytes());
    }
    let _ = stream.flush();
}

// --- WebSocket -------------------------------------------------------------

/// Replay-then-passthrough stream: tungstenite's server handshake wants to
/// read the HTTP upgrade request itself, but we already consumed it.
struct DuplexStream {
    head: Cursor<Vec<u8>>,
    stream: TcpStream,
}

impl Read for DuplexStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if (self.head.position() as usize) < self.head.get_ref().len() {
            self.head.read(buf)
        } else {
            self.stream.read(buf)
        }
    }
}

impl Write for DuplexStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

fn serve_websocket(mut stream: TcpStream, request: &RawRequest) {
    // `/ws/denied` refuses the upgrade with a plain HTTP 401 envelope, the
    // way the brain rejects a bad token before switching protocols.
    if request.path == "/ws/denied" {
        write_response(&mut stream, &Reply::error(401, "unauthorized", "bad token"));
        let _ = stream.shutdown(Shutdown::Write);
        return;
    }
    let mut duplex = DuplexStream {
        head: Cursor::new(request.head.clone()),
        stream,
    };
    let Ok(mut socket) = tungstenite::accept(&mut duplex) else {
        return;
    };
    for text in ws_frames(&request.path) {
        if socket
            .send(tungstenite::Message::Text(text.into()))
            .is_err()
        {
            return;
        }
    }
    let _ = socket.close(None);
    let _ = socket.flush();
    // Drain the client's close ack (bounded by a short read timeout) so the
    // socket ends with a clean FIN and never a RST that could eat frames the
    // client has not read yet.
    let _ = socket
        .get_mut()
        .stream
        .set_read_timeout(Some(Duration::from_millis(100)));
    while socket.read().is_ok() {}
}

/// The canned frames per WS path; anything but `/ws/live` and `/ws/garbage`
/// (i.e. `/ws/silent`) gets a handshake and an immediate clean close.
fn ws_frames(path: &str) -> Vec<String> {
    match path {
        "/ws/live" => {
            let snapshot = Snapshot {
                cursor: LiveCursor::new(3, 1),
                bots: vec![stub_bot()],
                conversations: vec![stub_conversation()],
                entries: vec![stub_entry()],
                requests: Vec::new(),
                tasks: Vec::new(),
                nodes: Vec::new(),
                turns: Vec::new(),
            };
            let events = [
                LiveEvent::Snapshot(Box::new(snapshot)),
                LiveEvent::EntryUpsert {
                    cursor: LiveCursor::new(3, 2),
                    entry: stub_reply(),
                },
                LiveEvent::Synchronized {
                    cursor: LiveCursor::new(3, 3),
                },
            ];
            events
                .iter()
                .filter_map(|event| serde_json::to_string(event).ok())
                .collect()
        }
        "/ws/garbage" => vec!["this is not json {{{".to_string()],
        _ => Vec::new(),
    }
}

// --- routing ----------------------------------------------------------------

fn route(request: &RawRequest) -> Reply {
    let segments: Vec<&str> = request.path.split('/').filter(|s| !s.is_empty()).collect();
    let method = request.method.as_str();
    let body = || String::from_utf8_lossy(&request.body).to_string();
    match (method, segments.as_slice()) {
        ("GET", ["api", "health"]) => Reply::ok(json!({
            "ok": true, "version": "0.1.0", "epoch": 7, "uptime_secs": 42
        })),
        ("POST", ["api", "session"]) => Reply::ok(json!({"ok": true}))
            .with_header("set-cookie", "swarmbot_session=stub-session; Path=/"),
        ("GET", ["api", "bots"]) => Reply::value(&[stub_bot()]),
        ("POST", ["api", "bots"]) => Reply::json(201, to_value(&echo_bot(&body()))),
        ("GET", ["api", "bots", "missing"]) => {
            Reply::error(404, "not_found", "bot not found: missing")
        }
        ("GET" | "PATCH", ["api", "bots", _id]) => Reply::value(&stub_bot()),
        ("POST", ["api", "bots", _id, "session"]) => Reply::ok(json!({"folded": 3})),
        ("GET", ["api", "conversations"]) => Reply::value(&[stub_conversation()]),
        ("POST", ["api", "conversations"]) => Reply::json(201, to_value(&echo_group(&body()))),
        ("GET", ["api", "conversations", _id]) => Reply::value(&stub_conversation()),
        ("PATCH", ["api", "conversations", id]) => Reply::value(&echo_group_update(id, &body())),
        ("GET", ["api", "conversations", _id, "entries" | "tail"]) => Reply::value(&[stub_entry()]),
        ("POST", ["api", "conversations", _id, "messages"]) => {
            Reply::ok(echo_sent_message(&body()))
        }
        ("POST", ["api", "conversations", _id, "interrupt"]) => Reply::ok(json!({"stopped": true})),
        ("GET", ["api", "requests"]) => Reply::value(&[stub_request()]),
        ("POST", ["api", "requests", _id, "respond"]) => Reply::value(&echo_answered(&body())),
        ("GET", ["api", "tasks"]) => Reply::value(&[stub_task()]),
        ("GET", ["api", "tasks", _id]) => {
            Reply::ok(json!({"task": stub_task(), "steps": [stub_step()]}))
        }
        ("POST", ["api", "tasks", _id, "cancel"]) => {
            let mut task = stub_task();
            task.status = TaskStatus::Cancelled;
            Reply::value(&task)
        }
        ("GET", ["api", "nodes"]) => Reply::value(&[NodeInfo::local(
            "stub-node",
            Vec::new(),
            PolicyMap::default(),
        )]),
        ("POST", ["api", "nodes", "pairing-codes"]) => Reply::ok(json!({
            "code": "ABCD-EFGH", "expires_at": "2026-01-01T00:00:00Z", "node_id": null
        })),
        ("GET", ["api", "rules"]) => Reply::value(&[stub_rule()]),
        ("POST", ["api", "rules"]) => match serde_json::from_str::<ApprovalRule>(&body()) {
            Ok(rule) => Reply::json(201, to_value(&rule)),
            Err(err) => Reply::error(400, "invalid_input", &err.to_string()),
        },
        ("GET", ["api", "settings"]) => Reply::value(&Settings::default()),
        ("PATCH", ["api", "settings"]) => Reply::ok(merge_settings(&body())),
        ("GET", ["api", "backends"]) => Reply::ok(json!([BackendSpec::fake("fake").public_view()])),
        ("GET", ["api", "backends", "probe"]) => Reply::ok(json!([{
            "backend": "fake", "ok": true, "detail": null, "model": "fake", "version": null
        }])),
        ("POST", ["api", "shutdown"]) => Reply::ok(json!({"ok": true})),
        // Explicit failure routes so error mapping can be tested end to end.
        ("GET", ["api", "boom", "409"]) => Reply::error(409, "conflict", "stub conflict"),
        // Not JSON at all: the client must still keep the status and truncate.
        ("GET", ["api", "boom", "502"]) => Reply {
            status: 502,
            headers: Vec::new(),
            body: format!("upstream exploded: {}", "x".repeat(600)),
        },
        // Every DELETE in the API answers 204 with no body.
        ("DELETE", _) => Reply::no_content(),
        _ => Reply::error(
            404,
            "not_found",
            &format!("the stub has no route for {method} {}", request.path),
        ),
    }
}

fn to_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Top-level JSON merge over the default settings, mirroring the brain's
/// documented `PATCH /api/settings` behaviour.
fn merge_settings(body: &str) -> Value {
    let mut merged = to_value(&Settings::default());
    if let Ok(patch) = serde_json::from_str::<Value>(body)
        && let (Some(target), Some(source)) = (merged.as_object_mut(), patch.as_object())
    {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    merged
}

// --- canned entities (fixed ids so tests can assert on them) ----------------

fn stub_bot() -> Bot {
    let mut bot = Bot::new(
        "stub",
        "Stub Bot",
        "a canned bot for tests",
        "fake",
        stub_conversation().id,
    );
    bot.id = BotId::from_suffix("000000000000000000000000000000b1");
    bot
}

fn stub_conversation() -> Conversation {
    let mut conversation =
        Conversation::direct(BotId::from_suffix("000000000000000000000000000000b1"));
    conversation.id = ConversationId::from_suffix("000000000000000000000000000000c1");
    conversation
}

fn stub_entry() -> Entry {
    Entry::owner_message(stub_conversation().id, 1, "canned entry")
}

fn stub_reply() -> Entry {
    Entry::bot_message(stub_conversation().id, 2, stub_bot().id, "stub reply", None)
}

fn stub_task() -> Task {
    let mut task = Task::own(
        stub_bot().id,
        stub_conversation().id,
        "stub task",
        "a canned brief",
    );
    task.id = TaskId::from_suffix("000000000000000000000000000000d1");
    task
}

fn stub_step() -> TaskStep {
    TaskStep::tool(stub_task().id, 1, "shell", "cargo test")
}

fn stub_request() -> RuntimeRequest {
    let mut request = RuntimeRequest::approval(
        stub_bot().id,
        stub_conversation().id,
        Surface::Shell,
        "shell",
        "cargo test",
        None,
    );
    request.id = RequestId::from_suffix("000000000000000000000000000000e1");
    request
}

fn stub_rule() -> ApprovalRule {
    ApprovalRule::new(RuleScope::Global, RuleEffect::Allow)
}

// --- request echoes: the stub answers with what the client actually sent ----

/// `POST /api/bots`: the created bot carries the requested name and label.
fn echo_bot(body: &str) -> Bot {
    let mut bot = stub_bot();
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(name) = value["name"].as_str() {
            bot.name = name.to_string();
        }
        if let Some(label) = value["label"].as_str() {
            bot.label = label.to_string();
        }
    }
    bot
}

#[derive(Deserialize)]
struct GroupCreateBody {
    #[serde(default)]
    title: String,
    #[serde(default)]
    members: Vec<BotId>,
    #[serde(default)]
    lead: Option<BotId>,
}

/// `POST /api/conversations`: a group carrying the requested title, members, and lead.
fn echo_group(body: &str) -> Conversation {
    let parsed: GroupCreateBody = serde_json::from_str(body).unwrap_or(GroupCreateBody {
        title: String::new(),
        members: Vec::new(),
        lead: None,
    });
    let default_a = stub_bot().id;
    let default_b = BotId::from_suffix("000000000000000000000000000000b2");
    let members = if parsed.members.is_empty() {
        vec![default_a.clone(), default_b]
    } else {
        parsed.members
    };
    let lead = parsed
        .lead
        .unwrap_or_else(|| members.first().cloned().unwrap_or(default_a));
    let title = if parsed.title.trim().is_empty() {
        "Stub Group".to_string()
    } else {
        parsed.title
    };
    let base = stub_conversation();
    Conversation {
        id: ConversationId::new(),
        kind: ConversationKind::Group { members, lead },
        title: Some(title),
        created_at: base.created_at,
        updated_at: base.updated_at,
        last_seq: 0,
        archived: false,
    }
}

#[derive(Deserialize)]
struct GroupPatchBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    members: Option<Vec<BotId>>,
    #[serde(default)]
    lead: Option<BotId>,
}

/// `PATCH /api/conversations/{id}`: the updated group conversation.
fn echo_group_update(id: &str, body: &str) -> Conversation {
    let parsed: GroupPatchBody = serde_json::from_str(body).unwrap_or(GroupPatchBody {
        title: None,
        members: None,
        lead: None,
    });
    let default_a = stub_bot().id;
    let default_b = BotId::from_suffix("000000000000000000000000000000b2");
    let members = match parsed.members {
        Some(members) => members,
        None => {
            let mut members = vec![default_a.clone(), default_b];
            if let Some(ref lead) = parsed.lead
                && !members.contains(lead)
            {
                members.push(lead.clone());
            }
            members
        }
    };
    let lead = parsed
        .lead
        .unwrap_or_else(|| members.first().cloned().unwrap_or(default_a));
    let base = stub_conversation();
    Conversation {
        id: ConversationId::parse(id).unwrap_or(base.id),
        kind: ConversationKind::Group { members, lead },
        title: Some(parsed.title.unwrap_or_else(|| "Stub Group".to_string())),
        created_at: base.created_at,
        updated_at: base.updated_at,
        last_seq: base.last_seq,
        archived: false,
    }
}

#[derive(Deserialize)]
struct MessageBody {
    #[serde(default)]
    text: String,
    #[serde(default)]
    addressed: Vec<BotId>,
    #[serde(default)]
    reply_to: Option<EntryId>,
}

/// `POST …/messages`: the committed entry carries the text the client sent,
/// and the targets are the addressed bots.
fn echo_sent_message(body: &str) -> Value {
    let parsed: MessageBody = serde_json::from_str(body).unwrap_or(MessageBody {
        text: String::new(),
        addressed: Vec::new(),
        reply_to: None,
    });
    let mut entry = Entry::owner_message(stub_conversation().id, 7, &parsed.text);
    if let EntryKind::Message(message) = &mut entry.kind {
        message.reply_to = parsed.reply_to;
    }
    let targets = if parsed.addressed.is_empty() {
        vec![stub_bot().id]
    } else {
        parsed.addressed
    };
    json!({"entry": entry, "targets": targets, "queued": [], "reason": "lead"})
}

#[derive(Deserialize)]
struct RespondBody {
    #[serde(default)]
    decision: Option<Decision>,
    #[serde(default)]
    answer: Option<Value>,
}

/// `POST …/respond`: the same request, settled with what the client sent.
fn echo_answered(body: &str) -> RuntimeRequest {
    let parsed: RespondBody = serde_json::from_str(body).unwrap_or(RespondBody {
        decision: None,
        answer: None,
    });
    let mut request = stub_request();
    request.status = RequestStatus::Answered;
    request.decision = parsed.decision;
    request.answer = parsed.answer;
    request.responded_by = Some(Responder::Owner);
    request.resolved_at = Some(request.created_at);
    request
}
