//! SwarmBot MCP: Model Context Protocol server exposing the tool registry.
//!
//! Provides two transports:
//! 1. Standard I/O (`stdio`): for CLI harnesses like Claude CLI or ACP agents.
//! 2. Streamable HTTP: for the brain's internal listener (`/mcp`).

pub mod transport;

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData, ImageContent,
    Implementation, InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ServerCapabilities, ServerConfig, TextContent, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::Value;
use swarmbot_contracts::{
    Capabilities, InvocationId, ToolCallId, ToolCallRequest, ToolError, ToolResult, Toolset,
};
use swarmbot_tools::{ToolContext, ToolRegistry};

pub use rmcp;
pub use transport::{
    McpHttpService, StreamableHttpServerConfig, TurnMcpHandle, TurnMcpRegistry,
    create_default_http_service, create_http_service, serve_stdio, serve_stdio_with_ct,
    serve_transport, serve_transport_with_ct,
};

const ALL_TOOLSETS: [Toolset; 4] = [Toolset::Bot, Toolset::Node, Toolset::Owner, Toolset::Task];

/// Filter configuration for which tools are exposed over MCP.
#[derive(Clone, Debug, Default)]
pub struct ToolFilter {
    pub toolsets: Option<Vec<Toolset>>,
    pub node_caps: Option<Capabilities>,
    pub in_task: bool,
}

/// A factory producing a [`ToolContext`] for each tool invocation.
pub type ContextFactory = Arc<dyn Fn() -> ToolContext + Send + Sync>;

/// One `tools/call` this server answered, reported after the fact.
///
/// It exists because a backend that owns its tool loop (every ACP agent,
/// `kind = "acp"`) never hands its tool calls back to the runner: the call
/// happens here, inside the MCP server, so the runner sees a turn with
/// `response.tool_calls` empty and would otherwise record a turn that used
/// `message_bot` as a turn that used nothing. The recorder is the seam that
/// lets the brain write the same transcript entry and count the same tool call
/// it would have written for an HTTP backend — without `mcp` knowing what a
/// transcript is (the dependency rules put `core` above this crate).
#[derive(Clone, Debug)]
pub struct McpCallRecord {
    /// Whose turn this was: the template context's invocation, so the recorder
    /// can name the bot, conversation, turn and task without guessing.
    pub invocation: swarmbot_contracts::Invocation,
    pub tool: String,
    pub args: Value,
    /// The text the model will actually read, exactly as it was returned.
    pub result_text: String,
    pub is_error: bool,
    pub duration_ms: u64,
}

/// Called once per answered `tools/call`, including a call the filter refused:
/// the model asked for it, so a turn that asked for a hidden tool did make a
/// tool call, and the audit/usage trail should say so.
pub type CallRecorder = Arc<dyn Fn(McpCallRecord) + Send + Sync>;

/// MCP server handler exposing SwarmBot's [`ToolRegistry`].
#[derive(Clone)]
pub struct SwarmMcpServer {
    registry: Arc<ToolRegistry>,
    context_factory: ContextFactory,
    filter: Option<ToolFilter>,
    recorder: Option<CallRecorder>,
}

impl SwarmMcpServer {
    /// Create a new server with a template [`ToolContext`].
    pub fn new(registry: Arc<ToolRegistry>, template: ToolContext) -> Self {
        let factory: ContextFactory = Arc::new(move || {
            let mut ctx = template.clone();
            ctx.invocation.id = InvocationId::new();
            ctx
        });
        Self {
            registry,
            context_factory: factory,
            filter: None,
            recorder: None,
        }
    }

    /// Create a new server with a custom context factory.
    pub fn with_context_factory(registry: Arc<ToolRegistry>, factory: ContextFactory) -> Self {
        Self {
            registry,
            context_factory: factory,
            filter: None,
            recorder: None,
        }
    }

    /// Set a filter on which tools are visible.
    pub fn with_filter(mut self, filter: ToolFilter) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Report every answered `tools/call` to `recorder`.
    ///
    /// Without this, a backend that owns its own tool loop leaves no trace of
    /// its tool calls in the brain: the runner sees `response.tool_calls` empty
    /// (the agent used the tool, not asked us to), so the turn is stored with
    /// `usage.tool_calls == 0` and no transcript card — a bot that messaged
    /// three teammates reads as a bot that did nothing. The recorder is where
    /// that fact is handed back, because this crate cannot write a transcript
    /// itself (the dependency rules put `core` above `mcp`).
    pub fn with_recorder(mut self, recorder: CallRecorder) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// Tell the recorder about one answered call. Never panics out of this
    /// crate: a recorder that fails is the recorder's own `tracing::error!`.
    fn record(&self, call: McpCallRecord) {
        if let Some(recorder) = &self.recorder {
            recorder(call);
        }
    }

    /// Returns the active tool filter, if any.
    pub fn filter(&self) -> Option<&ToolFilter> {
        self.filter.as_ref()
    }

