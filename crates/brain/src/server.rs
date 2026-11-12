//! The listeners, the middleware stack, and the graceful stop.
//!
//! Three things live here that would otherwise be scattered across handlers:
//!
//! * **the middleware stack**, in the order it must run: a body limit, then a net
//!   that gives every failed response the documented envelope, then the access
//!   log, then CORS outside everything. That net exists because axum answers a
//!   wrong method with a bare 405 and no body, and a client that decodes every
//!   failure as `{"error":{…}}` should not have to special-case the framework's
//!   own replies.
//! * **the two listeners.** The public one carries the API; the internal one is
//!   loopback-only by configuration and carries the MCP gateway, so a harness on
//!   the same machine cannot reach the owner's API through it by accident.
//! * **one stop path.** SIGINT, SIGTERM and `POST /api/shutdown` all end up in the
//!   same place: stop accepting, let the in-flight requests finish, then return so
//!   the caller can shut the swarm down. A second signal exits at once, because
//!   "stop" twice means the first stop was too slow.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use tokio::net::TcpListener;
use tower_http::cors::{AllowHeaders, AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{debug, info, warn};

use crate::error::ApiError;
use crate::routes;
use crate::state::AppState;
use crate::telemetry::{describe_request, request_span};

/// The largest request body accepted.
///
/// Transcript entries and bot instructions are text; 8 MiB is far above anything
/// this API documents and far below what would hurt to allocate per request.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// How much of a failed response body the envelope net will buffer to check
/// whether a handler already wrote one. Error bodies here are a sentence long.
const ERROR_BODY_LIMIT: usize = 64 * 1024;

/// How much of the framework's own rejection text is folded into the message:
/// enough to diagnose, short enough for one log line and one terminal.
const DETAIL_CHARS: usize = 300;

/// How `serve` should behave.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServeOptions {
    /// Stop on SIGINT/SIGTERM. Off under test: a developer's Ctrl-C during
    /// `cargo test` must not tear down a swarm another test is asserting on.
    pub signals: bool,
    /// A second signal during a graceful stop exits the process at once. Off under
    /// test, where `process::exit` would take the whole test binary with it.
    pub hard_exit_on_second_signal: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            signals: true,
            hard_exit_on_second_signal: true,
        }
    }
}

impl ServeOptions {
    /// In-process serving for tests: only `POST /api/shutdown` stops it.
    pub fn in_process() -> Self {
        Self {
            signals: false,
            hard_exit_on_second_signal: false,
        }
    }
}

/// The public application: REST, the live WebSocket and the node gateway.
pub fn app(state: AppState) -> Router {
    let router = Router::new().merge(routes::router(state.clone()));
    instrumented(router, &state).layer(cors_layer(&state))
}

/// The internal application: the MCP gateway, and nothing else.
///
/// No CORS and no owner routes: this listener is for a harness on the same
/// machine, and a second path to the API would be a second place to get
/// authorization wrong.
pub fn internal_app(state: &AppState) -> Router {
    let mcp = routes::node_gateway::create_mcp_service(state);
    instrumented(
        routes::internal_router(mcp, Arc::clone(&state.turn_mcp)),
        state,
    )
}

/// The layers both listeners share, innermost first.
fn instrumented(router: Router, state: &AppState) -> Router {
    router
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        // The net needs the state for the log scrubber, so it is a
        // `from_fn_with_state` middleware rather than a plain response mapper.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            envelope_for_failures,
        ))
        .layer(
            TraceLayer::new_for_http()
                // `request_span` renders method, path and version only: `Debug`
                // on a `HeaderMap` would print `Authorization` and `Cookie`.
                .make_span_with(|request: &Request| request_span(request))
                .on_response(
                    |response: &axum::http::Response<Body>,
                     latency: Duration,
                     _span: &tracing::Span| {
                        let status = response.status();
                        if status.is_server_error() {
                            warn!(%status, latency_ms = latency.as_millis() as u64, "request failed");
                        } else {
                            info!(%status, latency_ms = latency.as_millis() as u64, "request finished");
                        }
                    },
                ),
        )
}

