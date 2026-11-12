//! The roster: `/api/bots`.
//!
//! Two things this module has to get right that a plain CRUD layer would miss:
//!
//! * **a new bot needs a runner.** [`swarmbot_core::BotService::create`] writes
//!   the bot and its DM; nothing spawns its runner. Without
//!   [`swarmbot_core::Swarm::ensure_runners`] right after, a message to that bot
//!   would park forever and `wait_for_idle` would never return — which looks
//!   exactly like a hung brain.
//! * **a handle is an address, not a label.** Renaming a bot would break every
//!   `@mention` in flight, so `PATCH` refuses a name change with 409 rather than
//!   quietly rewriting history.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use swarmbot_contracts::bot::{Bot, normalize_name};
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::NodeId;
use swarmbot_contracts::tool::Toolset;
use swarmbot_core::NewBot;

use crate::error::{ApiError, ApiResult};
use crate::routes::{JsonBody, node_id, resolve_bot};
use crate::state::AppState;

/// `POST /api/bots` body.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewBotRequest {
    /// Lowercased into a handle: `@Scout Researcher` becomes `scout-researcher`.
    pub name: String,
    /// Display name; the handle is used when this is empty.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub instructions: String,
    /// Backend id; the configured default when absent.
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// `#rrggbb`; generated from the bot id when absent.
    #[serde(default)]
    pub color: Option<String>,
    /// A node id; the local node when absent.
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub toolsets: Option<Vec<Toolset>>,
    #[serde(default)]
    pub skills: Vec<String>,
}

