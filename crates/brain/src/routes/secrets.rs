//! Credentials: `GET/PUT/DELETE /api/secrets` — names in, names out, never a
//! value.
//!
//! Before this, rotating an API key meant editing `<data>/secrets.json` by hand
//! and restarting. The route closes that, and it is built around one rule that is
//! worth more than the convenience: **no response carries a secret value.** `GET`
//! lists names; `PUT` and `DELETE` answer the name and what it took for the change
//! to become effective. There is no read-back endpoint, not even a masked one — a
//! masked key still leaks its length and prefix, and a client that can display
//! "sk-…abcd" will eventually paste it somewhere.
//!
//! Three consequences the wire shapes make explicit rather than hide:
//!
//! * **the file is written atomically, 0600 from the first byte**
//!   ([`crate::config::write_secret_file`]). A crash mid-write leaves the old file
//!   intact; there is no moment when the key is world-readable or half-written.
//! * **`applies` tells the truth.** The backend registry resolves each key
//!   **once**, while the swarm is composed (`BackendRegistry::with_transport` →
//!   `resolve_key`, and each adapter keeps the key in a private field). Nothing in
//!   this brain re-reads `<data>/secrets.json` afterwards, so a changed value
//!   cannot reach a live adapter: the answer is `"restart"`, and the doc of
//!   [`Applies`] says why. The value *is* registered with the log scrubber
//!   immediately — that part is hot, and it is the part that protects the key
//!   from being written to a log line in the meantime.
//! * **a name can be provided twice** (the file, and the brain's own environment).
//!   The file wins at boot (`load_secrets`), so `PUT` can override an
//!   environment-provided key — but `DELETE` cannot remove one, because a process
//!   cannot unset its own environment. That is answered with a 409 naming the
//!   remedy instead of a cheerful 200 for a key that is still in use.
//!
//! The file is the source the brain actually loads, so `GET` reports the union of
//! the file and the environment-provided names for the configured backends: a key
//! that is in use but invisible on the credentials screen is how an owner ends up
//! believing a rotation took effect when it did not.

use std::collections::BTreeMap;
use std::sync::Mutex;

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use swarmbot_contracts::config::BackendSpec;

use crate::config::{read_secret_file, write_secret_file};
use crate::error::{ApiError, ApiResult};
use crate::routes::JsonBody;
use crate::state::AppState;

/// Serializes the read-modify-write of one file.
///
/// The brain is the only writer of its own data dir, but *requests* are
/// concurrent: two `PUT`s that both read the file, each add their own name and
/// each write the whole map back would lose one of the two. A process-wide lock
/// held inside the blocking closure — never across an `await` — is enough, and
/// keeps the file-format knowledge in one place rather than growing an `AppState`
/// field for it.
static SECRET_WRITES: Mutex<()> = Mutex::new(());

/// The longest a name may be. It names an environment variable, and 128 characters
/// is far past any real `*_API_KEY` while still refusing a body that is trying to
/// be a paragraph.
const MAX_NAME_CHARS: usize = 128;

/// The longest a value may be. Keys are short; anything near this is a mistake
/// worth naming before it lands in a file the owner has to read.
const MAX_VALUE_BYTES: usize = 16 * 1024;

/// One credential name, with the backends that ask for it.
///
/// `used_by` is the backend id from the brain's configuration whose
/// `api_key_env` is this name — the answer to "what breaks if I delete this?".
/// An empty list is a real answer: a key written for a backend that has since been
/// removed from the config is still in the file, and a `GET` that hid it would
/// leave the owner unable to delete it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretName {
    pub name: String,
    #[serde(default)]
    pub used_by: Vec<String>,
}

/// What a write actually did, in the only terms that are true.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretWrite {
    pub name: String,
    /// `"restart"` — see [`Applies`]. Kept a plain string because it is display
    /// text on two front ends, and a closed enum would make a *future* `"hot"`
    /// a decode failure on an old client instead of a word it does not know.
    pub applies: String,
}

/// Why `applies` is `"restart"` and not `"hot"`.
///
/// A named constant pair instead of two string literals, so a future change that
/// really does make a key live has one place to say so — and so no route can
/// answer `"hot"` by accident. The registry resolves every key once, during
/// composition (`swarmbot_llm::BackendRegistry::with_transport` →
/// `resolve_key`), into an adapter's private field. Editing the file therefore
/// changes what the *next* process will use; nothing currently running re-reads
/// it. Saying `"hot"` would be the exact class of lie `AGENTS.md` forbids: a
/// success the caller cannot observe until it restarts and forgets to check.
pub const APPLIES_RESTART: &str = "restart";

/// `PUT /api/secrets` body.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutSecretRequest {
    /// The environment-variable name the backends name.
    pub name: String,
    /// The value. Never logged, never echoed back, never stored anywhere but the
    /// 0600 file and the log scrubber.
    pub value: String,
}

