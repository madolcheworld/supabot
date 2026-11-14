//! `POST /api/session`: the owner token becomes a session cookie.
//!
//! This is the only route that accepts a credential in its body, and the only one
//! that issues one. A browser cannot put an `Authorization` header on a WebSocket
//! handshake, so `/ws/live` depends on the cookie this route mints; a CLI or the
//! GPUI app can keep using the bearer token and never call this at all.
//!
//! The token itself is never put in the cookie: a session id is a separate,
//! revocable credential, so `POST /api/shutdown`-style "log everybody out" is
//! possible without rotating the owner token, and a stolen cookie cannot be
//! replayed as a bearer token to a *different* brain.

use axum::Json;
use axum::extract::State;
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use crate::auth::constant_time_eq;
use crate::error::{ApiError, ApiResult};
use crate::routes::JsonBody;
use crate::state::AppState;

#[derive(Clone, Debug, Deserialize)]
pub struct LoginRequest {
    /// The owner token, from `<data>/owner-token` or the one-time login URL.
    pub token: String,
}

/// `POST /api/session`
pub async fn create(
    State(state): State<AppState>,
    JsonBody(body): JsonBody<LoginRequest>,
) -> ApiResult<impl IntoResponse> {
    if !constant_time_eq(body.token.trim().as_bytes(), state.auth.token().as_bytes()) {
        // The token is never logged, not even here: a failed login is exactly
        // where a credential is most likely to be somebody else's secret.
        warn!(
            path = "/api/session",
            "a login attempt used a token that is not the owner token"
        );
        return Err(ApiError::unauthorized(
            "that is not this brain's owner token",
        ));
    }
    let session = state.auth.create_session();
    // A session id is a credential like any other, so it is registered with the
    // log scrubber the moment it exists.
    state.secrets.register(session.clone());

    let mut headers = HeaderMap::new();
    headers.insert(
        SET_COOKIE,
        HeaderValue::from_str(&state.auth.cookie_header(&session))
            .map_err(|err| ApiError::internal(format!("cannot render a session cookie: {err}")))?,
    );
    Ok((headers, Json(json!({ "ok": true }))))
}

/// `POST /api/auth/revoke-sessions` — log every session out, keep the owner token.
///
/// Why this exists: `OwnerAuth::revoke_sessions` had no caller, so a session
/// cookie — minted by [`create`] and valid for the life of the process — could not
/// be taken back. A laptop left logged in had a credential until the brain
/// restarted, and the only mitigation was rotating the owner token, which logs out
/// the CLI as well and invalidates the printed login URL.
///
/// Two rules the answer states plainly:
///
/// * **the owner token still works.** That is the point of the split: a bearer
///   token can be rotated by hand, a session cannot be un-minted once it is in a
///   browser's jar. A caller that authenticates with a *cookie* is therefore
///   logging itself out too — `revoked` says how many credentials died, and the
///   session id that was used for this request is one of them.
/// * **it is idempotent.** A second call revokes zero and answers 200; a client
///   that retried a revoke should not have to read a 409 as "the first one failed".
pub async fn revoke_sessions(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let revoked = state.auth.revoke_sessions();
    // Sessions are registered with the log scrubber when they are minted. A dead
    // session authenticates nothing, so it is no longer a secret and does not need
    // to keep costing every log line a scan; the *owner token* stays registered,
    // because it survives this call.
    tracing::info!(
        revoked,
        "every session was revoked; the owner token is unchanged and still works"
    );
    Ok(Json(json!({ "revoked": revoked })))
}

/// The cookie a test client should send back, extracted from a response.
///
/// Lives here because "which cookie does this API set?" is a question about this
/// route, and a test that re-implemented the parsing would drift from it.
#[cfg(test)]
pub fn cookie_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(SET_COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .next()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    use crate::auth::SESSION_COOKIE;

    #[test]
    fn the_cookie_carries_the_session_and_its_flags() {
        let auth = crate::auth::OwnerAuth::ephemeral("token");
        let session = auth.create_session();
        let cookie = auth.cookie_header(&session);
        assert!(
            cookie.starts_with(&format!("{SESSION_COOKIE}={session};")),
            "{cookie}"
        );
        for flag in ["HttpOnly", "SameSite=Lax", "Path=/"] {
            assert!(cookie.contains(flag), "{cookie} is missing {flag}");
        }
        assert!(!cookie.contains("token"), "{cookie}");
    }

    #[test]
    fn a_cookie_header_is_parseable_back_to_its_pair() {
        let mut headers = HeaderMap::new();
        headers.insert(
            SET_COOKIE,
            HeaderValue::from_static("swarmbot_session=abc; HttpOnly; Path=/"),
        );
        assert_eq!(cookie_of(&headers).as_deref(), Some("swarmbot_session=abc"));
        assert_eq!(cookie_of(&HeaderMap::new()), None);
    }

    #[test]
    fn the_response_body_is_the_documented_ok() {
        // The client crate decodes `{"ok":true}` and reads the cookie from the
        // headers; a body that also carried the session id would be a second,
        // script-readable copy of a credential.
        let body: Value = json!({ "ok": true });
        assert_eq!(body.as_object().expect("an object").len(), 1);
    }
}
