//! Transports for the SwarmBot MCP server: stdio and streamable HTTP.

use std::sync::Arc;

use rmcp::service::{RoleServer, RunningService, ServerInitializeError};
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use tokio_util::sync::CancellationToken;

use crate::SwarmMcpServer;

/// Serve the MCP server over an arbitrary [`tokio::io::AsyncRead`] and [`tokio::io::AsyncWrite`].
#[allow(clippy::result_large_err)]
pub async fn serve_transport<R, W>(
    server: SwarmMcpServer,
    reader: R,
    writer: W,
) -> Result<RunningService<RoleServer, SwarmMcpServer>, ServerInitializeError>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let transport = AsyncRwTransport::new(reader, writer);
    rmcp::service::serve_server(server, transport).await
}

/// Serve the MCP server over an arbitrary reader and writer with cancellation support.
#[allow(clippy::result_large_err)]
pub async fn serve_transport_with_ct<R, W>(
    server: SwarmMcpServer,
    reader: R,
    writer: W,
    ct: CancellationToken,
) -> Result<RunningService<RoleServer, SwarmMcpServer>, ServerInitializeError>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let transport = AsyncRwTransport::new(reader, writer);
    rmcp::service::serve_server_with_ct(server, transport, ct).await
}

/// Serve the MCP server over process standard I/O (stdin and stdout).
#[allow(clippy::result_large_err)]
pub async fn serve_stdio(
    server: SwarmMcpServer,
) -> Result<RunningService<RoleServer, SwarmMcpServer>, ServerInitializeError> {
    serve_transport(server, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Serve the MCP server over process standard I/O with cancellation support.
#[allow(clippy::result_large_err)]
pub async fn serve_stdio_with_ct(
    server: SwarmMcpServer,
    ct: CancellationToken,
) -> Result<RunningService<RoleServer, SwarmMcpServer>, ServerInitializeError> {
    serve_transport_with_ct(server, tokio::io::stdin(), tokio::io::stdout(), ct).await
}

pub type McpHttpService = StreamableHttpService<SwarmMcpServer, LocalSessionManager>;

/// Create a Tower/HTTP service implementing the MCP Streamable HTTP transport.
pub fn create_http_service(
    server: SwarmMcpServer,
    session_manager: Arc<LocalSessionManager>,
    config: StreamableHttpServerConfig,
) -> McpHttpService {
    StreamableHttpService::new(move || Ok(server.clone()), session_manager, config)
}

/// Create a default Streamable HTTP service with an in-memory session manager.
pub fn create_default_http_service(server: SwarmMcpServer) -> McpHttpService {
    let session_manager = Arc::new(LocalSessionManager::default());
    let config = StreamableHttpServerConfig::default();
    create_http_service(server, session_manager, config)
}
