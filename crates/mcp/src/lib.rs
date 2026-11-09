//! SwarmBot MCP: Model Context Protocol server exposing the tool registry.
//!
//! Provides two transports:
//! 1. Standard I/O (`stdio`): for CLI harnesses like Claude CLI or ACP agents.
//! 2. Streamable HTTP: for the brain's internal listener (`/mcp`).

pub mod transport;

use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData, ImageContent,
    Implementation, InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ServerCapabilities, ServerConfig, TextContent, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::Value;
use swarmbot_contracts::{Capabilities, InvocationId, ToolCallId, ToolCallRequest, Toolset};
use swarmbot_tools::{ToolContext, ToolRegistry};

pub use rmcp;
pub use transport::{
    McpHttpService, create_default_http_service, create_http_service, serve_stdio,
    serve_stdio_with_ct, serve_transport, serve_transport_with_ct,
};

/// Filter configuration for which tools are exposed over MCP.
#[derive(Clone, Debug, Default)]
pub struct ToolFilter {
    pub toolsets: Option<Vec<Toolset>>,
    pub node_caps: Option<Capabilities>,
    pub in_task: bool,
}

/// A factory producing a [`ToolContext`] for each tool invocation.
pub type ContextFactory = Arc<dyn Fn() -> ToolContext + Send + Sync>;

/// MCP server handler exposing SwarmBot's [`ToolRegistry`].
#[derive(Clone)]
pub struct SwarmMcpServer {
    registry: Arc<ToolRegistry>,
    context_factory: ContextFactory,
    filter: Option<ToolFilter>,
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
        }
    }

    /// Create a new server with a custom context factory.
    pub fn with_context_factory(registry: Arc<ToolRegistry>, factory: ContextFactory) -> Self {
        Self {
            registry,
            context_factory: factory,
            filter: None,
        }
    }

    /// Set a filter on which tools are visible.
    pub fn with_filter(mut self, filter: ToolFilter) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Returns the specs visible according to the active filter.
    pub fn visible_specs(&self) -> Vec<swarmbot_contracts::ToolSpec> {
        if let Some(filter) = &self.filter
            && let Some(toolsets) = &filter.toolsets
        {
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
        let tool_name = name.to_string();
        let tool_call_id = ToolCallId::new();

        let call_req = ToolCallRequest {
            id: tool_call_id.clone(),
            name: tool_name,
            args,
        };

        let mut ctx = (self.context_factory)();
        ctx.invocation.id = InvocationId::new();
        ctx.invocation.tool_call_id = Some(tool_call_id);

        let tool_result = self.registry.dispatch(&ctx, call_req).await;

        let mut content_blocks = Vec::new();
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

        Ok(CallToolResponse::Complete(res))
    }
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
