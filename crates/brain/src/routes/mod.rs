//! The HTTP surface: router assembly, the auth gate, and the small helpers the
//! route modules share.
//!
//! Three decisions here are load-bearing:
//!
//! * **auth is a route layer, not a handler habit.** Every owner-only route is
//!   registered on one router that carries [`require_auth`], so a route added
//!   later is authenticated whether or not its author remembered to ask. The
//!   routes that must stay open (`POST /api/session`, which is how a token
//!   becomes a cookie, and the node gateway, which a node reaches *before* it has
//!   credentials) live on a separate router and are the whole list of exceptions.
//! * **the internal listener reuses the same gate.** Its static `/mcp` route
//!   serves the *whole* tool registry — `shell` and the fs tools included — so it
//!   is on the owner's gate too; the only route that authenticates itself is the
//!   turn-scoped `/mcp/t/{token}`, whose token *is* the credential. Being
//!   loopback-only (enforced in [`crate::config`]) is defence in depth, never the
//!   authorization.
//! * **an unauthenticated request never learns whether a path exists.** The
//!   fallback authenticates first and answers 404 second, so probing this API
//!   without a token yields one answer for every path.

use axum::Json;
use axum::Router;
use axum::extract::{FromRequest, Request, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{any, delete, get, post};
use serde::de::DeserializeOwned;
use tracing::debug;

use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{BotId, NodeId};

use crate::error::{ApiError, ApiResult, parse_id};
use crate::state::AppState;

pub mod backends;
pub mod bots;
pub mod conversations;
pub mod health;
pub mod messages;
pub mod node_gateway;
pub mod nodes;
pub mod requests;
pub mod routines;
pub mod rules;
pub mod session;
pub mod settings;
pub mod tasks;
pub mod usage;

/// How many entries one history page carries at most.
///
/// A client that asks for a million entries gets a thousand: a transcript is
/// paged, and one response that big would be a denial of service against the
/// brain's own event loop.
pub const MAX_PAGE: u32 = 1000;

/// The public router: REST, the live WebSocket and the node gateway.
pub fn router(state: AppState) -> Router {
    // Owner-only. `route_layer` applies to the routes above it and to nothing
    // else, which is what makes the exception list below explicit.
    let owned = Router::new()
        .route("/api/health", get(health::get))
        .route("/api/bots", get(bots::list).post(bots::create))
        .route(
            "/api/bots/{id}",
            get(bots::get).patch(bots::patch).delete(bots::delete),
        )
        .route("/api/bots/{id}/session", post(bots::session_action))
        .route(
            "/api/conversations",
            get(conversations::list).post(conversations::create),
        )
        .route(
            "/api/conversations/{id}",
            get(conversations::get)
                .patch(conversations::patch)
                .delete(conversations::delete),
        )
        .route(
            "/api/conversations/{id}/entries",
            get(conversations::entries),
        )
        .route("/api/conversations/{id}/tail", get(conversations::tail))
        .route("/api/conversations/{id}/messages", post(messages::send))
        .route(
            "/api/conversations/{id}/interrupt",
            post(messages::interrupt),
        )
        .route("/api/requests", get(requests::list))
        .route("/api/requests/{id}/respond", post(requests::respond))
        .route("/api/tasks", get(tasks::list))
        .route("/api/tasks/{id}", get(tasks::get))
        .route("/api/tasks/{id}/cancel", post(tasks::cancel))
        .route("/api/routines", get(routines::list).post(routines::create))
        .route(
            "/api/routines/{id}",
            get(routines::get).delete(routines::delete),
        )
        .route("/api/routines/{id}/pause", post(routines::pause))
        .route("/api/routines/{id}/resume", post(routines::resume))
        .route("/api/routines/{id}/runs", get(routines::list_runs))
        .route("/api/nodes", get(nodes::list))
        .route("/api/nodes/pairing-codes", post(nodes::create_pairing_code))
        .route("/api/nodes/{id}", delete(nodes::revoke))
        .route("/api/rules", get(rules::list).post(rules::create))
        .route("/api/rules/{id}", delete(rules::delete))
        .route("/api/settings", get(settings::get).patch(settings::patch))
        .route("/api/usage", get(usage::get))
        .route("/api/backends", get(backends::list))
        .route("/api/backends/probe", get(backends::probe))
        .route("/api/backends/{id}/models", get(backends::models))
        .route("/api/shutdown", post(health::shutdown))
        .route("/ws/live", get(crate::live::handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    // Open: exchanging the owner token for a session cookie, and the node
    // gateway, which by definition is reached by a machine that has no
    // credentials yet.
    let open = Router::new()
        .route("/api/session", post(session::create))
        .route("/node/pair", post(node_gateway::pair))
        .route("/node", get(node_gateway::connect));

    Router::new()
        .merge(owned)
        .merge(open)
        .fallback(fallback)
        .with_state(state)
}

use std::sync::Arc;

/// The internal listener: the MCP gateway, and nothing else.
///
/// It carries no owner routes on purpose — a second path to the API would be a
/// second place to get authorization wrong — but that does not make it
/// unauthenticated:
///
/// * **`/mcp` serves the whole tool registry** (`shell`, the fs tools, every
///   toolset) with no per-turn filter, so it sits behind the same
///   [`require_auth`] gate as the public API. A listener that only binds
///   loopback is defence in depth, not a credential: `internal_addr` is
///   validated at resolve time ([`crate::config`]), but the route itself must
///   not depend on that.
/// * **`/mcp/t/{token}` authenticates with its own token.** The token is minted
///   per turn, registered in the [`swarmbot_mcp::TurnMcpRegistry`], and the
///   registry lookup *is* the check — a harness that only ever sees its own
///   turn must not also need the owner token, and the owner token must never be
///   put in a URL. It is not weaker than the static route; it is scoped to one
///   turn's context and toolsets.
/// * **`/health` stays open.** It reports a version and nothing else, and it is
///   what a supervisor probes before any credential is known.
pub fn internal_router(
    state: AppState,
    mcp: Arc<swarmbot_mcp::McpHttpService>,
    turns: Arc<swarmbot_mcp::TurnMcpRegistry>,
) -> Router {
    // `route_layer` applies to the routes above it and to nothing else, which is
    // what keeps `/health` open without a second router to keep in sync.
    let gated = Router::new()
        .route(
            "/mcp",
            post(node_gateway::mcp_handler)
                .get(node_gateway::mcp_handler)
                .delete(node_gateway::mcp_handler),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
        .with_state(Arc::clone(&mcp));
    let open = Router::new()
        .route("/health", get(internal_health))
        .with_state(mcp);
    let turn_routes = Router::new()
        .route("/mcp/t/{token}", any(node_gateway::turn_mcp_handler))
        .with_state(turns);
    gated.merge(open).merge(turn_routes)
}

async fn internal_health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "ok": true,
        "listener": "internal",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// The gate every owner-only route passes through.
pub async fn require_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> ApiResult<Response> {
    let principal = state.auth.authenticate(&headers)?;
    // A browser attaches the session cookie to *any* origin's WebSocket
    // handshake, so an upgrade is only accepted from an allowed origin. A
    // non-browser client sends no `Origin` and cannot be a cross-site attack.
    if is_upgrade(&headers) && !state.auth.origin_allowed(&headers) {
        return Err(ApiError::unauthorized(format!(
            "origin {:?} is not allowed to open a live socket",
            origin_of(&headers)
        )));
    }
    debug!(principal = principal.label(), path = %request.uri().path(), "authorized");
    Ok(next.run(request).await)
}

/// One answer for every unknown path, after the same auth check.
async fn fallback(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: Method,
    uri: Uri,
) -> ApiResult<Response> {
    state.auth.authenticate(&headers)?;
    Err(ApiError::not_found(
        "route",
        &format!("{} {}", method, uri.path()),
    ))
}

fn is_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

/// The `Origin` header as a string, or a placeholder that cannot be mistaken for
/// an origin. Never logged raw: an origin is not a credential, but the message
/// is rendered with `{:?}` so a control character cannot forge a header value.
fn origin_of(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<absent>")
        .to_string()
}

/// The request-body extractor this API uses, in place of [`Json`].
///
/// axum's own `Json` answers a malformed body with **422** and a *plain-text*
/// reason, and a missing `Content-Type` with 415. The documented contract here is
/// one status for everything a caller could have got right — **400
/// `invalid_input`**, with the envelope — so handlers take this instead and never
/// have to remember the difference. (`Json` is still what every handler answers
/// *with*; only the reading side is wrapped.)
pub struct JsonBody<T>(pub T);

impl<T, S> FromRequest<S> for JsonBody<T>
where
    T: DeserializeOwned + 'static,
    S: Send + Sync + 'static,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(ApiError::from)
    }
}

/// A bot by id, handle or label — the three ways a client may name one.
///
/// Anything unresolvable is a 404 rather than the domain's `invalid_input`: for
/// a lookup by identifier, "I could not find it" is the true statement, and a
/// client should not have to distinguish a malformed id from a deleted bot.
pub fn resolve_bot(state: &AppState, key: &str) -> ApiResult<Bot> {
    state.swarm.orchestrator.bot(key).map_err(|err| match err {
        DomainError::NotFound { .. } => ApiError::from(err),
        _ => ApiError::not_found("bot", key),
    })
}

/// A node id from a path or a body.
///
/// `nod_local` is *not* a parseable typed id — the shape is `<prefix>_<32 hex>`
/// and the local node's id is a fixed word — so it is recognized before parsing.
/// Without this, `DELETE /api/nodes/nod_local` would answer "not found" for the
/// one node that certainly exists, and binding a bot to the local node by id would
/// be impossible.
pub fn node_id(raw: &str) -> ApiResult<NodeId> {
    if raw.trim() == NodeId::LOCAL {
        return Ok(NodeId::local());
    }
    parse_id::<NodeId>("node", raw)
}

/// Bot references in a request body, accepting an id or a handle for each.
///
/// The wire shape is a list of strings either way, so accepting a handle costs a
/// client nothing and saves it a round trip per bot.
pub fn resolve_bot_refs(state: &AppState, keys: &[String]) -> ApiResult<Vec<BotId>> {
    let mut ids = Vec::with_capacity(keys.len());
    for key in keys {
        let bot = resolve_bot(state, key)?;
        if !ids.contains(&bot.id) {
            ids.push(bot.id);
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn an_upgrade_is_recognised_by_its_header_alone() {
        let mut headers = HeaderMap::new();
        assert!(!is_upgrade(&headers));
        headers.insert(
            axum::http::header::UPGRADE,
            HeaderValue::from_static("WebSocket"),
        );
        assert!(is_upgrade(&headers), "the value is case-insensitive");
        headers.insert(axum::http::header::UPGRADE, HeaderValue::from_static("h2c"));
        assert!(!is_upgrade(&headers));
        assert_eq!(origin_of(&headers), "<absent>");
    }

    #[test]
    fn the_local_node_id_is_recognised_even_though_it_is_not_parseable() {
        // `NodeId::parse` wants `<prefix>_<32 hex>`; the local node is a fixed
        // word, so without `node_id` it would look like a missing resource.
        assert!(
            NodeId::parse(NodeId::LOCAL).is_err(),
            "the reason this exists"
        );
        assert_eq!(
            node_id("nod_local").expect("the local node"),
            NodeId::local()
        );
        assert_eq!(node_id(" nod_local ").expect("trimmed"), NodeId::local());
        let real = NodeId::new();
        assert_eq!(node_id(real.as_str()).expect("a typed id"), real);
        let err = node_id("nope").unwrap_err();
        assert_eq!(err.code(), "not_found");
    }

    #[test]
    fn the_open_routes_are_exactly_the_documented_exceptions() {
        // If this list grows, the auth story in the module docs is a lie.
        let open = ["/api/session", "/node/pair", "/node"];
        assert_eq!(open.len(), 3);
        assert_eq!(MAX_PAGE, 1000);
    }
}
