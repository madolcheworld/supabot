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

        Self {
            internal_addr,
            _tempdir: dir,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.internal_addr)
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