    /// Returns `true` when `name` is visible under the active [`ToolFilter`].
    pub fn is_tool_visible(&self, name: &str) -> bool {
        self.visible_specs().iter().any(|spec| spec.name == name)
    }

    /// Returns the specs visible according to the active filter.
    pub fn visible_specs(&self) -> Vec<swarmbot_contracts::ToolSpec> {
        if let Some(filter) = &self.filter {
            // `toolsets: None` means "do not restrict by toolset", while still
            // applying node capability and `in_task` gating.
            let toolsets = filter.toolsets.as_deref().unwrap_or(&ALL_TOOLSETS);
            return self.registry.visible_specs(
                toolsets,
                filter.node_caps.as_ref(),
                filter.in_task,
            );
        }
        self.registry.specs()
    }

    /// Execute a tool call directly without an MCP [`RequestContext`].
    pub async fn dispatch_call(
        &self,
        name: &str,
        args: Value,
    ) -> Result<CallToolResponse, ErrorData> {
        if self.filter.is_some() && !self.is_tool_visible(name) {
            let result = if self.registry.get(name).is_some() {
                ToolResult::error(format!("tool `{name}` is not available"))
            } else {
                ToolError::UnknownTool {
                    tool: name.to_string(),
                }
                .to_result()
            };
            // A refused call is still a call the model made: reported, so the
            // turn does not look tool-free and an audit reader can see what was
            // asked for. The invocation comes from a fresh context because no
            // call ran — that still names the bot, conversation and turn.
            let ctx = (self.context_factory)();
            self.record(McpCallRecord {
                invocation: ctx.invocation,
                tool: name.to_string(),
                args: args.clone(),
                result_text: result.text_only(),
                is_error: result.is_error,
                duration_ms: 0,
            });
            return Ok(tool_result_to_response(result));
        }

        let tool_name = name.to_string();
        let tool_call_id = ToolCallId::new();

        let call_req = ToolCallRequest {
            id: tool_call_id.clone(),
            name: tool_name.clone(),
            args: args.clone(),
        };

        let mut ctx = (self.context_factory)();
        ctx.invocation.id = InvocationId::new();
        ctx.invocation.tool_call_id = Some(tool_call_id);

        let invocation = ctx.invocation.clone();
        let started = Instant::now();
        let tool_result = self.registry.dispatch(&ctx, call_req).await;
        let duration_ms = started.elapsed().as_millis() as u64;
        self.record(McpCallRecord {
            invocation,
            tool: tool_name,
            args,
            result_text: tool_result.text_only(),
            is_error: tool_result.is_error,
            duration_ms,
        });
        Ok(tool_result_to_response(tool_result))
    }
}

impl fmt::Debug for SwarmMcpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SwarmMcpServer")
            .field("filter", &self.filter)
            .finish_non_exhaustive()
    }
}

fn tool_result_to_response(tool_result: ToolResult) -> CallToolResponse {
    let mut content_blocks = Vec::with_capacity(tool_result.content.len());
    for item in tool_result.content {
        match item {
            swarmbot_contracts::Content::Text { text } => {
                content_blocks.push(ContentBlock::Text(TextContent::new(text)));
            }
            swarmbot_contracts::Content::Image { mime, data_base64 } => {
                content_blocks.push(ContentBlock::Image(ImageContent::new(data_base64, mime)));
            }
        }
    }

    let res = if tool_result.is_error {
        CallToolResult::error(content_blocks)
    } else {
        CallToolResult::success(content_blocks)
    };

    CallToolResponse::Complete(res)
}

/// Convert a SwarmBot [`swarmbot_contracts::ToolSpec`] into an MCP [`Tool`].
pub fn spec_to_mcp_tool(spec: &swarmbot_contracts::ToolSpec) -> Tool {
    let schema_map = match &spec.parameters {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    Tool::new(
        spec.name.clone(),
        spec.description.clone(),
        Arc::new(schema_map),
    )
}

impl ServerHandler for SwarmMcpServer {
    fn get_info(&self) -> ServerConfig {
        let info = Implementation::new("swarmbot", env!("CARGO_PKG_VERSION"))
            .with_title("SwarmBot Tool Registry")
            .with_description("MCP server exposing SwarmBot tools");
        let capabilities = ServerCapabilities::builder().enable_tools().build();
        InitializeResult::new(capabilities)
            .with_server_info(info)
            .with_protocol_version(ProtocolVersion::default())
            .with_instructions("SwarmBot multi-agent tool registry")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self.visible_specs().iter().map(spec_to_mcp_tool).collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.visible_specs()
            .into_iter()
            .find(|s| s.name == name)
            .map(|spec| spec_to_mcp_tool(&spec))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = match request.arguments {
            Some(map) => Value::Object(map),
            None => Value::Object(serde_json::Map::new()),
        };
        self.dispatch_call(&request.name, args).await
    }
}
