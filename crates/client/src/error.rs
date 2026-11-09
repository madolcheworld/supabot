//! Every way a call to the brain can fail, as one client-side error type.
//!
//! The brain answers failures with a documented envelope
//! (`{"error":{"code":"…","message":"…"}}`) and a matching HTTP status. This
//! module keeps that information intact — the status is *never* lost, even
//! when the body is not the envelope — and maps into
//! [`swarmbot_contracts::DomainError`] so a CLI command can print a single
//! error type no matter whether the failure happened in the brain or on the
//! way there.

use serde::Deserialize;
use swarmbot_contracts::{DomainError, NodeId};

/// How a brain call failed.
#[derive(thiserror::Error, Debug)]
pub enum ClientError {
    /// The brain answered with a 4xx/5xx status. `code` is the domain error
    /// code from the envelope, or `"unknown"` when the body did not parse.
    #[error("the brain refused: {status} {code}: {message}")]
    Http {
        status: u16,
        code: String,
        message: String,
    },

    /// The request never got a usable answer: DNS, connect, TLS, timeout, or
    /// a dropped WebSocket.
    #[error("cannot reach the brain at {url}: {reason}")]
    Transport { url: String, reason: String },

    /// A response (or live frame) arrived but this client cannot decode it —
    /// usually a brain newer than the client, which should not happen on the
    /// documented wire types but must never panic.
    #[error("the brain sent something this client cannot read: {reason}")]
    Decode { reason: String },

    /// The brain said 401, or the client knows it never authenticated. The
    /// fix is always the same, so the message says it.
    #[error("not logged in: pass a token or call login() first")]
    Unauthorized,
}

/// How much of a non-envelope error body is kept as the message: enough to
/// diagnose, short enough not to flood a terminal.
const RAW_BODY_MESSAGE_CHARS: usize = 500;

impl ClientError {
    /// Build the error for a non-2xx response.
    ///
    /// A 401 becomes [`ClientError::Unauthorized`] whatever the body says —
    /// the actionable advice is identical. Everything else keeps its status;
    /// a body that is not the documented envelope yields `code: "unknown"`
    /// and the raw body (truncated to [`RAW_BODY_MESSAGE_CHARS`] chars) as
    /// the message, so nothing is silently dropped.
    pub(crate) fn from_response(status: u16, body: &str) -> Self {
        if status == 401 {
            return Self::Unauthorized;
        }
        if let Ok(envelope) = serde_json::from_str::<ErrorEnvelope>(body) {
            let message = if envelope.error.message.trim().is_empty() {
                truncate_chars(body, RAW_BODY_MESSAGE_CHARS)
            } else {
                envelope.error.message
            };
            return Self::Http {
                status,
                code: envelope.error.code,
                message,
            };
        }
        Self::Http {
            status,
            code: "unknown".to_string(),
            message: truncate_chars(body, RAW_BODY_MESSAGE_CHARS),
        }
    }

    /// The domain error code, when this error carries one (`Http` from the
    /// envelope, or `Unauthorized`).
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Http { code, .. } => Some(code),
            Self::Unauthorized => Some("unauthorized"),
            _ => None,
        }
    }

    /// The brain said "no such thing" (404 / `not_found`).
    pub fn is_not_found(&self) -> bool {
        match self {
            Self::Http { status, code, .. } => *status == 404 || code == "not_found",
            _ => false,
        }
    }

    /// The brain said "that clashes with the current state" (409 / `conflict`).
    pub fn is_conflict(&self) -> bool {
        match self {
            Self::Http { status, code, .. } => *status == 409 || code == "conflict",
            _ => false,
        }
    }
}

/// The documented error envelope: `{"error":{"code","message"}}`.
#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: String,
    #[serde(default)]
    message: String,
}

/// Truncate by chars, never mid-codepoint: bodies are human text.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

