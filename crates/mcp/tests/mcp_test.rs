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
    async fn exec(
        &self,
        _node: &NodeId,
        _invocation: &Invocation,
        _call: NodeCall,
    ) -> Result<ToolResult, ToolError> {
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

mod per_turn_and_filter_tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rmcp::handler::server::ServerHandler;
    use rmcp::model::{CallToolResponse, ContentBlock};
    use serde_json::{Value, json};
    use swarmbot_contracts::{
        BotId, Capabilities, ConversationId, TaskId, ToolError, ToolResult, ToolSpec, Toolset,
        TurnId,
    };
    use swarmbot_mcp::{
        StreamableHttpServerConfig, SwarmMcpServer, ToolFilter, TurnMcpRegistry, serve_transport,
    };
    use swarmbot_tools::{ToolContext, ToolHandler, ToolRegistry};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio_util::sync::CancellationToken;

    use super::test_context;

    struct CountingHandler {
        name: &'static str,
        toolset: Toolset,
        needs_node: bool,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ToolHandler for CountingHandler {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.to_string(),
                description: format!("counting handler for {}", self.name),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "msg": { "type": "string" }
                    },
                }),
                toolset: self.toolset,
                read_only: true,
                needs_node: self.needs_node,
            }
        }

        async fn call(&self, _ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let msg = args.get("msg").and_then(|v| v.as_str()).unwrap_or("ok");
            Ok(ToolResult::text(format!("{}:{msg}", self.name)))
        }
    }

    struct InspectContextHandler;

    #[async_trait::async_trait]
    impl ToolHandler for InspectContextHandler {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "inspect_ctx".to_string(),
                description: "returns the active ToolContext metadata as JSON".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                }),
                toolset: Toolset::Bot,
                read_only: true,
                needs_node: false,
            }
        }

        async fn call(&self, ctx: &ToolContext, _args: Value) -> Result<ToolResult, ToolError> {
            let payload = json!({
                "invocation_id": ctx.invocation.id.as_str(),
                "tool_call_id": ctx.invocation.tool_call_id.as_ref().map(|id| id.as_str()),
                "bot_id": ctx.invocation.bot_id.as_str(),
                "conversation_id": ctx.invocation.conversation_id.as_str(),
                "turn_id": ctx.invocation.turn_id.as_ref().map(|id| id.as_str()),
                "task_id": ctx.invocation.task_id.as_ref().map(|id| id.as_str()),
                "workspace": ctx.workspace.display().to_string(),
                "spool_dir": ctx.spool_dir.display().to_string(),
            });
            Ok(ToolResult::text(payload.to_string()))
        }
    }

    struct CountingRegistry {
        registry: Arc<ToolRegistry>,
        message_calls: Arc<AtomicUsize>,
        start_task_calls: Arc<AtomicUsize>,
        ask_user_calls: Arc<AtomicUsize>,
        read_file_calls: Arc<AtomicUsize>,
        shell_calls: Arc<AtomicUsize>,
        owner_calls: Arc<AtomicUsize>,
    }

    fn build_filter_test_registry() -> CountingRegistry {
        let mut reg = ToolRegistry::new();
        let message_calls = Arc::new(AtomicUsize::new(0));
        let start_task_calls = Arc::new(AtomicUsize::new(0));
        let ask_user_calls = Arc::new(AtomicUsize::new(0));
        let read_file_calls = Arc::new(AtomicUsize::new(0));
        let shell_calls = Arc::new(AtomicUsize::new(0));
        let owner_calls = Arc::new(AtomicUsize::new(0));

        reg.register(Arc::new(CountingHandler {
            name: "message_bot",
            toolset: Toolset::Bot,
            needs_node: false,
            calls: Arc::clone(&message_calls),
        }));
        reg.register(Arc::new(CountingHandler {
            name: "start_task",
            toolset: Toolset::Task,
            needs_node: false,
            calls: Arc::clone(&start_task_calls),
        }));
        reg.register(Arc::new(CountingHandler {
            name: "ask_user",
            toolset: Toolset::Owner,
            needs_node: false,
            calls: Arc::clone(&ask_user_calls),
        }));
        reg.register(Arc::new(CountingHandler {
            name: "owner_ping",
            toolset: Toolset::Owner,
            needs_node: false,
            calls: Arc::clone(&owner_calls),
        }));
        reg.register(Arc::new(CountingHandler {
            name: "read_file",
            toolset: Toolset::Node,
            needs_node: true,
            calls: Arc::clone(&read_file_calls),
        }));
        reg.register(Arc::new(CountingHandler {
            name: "shell",
            toolset: Toolset::Node,
            needs_node: true,
            calls: Arc::clone(&shell_calls),
        }));
        reg.register(Arc::new(InspectContextHandler));

        CountingRegistry {
            registry: Arc::new(reg),
            message_calls,
            start_task_calls,
            ask_user_calls,
            read_file_calls,
            shell_calls,
            owner_calls,
        }
    }

    fn turn_context(
        bot_id: BotId,
        conversation_id: ConversationId,
        turn_id: TurnId,
        task_id: Option<TaskId>,
        workspace: &str,
        spool_dir: &str,
    ) -> ToolContext {
        let mut ctx = test_context();
        ctx.invocation.bot_id = bot_id;
        ctx.invocation.conversation_id = conversation_id;
        ctx.invocation.turn_id = Some(turn_id);
        ctx.invocation.task_id = task_id;
        ctx.workspace = PathBuf::from(workspace);
        ctx.spool_dir = PathBuf::from(spool_dir);
        ctx
    }

    fn response_text_and_error(resp: CallToolResponse) -> (bool, String) {
        match resp {
            CallToolResponse::Complete(res) => {
                let is_error = res.is_error.unwrap_or(false);
                let text = match res.content.first() {
                    Some(ContentBlock::Text(t)) => t.text.clone(),
                    _ => String::new(),
                };
                (is_error, text)
            }
            other => panic!("unexpected non-complete response: {other:?}"),
        }
    }

    struct DuplexMcpSession {
        write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
        lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
        next_id: i64,
        service: rmcp::service::RunningService<rmcp::service::RoleServer, SwarmMcpServer>,
    }

    impl DuplexMcpSession {
        async fn start(server: SwarmMcpServer) -> Self {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (server_read, server_write) = tokio::io::split(server_io);
            let (client_read, mut client_write) = tokio::io::split(client_io);

            // `serve_transport` drives the MCP `initialize` -> `notifications/initialized`
            // handshake before resolving `RunningService`, so run it concurrently while the
            // client writes the handshake frames.
            let server_task =
                tokio::spawn(
                    async move { serve_transport(server, server_read, server_write).await },
                );

            let mut lines = BufReader::new(client_read).lines();

            let init_req = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "test-client", "version": "0.1" }
                }
            });
            client_write
                .write_all(format!("{init_req}\n").as_bytes())
                .await
                .expect("write init");
            let init_line = lines
                .next_line()
                .await
                .expect("read init")
                .expect("init line");
            let init_res: Value = serde_json::from_str(&init_line).expect("init json");
            assert_eq!(init_res["id"], 1);
            assert!(init_res["result"].is_object());

            let initialized = json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            });
            client_write
                .write_all(format!("{initialized}\n").as_bytes())
                .await
                .expect("write initialized");

            let service = server_task
                .await
                .expect("server handshake task joined")
                .expect("serve_transport initializes");

            Self {
                write: client_write,
                lines,
                next_id: 2,
                service,
            }
        }

        async fn list_tools(&mut self) -> Vec<String> {
            let id = self.next_id;
            self.next_id += 1;
            let req = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/list",
                "params": {}
            });
            self.write
                .write_all(format!("{req}\n").as_bytes())
                .await
                .expect("write tools/list");
            let line = self
                .lines
                .next_line()
                .await
                .expect("read tools/list")
                .expect("line present");
            let res: Value = serde_json::from_str(&line).expect("json");
            assert_eq!(res["id"], id);
            res["result"]["tools"]
                .as_array()
                .expect("tools array")
                .iter()
                .filter_map(|t| t["name"].as_str().map(str::to_string))
                .collect()
        }

        async fn call_tool(&mut self, name: &str, arguments: Value) -> (bool, String) {
            let id = self.next_id;
            self.next_id += 1;
            let req = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": name,
                    "arguments": arguments,
                }
            });
            self.write
                .write_all(format!("{req}\n").as_bytes())
                .await
                .expect("write tools/call");
            let line = self
                .lines
                .next_line()
                .await
                .expect("read tools/call")
                .expect("line present");
            let res: Value = serde_json::from_str(&line).expect("json");
            assert_eq!(res["id"], id);
            let is_error = res["result"]["isError"].as_bool().unwrap_or(false);
            let text = res["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            (is_error, text)
        }

        async fn close(self) {
            drop(self.write);
            drop(self.lines);
            let _ = self.service.cancel().await;
        }
    }

    #[tokio::test]
    async fn filter_with_toolsets_hides_and_refuses_restricted_tools_across_handlers() {
        let built = build_filter_test_registry();
        let server = SwarmMcpServer::new(Arc::clone(&built.registry), test_context()).with_filter(
            ToolFilter {
                toolsets: Some(vec![Toolset::Bot]),
                node_caps: None,
                in_task: false,
            },
        );

        let visible: Vec<String> = server.visible_specs().into_iter().map(|s| s.name).collect();
        assert!(visible.contains(&"message_bot".to_string()));
        assert!(visible.contains(&"inspect_ctx".to_string()));
        assert!(!visible.contains(&"shell".to_string()));
        assert!(!visible.contains(&"read_file".to_string()));
        assert!(!visible.contains(&"start_task".to_string()));
        assert!(!visible.contains(&"ask_user".to_string()));

        assert!(server.get_tool("message_bot").is_some());
        assert!(server.get_tool("shell").is_none());
        assert!(server.get_tool("start_task").is_none());
        assert!(server.get_tool("ask_user").is_none());

        // Direct dispatch_call succeeds for visible tool and refuses filtered tools
        // without ever invoking their handlers.
        let (is_err, ok_text) = response_text_and_error(
            server
                .dispatch_call("message_bot", json!({ "msg": "hi" }))
                .await
                .expect("dispatch message_bot"),
        );
        assert!(!is_err);
        assert_eq!(ok_text, "message_bot:hi");
        assert_eq!(built.message_calls.load(Ordering::SeqCst), 1);

        for hidden in ["shell", "read_file", "start_task", "ask_user", "owner_ping"] {
            let (is_err, err_text) = response_text_and_error(
                server
                    .dispatch_call(hidden, json!({}))
                    .await
                    .expect("refusal is returned as tool result"),
            );
            assert!(is_err, "{hidden} must fail");
            assert!(
                err_text.starts_with("ERROR: ") && err_text.contains("is not available"),
                "unexpected error for {hidden}: {err_text}"
            );
        }

        // Unknown tools still report unknown-tool cleanly.
        let (is_err, unknown_text) = response_text_and_error(
            server
                .dispatch_call("does_not_exist", json!({}))
                .await
                .expect("unknown tool returns tool error"),
        );
        assert!(is_err);
        assert!(unknown_text.contains("unknown tool"));

        assert_eq!(built.shell_calls.load(Ordering::SeqCst), 0);
        assert_eq!(built.read_file_calls.load(Ordering::SeqCst), 0);
        assert_eq!(built.start_task_calls.load(Ordering::SeqCst), 0);
        assert_eq!(built.ask_user_calls.load(Ordering::SeqCst), 0);
        assert_eq!(built.owner_calls.load(Ordering::SeqCst), 0);

        // Verify MCP wire protocol tools/list and tools/call enforce the exact same filter.
        let mut session = DuplexMcpSession::start(server).await;
        let listed = session.list_tools().await;
        assert!(listed.contains(&"message_bot".to_string()));
        assert!(!listed.contains(&"shell".to_string()));
        assert!(!listed.contains(&"start_task".to_string()));

        let (is_err, msg_res) = session
            .call_tool("message_bot", json!({ "msg": "wire" }))
            .await;
        assert!(!is_err);
        assert_eq!(msg_res, "message_bot:wire");

        let (is_err, denied_res) = session.call_tool("shell", json!({})).await;
        assert!(is_err);
        assert!(denied_res.contains("is not available"));
        assert_eq!(built.shell_calls.load(Ordering::SeqCst), 0);
        assert_eq!(built.message_calls.load(Ordering::SeqCst), 2);
        session.close().await;
    }

    #[tokio::test]
    async fn filter_with_none_toolsets_still_enforces_in_task_and_node_caps() {
        let built = build_filter_test_registry();
        let server = SwarmMcpServer::new(Arc::clone(&built.registry), test_context()).with_filter(
            ToolFilter {
                toolsets: None,
                node_caps: Some(Capabilities::new(["fs@1"])),
                in_task: true,
            },
        );

        let visible: Vec<String> = server.visible_specs().into_iter().map(|s| s.name).collect();
        // `in_task: true` hides `start_task` and `ask_user`, but keeps other Bot/Owner tools.
        assert!(!visible.contains(&"start_task".to_string()));
        assert!(!visible.contains(&"ask_user".to_string()));
        assert!(visible.contains(&"message_bot".to_string()));
        assert!(visible.contains(&"owner_ping".to_string()));
        // `node_caps: Some(["fs@1"])` keeps `read_file` and hides `shell`.
        assert!(visible.contains(&"read_file".to_string()));
        assert!(!visible.contains(&"shell".to_string()));

        assert!(server.get_tool("start_task").is_none());
        assert!(server.get_tool("ask_user").is_none());
        assert!(server.get_tool("shell").is_none());
        assert!(server.get_tool("read_file").is_some());
        assert!(server.get_tool("message_bot").is_some());

        for hidden in ["start_task", "ask_user", "shell"] {
            let (is_err, err_text) = response_text_and_error(
                server
                    .dispatch_call(hidden, json!({}))
                    .await
                    .expect("refusal result"),
            );
            assert!(is_err, "{hidden} must be refused");
            assert!(err_text.contains("is not available"), "{err_text}");
        }

        let (is_err, rf_text) = response_text_and_error(
            server
                .dispatch_call("read_file", json!({ "msg": "allowed" }))
                .await
                .expect("read_file allowed"),
        );
        assert!(!is_err);
        assert_eq!(rf_text, "read_file:allowed");
        assert_eq!(built.read_file_calls.load(Ordering::SeqCst), 1);
        assert_eq!(built.start_task_calls.load(Ordering::SeqCst), 0);
        assert_eq!(built.ask_user_calls.load(Ordering::SeqCst), 0);
        assert_eq!(built.shell_calls.load(Ordering::SeqCst), 0);

        // Empty toolset slice hides and refuses everything.
        let empty_server = SwarmMcpServer::new(Arc::clone(&built.registry), test_context())
            .with_filter(ToolFilter {
                toolsets: Some(vec![]),
                node_caps: None,
                in_task: false,
            });
        assert!(empty_server.visible_specs().is_empty());
        assert!(empty_server.get_tool("message_bot").is_none());
        let (is_err, empty_err) = response_text_and_error(
            empty_server
                .dispatch_call("message_bot", json!({}))
                .await
                .expect("refusal result"),
        );
        assert!(is_err);
        assert!(empty_err.contains("is not available"));
        assert_eq!(built.message_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn turn_registry_isolates_per_turn_contexts_and_filters() {
        let built = build_filter_test_registry();
        let turn_registry = TurnMcpRegistry::new();
        assert!(turn_registry.is_empty());

        let bot_a = BotId::new();
        let conv_a = ConversationId::new();
        let turn_a = TurnId::new();
        let task_a = TaskId::new();
        let ctx_a = turn_context(
            bot_a.clone(),
            conv_a.clone(),
            turn_a.clone(),
            Some(task_a.clone()),
            "/tmp/ws-alpha",
            "/tmp/spool-alpha",
        );

        let bot_b = BotId::new();
        let conv_b = ConversationId::new();
        let turn_b = TurnId::new();
        let ctx_b = turn_context(
            bot_b.clone(),
            conv_b.clone(),
            turn_b.clone(),
            None,
            "/tmp/ws-beta",
            "/tmp/spool-beta",
        );

        let server_a =
            SwarmMcpServer::new(Arc::clone(&built.registry), ctx_a).with_filter(ToolFilter {
                toolsets: Some(vec![Toolset::Bot, Toolset::Node]),
                node_caps: Some(Capabilities::new(["fs@1"])),
                in_task: true,
            });
        let server_b =
            SwarmMcpServer::new(Arc::clone(&built.registry), ctx_b).with_filter(ToolFilter {
                toolsets: Some(vec![Toolset::Bot]),
                node_caps: None,
                in_task: false,
            });

        let token_a = turn_registry.register(server_a);
        let token_b = turn_registry.register(server_b);
        assert_ne!(token_a, token_b);
        assert_eq!(turn_registry.len(), 2);
        assert!(!turn_registry.is_empty());
        assert!(turn_registry.contains(&token_a));
        assert!(turn_registry.contains(&token_b));

        let svc_a = turn_registry.get_service(&token_a).expect("svc_a");
        let svc_b = turn_registry.get_service(&token_b).expect("svc_b");
        assert!(!Arc::ptr_eq(&svc_a, &svc_b));
        assert!(turn_registry.get_service("unknown-token").is_none());
        assert!(turn_registry.get_server("unknown-token").is_none());

        let srv_a = turn_registry.get_server(&token_a).expect("srv_a");
        let srv_b = turn_registry.get_server(&token_b).expect("srv_b");

        let (mut sess_a, mut sess_b) = tokio::join!(
            DuplexMcpSession::start(srv_a),
            DuplexMcpSession::start(srv_b)
        );

        let (tools_a, tools_b) = tokio::join!(sess_a.list_tools(), sess_b.list_tools());
        assert!(tools_a.contains(&"read_file".to_string()));
        assert!(!tools_a.contains(&"shell".to_string()));
        assert!(!tools_b.contains(&"read_file".to_string()));
        assert!(tools_b.contains(&"inspect_ctx".to_string()));

        // Run tools/call concurrently against both turns and verify full ToolContext isolation.
        let ((err_a1, body_a1), (err_b, body_b)) = tokio::join!(
            sess_a.call_tool("inspect_ctx", json!({})),
            sess_b.call_tool("inspect_ctx", json!({}))
        );
        assert!(!err_a1);
        assert!(!err_b);

        let (err_a2, body_a2) = sess_a.call_tool("inspect_ctx", json!({})).await;
        assert!(!err_a2);

        let info_a1: Value = serde_json::from_str(&body_a1).expect("json a1");
        let info_a2: Value = serde_json::from_str(&body_a2).expect("json a2");
        let info_b: Value = serde_json::from_str(&body_b).expect("json b");

        assert_eq!(info_a1["bot_id"], bot_a.as_str());
        assert_eq!(info_a1["conversation_id"], conv_a.as_str());
        assert_eq!(info_a1["turn_id"], turn_a.as_str());
        assert_eq!(info_a1["task_id"], task_a.as_str());
        assert_eq!(info_a1["workspace"], "/tmp/ws-alpha");
        assert_eq!(info_a1["spool_dir"], "/tmp/spool-alpha");

        assert_eq!(info_b["bot_id"], bot_b.as_str());
        assert_eq!(info_b["conversation_id"], conv_b.as_str());
        assert_eq!(info_b["turn_id"], turn_b.as_str());
        assert!(info_b["task_id"].is_null());
        assert_eq!(info_b["workspace"], "/tmp/ws-beta");
        assert_eq!(info_b["spool_dir"], "/tmp/spool-beta");

        // Every call inside a turn also allocates a fresh InvocationId & ToolCallId.
        assert_ne!(info_a1["invocation_id"], info_a2["invocation_id"]);
        assert_ne!(info_a1["tool_call_id"], info_a2["tool_call_id"]);

        // Turn B cannot invoke read_file because Turn B only allows Toolset::Bot.
        let (err_rf_b, msg_rf_b) = sess_b.call_tool("read_file", json!({})).await;
        assert!(err_rf_b);
        assert!(msg_rf_b.contains("is not available"));
        assert_eq!(built.read_file_calls.load(Ordering::SeqCst), 0);

        // Turn A can invoke read_file.
        let (err_rf_a, msg_rf_a) = sess_a
            .call_tool("read_file", json!({ "msg": "alpha-file" }))
            .await;
        assert!(!err_rf_a);
        assert_eq!(msg_rf_a, "read_file:alpha-file");
        assert_eq!(built.read_file_calls.load(Ordering::SeqCst), 1);

        sess_a.close().await;
        sess_b.close().await;
    }

    #[test]
    fn unregister_and_raii_handle_remove_tokens_cancel_only_target_and_redact_debug() {
        let built = build_filter_test_registry();
        let parent_ct = CancellationToken::new();
        let mut config = StreamableHttpServerConfig::default();
        config.cancellation_token = parent_ct.clone();
        config.json_response = true;

        let turn_registry = TurnMcpRegistry::with_config(config);

        let token_a = turn_registry.register(SwarmMcpServer::new(
            Arc::clone(&built.registry),
            test_context(),
        ));
        let token_b = turn_registry.register(SwarmMcpServer::new(
            Arc::clone(&built.registry),
            test_context(),
        ));

        let svc_a = turn_registry.get_service(&token_a).expect("svc_a");
        let svc_b = turn_registry.get_service(&token_b).expect("svc_b");
        assert!(svc_a.config.json_response);
        assert!(!svc_a.config.cancellation_token.is_cancelled());
        assert!(!svc_b.config.cancellation_token.is_cancelled());

        // Unregistering token_a cancels svc_a and removes lookups, leaving token_b untouched.
        let removed = turn_registry.unregister(&token_a);
        assert!(removed.is_some());
        assert!(turn_registry.unregister(&token_a).is_none());
        assert!(!turn_registry.contains(&token_a));
        assert!(turn_registry.get_service(&token_a).is_none());
        assert!(turn_registry.get_server(&token_a).is_none());
        assert_eq!(turn_registry.len(), 1);
        assert!(svc_a.config.cancellation_token.is_cancelled());
        assert!(!svc_b.config.cancellation_token.is_cancelled());
        assert!(turn_registry.get_service(&token_b).is_some());

        // RAII handle registers, exposes service, redacts Debug, and unregisters on drop.
        let handle = turn_registry.register_handle(SwarmMcpServer::new(
            Arc::clone(&built.registry),
            test_context(),
        ));
        let handle_token = handle.token().to_string();
        let handle_svc = Arc::clone(handle.service());
        assert!(turn_registry.contains(&handle_token));
        assert_eq!(turn_registry.len(), 2);

        let handle_dbg = format!("{handle:?}");
        assert!(!handle_dbg.contains(&handle_token));
        assert!(handle_dbg.contains("<redacted>"));

        let reg_dbg = format!("{turn_registry:?}");
        assert!(!reg_dbg.contains(&handle_token));
        assert!(!reg_dbg.contains(&token_b));

        drop(handle);
        assert!(!turn_registry.contains(&handle_token));
        assert!(turn_registry.get_service(&handle_token).is_none());
        assert!(handle_svc.config.cancellation_token.is_cancelled());
        assert!(!svc_b.config.cancellation_token.is_cancelled());
        assert_eq!(turn_registry.len(), 1);

        // Explicit TurnMcpHandle::unregister also removes immediately.
        let handle2 = turn_registry.register_handle(SwarmMcpServer::new(
            Arc::clone(&built.registry),
            test_context(),
        ));
        let handle2_token = handle2.token().to_string();
        let handle2_svc = Arc::clone(handle2.service());
        assert!(handle2.unregister().is_some());
        assert!(!turn_registry.contains(&handle2_token));
        assert!(handle2_svc.config.cancellation_token.is_cancelled());

        // Parent cancellation cascades to active child services, and clear empties the registry.
        parent_ct.cancel();
        assert!(svc_b.config.cancellation_token.is_cancelled());
        turn_registry.clear();
        assert!(turn_registry.is_empty());
        assert!(turn_registry.get_service(&token_b).is_none());
    }
}
