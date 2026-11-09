//! Typed domain errors.
//!
//! Services never panic and never return stringly-typed failures: every
//! fallible port returns [`DomainError`], which the API edges map to HTTP
//! statuses and which the transcript renders as a `notice` entry.

use serde::{Deserialize, Serialize};

use crate::id::{NodeId, RequestId};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DomainError {
    #[error("{what} not found: {id}")]
    NotFound { what: String, id: String },

    #[error("invalid input: {message}")]
    InvalidInput { message: String },

    #[error("conflict: {message}")]
    Conflict { message: String },

    #[error("unauthorized: {message}")]
    Unauthorized { message: String },

    #[error("node {node_id} is unavailable: {reason}")]
    NodeUnavailable { node_id: NodeId, reason: String },

    #[error("backend `{backend}` is unavailable: {reason}")]
    BackendUnavailable { backend: String, reason: String },

    #[error("request {request_id} is not pending")]
    RequestNotPending { request_id: RequestId },

    #[error("internal error: {message}")]
    Internal { message: String },
}

impl DomainError {
    pub fn not_found(what: impl Into<String>, id: impl std::fmt::Display) -> Self {
        Self::NotFound {
            what: what.into(),
            id: id.to_string(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput {
            message: message.into(),
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict {
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }

    /// A stable machine-readable code, used by the HTTP API and by tools that
    /// have to tell a model whether retrying could help.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "not_found",
            Self::InvalidInput { .. } => "invalid_input",
            Self::Conflict { .. } => "conflict",
            Self::Unauthorized { .. } => "unauthorized",
            Self::NodeUnavailable { .. } => "node_unavailable",
            Self::BackendUnavailable { .. } => "backend_unavailable",
            Self::RequestNotPending { .. } => "request_not_pending",
            Self::Internal { .. } => "internal",
        }
    }

    /// Whether the same call could succeed later (offline node, dead backend).
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::NodeUnavailable { .. } | Self::BackendUnavailable { .. } | Self::Internal { .. }
        )
    }
}

/// Convenience constructors for the most common "not found" cases, so callers
/// never have to spell the entity name by hand.
pub mod not_found {
    use super::DomainError;
    use crate::id::{BotId, ConversationId, NodeId, RequestId, TaskId, TurnId};

    pub fn bot(id: &BotId) -> DomainError {
        DomainError::not_found("bot", id)
    }
    pub fn conversation(id: &ConversationId) -> DomainError {
        DomainError::not_found("conversation", id)
    }
    pub fn entry(id: &crate::id::EntryId) -> DomainError {
        DomainError::not_found("entry", id)
    }
    pub fn turn(id: &TurnId) -> DomainError {
        DomainError::not_found("turn", id)
    }
    pub fn task(id: &TaskId) -> DomainError {
        DomainError::not_found("task", id)
    }
    pub fn request(id: &RequestId) -> DomainError {
        DomainError::not_found("request", id)
    }
    pub fn node(id: &NodeId) -> DomainError {
        DomainError::not_found("node", id)
    }
    pub fn routine(id: &crate::id::RoutineId) -> DomainError {
        DomainError::not_found("routine", id)
    }
    pub fn routine_run(id: &crate::id::RunId) -> DomainError {
        DomainError::not_found("routine_run", id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::BotId;

    #[test]
    fn messages_read_on_their_own() {
        let err = not_found::bot(&BotId::from_suffix("0189fbd3a4d37f0e9c0e6a3f8f1c9d2e"));
        assert_eq!(
            err.to_string(),
            "bot not found: bot_0189fbd3a4d37f0e9c0e6a3f8f1c9d2e"
        );
        assert_eq!(err.code(), "not_found");
        assert!(!err.retryable());
    }

    #[test]
    fn node_and_backend_errors_are_retryable() {
        let offline = DomainError::NodeUnavailable {
            node_id: NodeId::new(),
            reason: "socket closed".into(),
        };
        assert!(offline.retryable());
        let dead = DomainError::BackendUnavailable {
            backend: "inferhub".into(),
            reason: "402 no_provider_under_bid".into(),
        };
        assert!(dead.retryable());
        assert_eq!(dead.code(), "backend_unavailable");
    }

    #[test]
    fn serde_is_tagged_and_round_trips() {
        let err = DomainError::invalid("`to` must name a bot");
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["kind"], "invalid_input");
        assert_eq!(
            serde_json::from_value::<DomainError>(json.clone()).unwrap(),
            err
        );
    }
}