/// Cross-origin rules for a browser UI.
///
/// A native client sends no `Origin` and is unaffected. A browser is only allowed
/// from the configured public origin or from loopback — the same rule the
/// WebSocket upgrade uses, so there is one notion of "whose page is this", not two.
fn cors_layer(state: &AppState) -> CorsLayer {
    let auth = state.auth.clone();
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(
            move |origin: &HeaderValue, _parts| {
                origin
                    .to_str()
                    .is_ok_and(|origin| auth.origin_value_allowed(origin))
            },
        ))
        // The session cookie is the only credential a browser UI can carry, so a
        // CORS response without credentials would make that cookie useless.
        .allow_credentials(true)
        // With credentials on, a wildcard is invalid — and tower-http panics at
        // request time rather than at build time, so these are spelled out and
        // the preflight test below is what keeps them honest.
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers(AllowHeaders::mirror_request())
        .max_age(Duration::from_secs(600))
}

/// Make every failed response carry the documented envelope.
///
/// Two kinds of failure reach a client without one, and both are the framework's
/// rather than this crate's:
///
/// * a response with **no body at all** — axum answers a wrong method with a bare
///   405;
/// * a response with a **plain-text body** — when an extractor rejects a request
///   the handler never runs, so the rejection's own `IntoResponse` answers, and
///   that is prose ("Failed to deserialize the JSON body into the target type: …").
///
/// A client that decodes every failure as `{"error":{"code","message"}}` must not
/// have to special-case either, so both are rewritten here — with the original
/// status kept (a 405 stays a 405) and the framework's own words folded into the
/// message, because "invalid input" on its own is not actionable.
///
/// A body that is *already* the envelope is passed through untouched, and so is
/// anything informational: a 101 is a WebSocket upgrade, whose "body" is the
/// socket.
async fn envelope_for_failures(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    // Rendered before the handler runs, because the request is gone afterwards.
    let label = describe_request(&request);
    fill_in_envelope(Some(&state.secrets), &label, next.run(request).await).await
}

/// The half of the net that does not need a service stack — and so can be tested
/// on its own.
async fn fill_in_envelope(
    secrets: Option<&crate::telemetry::SecretRegistry>,
    label: &str,
    response: Response,
) -> Response {
    let status = response.status();
    if status.is_success() || status.is_informational() || status == StatusCode::NO_CONTENT {
        return response;
    }
    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, ERROR_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        // A body that cannot be read is not something to replace with a guess:
        // pass the status through untouched rather than inventing a reason.
        Err(err) => {
            warn!(error = %err, %status, "cannot read a failed response body; leaving it as it is");
            return (parts.status, parts.headers).into_response();
        }
    };
    if is_envelope(&bytes) {
        return (parts, Body::from(bytes)).into_response();
    }

    // The framework's prose, if any, becomes part of the message — scrubbed
    // first, because a rejection can quote what it was given.
    let detail = if bytes.is_empty() {
        None
    } else {
        let text = String::from_utf8_lossy(&bytes);
        let trimmed = text.trim();
        let scrubbed = match secrets {
            Some(secrets) => secrets.redact(trimmed).into_owned(),
            None => trimmed.to_string(),
        };
        Some(truncate(&scrubbed, DETAIL_CHARS))
    };
    let mut envelope = ApiError::for_status(status, label).envelope();
    if let Some(detail) = detail {
        let message = envelope["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        envelope["error"]["message"] = serde_json::Value::String(format!("{message}: {detail}"));
    }
    debug!(%status, request = %label, "an error was rendered as the documented envelope");
    (status, axum::Json(envelope)).into_response()
}

/// Is this body already `{"error":{"code",…}}`?
fn is_envelope(bytes: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    value
        .get("error")
        .and_then(|error| error.get("code"))
        .is_some()
}

/// Truncate by characters, never mid-codepoint: the message is human text.
fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Bind one listener.
pub async fn bind(addr: &str) -> anyhow::Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind {addr}"))
}

