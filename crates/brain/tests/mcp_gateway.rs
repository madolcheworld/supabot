//! Tests for the MCP gateway served over the brain's internal listener.

mod common;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};
use swarmbot_brain::Brain;
use swarmbot_brain::config::{Paths, Resolved};
use swarmbot_brain::server::{self, ServeOptions};
use swarmbot_brain::telemetry::SecretRegistry;
use swarmbot_contracts::BackendSpec;
use swarmbot_contracts::config::BrainConfig;
use tokio::net::TcpListener;

const TIMEOUT: Duration = Duration::from_secs(5);

async fn wait_for_port(addr: SocketAddr) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("port {addr} never opened");
}

struct RunningInternalBrain {
    brain: Brain,
    internal_addr: SocketAddr,
    _tempdir: tempfile::TempDir,
}

impl RunningInternalBrain {
    async fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = BrainConfig {
            public_addr: "127.0.0.1:0".into(),
            internal_addr: Some("127.0.0.1:0".into()),
            default_backend: "fake".into(),
            backends: vec![BackendSpec::fake("fake")],
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        };
        let resolved = Resolved {
            config,
            data_dir: dir.path().to_path_buf(),
            paths: Paths::new(dir.path()),
            config_path: None,
            team_path: None,
            roots: Vec::new(),
            expiry_ticker: false,
            offline: true,
            fresh: true,
        };
        let mcp_dir = dir.path().join("workspaces").join("mcp");
        std::fs::create_dir_all(&mcp_dir).expect("mcp dir");
        std::fs::write(mcp_dir.join("marker.txt"), "hello from mcp test").expect("write marker");

        let brain =
            Brain::memory(resolved, &BTreeMap::new(), SecretRegistry::new()).expect("brain boots");

        let public_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind public");
        let internal_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind internal");
        let internal_addr = internal_listener.local_addr().expect("internal addr");
        let public_addr = public_listener.local_addr().expect("public addr");

        tokio::spawn(server::serve(
            brain.state.clone(),
            public_listener,
            Some(internal_listener),
            ServeOptions::in_process(),
        ));

        wait_for_port(public_addr).await;
        wait_for_port(internal_addr).await;

        let client = Client::builder().timeout(TIMEOUT).build().expect("client");
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            if let Ok(resp) = client
                .get(format!("http://{internal_addr}/health"))
                .send()
                .await
                && resp.status().is_success()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        Self {
            brain,
            internal_addr,
            _tempdir: dir,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.internal_addr)
    }

    async fn stop(self) {
        self.brain.state.request_shutdown();
        self.brain.swarm.shutdown().await;
    }
}

#[tokio::test]
async fn internal_listener_health_check() {
    let brain = RunningInternalBrain::start().await;
    let client = Client::builder().timeout(TIMEOUT).build().expect("client");

    let resp = client
        .get(brain.url("/health"))
        .send()
        .await
        .expect("send /health");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let val: Value = resp.json().await.expect("json");
    assert_eq!(val["ok"], json!(true));
    assert_eq!(val["listener"], json!("internal"));
    assert_eq!(val["version"], json!(env!("CARGO_PKG_VERSION")));
}

#[tokio::test]
async fn internal_listener_writes_mcp_config_file() {
    let brain = RunningInternalBrain::start().await;
    let mcp_cfg_path = brain._tempdir.path().join("swarmbot-mcp.json");
    assert!(mcp_cfg_path.exists(), "mcp config file should be written");
    let content = std::fs::read_to_string(&mcp_cfg_path).expect("read mcp config");
    let val: Value = serde_json::from_str(&content).expect("parse mcp config json");
    let expected_url = brain.url("/mcp");
    assert_eq!(val["mcpServers"]["swarmbot"]["url"], json!(expected_url));
}

fn parse_sse_or_json(text: &str) -> Value {
    if text.contains("data:") {
        for line in text.lines() {
            if let Some(payload) = line.strip_prefix("data:") {
                let trimmed = payload.trim();
                if !trimmed.is_empty()
                    && let Ok(val) = serde_json::from_str(trimmed)
                {
                    return val;
                }
            }
        }
        panic!("could not find valid json in sse data: {text}");
    } else {
        serde_json::from_str(text).expect("parse direct json")
    }
}