/// `PATCH /api/bots/{id}` body: every field optional, only what is present
/// changes.
///
/// `model`, `color` and `node` are cleared with an empty string, because JSON
/// `null` and "absent" are the same thing to a `Option<String>` field and a
/// client has to be able to say "go back to the default".
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchBotRequest {
    /// Accepted for compatibility, refused when it would actually change the
    /// handle.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub toolsets: Option<Vec<Toolset>>,
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    /// `false` disables the bot: it is not woken by anything and its runner is
    /// shut down.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `POST /api/bots/{id}/session` body.
#[derive(Clone, Debug, Deserialize)]
pub struct SessionRequest {
    pub action: SessionAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAction {
    /// Forget the model's context; the read cursors stay, so the bot does not
    /// re-read a transcript it has already seen.
    New,
    /// Fold the old part of the history into a summary.
    Compact,
}

/// `GET /api/bots`
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<Bot>>> {
    Ok(Json(state.swarm.bots.list()?))
}

/// `POST /api/bots`
pub async fn create(
    State(state): State<AppState>,
    JsonBody(body): JsonBody<NewBotRequest>,
) -> ApiResult<(StatusCode, Json<Bot>)> {
    let backend = body
        .backend
        .clone()
        .unwrap_or_else(|| state.config.config.default_backend.clone());
    check_backend(&state, &backend)?;
    let node = match body.node {
        Some(node) => Some(resolve_node(&state, &node)?),
        None => None,
    };
    let mut bot = state.swarm.bots.create(NewBot {
        name: body.name.clone(),
        label: body.label,
        instructions: body.instructions,
        backend,
        model: body.model,
        color: body.color,
        node,
        toolsets: body.toolsets,
    })?;
    // `NewBot` has no skills field, so a roster skill list is applied through the
    // same update path the API uses — one writer, one published revision.
    if !body.skills.is_empty() {
        bot.skills = body.skills;
        bot = state.swarm.bots.update(&bot)?;
    }
    state.swarm.ensure_bot_workspace(&bot.name)?;
    Ok((StatusCode::CREATED, finish(&state, bot, true)?))
}

/// `GET /api/bots/{id}` — by id, handle or label.
pub async fn get(State(state): State<AppState>, Path(key): Path<String>) -> ApiResult<Json<Bot>> {
    Ok(Json(resolve_bot(&state, &key)?))
}

/// `PATCH /api/bots/{id}`
pub async fn patch(
    State(state): State<AppState>,
    Path(key): Path<String>,
    JsonBody(body): JsonBody<PatchBotRequest>,
) -> ApiResult<Json<Bot>> {
    let bot = resolve_bot(&state, &key)?;
    if let Some(name) = &body.name
        && normalize_name(name) != bot.name
    {
        return Err(ApiError::conflict(
            "a bot's handle cannot change while the team uses it",
        ));
    }
    let mut updated = bot.clone();
    if let Some(label) = body.label {
        updated.label = label;
    }
    if let Some(instructions) = body.instructions {
        updated.instructions = instructions;
    }
    if let Some(backend) = body.backend {
        check_backend(&state, &backend)?;
        updated.backend = backend;
    }
    if let Some(model) = body.model {
        updated.model = optional(model);
    }
    if let Some(color) = body.color {
        updated.color = match optional(color) {
            Some(color) => {
                check_color(&color)?;
                Some(color)
            }
            None => None,
        };
    }
    if let Some(node) = body.node {
        updated.node = match optional(node) {
            Some(node) => Some(resolve_node(&state, &node)?),
            None => None,
        };
    }
    if let Some(toolsets) = body.toolsets {
        updated.toolsets = toolsets;
    }
    if let Some(skills) = body.skills {
        updated.skills = skills;
    }
    let saved = state.swarm.bots.update(&updated)?;
    // Disabling shuts down the runner; `finish` below calls `ensure_runners`,
    // so re-enabling restarts it.
    let saved = match body.enabled {
        Some(enabled) => state.swarm.bots.set_enabled(&saved.id, enabled)?,
        None => saved,
    };
    finish(&state, saved, false)
}

/// `DELETE /api/bots/{id}`
///
/// The bot's transcript stays: deleting the record of what a team did would make
/// the history a lie.
pub async fn delete(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> ApiResult<StatusCode> {
    let bot = resolve_bot(&state, &key)?;
    state.swarm.bots.delete(&bot.id)?;
    info!(bot = %bot.name, "bot deleted through the API");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/bots/{id}/session`
///
/// Both actions are refused with 409 while the bot is working: folding a session
/// under a running turn would drop what that turn is about to write.
pub async fn session_action(
    State(state): State<AppState>,
    Path(key): Path<String>,
    JsonBody(body): JsonBody<SessionRequest>,
) -> ApiResult<Json<Value>> {
    let bot = resolve_bot(&state, &key)?;
    let folded = match body.action {
        SessionAction::New => {
            state.swarm.bots.new_session(&bot.id)?;
            info!(bot = %bot.name, "a bot's model session was reset");
            0
        }
        SessionAction::Compact => {
            let folded = state.swarm.bots.compact(&bot.id)?;
            info!(bot = %bot.name, folded, "a bot's session was compacted");
            folded
        }
    };
    Ok(Json(json!({ "folded": folded })))
}

/// Persist, make sure the bot has a runner, and render it.
fn finish(state: &AppState, bot: Bot, created: bool) -> ApiResult<Json<Bot>> {
    // `ensure_runners` is idempotent and cheap; skipping it is what turns a
    // freshly created bot into a bot that can be messaged but never answers.
    state.swarm.ensure_runners()?;
    if created {
        info!(bot = %bot.name, backend = %bot.backend, "bot created through the API");
    }
    Ok(Json(bot))
}

/// A backend a bot can actually run on.
///
/// The registry is consulted rather than the config, because `fake` always
/// resolves — a bot on `fake` works in a brain that has no credentials at all.
fn check_backend(state: &AppState, backend: &str) -> ApiResult<()> {
    if state.swarm.backends.registry().get(backend).is_err() {
        let known = state.swarm.backends.registry().list();
        let known: Vec<&str> = known.iter().map(|info| info.id.as_str()).collect();
        return Err(ApiError::from(DomainError::invalid(format!(
            "backend `{backend}` is not configured; the known backends are {}",
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        ))));
    }
    Ok(())
}

/// A node that exists. Binding a bot to a node that never paired would leave
/// every one of its tool calls failing with `node_unavailable`, later and
/// confusingly.
fn resolve_node(state: &AppState, node: &str) -> ApiResult<NodeId> {
    // `nod_local` is a valid node but not a parseable typed id, so this goes
    // through the shared resolver rather than `NodeId::parse`.
    let id = match node_id(node) {
        Ok(id) => id,
        Err(_) => {
            return Err(ApiError::invalid(format!(
                "`{node}` is not a node id; use one from GET /api/nodes"
            )));
        }
    };
    let exists = state
        .swarm
        .store
        .get_node(&id)?
        .is_some_and(|found| !found.revoked);
    if !exists {
        return Err(ApiError::not_found("node", node));
    }
    Ok(id)
}

/// An empty string means "clear this optional field".
fn optional(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn check_color(color: &str) -> ApiResult<()> {
    let valid = color.len() == 7
        && color.starts_with('#')
        && color[1..].chars().all(|c| c.is_ascii_hexdigit());
    if !valid {
        return Err(ApiError::invalid(format!(
            "`{color}` is not a #rrggbb colour"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_string_clears_an_optional_field() {
        assert_eq!(optional("  ".to_string()), None);
        assert_eq!(optional("#123456".to_string()).as_deref(), Some("#123456"));
    }

    #[test]
    fn colours_have_to_be_hex_triplets() {
        assert!(check_color("#123456").is_ok());
        assert!(check_color("#1234567").is_err());
        assert!(check_color("123456").is_err());
        assert!(check_color("#gggggg").is_err());
    }

    #[test]
    fn the_session_actions_are_the_documented_words() {
        let parsed: SessionRequest =
            serde_json::from_value(json!({"action": "compact"})).expect("parses");
        assert_eq!(parsed.action, SessionAction::Compact);
        let parsed: SessionRequest =
            serde_json::from_value(json!({"action": "new"})).expect("parses");
        assert_eq!(parsed.action, SessionAction::New);
        assert!(serde_json::from_value::<SessionRequest>(json!({"action": "wipe"})).is_err());
    }

    #[test]
    fn a_create_body_takes_the_documented_fields() {
        let body: NewBotRequest = serde_json::from_value(json!({
            "name": "scout",
            "label": "Scout",
            "instructions": "explore",
            "backend": "fake",
            "model": "fake",
            "color": "#a78bfa",
            "toolsets": ["bot", "node"],
        }))
        .expect("parses");
        assert_eq!(body.name, "scout");
        assert_eq!(body.toolsets.expect("toolsets").len(), 2);
        assert!(body.node.is_none());

        // A field this API does not document is refused rather than dropped, so
        // a client typo surfaces as a 400 instead of a bot that silently lacks
        // what its author asked for.
        assert!(
            serde_json::from_value::<NewBotRequest>(json!({
                "name": "scout", "personality": "keen"
            }))
            .is_err()
        );
    }

    #[test]
    fn a_patch_body_may_be_empty() {
        let body: PatchBotRequest = serde_json::from_value(json!({})).expect("parses");
        assert!(body.label.is_none() && body.enabled.is_none());
        let body: PatchBotRequest =
            serde_json::from_value(json!({"enabled": false})).expect("parses");
        assert_eq!(body.enabled, Some(false));
    }
}
