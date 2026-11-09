use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use serde_json::{Value, json};
use swarmbot_contracts::{
    BotId, ConversationId, DomainError, Invocation, NodeId, ToolError, ToolResult, ToolSpec,
    Toolset,
};
use swarmbot_mcp::{SwarmMcpServer, ToolFilter};
use swarmbot_tools::{
    ApprovalAsk, ApprovalGate, NodeCall, NodeExecutor, ToolContext, ToolHandler, ToolRegistry,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct DummyApproval;
#[async_trait::async_trait]
impl ApprovalGate for DummyApproval {
    async fn request(&self, _ctx: &ToolContext, _ask: ApprovalAsk) -> Result<bool, DomainError> {
        Ok(true)
    }
}

struct DummyExecutor;
#[async_trait::async_trait]
impl NodeExecutor for DummyExecutor {
    async fn exec(&self, _node: &NodeId, _call: NodeCall) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text("dummy exec"))
    }
}

struct EchoHandler;
#[async_trait::async_trait]
impl ToolHandler for EchoHandler {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo_tool".to_string(),
            description: "Echoes input back".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "msg": { "type": "string" }
                },
                "required": ["msg"]
            }),
            toolset: Toolset::Node,
            read_only: false,
            needs_node: false,
        }
    }

    async fn call(&self, _ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let msg = args.get("msg").and_then(Value::as_str).unwrap_or("");
        Ok(ToolResult::text(format!("echo: {msg}")))
    }
}

struct BotGreetingHandler;
#[async_trait::async_trait]
impl ToolHandler for BotGreetingHandler {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bot_greet".to_string(),
            description: "Greets a bot".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" }
                },
                "required": ["name"]
            }),
            toolset: Toolset::Bot,
            read_only: true,
            needs_node: false,
        }
    }

    async fn call(&self, _ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let name = args.get("name").and_then(Value::as_str).unwrap_or("");
        Ok(ToolResult::text(format!("hello {name}")))
    }
}

fn test_context() -> ToolContext {
    ToolContext::local(
        Invocation::new(BotId::new(), ConversationId::new()),
        PathBuf::from("/tmp/test-workspace"),
        Arc::new(DummyApproval),
        Arc::new(DummyExecutor),
        PathBuf::from("/tmp/test-spool"),
    )
}

fn build_test_registry() -> Arc<ToolRegistry> {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoHandler));
    reg.register(Arc::new(BotGreetingHandler));
    Arc::new(reg)
}

#[tokio::test]
async fn server_info_announces_capabilities_and_metadata() {
    let registry = build_test_registry();
    let server = SwarmMcpServer::new(registry, test_context());
    let info = server.get_info();

    assert_eq!(info.server_info.name, "swarmbot");
    assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
    assert!(info.capabilities.tools.is_some());
}

#[tokio::test]
async fn visible_specs_and_tool_filtering() {
    let registry = build_test_registry();

    // Without filter: all specs
    let server = SwarmMcpServer::new(Arc::clone(&registry), test_context());
    assert_eq!(server.visible_specs().len(), 2);

    // With filter for Node toolset only
    let filter = ToolFilter {
        toolsets: Some(vec![Toolset::Node]),
        node_caps: None,
        in_task: false,
    };
    let filtered_server = server.with_filter(filter);
    let specs = filtered_server.visible_specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, "echo_tool");
}

#[tokio::test]
async fn get_tool_finds_single_tool() {
    let registry = build_test_registry();
    let server = SwarmMcpServer::new(registry, test_context());

    let tool = server.get_tool("echo_tool").expect("finds tool");
    assert_eq!(tool.name.as_ref(), "echo_tool");
    assert_eq!(tool.description.as_deref(), Some("Echoes input back"));

    assert!(server.get_tool("nonexistent").is_none());
}

