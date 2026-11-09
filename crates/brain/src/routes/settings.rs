//! Settings: `/api/settings`.
//!
//! `PATCH` is a JSON merge, not a replacement: only the keys present change. That
//! matters because these values include the bot-messaging kill switch and the
//! safety ceilings — a client that had to send the whole object would have to
//! read it first, and a stale read would silently roll back something the owner
//! changed a moment ago in another window.
//!
//! The merged value is validated before it is written, so a `max_hops` of 0 or a
//! `compact_at_fraction` of 2 is a 400 rather than a brain that cannot delegate.

use axum::Json;
use axum::extract::State;
use serde_json::Value;
use tracing::info;

use swarmbot_contracts::config::Settings;

use crate::config::merge_settings;
use crate::error::ApiResult;
use crate::routes::JsonBody;
use crate::state::AppState;

/// `GET /api/settings`
pub async fn get(State(state): State<AppState>) -> ApiResult<Json<Settings>> {
    Ok(Json(state.swarm.settings.get()))
}

/// `PATCH /api/settings`
pub async fn patch(
    State(state): State<AppState>,
    JsonBody(patch): JsonBody<Value>,
) -> ApiResult<Json<Settings>> {
    let current = state.swarm.settings.get();
    let merged = merge_settings(&current, &patch)?;
    // `update` persists and refreshes the cached copy in one step, so the next
    // turn sees the new ceiling without a restart.
    let saved = state.swarm.settings.update(|slot| *slot = merged)?;
    info!(changed = %changed_keys(&current, &saved), "settings changed through the API");
    Ok(Json(saved))
}

/// The keys that actually changed, for one log line. Values are left out: a
/// ceiling is not a secret, but a settings dump in a log is noise either way.
fn changed_keys(before: &Settings, after: &Settings) -> String {
    let (Ok(before), Ok(after)) = (serde_json::to_value(before), serde_json::to_value(after))
    else {
        return "<unrenderable>".to_string();
    };
    let (Some(before), Some(after)) = (before.as_object(), after.as_object()) else {
        return "<unrenderable>".to_string();
    };
    let changed: Vec<&str> = after
        .iter()
        .filter(|(key, value)| before.get(*key) != Some(*value))
        .map(|(key, _)| key.as_str())
        .collect();
    if changed.is_empty() {
        "nothing".to_string()
    } else {
        changed.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::config::validate_settings;

    #[test]
    fn the_log_names_the_keys_that_moved() {
        let before = Settings::default();
        let mut after = before.clone();
        after.max_hops = 3;
        after.bot_messaging = false;
        let changed = changed_keys(&before, &after);
        assert!(changed.contains("bot_messaging"), "{changed}");
        assert!(changed.contains("max_hops"), "{changed}");
        assert!(!changed.contains("max_tool_rounds"), "{changed}");
        assert_eq!(changed_keys(&before, &before), "nothing");
    }

    #[test]
    fn the_defaults_are_valid() {
        assert!(validate_settings(&Settings::default()).is_ok());
    }

    #[test]
    fn a_merge_changes_only_what_it_names() {
        let merged = merge_settings(&Settings::default(), &json!({"max_hops": 3})).expect("merges");
        assert_eq!(merged.max_hops, 3);
        assert_eq!(merged.max_tool_rounds, Settings::default().max_tool_rounds);
    }
}
