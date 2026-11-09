//! Runtime requests: `/api/requests`.
//!
//! A request is a card a bot raised — an approval, a question, a form, a secret
//! or a hand-off — and this route is the owner's answer to it. The service does
//! the work (it writes the rule an `allow_always` implies, updates the card
//! entry, and wakes the bot with the answer); the API's job is only to be a
//! faithful doorway to [`swarmbot_core::Orchestrator::respond_request`].
//!
//! Answering twice is a 409 with code `request_not_pending`, not a silent
//! overwrite: the first answer already woke a bot, and pretending the second one
//! happened would be a lie about what the team was told.

use axum::Json;
use axum::extract::{Path, State};
use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;

use swarmbot_contracts::id::RequestId;
use swarmbot_contracts::request::{Decision, RuntimeRequest};

use crate::error::{ApiResult, parse_id};
use crate::gateway::RecordedDecision;
use crate::routes::JsonBody;
use crate::state::AppState;

/// `POST /api/requests/{id}/respond` body.
///
/// Both keys are optional *and* nullable, which is what the client sends: an
/// approval carries a `decision` and a null `answer`, a question the other way
/// round.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RespondRequest {
    #[serde(default)]
    pub decision: Option<Decision>,
    #[serde(default)]
    pub answer: Option<Value>,
}

/// `GET /api/requests` — everything still waiting for an answer.
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<RuntimeRequest>>> {
    Ok(Json(state.swarm.approval.pending()?))
}

/// `POST /api/requests/{id}/respond`
pub async fn respond(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<RespondRequest>,
) -> ApiResult<Json<RuntimeRequest>> {
    let id = parse_id::<RequestId>("request", &id)?;
    let decision = body.decision;
    let request = state
        .swarm
        .orchestrator
        .respond_request(&id, decision, body.answer)?;
    // Remember what the owner decided, so a node that asks about the identical
    // action moments later — which it will, because policy is enforced twice
    // (D1.4) and the node cannot know the brain already asked — is answered from
    // this decision instead of with a second card for the same question.
    if let (Some(decision), Some(surface), Some(target)) =
        (decision, request.surface, request.target.clone())
    {
        state.gateway.note_owner_decision(RecordedDecision {
            surface,
            target,
            decision,
            at: Utc::now(),
            conversation: request.conversation_id.clone(),
        });
    }
    Ok(Json(request))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_respond_body_accepts_nulls_and_absences_alike() {
        // What the client sends for a question it answers with JSON.
        let body: RespondRequest =
            serde_json::from_value(json!({"decision": null, "answer": {"port": 8080}}))
                .expect("parses");
        assert!(body.decision.is_none());
        assert_eq!(body.answer, Some(json!({"port": 8080})));

        // An approval.
        let body: RespondRequest =
            serde_json::from_value(json!({"decision": "allow_always", "answer": null}))
                .expect("parses");
        assert_eq!(body.decision, Some(Decision::AllowAlways));

        // Every documented decision word.
        for (word, expected) in [
            ("allow_once", Decision::AllowOnce),
            ("allow_always", Decision::AllowAlways),
            ("deny", Decision::Deny),
        ] {
            let body: RespondRequest =
                serde_json::from_value(json!({"decision": word})).expect("parses");
            assert_eq!(body.decision, Some(expected), "{word}");
        }

        // An empty body is not a malformed one: it answers "yes, no comment".
        let body: RespondRequest = serde_json::from_value(json!({})).expect("parses");
        assert!(body.decision.is_none() && body.answer.is_none());
        assert!(serde_json::from_value::<RespondRequest>(json!({"decision": "maybe"})).is_err());
    }
}
