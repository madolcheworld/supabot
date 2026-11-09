//! `GET /api/health` and `POST /api/shutdown`.
//!
//! Health is behind the same auth gate as everything else: an unauthenticated
//! probe should not learn that a brain exists, what version it runs or how long
//! it has been up. A supervisor that needs an unauthenticated liveness check can
//! use the internal listener's `/health`, which is loopback-only by
//! configuration.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};
use tracing::info;

use crate::error::ApiResult;
use crate::state::AppState;

/// How long the shutdown request waits before it stops the listener.
///
/// The response has to reach the client first: a graceful stop closes the
/// listener, and a client that asked for a shutdown and got a reset connection
/// cannot tell "it worked" from "it died".
const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

/// `GET /api/health`
///
/// `epoch` is the live event log's epoch. A client whose stored cursor carries a
/// different epoch must resync instead of resuming, so this is the cheap answer
/// to "did the brain restart under me?".
pub async fn get(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "ok": true,
        "version": state.version(),
        "epoch": state.epoch(),
        "uptime_secs": state.uptime_secs(),
    })))
}

/// `POST /api/shutdown`
///
/// Answers at once, then stops accepting connections and lets the swarm finish
/// the turns in flight. This is the same path a signal takes, so a supervised
/// brain and a UI-initiated one stop identically.
pub async fn shutdown(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    info!("a shutdown was requested through the API");
    let trigger = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(SHUTDOWN_GRACE).await;
        trigger.request_shutdown();
    });
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grace_period_is_long_enough_to_flush_a_response() {
        assert!(SHUTDOWN_GRACE >= Duration::from_millis(50));
        assert!(
            SHUTDOWN_GRACE < Duration::from_secs(2),
            "…and short enough that a test does not wait on it"
        );
    }
}