#[tokio::test]
async fn internal_listener_serves_mcp_initialize_and_tools_list() {
    let brain = RunningInternalBrain::start().await;
    let client = Client::builder().timeout(TIMEOUT).build().expect("client");

    // 1. Send MCP initialize request to /mcp
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test-harness", "version": "1.0" }
        }
    });

    let resp = client
        .post(brain.url("/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .json(&init_req)
        .send()
        .await
        .expect("send /mcp init");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let session_id = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    let body_text = resp.text().await.expect("body text");
    let val = parse_sse_or_json(&body_text);

    assert_eq!(val["id"], 1);
    assert_eq!(val["result"]["serverInfo"]["name"], "swarmbot");
    assert!(val["result"]["capabilities"]["tools"].is_object());

    // 2. Initialized notification
    let notif = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let mut notif_builder = client
        .post(brain.url("/mcp"))
        .header("Accept", "application/json, text/event-stream");
    if let Some(ref sid) = session_id {
        notif_builder = notif_builder.header("mcp-session-id", sid.as_str());
    }
    let notif_resp = notif_builder
        .json(&notif)
        .send()
        .await
        .expect("send initialized notif");
    assert!(
        notif_resp.status().is_success() || notif_resp.status() == reqwest::StatusCode::ACCEPTED,
        "notif status: {}",
        notif_resp.status()
    );

    // 3. List tools over HTTP
    let list_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });

    let mut list_builder = client
        .post(brain.url("/mcp"))
        .header("Accept", "application/json, text/event-stream");
    if let Some(ref sid) = session_id {
        list_builder = list_builder.header("mcp-session-id", sid.as_str());
    }
    let list_resp = list_builder
        .json(&list_req)
        .send()
        .await
        .expect("send tools/list");

    assert_eq!(list_resp.status(), reqwest::StatusCode::OK);
    let list_text = list_resp.text().await.expect("list text");
    let list_val = parse_sse_or_json(&list_text);
    assert_eq!(list_val["id"], 2);

    let tools = list_val["result"]["tools"].as_array().expect("tools array");
    assert!(!tools.is_empty(), "tools should not be empty");

    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // Verify built-in tools are exposed
    assert!(
        tool_names.contains(&"read_file"),
        "should contain read_file"
    );
    assert!(tool_names.contains(&"shell"), "should contain shell");

    // 4. Call tool read_file over HTTP
    let call_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "read_file",
            "arguments": {
                "path": "marker.txt"
            }
        }
    });

    let mut call_builder = client
        .post(brain.url("/mcp"))
        .header("Accept", "application/json, text/event-stream");
    if let Some(ref sid) = session_id {
        call_builder = call_builder.header("mcp-session-id", sid.as_str());
    }
    let call_resp = call_builder
        .json(&call_req)
        .send()
        .await
        .expect("send tools/call");

    assert_eq!(call_resp.status(), reqwest::StatusCode::OK);
    let call_text = call_resp.text().await.expect("call text");
    let call_val = parse_sse_or_json(&call_text);
    assert_eq!(call_val["id"], 3);

    let content = call_val["result"]["content"]
        .as_array()
        .expect("content array");
    assert!(!content.is_empty(), "content should not be empty");
    let text = content[0]["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("hello from mcp test"),
        "expected output in text: {text}"
    );
}

#[tokio::test]
async fn internal_listener_refuses_owner_routes() {
    let brain = RunningInternalBrain::start().await;
    let client = Client::builder().timeout(TIMEOUT).build().expect("client");

    // Owner routes must not be exposed on the internal listener
    let resp = client
        .get(brain.url("/api/bots"))
        .send()
        .await
        .expect("send /api/bots");

    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

async fn mcp_post(
    client: &Client,
    url: &str,
    session_id: Option<&str>,
    payload: Value,
) -> (reqwest::StatusCode, Option<String>, Option<Value>) {
    let mut builder = client
        .post(url)
        .header("Accept", "application/json, text/event-stream");
    if let Some(sid) = session_id {
        builder = builder.header("mcp-session-id", sid);
    }
    let resp = builder.json(&payload).send().await.expect("send mcp post");
    let status = resp.status();
    let new_sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|h| h.to_str().ok())
        .map(ToString::to_string);
    let body = resp.text().await.expect("mcp response text");
    let parsed = if body.trim().is_empty() || !status.is_success() {
        serde_json::from_str::<Value>(&body).ok()
    } else {
        Some(parse_sse_or_json(&body))
    };
    (status, new_sid, parsed)
}

