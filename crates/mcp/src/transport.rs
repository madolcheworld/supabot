//! Transports for the SwarmBot MCP server: stdio and streamable HTTP.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rmcp::service::{RoleServer, RunningService, ServerInitializeError};
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
pub use rmcp::transport::streamable_http_server::tower::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;
use sha2::{Digest, Sha256};
use swarmbot_contracts::{InvocationId, ToolCallId};
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

/// Per-turn registry mapping opaque turn tokens to isolated [`McpHttpService`]
/// instances (each bound to its own [`SwarmMcpServer`] with its own
/// `ToolContext`, `ToolFilter`, session manager, and child cancellation token).
///
/// The brain's HTTP router mounts `/mcp/t/{token}` against
/// [`TurnMcpRegistry::get_service`] so each bot turn hits only its own context.
#[derive(Clone)]
pub struct TurnMcpRegistry {
    inner: Arc<TurnMcpRegistryInner>,
}

struct TurnMcpRegistryInner {
    entries: RwLock<HashMap<String, TurnMcpEntry>>,
    base_config: StreamableHttpServerConfig,
    next_seq: AtomicU64,
}

struct TurnMcpEntry {
    server: SwarmMcpServer,
    service: Arc<McpHttpService>,
    cancellation_token: CancellationToken,
}

impl TurnMcpRegistry {
    /// Create a new empty per-turn MCP registry with default HTTP config.
    pub fn new() -> Self {
        Self::with_config(StreamableHttpServerConfig::default())
    }

    /// Create a new empty per-turn MCP registry whose services inherit
    /// `base_config` (each turn receives a child `cancellation_token` so
    /// unregistering one turn cancels only that turn's streams).
    pub fn with_config(base_config: StreamableHttpServerConfig) -> Self {
        Self {
            inner: Arc::new(TurnMcpRegistryInner {
                entries: RwLock::new(HashMap::new()),
                base_config,
                next_seq: AtomicU64::new(1),
            }),
        }
    }

    /// Register a turn-scoped [`SwarmMcpServer`] and return its opaque turn token.
    pub fn register(&self, server: SwarmMcpServer) -> String {
        self.register_entry(server).0
    }

    /// Register a turn-scoped [`SwarmMcpServer`] and return an RAII
    /// [`TurnMcpHandle`] that automatically unregisters the token on drop.
    pub fn register_handle(&self, server: SwarmMcpServer) -> TurnMcpHandle {
        let (token, service) = self.register_entry(server);
        TurnMcpHandle {
            registry: self.clone(),
            token,
            service,
            active: true,
        }
    }

    fn register_entry(&self, server: SwarmMcpServer) -> (String, Arc<McpHttpService>) {
        let turn_ct = self.inner.base_config.cancellation_token.child_token();
        let mut config = self.inner.base_config.clone();
        config.cancellation_token = turn_ct.clone();

        let session_manager = Arc::new(LocalSessionManager::default());
        let service = Arc::new(create_http_service(server.clone(), session_manager, config));
        let entry = TurnMcpEntry {
            server,
            service: Arc::clone(&service),
            cancellation_token: turn_ct,
        };

        let mut entries = self
            .inner
            .entries
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let mut token = self.next_token();
        while entries.contains_key(&token) {
            token = self.next_token();
        }
        entries.insert(token.clone(), entry);
        (token, service)
    }

    fn next_token(&self) -> String {
        let seq = self.inner.next_seq.fetch_add(1, Ordering::Relaxed);
        let inv = InvocationId::new();
        let call = ToolCallId::new();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();

        let mut hasher = Sha256::new();
        hasher.update(inv.as_str().as_bytes());
        hasher.update(b":");
        hasher.update(call.as_str().as_bytes());
        hasher.update(b":");
        hasher.update(seq.to_le_bytes());
        hasher.update(nanos.to_le_bytes());
        hasher.update(pid.to_le_bytes());
        hex::encode(hasher.finalize())
    }

    /// Unregister `token`, cancelling its per-turn [`McpHttpService`] token so
    /// active SSE streams / sessions shut down cleanly without affecting other turns.
    pub fn unregister(&self, token: &str) -> Option<Arc<McpHttpService>> {
        let removed = {
            let mut entries = self
                .inner
                .entries
                .write()
                .unwrap_or_else(|e| e.into_inner());
            entries.remove(token)
        };
        if let Some(entry) = removed {
            entry.cancellation_token.cancel();
            Some(entry.service)
        } else {
            None
        }
    }

    /// Look up the [`McpHttpService`] bound to `token`, if registered.
    pub fn get_service(&self, token: &str) -> Option<Arc<McpHttpService>> {
        let entries = self.inner.entries.read().unwrap_or_else(|e| e.into_inner());
        entries.get(token).map(|entry| Arc::clone(&entry.service))
    }

    /// Look up a clone of the [`SwarmMcpServer`] bound to `token`, if registered.
    pub fn get_server(&self, token: &str) -> Option<SwarmMcpServer> {
        let entries = self.inner.entries.read().unwrap_or_else(|e| e.into_inner());
        entries.get(token).map(|entry| entry.server.clone())
    }

    /// Returns `true` if `token` is currently registered.
    pub fn contains(&self, token: &str) -> bool {
        let entries = self.inner.entries.read().unwrap_or_else(|e| e.into_inner());
        entries.contains_key(token)
    }

    /// Number of currently registered turn services.
    pub fn len(&self) -> usize {
        let entries = self.inner.entries.read().unwrap_or_else(|e| e.into_inner());
        entries.len()
    }

    /// Returns `true` if no turn services are currently registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove all registered turns and cancel each one's service token.
    pub fn clear(&self) {
        let removed: Vec<TurnMcpEntry> = {
            let mut entries = self
                .inner
                .entries
                .write()
                .unwrap_or_else(|e| e.into_inner());
            entries.drain().map(|(_, entry)| entry).collect()
        };
        for entry in removed {
            entry.cancellation_token.cancel();
        }
    }
}

impl Default for TurnMcpRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for TurnMcpRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnMcpRegistry")
            .field("active_turns", &self.len())
            .finish()
    }
}

/// RAII handle for a turn registered in a [`TurnMcpRegistry`].
///
/// Dropping the handle automatically unregisters its token and cancels that
/// turn's HTTP service streams unless [`TurnMcpHandle::unregister`] was already
/// called.
pub struct TurnMcpHandle {
    registry: TurnMcpRegistry,
    token: String,
    service: Arc<McpHttpService>,
    active: bool,
}

impl TurnMcpHandle {
    /// The opaque per-turn token registered in the registry.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The per-turn [`McpHttpService`] bound to this token.
    pub fn service(&self) -> &Arc<McpHttpService> {
        &self.service
    }

    /// Explicitly unregister this turn immediately rather than waiting for drop.
    pub fn unregister(mut self) -> Option<Arc<McpHttpService>> {
        self.active = false;
        self.registry.unregister(&self.token)
    }
}

impl Drop for TurnMcpHandle {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            self.registry.unregister(&self.token);
        }
    }
}

impl fmt::Debug for TurnMcpHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Turn tokens act as bearer-like route keys; redact them in logs/Debug.
        f.debug_struct("TurnMcpHandle")
            .field("token", &"<redacted>")
            .field("active", &self.active)
            .finish()
    }
}
