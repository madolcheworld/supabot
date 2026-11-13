//! The backend registry: config in, live adapters out.
//!
//! The brain hands the registry the configured [`BackendSpec`]s plus its
//! secrets store; everything downstream (runner, Settings screen, CLI) only
//! ever talks to `Arc<dyn LlmBackend>` fetched by id. Invariants:
//!
//! * a backend named **`fake` is always available**, configured or not —
//!   tests and the offline demo must never depend on a config file;
//! * API keys are resolved once, at build time (secrets store first, process
//!   environment second), and live only inside the adapters — the registry
//!   itself never renders one (`Debug` is manual);
//! * `probe_all` always yields one [`BackendStatus`] row per backend, even
//!   when probing fails: the Settings screen shows failures, it does not
//!   crash on them.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use swarmbot_contracts::{BackendKind, BackendSpec, DomainError};

use crate::acp::AcpAgent;
use crate::backend::{LlmBackend, LlmError};
use crate::fake::FakeBackend;
use crate::harness::ClaudeCli;
use crate::openai::{HttpTransport, OpenAiCompat, ReqwestTransport};
use crate::types::BackendStatus;

/// The id that is guaranteed to resolve, even with an empty config.
pub const FAKE_BACKEND_ID: &str = "fake";

/// What the Settings screen lists per backend. Never contains a key — only
/// whether one is in place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendInfo {
    pub id: String,
    pub kind: BackendKind,
    pub model: Option<String>,
    /// A key is present, or none is needed. `false` means `chat` would fail
    /// with [`LlmError::MissingKey`] until the operator provides one.
    pub ready: bool,
}

struct Entry {
    backend: Arc<dyn LlmBackend>,
    info: BackendInfo,
}

/// Live adapters built from config, addressable by backend id.
pub struct BackendRegistry {
    default_id: String,
    entries: BTreeMap<String, Entry>,
    /// Direct handles to the fake backends, so the demo and tests can script
    /// turns through the registry without downcasting.
    fakes: BTreeMap<String, Arc<FakeBackend>>,
}

impl fmt::Debug for BackendRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Manual: the default derive would try to render adapters (which hold
        // keys behind their own redacted Debugs); listing ids and readiness
        // is all a log line ever needs.
        f.debug_struct("BackendRegistry")
            .field("default_id", &self.default_id)
            .field("backends", &self.list())
            .finish()
    }
}

impl BackendRegistry {
    /// Build every configured backend with the real HTTP transport. `secrets`
    /// maps an `api_key_env` name to its key (the brain loads it from
    /// `<data>/secrets.json`); the process environment is the fallback.
    ///
    /// Fails fast on invalid config (unknown kind requirements, duplicate
    /// ids): a backend that cannot be built should stop the boot, not lurk
    /// until a bot's first turn.
    pub fn new(
        default_id: impl Into<String>,
        specs: Vec<BackendSpec>,
        secrets: &BTreeMap<String, String>,
    ) -> Result<Self, LlmError> {
        let transport =
            Arc::new(
                ReqwestTransport::new().map_err(|error| LlmError::Unavailable {
                    backend: "registry".to_string(),
                    reason: error.to_string(),
                })?,
            );
        Self::with_transport(default_id, specs, secrets, transport)
    }