/// Serve until something asks to stop, then let the listeners drain.
///
/// Returns once both listeners have finished. The swarm is *not* shut down here:
/// a listener error still has to be reported, and the caller owns the ordering
/// "stop accepting → settle the turns → flush the log".
pub async fn serve(
    state: AppState,
    public: TcpListener,
    internal: Option<TcpListener>,
    options: ServeOptions,
) -> anyhow::Result<()> {
    let public_addr = public
        .local_addr()
        .context("cannot read the bound address")?;
    // Publish the internal address before spawning either listener so any turn
    // woken via the public listener can register a reachable `/mcp/t/{token}`
    // endpoint right away.
    let internal_addr = match internal.as_ref() {
        Some(listener) => {
            let addr = listener
                .local_addr()
                .context("cannot read the bound internal address")?;
            state.set_internal_addr(addr);
            Some(addr)
        }
        None => None,
    };
    info!(
        listener = %public_addr,
        "the public listener is up: REST, /ws/live and the node gateway"
    );

    let serving = state.clone();
    let public_task = tokio::spawn(async move {
        axum::serve(public, app(serving.clone()))
            .with_graceful_shutdown(async move { serving.wait_for_shutdown().await })
            .await
            .context("the public listener failed")
    });

    let internal_task = internal.map(|listener| {
        let serving = state.clone();
        // Built here, not inside the task: the router needs the state by
        // reference, and the task owns it.
        let router = internal_app(&serving);
        tokio::spawn(async move {
            if let Some(addr) = internal_addr {
                info!(listener = %addr, "the internal listener is up: the MCP gateway");
                write_mcp_config(&serving.config.paths, &addr);
            }
            axum::serve(listener, router)
                .with_graceful_shutdown(async move { serving.wait_for_shutdown().await })
                .await
                .context("the internal listener failed")
        })
    });

    wait_for_stop(&state, &options).await;
    // Both listeners watch this flag. Asking again is harmless, and it covers the
    // case where a signal — rather than the API — started the stop.
    state.request_shutdown();

    let mut failure = public_task
        .await
        .context("the public listener task panicked")
        .and_then(|outcome| outcome);
    if let Some(task) = internal_task {
        let outcome = task
            .await
            .context("the internal listener task panicked")
            .and_then(|outcome| outcome);
        if failure.is_err() {
            failure = outcome;
        }
    }
    failure?;
    info!(
        uptime_secs = state.uptime_secs(),
        "the listeners have stopped"
    );
    Ok(())
}

/// Write the MCP client configuration file for CLI harnesses on this machine.
fn write_mcp_config(paths: &crate::config::Paths, addr: &std::net::SocketAddr) {
    let mcp_url = format!("http://{addr}/mcp");
    let json_val = serde_json::json!({
        "mcpServers": {
            "swarmbot": {
                "url": mcp_url
            }
        }
    });
    let target = paths.mcp_config();
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string_pretty(&json_val) {
        if let Err(err) = std::fs::write(&target, content) {
            debug!(path = %target.display(), %err, "could not write MCP config file");
        } else {
            info!(path = %target.display(), "wrote MCP harness configuration");
        }
    }
}

/// Wait for the first stop request: a signal, or the API's shutdown flag.
async fn wait_for_stop(state: &AppState, options: &ServeOptions) {
    if !options.signals {
        state.wait_for_shutdown().await;
        return;
    }
    tokio::select! {
        _ = wait_for_signal() => info!("a stop signal arrived; draining the listeners"),
        _ = state.wait_for_shutdown() => info!("a shutdown was requested through the API; draining the listeners"),
    }
    if options.hard_exit_on_second_signal {
        tokio::spawn(async {
            wait_for_signal().await;
            // `process::exit` runs no destructors and flushes nothing, so the
            // reason goes to stderr as well as to the log: the last thing an
            // operator reads should be why the brain stopped.
            warn!("a second stop signal arrived; exiting now and leaving turns in flight");
            eprintln!("swarmbot-brain: second signal, exiting now");
            std::process::exit(0);
        });
    }
}

