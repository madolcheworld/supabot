//! The backend port: one trait every model adapter implements.
//!
//! The runner talks to this trait and nothing else, so adding a backend
//! (HTTP gateway, CLI harness, fake) cannot ripple into orchestration. Two
//! rules keep that safe:
//!
//! * [`LlmBackend::chat`] returns the **authoritative** [`ChatResponse`];
//!   the streamed [`LlmEvent`]s are best-effort progress (a retry restarts
//!   the stream, so a delta may have been emitted for an attempt that never
//!   completed).
//! * every failure is a value ([`LlmError`]) — adapters never panic — and
//!   [`LlmError::retryable`] is the single place that decides whether the
//!   runner's retry loop may try again.

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use swarmbot_contracts::{BackendKind, DomainError};

use crate::types::{BackendStatus, Capabilities, ChatRequest, ChatResponse, LlmEvent};

/// The model-layer port. Implementations: [`crate::openai::OpenAiCompat`],
/// [`crate::fake::FakeBackend`], [`crate::harness::ClaudeCli`].
#[async_trait]
pub trait LlmBackend: Send + Sync {
    /// Configured backend id (bots name one).
    fn id(&self) -> &str;

    fn kind(&self) -> BackendKind;

    fn capabilities(&self) -> Capabilities;

    /// Stream one assistant turn. Events go to `events`; the assembled
    /// response is returned. `cancel` aborts the HTTP stream promptly.
    ///
    /// Invariant: when this returns `Ok`, exactly one
    /// [`LlmEvent::Finished`] has been sent (unless the receiver was
    /// dropped); when it returns `Err`, the response must not be used.
    async fn chat(
        &self,
        request: ChatRequest,
        events: mpsc::Sender<LlmEvent>,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, LlmError>;

    /// Install/version/auth status without spending a model call.
    async fn probe(&self) -> Result<BackendStatus, LlmError>;

    /// The models this backend can run, when it can list them **without
    /// spending a completion**: an OpenAI-compatible gateway's `/models`, an
    /// ACP agent's advertised session models, the fake's one script model.
    ///
    /// An empty list is an honest "this backend does not advertise a choice"
    /// (opencode's ACP server, the `claude -p` skeleton) — the UI then offers
    /// a free-text field instead of pretending there is a menu. The default
    /// is empty so backends without a listing never fake one.
    async fn available_models(&self) -> Result<crate::types::ModelList, LlmError> {
        Ok(crate::types::ModelList::authoritative(Vec::new()))
    }
}

/// Every way a backend call can fail. Messages are operator-facing (they end
/// up in transcripts and the UI), so they must read on their own — and must
/// never contain an API key.
#[derive(thiserror::Error, Debug)]
pub enum LlmError {
    #[error("backend `{backend}` unavailable: {reason}")]
    Unavailable { backend: String, reason: String },

    #[error("backend `{backend}` refused the request: status {status}: {body}")]
    Refused {
        backend: String,
        status: u16,
        body: String,
    },

    #[error("backend `{backend}` sent something this build cannot read: {reason}")]
    Malformed { backend: String, reason: String },

    #[error("cancelled")]
    Cancelled,

    #[error("timed out after {secs}s")]
    Timeout { secs: u64 },

    #[error("no api key: set {env}")]
    MissingKey { env: String },
}

impl LlmError {
    /// Whether the same call could succeed on a retry.
    ///
    /// Retryable: 429, 5xx, timeouts and transport-level unavailability —
    /// transient conditions. Not retryable: other 4xx (401 bad key, 402 no
    /// provider under bid, 400 bad request), malformed streams, cancellation
    /// and missing keys — retrying those burns money or spins forever.
    pub fn retryable(&self) -> bool {
        match self {
            Self::Unavailable { .. } => true,
            Self::Refused { status, .. } => is_rate_limit(*status) || *status >= 500,
            Self::Timeout { .. } => true,
            Self::Malformed { .. } => false,
            Self::Cancelled => false,
            Self::MissingKey { .. } => false,
        }
    }

    /// The backend id this error came from, when the variant carries one.
    pub fn backend(&self) -> &str {
        match self {
            Self::Unavailable { backend, .. }
            | Self::Refused { backend, .. }
            | Self::Malformed { backend, .. } => backend,
            Self::Cancelled | Self::Timeout { .. } | Self::MissingKey { .. } => "unknown",
        }
    }
}

/// True for the one status that means "asked too often", not "refused".
pub fn is_rate_limit(status: u16) -> bool {
    status == 429
}

/// Send one progress event. A dropped receiver means the consumer walked away
/// (turn aborted, UI closed) — the only sane reading is cancellation, and
/// generation stops instead of producing output nobody reads.
pub(crate) async fn emit(events: &mpsc::Sender<LlmEvent>, event: LlmEvent) -> Result<(), LlmError> {
    events.send(event).await.map_err(|_| LlmError::Cancelled)
}

impl From<LlmError> for DomainError {
    /// The domain sees one failure mode: a backend that cannot do its job.
    /// The `LlmError` text carries the specifics.
    fn from(error: LlmError) -> Self {
        let backend = error.backend().to_string();
        DomainError::BackendUnavailable {
            backend,
            reason: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(status: u16) -> LlmError {
        LlmError::Refused {
            backend: "inferhub".into(),
            status,
            body: "{}".into(),
        }
    }

    #[test]
    fn retryable_matches_the_transient_permanent_split() {
        assert!(refused(429).retryable());
        assert!(refused(500).retryable());
        assert!(refused(503).retryable());
        assert!(!refused(400).retryable());
        assert!(!refused(401).retryable());
        assert!(!refused(402).retryable());

        assert!(
            LlmError::Unavailable {
                backend: "b".into(),
                reason: "dns".into()
            }
            .retryable()
        );
        assert!(LlmError::Timeout { secs: 30 }.retryable());
        assert!(!LlmError::Cancelled.retryable());
        assert!(!LlmError::MissingKey { env: "K".into() }.retryable());
        assert!(
            !LlmError::Malformed {
                backend: "b".into(),
                reason: "junk".into()
            }
            .retryable()
        );
    }

    #[test]
    fn rate_limit_is_exactly_429() {
        assert!(is_rate_limit(429));
        assert!(!is_rate_limit(503));
        assert!(!is_rate_limit(400));
    }

    #[test]
    fn domain_errors_keep_backend_and_reason() {
        let domain: DomainError = refused(402).into();
        match &domain {
            DomainError::BackendUnavailable { backend, reason } => {
                assert_eq!(backend, "inferhub");
                assert!(reason.contains("402"), "{reason}");
            }
            other => panic!("expected BackendUnavailable, got {other:?}"),
        }
        // Variants without a backend id still map, named `unknown`.
        let domain: DomainError = LlmError::Timeout { secs: 5 }.into();
        assert!(domain.to_string().contains("unknown"), "{domain}");
        assert!(domain.retryable());
    }
}