    /// Build with an injected HTTP transport — tests script gateway failures
    /// through this without a socket.
    pub fn with_transport(
        default_id: impl Into<String>,
        specs: Vec<BackendSpec>,
        secrets: &BTreeMap<String, String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, LlmError> {
        let mut entries = BTreeMap::new();
        let mut fakes = BTreeMap::new();

        for spec in specs {
            spec.validate()
                .map_err(|error: DomainError| LlmError::Unavailable {
                    backend: spec.id.clone(),
                    reason: error.to_string(),
                })?;
            if entries.contains_key(&spec.id) {
                return Err(LlmError::Unavailable {
                    backend: spec.id.clone(),
                    reason: "duplicate backend id".to_string(),
                });
            }
            let key = resolve_key(&spec, secrets);
            let ready = match spec.kind {
                // A keyless openai spec is only ready when it asks for no key
                // (an unauthenticated local gateway). An ACP backend follows
                // the same rule when it names a key env; otherwise it uses the
                // agent's own login, whose validity `probe` reports honestly.
                BackendKind::OpenAiCompat | BackendKind::Acp => {
                    key.is_some() || spec.api_key_env.is_none()
                }
                // The fake needs nothing; the CLI harness uses the owner's
                // own login, whose validity `probe` reports honestly.
                BackendKind::Fake | BackendKind::ClaudeCli => true,
            };
            let backend: Arc<dyn LlmBackend> = match spec.kind {
                BackendKind::Fake => {
                    let echo = spec
                        .options
                        .get("echo")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let fake = Arc::new(if echo {
                        FakeBackend::echo_with_id(&spec.id)
                    } else {
                        FakeBackend::new(&spec.id)
                    });
                    fakes.insert(spec.id.clone(), Arc::clone(&fake));
                    fake
                }
                BackendKind::OpenAiCompat => Arc::new(OpenAiCompat::new(
                    spec.clone(),
                    key,
                    Arc::clone(&transport),
                )?),
                BackendKind::ClaudeCli => Arc::new(ClaudeCli::from_spec(spec.clone())?),
                BackendKind::Acp => Arc::new(AcpAgent::from_spec(spec.clone(), key)?),
            };
            let info = BackendInfo {
                id: spec.id.clone(),
                kind: spec.kind,
                model: spec.model.clone(),
                ready,
            };
            entries.insert(spec.id.clone(), Entry { backend, info });
        }

        // Invariant: the fake is always there — an unconfigured brain, a
        // fresh checkout and every test still have a working backend.
        if !entries.contains_key(FAKE_BACKEND_ID) {
            let fake = Arc::new(FakeBackend::new(FAKE_BACKEND_ID));
            fakes.insert(FAKE_BACKEND_ID.to_string(), Arc::clone(&fake));
            entries.insert(
                FAKE_BACKEND_ID.to_string(),
                Entry {
                    backend: fake,
                    info: BackendInfo {
                        id: FAKE_BACKEND_ID.to_string(),
                        kind: BackendKind::Fake,
                        model: Some("fake".to_string()),
                        ready: true,
                    },
                },
            );
        }

        Ok(Self {
            default_id: default_id.into(),
            entries,
            fakes,
        })
    }

    /// The backend for `id`.
    pub fn get(&self, id: &str) -> Result<Arc<dyn LlmBackend>, LlmError> {
        self.entries
            .get(id)
            .map(|entry| Arc::clone(&entry.backend))
            .ok_or_else(|| LlmError::Unavailable {
                backend: id.to_string(),
                reason: "no such backend is configured".to_string(),
            })
    }

    /// The configured default backend (bots that name none use this).
    pub fn default(&self) -> Result<Arc<dyn LlmBackend>, LlmError> {
        self.get(&self.default_id)
    }

    pub fn default_id(&self) -> &str {
        &self.default_id
    }

    /// One row per backend, sorted by id — the Settings screen's list.
    pub fn list(&self) -> Vec<BackendInfo> {
        self.entries
            .values()
            .map(|entry| entry.info.clone())
            .collect()
    }

    /// Scriptable handle to a fake backend, for demos and tests that push
    /// turns after registration.
    pub fn fake(&self, id: &str) -> Option<Arc<FakeBackend>> {
        self.fakes.get(id).cloned()
    }

    /// Probe every backend concurrently and report one row each; a failed
    /// probe becomes a not-ok status rather than an error, because the
    /// Settings screen wants the whole picture in one call.
    pub async fn probe_all(&self) -> Vec<BackendStatus> {
        let probes = self.entries.iter().map(|(id, entry)| async move {
            match entry.backend.probe().await {
                Ok(status) => status,
                Err(error) => BackendStatus {
                    backend: id.clone(),
                    ok: false,
                    detail: error.to_string(),
                    model: entry.info.model.clone(),
                    version: None,
                },
            }
        });
        futures::future::join_all(probes).await
    }
}

/// Resolve the key a spec names: the secrets store wins over the process
/// environment, and an empty/whitespace value counts as absent (a blank key
/// in a file is a config mistake, not a key).
fn resolve_key(spec: &BackendSpec, secrets: &BTreeMap<String, String>) -> Option<String> {
    let name = spec.api_key_env.as_deref()?;
    let non_empty = |value: String| (!value.trim().is_empty()).then_some(value);
    secrets
        .get(name)
        .cloned()
        .and_then(non_empty)
        .or_else(|| std::env::var(name).ok().and_then(non_empty))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{ScriptedResponse, ScriptedTransport};
    use crate::types::ChatRequest;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn openai_spec(id: &str, key_env: &str) -> BackendSpec {
        BackendSpec::openai_compat(id, "https://api.example.invalid/v1", key_env, "test-model")
    }

    #[tokio::test]
    async fn fake_is_always_available_even_unconfigured() {
        let registry = BackendRegistry::with_transport(
            "fake",
            Vec::new(),
            &BTreeMap::new(),
            ScriptedTransport::new(Vec::new()),
        )
        .expect("builds");
        let backend = registry.get("fake").expect("the invariant");
        assert_eq!(backend.kind(), BackendKind::Fake);
        let status = backend.probe().await.expect("fake probes ok");
        assert!(status.ok);
        assert!(registry.fake("fake").is_some());
        assert_eq!(registry.default().expect("default fake").id(), "fake");
    }

    #[tokio::test]
    async fn an_explicit_fake_spec_wins_over_the_implicit_one() {
        let mut spec = BackendSpec::fake("fake");
        spec.options
            .insert("echo".into(), serde_json::Value::Bool(true));
        let registry = BackendRegistry::with_transport(
            "fake",
            vec![spec],
            &BTreeMap::new(),
            ScriptedTransport::new(Vec::new()),
        )
        .expect("builds");
        let fake = registry.fake("fake").expect("scriptable handle");
        let (tx, rx) = mpsc::channel(16);
        let mut request = ChatRequest::new("fake", "");
        request
            .messages
            .push(crate::types::ChatMessage::user("echo me"));
        let response = fake
            .chat(request, tx, CancellationToken::new())
            .await
            .expect("echo answers");
        drop(rx);
        assert_eq!(response.text, "echo me");
    }

    #[tokio::test]
    async fn secrets_resolve_keys_and_reach_the_wire() {
        let mut secrets = BTreeMap::new();
        secrets.insert("STORE_KEY".to_string(), "sk-from-store".to_string());
        let transport = ScriptedTransport::new(vec![ScriptedResponse::sse([
            "{\"object\":\"list\",\"data\":[{\"id\":\"test-model\"},{\"id\":\"other\"}]}",
        ])]);
        let registry = BackendRegistry::with_transport(
            "inferhub",
            vec![openai_spec("inferhub", "STORE_KEY")],
            &secrets,
            Arc::clone(&transport) as Arc<dyn HttpTransport>,
        )
        .expect("builds");

        assert!(registry.list().iter().all(|info| info.ready));
        let backend = registry.get("inferhub").expect("configured");
        let status = backend.probe().await.expect("probe ok");
        assert!(status.ok, "{status:?}");
        assert!(status.detail.contains("2 models"), "{status:?}");

        let recorded = transport.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].method, "GET");
        assert_eq!(recorded[0].url, "https://api.example.invalid/v1/models");
        assert_eq!(
            recorded[0].header("authorization"),
            Some("Bearer sk-from-store")
        );
    }

    #[tokio::test]
    async fn a_keyless_backend_is_listed_as_not_ready() {
        // An env name that is guaranteed unset: readiness must come from the
        // secrets store or the environment, and neither has it.
        let registry = BackendRegistry::with_transport(
            "inferhub",
            vec![openai_spec(
                "inferhub",
                "SWARMBOT_TEST_KEY_DEFINITELY_UNSET",
            )],
            &BTreeMap::new(),
            ScriptedTransport::new(Vec::new()),
        )
        .expect("builds");
        let infos = registry.list();
        let info = infos
            .iter()
            .find(|info| info.id == "inferhub")
            .expect("listed");
        assert!(!info.ready);
        // chat fails with MissingKey without spending a request.
        let backend = registry.get("inferhub").expect("configured");
        let (tx, rx) = mpsc::channel(4);
        let error = backend
            .chat(ChatRequest::new("m", ""), tx, CancellationToken::new())
            .await
            .expect_err("no key");
        drop(rx);
        assert!(matches!(error, LlmError::MissingKey { .. }));
        // probe is honest without hitting the network.
        let status = backend.probe().await.expect("probe reports");
        assert!(!status.ok);
        assert!(status.detail.contains("no api key"), "{status:?}");
    }

    #[tokio::test]
    async fn duplicate_ids_and_bad_specs_fail_fast() {
        let specs = vec![BackendSpec::fake("dupe"), BackendSpec::fake("dupe")];
        let error = BackendRegistry::with_transport(
            "dupe",
            specs,
            &BTreeMap::new(),
            ScriptedTransport::new(Vec::new()),
        )
        .expect_err("duplicate ids");
        assert!(matches!(error, LlmError::Unavailable { .. }));

        // openai_compat without base_url: caught at build time, not mid-turn.
        let mut broken = openai_spec("broken", "K");
        broken.base_url = None;
        let error = BackendRegistry::with_transport(
            "broken",
            vec![broken],
            &BTreeMap::new(),
            ScriptedTransport::new(Vec::new()),
        )
        .expect_err("invalid spec");
        assert!(error.to_string().contains("base_url"), "{error}");
    }

    #[tokio::test]
    async fn unknown_ids_and_defaults_report_clearly() {
        let registry = BackendRegistry::with_transport(
            "nope",
            Vec::new(),
            &BTreeMap::new(),
            ScriptedTransport::new(Vec::new()),
        )
        .expect("builds");
        let error = match registry.get("nope") {
            // Arc<dyn LlmBackend> has no Debug, so spell the expectation out.
            Ok(_) => panic!("an unconfigured backend must not resolve"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("no such backend"), "{error}");
        assert!(registry.default().is_err());
    }

    #[tokio::test]
    async fn probe_all_reports_one_row_per_backend_including_failures() {
        let transport = ScriptedTransport::new(vec![
            // GET /models for `inferhub` fails at the transport level.
            ScriptedResponse::fail("connection refused"),
        ]);
        let registry = BackendRegistry::with_transport(
            "fake",
            vec![openai_spec(
                "inferhub",
                "SWARMBOT_TEST_KEY_DEFINITELY_UNSET2",
            )],
            &BTreeMap::new(),
            Arc::clone(&transport) as Arc<dyn HttpTransport>,
        )
        .expect("builds");

        let statuses = registry.probe_all().await;
        // fake, inferhub — sorted by id, one row each.
        assert_eq!(
            statuses
                .iter()
                .map(|s| s.backend.as_str())
                .collect::<Vec<_>>(),
            vec!["fake", "inferhub"]
        );
        assert!(
            statuses
                .iter()
                .find(|s| s.backend == "fake")
                .is_some_and(|s| s.ok)
        );
        // The keyless openai backend reports the missing key (no request).
        let inferhub = statuses
            .iter()
            .find(|s| s.backend == "inferhub")
            .expect("row");
        assert!(!inferhub.ok);
        assert!(inferhub.detail.contains("no api key"), "{inferhub:?}");
        // The transport failure was never reached (keyless probe short-circuits).
        assert_eq!(transport.requests().len(), 0);
    }

    #[tokio::test]
    async fn registry_debug_lists_ids_without_keys() {
        let mut secrets = BTreeMap::new();
        secrets.insert("STORE_KEY".to_string(), "sk-secret-value".to_string());
        let registry = BackendRegistry::with_transport(
            "inferhub",
            vec![openai_spec("inferhub", "STORE_KEY")],
            &secrets,
            ScriptedTransport::new(Vec::new()),
        )
        .expect("builds");
        let debug = format!("{registry:?}");
        assert!(!debug.contains("sk-secret-value"), "{debug}");
        assert!(debug.contains("inferhub"), "{debug}");
    }
}
