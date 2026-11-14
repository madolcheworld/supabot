//! Backends: `/api/backends`, `/api/backends/probe` and
//! `/api/backends/{id}/connect`.
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
//!
//! `connect` exists because "reachable" is not one question. It is the Settings
//! screen's Connect button: one backend, the same handshake `probe` performs
//! (so it spends no model call), plus — when the owner picked one of the logins
//! the backend advertised — that login. Its answer is a row in exactly the
//! shape `probe` uses, produced by the same [`status_row`], so the two routes
//! cannot drift into two different vocabularies for the same fact.

use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
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
            .map(|(_, status)| status_row(&status))
            .collect(),
    ))
}

/// The body of `POST /api/backends/{id}/connect`. Optional: an empty body means
/// "just the handshake", which is what the Connect button sends before the owner
/// has picked a login.
#[derive(Debug, Default, Deserialize)]
pub struct ConnectRequest {
    /// One of the backend's own advertised `auth_methods` ids. Never invented by
    /// the client and never defaulted by this route: an unknown id is reported
    /// by the backend itself, naming what it actually offers.
    #[serde(default)]
    pub method: Option<String>,
}

/// `POST /api/backends/{id}/connect` → one row in the same shape as `probe`.
///
/// One backend, not all of them: a global probe spawns every configured agent,
/// while Connect touches the one the owner is looking at. The answer is a
/// report, not a promise — `ok` means the handshake (and, when a method was
/// named, that login) passed, and a real turn may still be refused (D9.28).
pub async fn connect(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<ConnectRequest>>,
) -> ApiResult<Json<Value>> {
    let method = body
        .and_then(|Json(request)| request.method)
        .filter(|method| !method.trim().is_empty());
    let status = state.swarm.backends.connect(&id, method.as_deref()).await?;
    Ok(Json(status_row(&status)))
}

/// One `BackendStatus` as the wire shape both `probe` and `connect` return.
///
/// A single serializer on purpose: the UI renders these rows with one code path,
/// so a field added to one route and forgotten in the other would become a bug
/// nobody could see from the outside.
fn status_row(status: &swarmbot_llm::BackendStatus) -> Value {
    json!({
        "backend": status.backend,
        "ok": status.ok,
        "detail": status.detail,
        "model": status.model,
        "version": status.version,
        // The logins the backend advertises, verbatim: an id, a human name and
        // — when the agent or the config named one — the exact command. The UI
        // offers these as buttons and never invents a method of its own.
        "auth_methods": status.auth_methods,
        // A code, not prose: `true` means the backend said "log in first"
        // (ACP `-32000`, a missing key, HTTP 401/403). `false` is not "logged
        // in", it is "not refused for that reason".
        "auth_required": status.auth_required,
        // The command the *config* names for logging in. Data, never executed
        // by the brain — an agent login is interactive and belongs to the owner.
        "login_command": status.login_command,
    })
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
