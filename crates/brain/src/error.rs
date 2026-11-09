//! The error envelope: one shape for every failure this API can produce.
//!
//! Every error body is `{"error":{"code":"…","message":"…"}}` and the code is
//! [`DomainError::code`], not an HTTP-status invention of this crate. That is
//! what lets a client branch on `"conflict"` without parsing prose, and what
//! keeps the CLI, the GPUI app and the tests reading the same vocabulary.
//!
//! Status mapping is fixed here and nowhere else:
//!
//! | code | status |
//! | --- | --- |
//! | `invalid_input` | 400 |
//! | `unauthorized` | 401 |
//! | `not_found` | 404 |
//! | `conflict` | 409 |
//! | `request_not_pending` | 409 |
//! | `node_unavailable` | 502 |
//! | `backend_unavailable` | 502 |
//! | `internal` | 500 |
//!
//! Axum's own rejections (an unparsable JSON body, a bad query string, a missing
//! path parameter) become `invalid_input`/400 rather than 415 or 422, so a
//! client only ever has to handle the codes above.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tracing::{debug, error};

use swarmbot_contracts::error::DomainError;

/// A failure the API can render.
#[derive(Clone, Debug)]
pub struct ApiError(pub DomainError);

/// The result type every handler in this crate returns.
pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self(DomainError::invalid(message))
    }

    pub fn not_found(what: &str, id: &str) -> Self {
        Self(DomainError::not_found(what, id))
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self(DomainError::conflict(message))
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self(DomainError::Unauthorized {
            message: message.into(),
        })
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self(DomainError::internal(message))
    }

    /// The status this error is rendered with.
    pub fn status(&self) -> StatusCode {
        match &self.0 {
            DomainError::InvalidInput { .. } => StatusCode::BAD_REQUEST,
            DomainError::Unauthorized { .. } => StatusCode::UNAUTHORIZED,
            DomainError::NotFound { .. } => StatusCode::NOT_FOUND,
            // An already-settled card is a conflict: the resource is there, the
            // state is not what the call assumed.
            DomainError::Conflict { .. } | DomainError::RequestNotPending { .. } => {
                StatusCode::CONFLICT
            }
            DomainError::NodeUnavailable { .. } | DomainError::BackendUnavailable { .. } => {
                StatusCode::BAD_GATEWAY
            }
            DomainError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        self.0.code()
    }

    /// The body a client receives.
    pub fn envelope(&self) -> serde_json::Value {
        json!({
            "error": {
                "code": self.code(),
                "message": self.0.to_string(),
            }
        })
    }

    /// The envelope for a status this crate did not produce itself.
    ///
    /// axum answers a wrong method with a bare 405 and no body; a client that
    /// decodes every failure as `{"error":{…}}` would then have to special-case
    /// it. Codes are chosen so the pair (status, code) still means what the table
    /// in this module's docs says it means.
    pub fn for_status(status: StatusCode, request: &str) -> Self {
        let message = match status {
            StatusCode::METHOD_NOT_ALLOWED => {
                format!("{request} does not accept that method")
            }
            StatusCode::NOT_FOUND => format!("there is no route at {request}"),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                "send `Authorization: Bearer <token>` or a session cookie".to_string()
            }
            StatusCode::PAYLOAD_TOO_LARGE => "the request body is too large".to_string(),
            _ if status.is_server_error() => {
                format!("the brain could not answer {request}")
            }
            _ => format!("{request} was refused with {status}"),
        };
        let err = match status.as_u16() {
            401 | 403 => DomainError::Unauthorized { message },
            404 => DomainError::not_found("route", request),
            409 => DomainError::Conflict { message },
            502 => DomainError::BackendUnavailable {
                backend: "unknown".into(),
                reason: message,
            },
            _ if status.is_server_error() => DomainError::Internal { message },
            // 400, 405, 413, 415 and anything else a client could have fixed.
            _ => DomainError::InvalidInput { message },
        };
        Self(err)
    }
}

impl From<DomainError> for ApiError {
    fn from(err: DomainError) -> Self {
        Self(err)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        // A 5xx is ours to fix, so it is logged with the detail; a 4xx is the
        // caller's mistake and is not worth a line in a busy log.
        if status.is_server_error() {
            error!(code = self.code(), error = %self.0, "request failed");
        } else {
            debug!(code = self.code(), status = %status, error = %self.0, "request refused");
        }
        (status, Json(self.envelope())).into_response()
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        // `body_text` quotes serde's complaint, which names *fields* — never a
        // value, so a token in a rejected body cannot be echoed back.
        Self::invalid(rejection.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        Self::invalid(rejection.body_text())
    }
}

impl From<PathRejection> for ApiError {
    fn from(rejection: PathRejection) -> Self {
        Self::invalid(rejection.body_text())
    }
}

/// Parse a typed id out of a path segment.
///
/// A segment that is not even shaped like an id cannot name an existing
/// resource, so this is a 404 and not a 400: telling a scanner "your id format
/// is wrong" is telling it something about the schema it did not know.
pub fn parse_id<T>(what: &str, value: &str) -> Result<T, ApiError>
where
    T: std::str::FromStr,
{
    value
        .parse::<T>()
        .map_err(|_| ApiError::not_found(what, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use swarmbot_contracts::id::{NodeId, RequestId};

    async fn render(err: ApiError) -> (StatusCode, serde_json::Value) {
        let response = err.into_response();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body");
        (status, serde_json::from_slice(&bytes).expect("json"))
    }

    #[tokio::test]
    async fn every_domain_error_maps_to_its_documented_status() {
        let cases = vec![
            (DomainError::invalid("nope"), 400, "invalid_input"),
            (
                DomainError::Unauthorized {
                    message: "who".into(),
                },
                401,
                "unauthorized",
            ),
            (DomainError::not_found("bot", "bot_1"), 404, "not_found"),
            (DomainError::conflict("busy"), 409, "conflict"),
            (
                DomainError::RequestNotPending {
                    request_id: RequestId::new(),
                },
                409,
                "request_not_pending",
            ),
            (
                DomainError::NodeUnavailable {
                    node_id: NodeId::local(),
                    reason: "offline".into(),
                },
                502,
                "node_unavailable",
            ),
            (
                DomainError::BackendUnavailable {
                    backend: "inferhub".into(),
                    reason: "402".into(),
                },
                502,
                "backend_unavailable",
            ),
            (DomainError::internal("boom"), 500, "internal"),
        ];
        for (err, status, code) in cases {
            let (rendered, body) = render(ApiError(err.clone())).await;
            assert_eq!(rendered.as_u16(), status, "{err}");
            assert_eq!(body["error"]["code"], code, "{err}");
            assert_eq!(body["error"]["message"], err.to_string(), "{err}");
            assert_eq!(
                body.as_object().expect("an object").len(),
                1,
                "only `error`"
            );
        }
    }

    #[tokio::test]
    async fn an_unparsable_body_is_invalid_input_not_a_serde_status() {
        use axum::extract::FromRequest;

        let rejection = Json::<serde_json::Value>::from_request(
            Request::builder()
                .header("content-type", "application/json")
                .body(axum::body::Body::from("{not json"))
                .expect("a request"),
            &(),
        )
        .await
        .expect_err("rejects");
        let (status, body) = render(ApiError::from(rejection)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_input");
    }

    #[test]
    fn an_id_that_cannot_exist_is_not_found() {
        let err = parse_id::<RequestId>("request", "nope").unwrap_err();
        assert_eq!(err.code(), "not_found");
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        let real = RequestId::new();
        assert!(parse_id::<RequestId>("request", real.as_str()).is_ok());
    }
}
