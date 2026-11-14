//! Backends: `/api/backends` and `/api/backends/probe`.
//!
//! The list is the redacted view of every backend that can actually run a bot —
//! [`swarmbot_llm::BackendSpec::public_view`], which names the environment
//! variable a key comes from and never a key. This is the endpoint whose whole
//! job is to be safe to render into a Settings screen, so it lists what the
//! *registry* holds rather than what the config names: `fake` always resolves,
//! even in a brain whose config never mentioned it, and hiding that would make
//! the offline demo look broken.
//!
//! `probe` is a separate call because it can be slow — it dials out to real
//! providers — and a client should be able to render the list first and fill in
//! the reachability afterwards.

use axum::Json;
use axum::extract::{Path, State};
use serde_json::{Value, json};

use swarmbot_contracts::config::BackendSpec;

use crate::error::ApiResult;
use crate::state::AppState;

/// `GET /api/backends` → `[BackendSpec::public_view()]`
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<Value>>> {
    let mut views = Vec::new();
    for info in state.swarm.backends.registry().list() {
        views.push(match state.swarm.backends.spec(&info.id) {
            Some(spec) => spec.public_view(),
            // A backend the registry always provides without a config entry
            // (`fake`): rendered in the same shape, with no key source to name.
            None => implicit_view(&info),
        });
    }
    Ok(Json(views))
}

/// `GET /api/backends/probe` → `[{"backend","ok","detail","model","version"}]`
///
/// One row per backend, including the ones that failed: a Settings screen shows
/// a failure, it does not crash on one.
pub async fn probe(State(state): State<AppState>) -> ApiResult<Json<Vec<Value>>> {
    let statuses = state.swarm.backends.probe_all().await;
    Ok(Json(
        statuses
            .into_iter()
            .map(|(id, status)| {
                json!({
                    "backend": id,
                    "ok": status.ok,
                    "detail": status.detail,
                    "model": status.model,
                    "version": status.version,
                })
            })
            .collect(),
    ))
}

/// `GET /api/backends/{id}/models` → the models the backend itself advertises.
///
/// An empty list is an honest "this backend offers no menu": the client then
/// shows a free-text field instead of a fabricated list. It is a real answer,
/// not a gap — an agent that advertises nothing over ACP and configures no
/// `options.models_command` has no list to give, and inventing one would put
/// models in the menu that the agent would then refuse (opencode used to
/// advertise nothing; as of its 2.x ACP server it does, so its list is no
/// longer the empty case). Like `probe`, this may spawn an agent or dial a
/// gateway, so clients call it with the probe-style timeout.
pub async fn models(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let backend = state.swarm.backends.backend(&id)?;
    let list = backend
        .available_models()
        .await
        .map_err(swarmbot_contracts::DomainError::from)?;
    // Read the flag before consuming the model vec: one value, two owners.
    let authoritative = list.authoritative;
    Ok(Json(json!({
        "models": list
            .models
            .into_iter()
            .map(|model| json!({"id": model.id, "label": model.label}))
            .collect::<Vec<_>>(),
        "authoritative": authoritative,
    })))
}

/// The public-view shape for a backend that has no spec.
///
/// Built from a [`swarmbot_llm::BackendInfo`], which carries no key material at
/// all — only a `ready` flag — so this cannot leak what `public_view` would not.
fn implicit_view(info: &swarmbot_llm::BackendInfo) -> Value {
    json!({
        "id": info.id,
        "kind": info.kind,
        "base_url": Value::Null,
        "api_key_env": Value::Null,
        "model": info.model,
        "extra_body": {},
        "timeout_secs": BackendSpec::fake(&info.id).timeout_secs,
        "options": {},
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::config::{BackendKind, BackendSpec as Spec};

    #[test]
    fn the_implicit_view_has_the_same_keys_as_a_public_view() {
        let spec = Spec::openai_compat("inferhub", "https://x/v1", "KEY_ENV", "m");
        let view = spec.public_view();
        let info = swarmbot_llm::BackendInfo {
            id: "fake".into(),
            kind: BackendKind::Fake,
            model: Some("fake".into()),
            ready: true,
        };
        let implicit = implicit_view(&info);
        let mut view_keys: Vec<&String> = view.as_object().expect("an object").keys().collect();
        let mut implicit_keys: Vec<&String> =
            implicit.as_object().expect("an object").keys().collect();
        view_keys.sort();
        implicit_keys.sort();
        assert_eq!(view_keys, implicit_keys);
        // Neither renders a key: `api_key_env` names where one comes from, and
        // there is no `api_key` field to hold a value.
        for rendered in [&view, &implicit] {
            let object = rendered.as_object().expect("an object");
            assert!(!object.contains_key("api_key"), "{rendered}");
            assert!(!object.contains_key("extra_headers"), "{rendered}");
            assert!(!rendered.to_string().contains("sk-"), "{rendered}");
        }
        assert_eq!(view["api_key_env"], "KEY_ENV", "the env name is shown");
        assert_eq!(implicit["api_key_env"], Value::Null);
    }
}