/// `GET /api/secrets` — names only.
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<SecretName>>> {
    let paths = state.config.paths.clone();
    let from_file = tokio::task::spawn_blocking(move || read_secret_file(&paths))
        .await
        .map_err(|err| ApiError::internal(format!("the secrets read did not finish: {err}")))?
        .map_err(|err| {
            // A malformed file is an error, not an empty list: "you have no keys"
            // and "your key file is broken" lead an owner to opposite next steps.
            ApiError::internal(format!("cannot read the secrets file: {err:#}"))
        })?;
    let mut names: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in from_file.keys() {
        names.insert(name.clone(), used_by(&state, name));
    }
    // A name the *environment* provides is a credential this brain is using, so it
    // belongs on the list even though it is not in the file. Ignoring it would make
    // GET answer "you have one key" for a brain running on three.
    for name in environment_names(&state) {
        names
            .entry(name.clone())
            .or_insert_with(|| used_by(&state, &name));
    }
    Ok(Json(
        names
            .into_iter()
            .map(|(name, used_by)| SecretName { name, used_by })
            .collect(),
    ))
}

/// `PUT /api/secrets` — add or replace one credential.
///
/// Replaces, not appends: rotating a key means the old one must stop being what
/// the next boot reads. Other names in the file are kept, including ones this
/// brain's configuration no longer mentions — deleting somebody else's entry
/// because it looked unused would be the API throwing away an owner's data.
pub async fn put_secret(
    State(state): State<AppState>,
    JsonBody(body): JsonBody<PutSecretRequest>,
) -> ApiResult<Json<SecretWrite>> {
    let name = validated_name(&body.name)?;
    let value = validated_value(&body.value)?;
    let paths = state.config.paths.clone();
    let (key, secret) = (name.clone(), value.clone());
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let _guard = SECRET_WRITES.lock().unwrap_or_else(|e| e.into_inner());
        let mut secrets =
            read_secret_file(&paths).map_err(|err| std::io::Error::other(format!("{err:#}")))?;
        secrets.insert(key, secret);
        write_secret_file(&paths, &secrets)
    })
    .await
    .map_err(|err| ApiError::internal(format!("the secrets write did not finish: {err}")))?
    .map_err(|err| ApiError::internal(format!("cannot write the secrets file: {err}")))?;
    // Registered *before* the value can appear anywhere else in this process: a
    // scrubber that learns a key after the first log line mentioning it has already
    // lost. Too-short values cannot be scrubbed (`SecretRegistry::register` ignores
    // them, because scrubbing "abc" would redact ordinary words), and that is said
    // out loud in the log rather than left as a silent hole.
    if value.trim().len() < 8 {
        tracing::warn!(
            key = %name,
            "this secret is shorter than 8 characters, so the log scrubber will not \
             redact it; keys that short are also unlikely to be real credentials"
        );
    }
    state.secrets.register(value);
    tracing::info!(
        key = %name,
        path = %state.config.paths.secrets().display(),
        applies = APPLIES_RESTART,
        "a credential was written; the file is 0600 and atomic, and the running \
         backend registry keeps the old value until a restart"
    );
    Ok(Json(SecretWrite {
        name,
        applies: APPLIES_RESTART.to_string(),
    }))
}

/// `DELETE /api/secrets/{name}` — remove one credential from the file.
pub async fn delete_secret(
    State(state): State<AppState>,
    Path(raw): Path<String>,
) -> ApiResult<Json<SecretWrite>> {
    let name = validated_name(&raw)?;
    // Refused when the environment also provides this key: a 200 here would tell
    // the owner a credential is gone while the brain keeps using it.
    if std::env::var(&name).is_ok_and(|value| !value.trim().is_empty()) {
        return Err(ApiError::conflict(format!(
            "`{name}` is also set in this brain's environment, which the API cannot \
             change: unset it in the process environment and restart, or leave the \
             file's copy in place. Nothing was deleted."
        )));
    }
    let paths = state.config.paths.clone();
    let wanted = name.clone();
    let removed = tokio::task::spawn_blocking(move || -> Result<bool, String> {
        let _guard = SECRET_WRITES.lock().unwrap_or_else(|e| e.into_inner());
        let mut secrets = read_secret_file(&paths).map_err(|err| format!("{err:#}"))?;
        let removed = secrets.remove(&wanted).is_some();
        if removed {
            write_secret_file(&paths, &secrets).map_err(|err| err.to_string())?;
        }
        Ok(removed)
    })
    .await
    .map_err(|err| ApiError::internal(format!("the secrets write did not finish: {err}")))?
    .map_err(|err| ApiError::internal(format!("cannot write the secrets file: {err}")))?;
    if !removed {
        // Not "unknown name": the name is valid, the brain simply does not hold it.
        // 404 keeps a retried delete from looking like it removed something new.
        return Err(ApiError::not_found("secret", &name));
    }
    state.secrets.forget(&name);
    tracing::info!(
        key = %name,
        applies = APPLIES_RESTART,
        "a credential was removed from the file; the running backend registry keeps \
         the old value until a restart"
    );
    Ok(Json(SecretWrite {
        name,
        applies: APPLIES_RESTART.to_string(),
    }))
}