/// The next SIGINT or SIGTERM.
///
/// A handler that cannot be installed waits forever instead of returning at once:
/// returning would be indistinguishable from "a signal arrived", and the brain
/// would stop for no reason at all.
async fn wait_for_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            warn!("cannot listen for SIGINT; only the API and SIGTERM can stop this brain");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                terminate.recv().await;
            }
            Err(err) => {
                warn!(error = %err, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LABEL: &str = "POST /api/bots HTTP/1.1";

    #[tokio::test]
    async fn a_bare_405_becomes_the_documented_envelope() {
        // axum's own reply for a known path and a wrong method has no body; the
        // client decodes `{"error":{"code","message"}}` from every failure.
        let filled =
            fill_in_envelope(None, LABEL, StatusCode::METHOD_NOT_ALLOWED.into_response()).await;
        assert_eq!(filled.status(), StatusCode::METHOD_NOT_ALLOWED);
        let body = body_json(filled).await;
        assert_eq!(body["error"]["code"], "invalid_input");
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("/api/bots")),
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_bare_404_and_500_get_their_own_codes() {
        let body =
            body_json(fill_in_envelope(None, LABEL, StatusCode::NOT_FOUND.into_response()).await)
                .await;
        assert_eq!(body["error"]["code"], "not_found");
        let body = body_json(
            fill_in_envelope(
                None,
                LABEL,
                StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            )
            .await,
        )
        .await;
        assert_eq!(body["error"]["code"], "internal");
    }

    #[tokio::test]
    async fn a_body_a_handler_wrote_is_left_alone() {
        // `Response<Body>` is not `Clone`, so the expectation is rendered from a
        // second, identically built error.
        let expected = body_json(ApiError::conflict("the bot is busy").into_response()).await;
        let mine = ApiError::conflict("the bot is busy").into_response();
        let after = fill_in_envelope(None, LABEL, mine).await;
        assert_eq!(body_json(after).await, expected, "the envelope survives");
    }

    #[tokio::test]
    async fn a_successful_response_is_never_rewritten() {
        let ok = (StatusCode::OK, axum::Json(serde_json::json!({"ok": true}))).into_response();
        let passed = fill_in_envelope(None, LABEL, ok).await;
        assert_eq!(passed.status(), StatusCode::OK);
        assert_eq!(body_json(passed).await["ok"], true);
    }

    #[tokio::test]
    async fn a_204_keeps_its_empty_body() {
        // A DELETE answers 204 and the client reads no body from it; filling one
        // in would be a protocol error.
        let passed = fill_in_envelope(None, LABEL, StatusCode::NO_CONTENT.into_response()).await;
        assert_eq!(passed.status(), StatusCode::NO_CONTENT);
        let bytes = axum::body::to_bytes(passed.into_body(), ERROR_BODY_LIMIT)
            .await
            .expect("a body");
        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn an_upgrade_response_is_passed_through() {
        // 101 is not a success status, and buffering its "body" would take the
        // socket it is handing over.
        let passed = fill_in_envelope(
            None,
            "GET /ws/live HTTP/1.1",
            StatusCode::SWITCHING_PROTOCOLS.into_response(),
        )
        .await;
        assert_eq!(passed.status(), StatusCode::SWITCHING_PROTOCOLS);
    }

    #[test]
    fn the_test_options_never_touch_signals() {
        let options = ServeOptions::in_process();
        assert!(!options.signals);
        assert!(!options.hard_exit_on_second_signal);
        assert!(ServeOptions::default().signals);
        assert!(ServeOptions::default().hard_exit_on_second_signal);
        const { assert!(MAX_BODY_BYTES > ERROR_BODY_LIMIT) };
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), ERROR_BODY_LIMIT)
            .await
            .expect("a body");
        if bytes.is_empty() {
            return serde_json::Value::Null;
        }
        serde_json::from_slice(&bytes).expect("the envelope is JSON")
    }
}
