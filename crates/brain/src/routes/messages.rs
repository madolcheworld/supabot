//! Sending the owner's messages, and stopping them:
//! `/api/conversations/{id}/messages` and `/interrupt`.
//!
//! The response reports *what the routing decided*, not just that the message was
//! stored: `targets` are the bots woken now, `queued` the ones busy elsewhere
//! (their copy waits), and `reason` why — `"mention"`, `"lead"`, `"reply_to"`,
//! … . A group that answers with the wrong bot is otherwise unexplainable, and
//! "the router said lead" turns it into a setting the owner can change.

use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::{Value, json};

use swarmbot_contracts::id::{ConversationId, EntryId};

use crate::error::{ApiError, ApiResult, parse_id};
use crate::routes::{JsonBody, resolve_bot_refs};
use crate::state::AppState;

/// `POST /api/conversations/{id}/messages` body.
#[derive(Clone, Debug, Deserialize)]
pub struct SendMessageRequest {
    pub text: String,
    /// The bots the UI named explicitly; empty means "route from the text".
    #[serde(default)]
    pub addressed: Vec<String>,
    /// The entry being replied to, which routes to its author.
    #[serde(default)]
    pub reply_to: Option<EntryId>,
}

/// `POST /api/conversations/{id}/messages`
pub async fn send(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<SendMessageRequest>,
) -> ApiResult<Json<Value>> {
    let conversation = parse_id::<ConversationId>("conversation", &id)?;
    // Checked here rather than left to the orchestrator so the refusal names the
    // field a client can fix: a whitespace-only message is the common case, from
    // a UI that let Enter through on an empty composer.
    if body.text.trim().is_empty() {
        return Err(ApiError::invalid("`text` cannot be empty"));
    }
    let addressed = resolve_bot_refs(&state, &body.addressed)?;
    let sent = state.swarm.orchestrator.send_owner_message(
        &conversation,
        &body.text,
        &addressed,
        body.reply_to.as_ref(),
    )?;
    Ok(Json(json!({
        "entry": sent.entry,
        "targets": sent.route.targets,
        "queued": sent.queued,
        // The serde form of `RouteReason`: "lead", "mention", "everyone",
        // "reply_to", "addressed", "none".
        "reason": sent.route.reason,
    })))
}

/// `POST /api/conversations/{id}/interrupt`
///
/// `stopped` is honest: `false` means nothing was running, not that stopping
/// failed.
pub async fn interrupt(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let conversation = parse_id::<ConversationId>("conversation", &id)?;
    let stopped = state.swarm.orchestrator.interrupt(&conversation)?;
    Ok(Json(json!({ "stopped": stopped })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_body_takes_the_documented_fields() {
        let body: SendMessageRequest = serde_json::from_value(json!({
            "text": "hi",
            "addressed": ["bot_0189fbd3a4d37f0e9c0e6a3f8f1c9d2e"],
            "reply_to": "ent_0189fbd3a4d37f0e9c0e6a3f8f1c9d2e",
        }))
        .expect("parses");
        assert_eq!(body.text, "hi");
        assert_eq!(body.addressed.len(), 1);
        assert!(body.reply_to.is_some());

        // The client omits `reply_to` when it is None.
        let body: SendMessageRequest =
            serde_json::from_value(json!({"text": "hi", "addressed": []})).expect("parses");
        assert!(body.reply_to.is_none());
        assert!(body.addressed.is_empty());

        // An id type is transparent over a string, so a malformed `reply_to`
        // parses here and is ignored by the lookup — the message is still sent,
        // which is better than refusing it over a stale reply target.
        let body: SendMessageRequest =
            serde_json::from_value(json!({"text": "hi", "reply_to": "ent_nope"})).expect("parses");
        assert_eq!(body.reply_to.expect("an id").as_str(), "ent_nope");
    }
}