/// The backends whose `api_key_env` is this name.
fn used_by(state: &AppState, name: &str) -> Vec<String> {
    let mut used: Vec<String> = state
        .config
        .config
        .backends
        .iter()
        .filter(|spec: &&BackendSpec| spec.api_key_env.as_deref() == Some(name))
        .map(|spec| spec.id.clone())
        .collect();
    used.sort();
    used
}

/// The secret names this process inherited from its environment.
///
/// Only the ones a configured backend names: the environment holds hundreds of
/// variables and none of the others are this brain's credentials.
fn environment_names(state: &AppState) -> Vec<String> {
    let mut names: Vec<String> = state
        .config
        .config
        .backends
        .iter()
        .filter_map(|spec| spec.api_key_env.clone())
        .filter(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// A name that can be an environment variable, because that is what it is.
fn validated_name(raw: &str) -> ApiResult<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(ApiError::invalid(
            "a secret needs a name: it is the environment variable a backend names",
        ));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(ApiError::invalid(format!(
            "a secret name is at most {MAX_NAME_CHARS} characters; got {}",
            name.chars().count()
        )));
    }
    let first = name.chars().next().unwrap_or('_');
    let ok = !first.is_ascii_digit()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
    if !ok {
        return Err(ApiError::invalid(format!(
            "`{name}` is not an environment-variable name: use letters, digits and \
             `_` (a dot is tolerated for `a.b`-style names) and do not start with a \
             digit"
        )));
    }
    Ok(name.to_string())
}

/// A value worth writing down.
fn validated_value(raw: &str) -> ApiResult<String> {
    if raw.trim().is_empty() {
        // `load_secrets` treats an empty value as absent, so storing one would be
        // a delete dressed as a write.
        return Err(ApiError::invalid(
            "a secret value cannot be empty: an empty value is treated as absent at \
             load time, so write the key or delete the name",
        ));
    }
    if raw.len() > MAX_VALUE_BYTES {
        return Err(ApiError::invalid(format!(
            "a secret value is at most {MAX_VALUE_BYTES} bytes; got {}",
            raw.len()
        )));
    }
    Ok(raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_has_to_look_like_an_environment_variable() {
        assert_eq!(
            validated_name("  INFERHUB_API_KEY ").expect("trims"),
            "INFERHUB_API_KEY"
        );
        assert!(validated_name("A").is_ok());
        assert!(validated_name("a.b").is_ok());
        for bad in ["", "   ", "\n", "2FA_KEY", "MY KEY", "MY-KEY", "KEY=VALUE"] {
            let err = validated_name(bad).expect_err("must be refused");
            assert_eq!(err.code(), "invalid_input", "{bad:?}");
        }
        // Surrounding whitespace is a paste artifact and is trimmed, not refused.
        assert_eq!(validated_name("KEY\n").expect("trimmed"), "KEY");
        let long = "K".repeat(MAX_NAME_CHARS + 1);
        assert!(validated_name(&long).is_err());
        assert!(validated_name(&"K".repeat(MAX_NAME_CHARS)).is_ok());
    }

    #[test]
    fn an_empty_value_is_refused_because_it_would_read_as_a_delete() {
        let err = validated_value("  \n ").expect_err("refused");
        assert_eq!(err.code(), "invalid_input");
        // Surrounding whitespace is *kept*: trimming a key would silently change a
        // credential, and the load path does not trim either.
        assert_eq!(
            validated_value(" sk-live-123 ").expect("kept"),
            " sk-live-123 "
        );
        assert!(validated_value(&"x".repeat(MAX_VALUE_BYTES + 1)).is_err());
        assert!(validated_value(&"x".repeat(MAX_VALUE_BYTES)).is_ok());
    }

    #[test]
    fn varies_are_only_ever_the_restart_word() {
        // A test that pins the honest answer: if somebody makes keys live without
        // rebuilding the registry, this constant is where they must say so.
        assert_eq!(APPLIES_RESTART, "restart");
    }

    #[test]
    fn the_body_refuses_a_field_it_does_not_document() {
        let parsed: PutSecretRequest =
            serde_json::from_value(serde_json::json!({"name": "K", "value": "v"})).expect("parses");
        assert_eq!(parsed.name, "K");
        assert!(
            serde_json::from_value::<PutSecretRequest>(
                serde_json::json!({"name": "K", "value": "v", "hot": true})
            )
            .is_err(),
            "a typo must be a 400, not a silently ignored field"
        );
    }

    #[test]
    fn a_name_list_answer_never_carries_a_value_field() {
        let names = SecretName {
            name: "INFERHUB_API_KEY".into(),
            used_by: vec!["inferhub".into()],
        };
        let value = serde_json::to_value(&names).expect("serializes");
        assert_eq!(
            value,
            serde_json::json!({"name": "INFERHUB_API_KEY", "used_by": ["inferhub"]})
        );
        let write = SecretWrite {
            name: "K".into(),
            applies: APPLIES_RESTART.into(),
        };
        assert_eq!(
            serde_json::to_value(&write).expect("serializes"),
            serde_json::json!({"name": "K", "applies": "restart"})
        );
    }
}