#[tokio::test]
async fn dispatch_call_executes_tool_successfully() {
    let registry = build_test_registry();
    let server = SwarmMcpServer::new(registry, test_context());

    let resp = server
        .dispatch_call("echo_tool", json!({ "msg": "world" }))
        .await
        .expect("dispatch_call succeeds");

    match resp {
        rmcp::model::CallToolResponse::Complete(res) => {
            assert_ne!(res.is_error, Some(true));
            assert_eq!(res.content.len(), 1);
            match &res.content[0] {
                rmcp::model::ContentBlock::Text(t) => {
                    assert_eq!(t.text, "echo: world");
                }
                _ => panic!("expected text content"),
            }
        }
        _ => panic!("expected complete response"),
    }
}

#[tokio::test]
async fn dispatch_call_returns_error_on_invalid_args() {
    let registry = build_test_registry();
    let server = SwarmMcpServer::new(registry, test_context());

    // Missing required "msg"
    let resp = server
        .dispatch_call("echo_tool", json!({}))
        .await
        .expect("dispatch_call succeeds");

    match resp {
        rmcp::model::CallToolResponse::Complete(res) => {
            assert_eq!(res.is_error, Some(true));
            match &res.content[0] {
                rmcp::model::ContentBlock::Text(t) => {
                    assert!(
                        t.text.starts_with("ERROR: invalid arguments"),
                        "text: {}",
                        t.text
                    );
                }
                _ => panic!("expected text content"),
            }
        }
        _ => panic!("expected complete response"),
    }
}

#[tokio::test]
async fn duplex_transport_stdio_protocol_handshake_and_dispatch() {
    let registry = build_test_registry();
    let server = SwarmMcpServer::new(registry, test_context());

    // Duplex pair: (client_side, server_side)
    let (client_io, server_io) = tokio::io::duplex(4096);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_lines = BufReader::new(client_read).lines();

    let server_task = tokio::spawn(async move {
        swarmbot_mcp::serve_transport(server, server_read, server_write).await
    });

    // 1. Send initialize
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test-client", "version": "1.0" }
        }
    });
    client_write
        .write_all(format!("{init_req}\n").as_bytes())
        .await
        .expect("write init");

    let init_line = client_lines
        .next_line()
        .await
        .expect("read line")
        .expect("line present");
    let init_res: Value = serde_json::from_str(&init_line).expect("valid json");
    assert_eq!(init_res["id"], 1);
    assert_eq!(init_res["result"]["serverInfo"]["name"], "swarmbot");

    // Wait for the server task to finish initializing
    let service = server_task
        .await
        .expect("task completes")
        .expect("serve_transport initializes");

    // 2. Send initialized notification
    let initialized_notif = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    client_write
        .write_all(format!("{initialized_notif}\n").as_bytes())
        .await
        .expect("write notif");

    // 3. List tools
    let list_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    client_write
        .write_all(format!("{list_req}\n").as_bytes())
        .await
        .expect("write list");

    let list_line = client_lines
        .next_line()
        .await
        .expect("read line")
        .expect("line present");
    let list_res: Value = serde_json::from_str(&list_line).expect("valid json");
    assert_eq!(list_res["id"], 2);
    let tools = list_res["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2);

    // 4. Call tool
    let call_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "echo_tool",
            "arguments": { "msg": "hello over duplex" }
        }
    });
    client_write
        .write_all(format!("{call_req}\n").as_bytes())
        .await
        .expect("write call");

    let call_line = client_lines
        .next_line()
        .await
        .expect("read line")
        .expect("line present");
    let call_res: Value = serde_json::from_str(&call_line).expect("valid json");
    assert_eq!(call_res["id"], 3);
    let text = call_res["result"]["content"][0]["text"]
        .as_str()
        .expect("text string");
    assert_eq!(text, "echo: hello over duplex");

    // Drop client side and verify clean shutdown
    drop(client_write);
    drop(client_lines);
    let _ = service.cancel().await;
}
