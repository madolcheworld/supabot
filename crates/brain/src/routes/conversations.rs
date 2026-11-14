//! Conversations and their transcripts: `/api/conversations`.
//!
//! A DM belongs to its bot and cannot be renamed or deleted — deleting it would
//! either orphan the bot or take its history with it, and both are worse than
//! the 409 that says "disable the bot instead". Groups are the owner's to create,
//! rename and delete, and deleting one takes its transcript, cards, queued
//! deliveries and unfinished tasks with it (see
//! [`swarmbot_core::ConversationService::delete_group`]).
//!
//! Two transcript readers live here, and they answer different questions:
//! [`entries`] pages (forwards with `after_seq`, backwards with `before_seq`), and
//! [`export`] hands the whole conversation over as NDJSON so it can be archived
//! without a client having to loop. [`tail`] is the third: the newest page, which
//! is what a transcript shows on open.

use std::io;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::Response;
use serde::Deserialize;

use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::entry::Entry;
use swarmbot_contracts::id::ConversationId;

use crate::error::{ApiError, ApiResult, parse_id};
use crate::routes::{JsonBody, MAX_PAGE, resolve_bot, resolve_bot_refs};
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
#[derive(Clone, Debug, Default, Deserialize)]
pub struct UpdateGroupRequest {
    /// New group title; empty/whitespace is rejected when present.
    #[serde(default)]
    pub title: Option<String>,
    /// Replacement member list (ids, handles, or labels); preserves duplicates
    /// so domain validation can reject them explicitly.
    #[serde(default)]
    pub members: Option<Vec<String>>,
    /// New lead bot (id, handle, or label); must be in the group's members.
    #[serde(default)]
    pub lead: Option<String>,
}

