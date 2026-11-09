//! Conversations and their transcripts: `/api/conversations`.
//!
//! A DM belongs to its bot and cannot be renamed or deleted — deleting it would
//! either orphan the bot or take its history with it, and both are worse than
//! the 409 that says "disable the bot instead". Groups are the owner's to create,
//! rename and delete, and deleting one takes its transcript, cards, queued
//! deliveries and unfinished tasks with it (see
//! [`swarmbot_core::ConversationService::delete_group`]).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::entry::Entry;
use swarmbot_contracts::id::ConversationId;

use crate::error::{ApiError, ApiResult, parse_id};
use crate::routes::{JsonBody, MAX_PAGE, resolve_bot_refs};
use crate::state::AppState;

/// `POST /api/conversations` body.
#[derive(Clone, Debug, Deserialize)]
pub struct CreateGroupRequest {
    /// Empty means "name it after its members", which beats an untitled group in
    /// a sidebar.
    #[serde(default)]
    pub title: String,
    /// Bot ids; a handle is accepted too, because a caller that just listed the
    /// roster should not have to look an id up to use a name it already has.
    pub members: Vec<String>,
    /// The bot that answers a message nobody was mentioned in; the first member
    /// when absent.
    #[serde(default)]
    pub lead: Option<String>,
}

/// `PATCH /api/conversations/{id}` body.
#[derive(Clone, Debug, Deserialize)]
pub struct RenameRequest {
    pub title: String,
}

/// `?after_seq=&limit=`
#[derive(Clone, Debug, Default, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub after_seq: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `?limit=`
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LimitQuery {
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `GET /api/conversations` — groups and DMs, archived ones left out.
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<Conversation>>> {
    Ok(Json(state.swarm.conversations.list()?))
}

/// `POST /api/conversations`
pub async fn create(
    State(state): State<AppState>,
    JsonBody(body): JsonBody<CreateGroupRequest>,
) -> ApiResult<(StatusCode, Json<Conversation>)> {
    let members = resolve_bot_refs(&state, &body.members)?;
    let lead = match body.lead {
        Some(lead) => Some(resolve_bot_refs(&state, std::slice::from_ref(&lead))?),
        None => None,
    }
    .and_then(|mut resolved| resolved.pop());
    let title = if body.title.trim().is_empty() {
        derived_title(&state, &members)
    } else {
        body.title.trim().to_string()
    };
    let conversation = state
        .swarm
        .conversations
        .create_group(&title, members, lead)?;
    Ok((StatusCode::CREATED, Json(conversation)))
}

/// `GET /api/conversations/{id}`
pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Conversation>> {
    let id = conversation_id(&id)?;
    Ok(Json(state.swarm.conversations.get(&id)?))
}

/// `PATCH /api/conversations/{id}`
pub async fn patch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<RenameRequest>,
) -> ApiResult<Json<Conversation>> {
    let id = conversation_id(&id)?;
    if body.title.trim().is_empty() {
        return Err(ApiError::invalid("a title cannot be empty"));
    }
    Ok(Json(
        state.swarm.conversations.rename(&id, body.title.trim())?,
    ))
}

/// `DELETE /api/conversations/{id}` — groups only; a DM answers 409.
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = conversation_id(&id)?;
    state.swarm.conversations.delete_group(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/conversations/{id}/entries?after_seq=&limit=` — ascending, after a
/// sequence number. This is the paging endpoint a transcript uses to catch up.
pub async fn entries(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<Vec<Entry>>> {
    let id = conversation_id(&id)?;
    let entries = state.swarm.conversations.history(
        &id,
        query.after_seq.unwrap_or(0),
        page_limit(query.limit),
    )?;
    Ok(Json(entries))
}

/// `GET /api/conversations/{id}/tail?limit=` — the newest entries, ascending,
/// which is the order a chat renders them in.
pub async fn tail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LimitQuery>,
) -> ApiResult<Json<Vec<Entry>>> {
    let id = conversation_id(&id)?;
    Ok(Json(
        state
            .swarm
            .conversations
            .tail(&id, page_limit(query.limit))?,
    ))
}

/// A `limit` of 0 or none means "the default page"; anything above the cap is
/// clamped rather than honoured (see [`MAX_PAGE`]).
fn page_limit(limit: Option<u32>) -> u32 {
    match limit {
        None | Some(0) => 0, // the service substitutes its own default
        Some(limit) => limit.min(MAX_PAGE),
    }
}

fn conversation_id(raw: &str) -> ApiResult<ConversationId> {
    parse_id::<ConversationId>("conversation", raw)
}

/// "@lead, @scout" — a title nobody has to think of.
fn derived_title(state: &AppState, members: &[swarmbot_contracts::id::BotId]) -> String {
    let names: Vec<String> = members
        .iter()
        .filter_map(|id| state.swarm.bots.get(id).ok())
        .map(|bot| format!("@{}", bot.name))
        .collect();
    if names.is_empty() {
        "Group".to_string()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_page_size_is_clamped_and_zero_means_default() {
        assert_eq!(page_limit(None), 0);
        assert_eq!(page_limit(Some(0)), 0);
        assert_eq!(page_limit(Some(50)), 50);
        assert_eq!(page_limit(Some(1_000_000)), MAX_PAGE);
    }

    #[test]
    fn the_documented_bodies_parse() {
        let body: CreateGroupRequest = serde_json::from_value(json!({
            "title": "Pair",
            "members": ["bot_0189fbd3a4d37f0e9c0e6a3f8f1c9d2e"],
            "lead": "bot_0189fbd3a4d37f0e9c0e6a3f8f1c9d2e",
        }))
        .expect("parses");
        assert_eq!(body.members.len(), 1);
        assert!(body.lead.is_some());

        // `lead` omitted entirely, which is what the client sends for `None`.
        let body: CreateGroupRequest =
            serde_json::from_value(json!({"title": "Pair", "members": ["a", "b"]}))
                .expect("parses");
        assert!(body.lead.is_none());

        let query: PageQuery =
            serde_json::from_value(json!({"after_seq": 4, "limit": 20})).expect("parses");
        assert_eq!(query.after_seq, Some(4));
        let query: PageQuery = serde_json::from_value(json!({})).expect("parses");
        assert!(query.after_seq.is_none() && query.limit.is_none());

        let rename: RenameRequest =
            serde_json::from_value(json!({"title": "Renamed"})).expect("parses");
        assert_eq!(rename.title, "Renamed");
    }

    #[test]
    fn a_conversation_id_has_to_look_like_one() {
        assert!(conversation_id("nope").is_err());
        let real = ConversationId::new();
        assert!(conversation_id(real.as_str()).is_ok());
    }
}