async fn mcp_initialize(client: &Client, url: &str) -> Option<String> {
    let (status, sid, init_val) = mcp_post(
        client,
        url,
        None,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "turn-mcp-test", "version": "1.0" }
            }
        }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let init_val = init_val.expect("initialize payload");
    assert_eq!(init_val["id"], 1);

    let (notif_status, _, _) = mcp_post(
        client,
        url,
        sid.as_deref(),
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    )
    .await;
    assert!(
        notif_status.is_success() || notif_status == reqwest::StatusCode::ACCEPTED,
        "unexpected initialized notification status: {notif_status}"
    );
    sid
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_turn_mcp_endpoint_runs_bot_tools_filters_by_request_and_returns_404_after_drop() {
    use swarmbot_contracts::conversation::ConversationKind;
    use swarmbot_contracts::{EntryKind, Toolset, TurnId};
    use swarmbot_core::McpTurnRequest;
    use swarmbot_llm::McpServerSpec;

    let running = RunningInternalBrain::start().await;
    let client = Client::builder().timeout(TIMEOUT).build().expect("client");

    let lead = running.brain.swarm.bots.by_name("lead").expect("lead bot");
    let scout = running
        .brain
        .swarm
        .bots
        .by_name("scout")
        .expect("scout bot");
    let group = running
        .brain
        .swarm
        .conversations
        .list()
        .expect("conversations")
        .into_iter()
        .find(|c| matches!(c.kind, ConversationKind::Group { .. }))
        .expect("demo group");

    let provider = running
        .brain
        .swarm
        .runner_env()
        .mcp_provider
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .expect("Brain wires McpTurnProvider onto Swarm");

    let workspace = running
        .brain
        .swarm
        .ensure_bot_workspace(&lead.name)
        .expect("lead workspace");
    let spool_dir = workspace.join(".spool");

    // 1. Turn inside a task: `in_task: true` must hide `start_task` and refuse calling it,
    //    while `toolsets: [Bot, Task]` must hide node tools (`read_file`, `shell`).
    let task_turn_reg = provider
        .register_turn(McpTurnRequest {
            bot_id: lead.id.clone(),
            conversation_id: group.id.clone(),
            turn_id: TurnId::new(),
            task_id: None,
            workspace: workspace.clone(),
            spool_dir: spool_dir.clone(),
            toolsets: vec![Toolset::Bot, Toolset::Task],
            node_caps: None,
            in_task: true,
        })
        .expect("register turn 1");

    assert_eq!(task_turn_reg.servers.len(), 1);
    let task_turn_url = match &task_turn_reg.servers[0] {
        McpServerSpec::Http { name, url, .. } => {
            assert_eq!(name, "swarmbot");
            assert!(
                url.starts_with(&format!("http://{}/mcp/t/", running.internal_addr)),
                "unexpected URL: {url}"
            );
            url.clone()
        }
        other => panic!("expected http McpServerSpec, got {other:?}"),
    };

    let sid = mcp_initialize(&client, &task_turn_url).await;
    let (list_status, _, list_val) = mcp_post(
        &client,
        &task_turn_url,
        sid.as_deref(),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    )
    .await;
    assert_eq!(list_status, reqwest::StatusCode::OK);
    let list_val = list_val.expect("list response");
    let tool_names: Vec<&str> = list_val["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(tool_names.contains(&"message_bot"), "{tool_names:?}");
    assert!(tool_names.contains(&"handoff_task"), "{tool_names:?}");
    assert!(!tool_names.contains(&"read_file"), "{tool_names:?}");
    assert!(!tool_names.contains(&"shell"), "{tool_names:?}");
    assert!(
        !tool_names.contains(&"start_task"),
        "in_task=true must hide start_task: {tool_names:?}"
    );
    assert!(
        !tool_names.contains(&"ask_user"),
        "in_task=true must hide ask_user: {tool_names:?}"
    );

    // Filtered-out tool call must be refused rather than executing.
    let (filtered_status, _, filtered_val) = mcp_post(
        &client,
        &task_turn_url,
        sid.as_deref(),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "start_task",
                "arguments": { "title": "bad", "task": "should fail" }
            }
        }),
    )
    .await;
    assert_eq!(filtered_status, reqwest::StatusCode::OK);
    let filtered_val = filtered_val.expect("filtered response");
    assert_eq!(
        filtered_val["result"]["isError"],
        json!(true),
        "{filtered_val}"
    );
    let err_text = filtered_val["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        err_text.contains("not available"),
        "unexpected error text: {err_text}"
    );

    // Real bot execution of `message_bot`: runs as `lead`, posts into the group, wakes `scout`.
    let (msg_status, _, msg_val) = mcp_post(
        &client,
        &task_turn_url,
        sid.as_deref(),
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "message_bot",
                "arguments": {
                    "to": "scout",
                    "text": "ping from per-turn mcp"
                }
            }
        }),
    )
    .await;
    assert_eq!(msg_status, reqwest::StatusCode::OK);
    let msg_val = msg_val.expect("message_bot response");
    assert_ne!(
        msg_val["result"]["isError"],
        json!(true),
        "message_bot failed: {msg_val}"
    );
    assert!(
        running.brain.swarm.wait_for_idle(common::IDLE_BUDGET).await,
        "swarm should settle after waking scout"
    );
    let entries = running
        .brain
        .swarm
        .conversations
        .history(&group.id, 0, 50)
        .expect("group history");
    assert!(
        entries.iter().any(|entry| {
            matches!(&entry.kind, EntryKind::Message(msg) if msg.text.contains("ping from per-turn mcp"))
                && entry.author == swarmbot_contracts::Author::bot(lead.id.clone())
        }),
        "expected group message authored by real `lead` bot, got {entries:?}"
    );

    // 2. Non-task turn (`in_task: false`) exposes `start_task` and executes `handoff_task`.
    let normal_turn_reg = provider
        .register_turn(McpTurnRequest {
            bot_id: lead.id.clone(),
            conversation_id: group.id.clone(),
            turn_id: TurnId::new(),
            task_id: None,
            workspace,
            spool_dir,
            toolsets: vec![Toolset::Bot, Toolset::Task],
            node_caps: None,
            in_task: false,
        })
        .expect("register turn 2");
    let normal_turn_url = match &normal_turn_reg.servers[0] {
        McpServerSpec::Http { url, .. } => url.clone(),
        other => panic!("expected http spec, got {other:?}"),
    };
    let sid2 = mcp_initialize(&client, &normal_turn_url).await;
    let (_, _, list2_val) = mcp_post(
        &client,
        &normal_turn_url,
        sid2.as_deref(),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    )
    .await;
    let list2_val = list2_val.expect("list2");
    let tool_names2: Vec<&str> = list2_val["result"]["tools"]
        .as_array()
        .expect("tools2")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        tool_names2.contains(&"start_task"),
        "in_task=false exposes start_task: {tool_names2:?}"
    );

    let (handoff_status, _, handoff_val) = mcp_post(
        &client,
        &normal_turn_url,
        sid2.as_deref(),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "handoff_task",
                "arguments": {
                    "to": "scout",
                    "title": "Inspect via MCP",
                    "task": "Verify the module layout"
                }
            }
        }),
    )
    .await;
    assert_eq!(handoff_status, reqwest::StatusCode::OK);
    let handoff_val = handoff_val.expect("handoff response");
    assert_ne!(
        handoff_val["result"]["isError"],
        json!(true),
        "handoff_task failed: {handoff_val}"
    );
    assert!(
        running.brain.swarm.wait_for_idle(common::IDLE_BUDGET).await,
        "swarm should settle after handoff task"
    );
    let tasks = running.brain.swarm.tasks.list(&scout.id).expect("tasks");
    assert!(
        tasks.iter().any(|t| {
            t.title == "Inspect via MCP"
                && t.owner == scout.id
                && t.requester == Some(lead.id.clone())
        }),
        "expected task assigned to scout and requested by lead, got {tasks:?}"
    );

    // 3. Dropping registrations invalidates the per-turn URLs with 404.
    drop(task_turn_reg);
    drop(normal_turn_reg);

    for dead_url in [
        task_turn_url,
        normal_turn_url,
        running.url("/mcp/t/unknown-token"),
    ] {
        let (status, _, _) = mcp_post(
            &client,
            &dead_url,
            None,
            json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "tools/list",
                "params": {}
            }),
        )
        .await;
        assert_eq!(
            status,
            reqwest::StatusCode::NOT_FOUND,
            "expected 404 after registration drop for {dead_url}"
        );
    }

    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unbound_internal_listener_returns_empty_mcp_turn_registration() {
    use swarmbot_contracts::{Toolset, TurnId};
    use swarmbot_core::McpTurnRequest;

    let brain = common::TestBrain::start().await;
    let lead = brain.bot("lead");
    let provider = brain
        .swarm
        .runner_env()
        .mcp_provider
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .expect("AppState attaches provider even when internal listener is disabled");

    let reg = provider
        .register_turn(McpTurnRequest {
            bot_id: lead.id,
            conversation_id: lead.direct_conversation_id,
            turn_id: TurnId::new(),
            task_id: None,
            workspace: std::path::PathBuf::from("."),
            spool_dir: std::path::PathBuf::from(".spool"),
            toolsets: vec![Toolset::Bot],
            node_caps: None,
            in_task: false,
        })
        .expect("register_turn succeeds with empty servers");
    assert!(
        reg.servers.is_empty(),
        "without internal listener, provider returns no MCP servers"
    );

    brain.stop().await;
}