/// `?after_seq=&before_seq=&limit=`
///
/// `after_seq` and `before_seq` are two directions of the same paging, and
/// exactly one is meaningful per request: a client that opens on the tail walks
/// **backwards** with `before_seq`, and one that is catching up walks
/// **forwards** with `after_seq`. Sending both is a bug in the caller and is
/// refused rather than silently preferring one — a page in the wrong direction
/// looks like missing messages, not like a bad request.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub after_seq: Option<u64>,
    /// Exclusive: only entries with `seq < before_seq`.
    #[serde(default)]
    pub before_seq: Option<u64>,
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
    JsonBody(body): JsonBody<UpdateGroupRequest>,
) -> ApiResult<Json<Conversation>> {
    let id = conversation_id(&id)?;
    let members = body
        .members
        .as_deref()
        .map(|keys| {
            keys.iter()
                .map(|key| resolve_bot(&state, key).map(|bot| bot.id))
                .collect::<ApiResult<Vec<_>>>()
        })
        .transpose()?;
    let lead = body
        .lead
        .as_deref()
        .map(|key| resolve_bot(&state, key).map(|bot| bot.id))
        .transpose()?;
    Ok(Json(
        state
            .swarm
            .conversations
            .update_group(&id, body.title, members, lead)?,
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

/// `GET /api/conversations/{id}/entries?after_seq=&before_seq=&limit=`
///
/// Ascending in both directions. `after_seq` pages forward (the catch-up read),
/// `before_seq` pages **backward** from a cursor the client already holds, which
/// is what makes a transcript that opens on its tail able to scroll into its own
/// past. Both are exclusive, so a page never repeats the row the caller has.
pub async fn entries(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<Vec<Entry>>> {
    let id = conversation_id(&id)?;
    let limit = page_limit(query.limit);
    let entries = match (query.after_seq, query.before_seq) {
        (Some(_), Some(_)) => {
            return Err(ApiError::invalid(
                "send `after_seq` or `before_seq`, not both: they page in opposite \
                 directions and one of the two pages would be the wrong one",
            ));
        }
        (None, Some(before_seq)) => state
            .swarm
            .conversations
            .history_before(&id, before_seq, limit)?,
        (after_seq, None) => {
            state
                .swarm
                .conversations
                .history(&id, after_seq.unwrap_or(0), limit)?
        }
    };
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

/// How many entries one chunk of an export reads, and therefore how large a piece
/// of the response the brain buffers at a time.
const EXPORT_PAGE: u32 = MAX_PAGE;

/// `GET /api/conversations/{id}/export` — the whole transcript, one entry per line.
///
/// `application/x-ndjson`: every line is one [`Entry`], exactly as `/entries`
/// serializes it, so an archive tool needs no second schema and `jq` reads it
/// without flags. Newline-delimited rather than a JSON array because the file is
/// meant to be appended to, tailed and grepped — and because a truncated export
/// then costs one line instead of the whole document.
///
/// It **streams**. The transcript is read in [`EXPORT_PAGE`]-entry pages on the
/// blocking pool and each page is written to the response as soon as it is ready,
/// so exporting a million-line conversation costs one page of memory rather than
/// all of it. That matters because the opposite shape — collect, then answer — is
/// a denial of service any owner can trigger by leaving a bot running for a year.
///
/// The conversation is looked up **before** the first byte: a 404 has to be a 404,
/// and once a 200 has been written a missing conversation would look like a
/// successful empty export.
pub async fn export(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let id = conversation_id(&id)?;
    let wanted = id.clone();
    let exists = state
        .async_store()
        .read(move |store| store.get_conversation(&wanted))
        .await?;
    if exists.is_none() {
        return Err(ApiError::not_found("conversation", id.as_str()));
    }
    let stream = futures::stream::unfold(
        ExportCursor {
            store: state.async_store(),
            conversation: id,
            after_seq: 0,
            done: false,
        },
        next_export_chunk,
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/x-ndjson")
        // An export is the owner's data, so it is not cached anywhere on the way
        // out and not sniffed into something executable.
        .header("cache-control", "no-store")
        .header("x-content-type-options", "nosniff")
        .body(Body::from_stream(stream))
        .map_err(|err| ApiError::internal(format!("cannot build the export response: {err}")))
}

/// Where one export has read up to.
struct ExportCursor {
    store: swarmbot_store::AsyncStore,
    conversation: ConversationId,
    /// Exclusive lower bound of the next page; 0 starts at the first entry.
    after_seq: u64,
    done: bool,
}

/// One page of the export, as bytes — the `unfold` step of [`export`].
///
/// A store failure mid-stream ends the body where it failed. That is the honest
/// outcome: the client's connection ends without its last line and **without**
/// anything that says the export was complete, rather than a body that silently
/// stops at page nine and looks whole. (The response carries no
/// `content-length`, precisely so a truncated transfer is detectable.)
async fn next_export_chunk(
    mut cursor: ExportCursor,
) -> Option<(Result<Bytes, io::Error>, ExportCursor)> {
    if cursor.done {
        return None;
    }
    let store = cursor.store.clone();
    let conversation = cursor.conversation.clone();
    let after = cursor.after_seq;
    let batch = store
        .read(move |store| store.entries_after(&conversation, after, EXPORT_PAGE))
        .await;
    let entries = match batch {
        Ok(entries) => entries,
        Err(err) => {
            cursor.done = true;
            return Some((
                Err(io::Error::other(format!(
                    "the transcript could not be read past entry {after}: {err}"
                ))),
                cursor,
            ));
        }
    };
    if entries.is_empty() {
        return None;
    }
    let mut text = String::new();
    for entry in &entries {
        match serde_json::to_string(entry) {
            Ok(line) => {
                text.push_str(&line);
                text.push('\n');
            }
            Err(err) => {
                // Cannot happen for a contract type, and if it ever does, saying so
                // in the stream is better than ending on a line nobody can parse.
                cursor.done = true;
                return Some((
                    Err(io::Error::other(format!(
                        "entry {} cannot be serialized: {err}",
                        entry.id
                    ))),
                    cursor,
                ));
            }
        }
    }
    if let Some(last) = entries.last() {
        cursor.after_seq = last.seq;
    }
    // A short page is the last one; a full page means ask again. The two cases
    // cannot be confused because the store's page is exact.
    cursor.done = (entries.len() as u32) < EXPORT_PAGE;
    Some((Ok(Bytes::from(text)), cursor))
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
        assert!(query.before_seq.is_none());
        let query: PageQuery = serde_json::from_value(json!({})).expect("parses");
        assert!(query.after_seq.is_none() && query.limit.is_none());

        // The backward cursor is its own parameter, and it parses next to the
        // forward one so a client can send either without a second endpoint.
        let query: PageQuery =
            serde_json::from_value(json!({"before_seq": 7, "limit": 5})).expect("parses");
        assert_eq!(query.before_seq, Some(7));
        assert!(query.after_seq.is_none());

        let rename: UpdateGroupRequest =
            serde_json::from_value(json!({"title": "Renamed"})).expect("parses");
        assert_eq!(rename.title.as_deref(), Some("Renamed"));
        assert!(rename.members.is_none());
        assert!(rename.lead.is_none());

        let full: UpdateGroupRequest = serde_json::from_value(json!({
            "title": "Team",
            "members": ["lead", "dev"],
            "lead": "dev",
        }))
        .expect("parses");
        assert_eq!(full.title.as_deref(), Some("Team"));
        assert_eq!(
            full.members.as_deref(),
            Some(["lead".to_string(), "dev".to_string()].as_slice())
        );
        assert_eq!(full.lead.as_deref(), Some("dev"));
    }

    #[test]
    fn a_conversation_id_has_to_look_like_one() {
        assert!(conversation_id("nope").is_err());
        let real = ConversationId::new();
        assert!(conversation_id(real.as_str()).is_ok());
    }
}