impl From<ClientError> for DomainError {
    /// Collapse a client failure into the domain error type the CLI already
    /// prints. Codes are preserved where [`DomainError`] has a matching
    /// variant; where it does not (`request_not_pending` needs an id the
    /// wire error does not carry, transport failures have no domain shape)
    /// the mapping falls back to the closest variant and keeps the text.
    fn from(err: ClientError) -> Self {
        match &err {
            ClientError::Http { code, message, .. } => match code.as_str() {
                "not_found" => DomainError::NotFound {
                    what: "resource".to_string(),
                    id: message.clone(),
                },
                "invalid_input" => DomainError::InvalidInput {
                    message: message.clone(),
                },
                "conflict" => DomainError::Conflict {
                    message: message.clone(),
                },
                "unauthorized" => DomainError::Unauthorized {
                    message: message.clone(),
                },
                "node_unavailable" => DomainError::NodeUnavailable {
                    // The envelope carries no node id; the reason text does,
                    // if the brain put one there.
                    node_id: NodeId::from_suffix("unknown"),
                    reason: message.clone(),
                },
                "backend_unavailable" => DomainError::BackendUnavailable {
                    backend: "unknown".to_string(),
                    reason: message.clone(),
                },
                "internal" => DomainError::Internal {
                    message: message.clone(),
                },
                // Unknown codes (including `request_not_pending`, a 409-class
                // error whose variant cannot carry the server's text) keep
                // the code visible inside the message.
                other => DomainError::Internal {
                    message: format!("{other}: {message}"),
                },
            },
            ClientError::Unauthorized => DomainError::Unauthorized {
                message: err.to_string(),
            },
            // Transport and decode failures are not the domain's fault, but a
            // CLI command still needs one printable error: `internal`.
            _ => DomainError::Internal {
                message: err.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_envelopes_keep_status_and_code() {
        let err = ClientError::from_response(
            404,
            r#"{"error":{"code":"not_found","message":"bot not found: bot_x"}}"#,
        );
        assert!(err.is_not_found());
        assert!(!err.is_conflict());
        assert_eq!(err.code(), Some("not_found"));
        match err {
            ClientError::Http {
                status,
                code,
                message,
            } => {
                assert_eq!(status, 404);
                assert_eq!(code, "not_found");
                assert_eq!(message, "bot not found: bot_x");
            }
            other => panic!("expected Http, got {other:?}"),
        }

        let conflict =
            ClientError::from_response(409, r#"{"error":{"code":"conflict","message":"busy"}}"#);
        assert!(conflict.is_conflict());
        assert_eq!(conflict.code(), Some("conflict"));
    }

    #[test]
    fn a_401_becomes_the_unauthorized_variant() {
        let err = ClientError::from_response(
            401,
            r#"{"error":{"code":"unauthorized","message":"bad token"}}"#,
        );
        assert!(matches!(err, ClientError::Unauthorized));
        assert_eq!(err.code(), Some("unauthorized"));
    }

    #[test]
    fn a_non_json_body_never_loses_the_status_and_is_truncated() {
        let long_body = "x".repeat(600);
        let err = ClientError::from_response(502, &long_body);
        match err {
            ClientError::Http {
                status,
                code,
                message,
            } => {
                assert_eq!(status, 502);
                assert_eq!(code, "unknown");
                assert_eq!(message.chars().count(), 500);
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_envelope_message_falls_back_to_the_body() {
        let err = ClientError::from_response(400, r#"{"error":{"code":"invalid_input"}}"#);
        assert_eq!(err.code(), Some("invalid_input"));
        assert!(err.to_string().contains("invalid_input"));
    }

    #[test]
    fn domain_error_mapping_keeps_the_code() {
        let cases = [
            (
                ClientError::from_response(
                    404,
                    r#"{"error":{"code":"not_found","message":"gone"}}"#,
                ),
                "not_found",
            ),
            (
                ClientError::from_response(
                    400,
                    r#"{"error":{"code":"invalid_input","message":"bad"}}"#,
                ),
                "invalid_input",
            ),
            (
                ClientError::from_response(
                    409,
                    r#"{"error":{"code":"conflict","message":"clash"}}"#,
                ),
                "conflict",
            ),
            (
                ClientError::from_response(
                    401,
                    r#"{"error":{"code":"unauthorized","message":"nope"}}"#,
                ),
                "unauthorized",
            ),
            (
                ClientError::from_response(
                    502,
                    r#"{"error":{"code":"backend_unavailable","message":"dead"}}"#,
                ),
                "backend_unavailable",
            ),
            (
                ClientError::from_response(
                    502,
                    r#"{"error":{"code":"node_unavailable","message":"offline"}}"#,
                ),
                "node_unavailable",
            ),
            (
                ClientError::from_response(
                    500,
                    r#"{"error":{"code":"internal","message":"boom"}}"#,
                ),
                "internal",
            ),
        ];
        for (client_err, expected_code) in cases {
            let domain: DomainError = client_err.into();
            assert_eq!(
                domain.code(),
                expected_code,
                "mapping {:?} lost the code",
                domain
            );
        }

        // Transport and decode failures collapse to `internal` but keep text.
        let transport = DomainError::from(ClientError::Transport {
            url: "http://127.0.0.1:1".into(),
            reason: "connection refused".into(),
        });
        assert_eq!(transport.code(), "internal");
        assert!(transport.to_string().contains("connection refused"));
    }

    #[test]
    fn truncation_is_by_chars_not_bytes() {
        let body = "é".repeat(600);
        let err = ClientError::from_response(500, &body);
        match err {
            ClientError::Http { message, .. } => assert_eq!(message.chars().count(), 500),
            other => panic!("expected Http, got {other:?}"),
        }
    }
}
