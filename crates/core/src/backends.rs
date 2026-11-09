//! Backends, as the orchestration sees them: a live object to call, plus the
//! spec it was built from.
//!
//! The split matters because the model call needs the *behaviour* while the
//! request needs the *configuration* (the model name, a gateway's `extra_body`,
//! its headers). Keeping them apart means the runner never has to know how a
//! backend was constructed, and the brain can add one without touching core.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use tracing::info;

use swarmbot_contracts::config::{BackendKind, BackendSpec, BrainConfig};
use swarmbot_contracts::error::DomainError;
use swarmbot_llm::{BackendRegistry, BackendStatus, LlmBackend};

pub struct BackendSet {
    registry: Arc<BackendRegistry>,
    specs: HashMap<String, BackendSpec>,
}

impl BackendSet {
    pub fn new(
        registry: Arc<BackendRegistry>,
        specs: impl IntoIterator<Item = BackendSpec>,
    ) -> Arc<Self> {
        let specs = specs
            .into_iter()
            .map(|spec| (spec.id.clone(), spec))
            .collect();
        Arc::new(Self { registry, specs })
    }

    /// The live backend for a bot. A missing id is a configuration error the
    /// owner can see, not a panic.
    pub fn backend(&self, id: &str) -> Result<Arc<dyn LlmBackend>, DomainError> {
        self.registry
            .get(id)
            .map_err(|err| DomainError::BackendUnavailable {
                backend: id.to_string(),
                reason: err.to_string(),
            })
    }

    /// Build every backend a config names. `secrets` maps an `api_key_env` name
    /// to its key (the brain loads it from `<data>/secrets.json`, 0600); the
    /// process environment is the adapter's fallback.
    pub fn from_config(
        config: &BrainConfig,
        secrets: &BTreeMap<String, String>,
    ) -> Result<Arc<Self>, DomainError> {
        let registry = BackendRegistry::new(
            config.default_backend.clone(),
            config.backends.clone(),
            secrets,
        )
        .map_err(|err| DomainError::BackendUnavailable {
            backend: config.default_backend.clone(),
            reason: err.to_string(),
        })?;
        Ok(Self::new(Arc::new(registry), config.backends.clone()))
    }

    pub fn spec(&self, id: &str) -> Option<&BackendSpec> {
        self.specs.get(id)
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.specs.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// What the Settings screen shows: is this backend installed, reachable and
    /// authenticated — without spending a model call.
    pub async fn probe_all(&self) -> Vec<(String, BackendStatus)> {
        let mut out = Vec::new();
        for id in self.ids() {
            let status = match self.backend(&id) {
                Ok(backend) => match backend.probe().await {
                    Ok(status) => status,
                    Err(err) => BackendStatus {
                        backend: id.clone(),
                        ok: false,
                        detail: err.to_string(),
                        model: None,
                        version: None,
                    },
                },
                Err(err) => BackendStatus {
                    backend: id.clone(),
                    ok: false,
                    detail: err.to_string(),
                    model: None,
                    version: None,
                },
            };
            info!(backend = %id, ok = status.ok, detail = %status.detail, "backend probe");
            out.push((id, status));
        }
        out
    }

    /// True when a backend runs its own tool loop, so the runner must not drive
    /// one and must read tool activity from the backend's events instead.
    pub fn owns_tool_loop(&self, id: &str) -> bool {
        self.spec(id).is_some_and(|spec| spec.kind.owns_tool_loop())
    }

    pub fn kind(&self, id: &str) -> Option<BackendKind> {
        self.spec(id).map(|spec| spec.kind)
    }

    pub fn registry(&self) -> Arc<BackendRegistry> {
        Arc::clone(&self.registry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_llm::BackendRegistry;

    fn set() -> Arc<BackendSet> {
        let specs = vec![
            BackendSpec::fake("fake"),
            BackendSpec::openai_compat(
                "inferhub",
                "https://api.inferhub.dev/v1",
                "INFERHUB_API_KEY",
                "ali/qwen3.8-flash",
            ),
        ];
        let registry = BackendRegistry::new("fake", specs.clone(), &BTreeMap::new())
            .expect("a registry builds without network access");
        BackendSet::new(Arc::new(registry), specs)
    }

    #[test]
    fn a_config_builds_its_backends() {
        let mut config = swarmbot_contracts::config::BrainConfig::default();
        config.backends.push(BackendSpec::openai_compat(
            "inferhub",
            "https://api.inferhub.dev/v1",
            "INFERHUB_API_KEY",
            "ali/qwen3.8-flash",
        ));
        let mut secrets = BTreeMap::new();
        secrets.insert("INFERHUB_API_KEY".to_string(), "sk-test".to_string());
        let set = BackendSet::from_config(&config, &secrets).unwrap();
        assert_eq!(set.ids(), vec!["fake", "inferhub"]);
        assert!(set.backend("fake").is_ok());
        assert!(set.backend("inferhub").is_ok());
    }

    #[test]
    fn specs_are_reachable_by_id() {
        let set = set();
        assert_eq!(set.ids(), vec!["fake", "inferhub"]);
        let spec = set.spec("inferhub").expect("configured");
        assert_eq!(spec.kind, BackendKind::OpenAiCompat);
        assert_eq!(spec.model.as_deref(), Some("ali/qwen3.8-flash"));
        assert!(set.spec("nope").is_none());
    }

    #[test]
    fn an_unknown_backend_is_a_typed_error() {
        let set = set();
        // `unwrap_err` would need `dyn LlmBackend: Debug`; match instead.
        let err = match set.backend("nope") {
            Err(err) => err,
            Ok(_) => panic!("an unknown backend must not resolve"),
        };
        assert_eq!(err.code(), "backend_unavailable");
        assert!(err.to_string().contains("nope"));
        assert!(
            err.retryable(),
            "a misconfigured backend may be fixed later"
        );
    }

    #[test]
    fn the_fake_backend_is_always_there() {
        let set = set();
        let backend = set.backend("fake").expect("fake is available offline");
        assert_eq!(backend.kind(), BackendKind::Fake);
        assert!(backend.capabilities().tool_calls);
    }

    #[test]
    fn harness_backends_own_their_tool_loop() {
        let set = set();
        assert!(!set.owns_tool_loop("fake"));
        assert!(!set.owns_tool_loop("inferhub"));
        assert!(!set.owns_tool_loop("missing"));
    }

    #[tokio::test]
    async fn probing_reports_every_backend() {
        let set = set();
        let statuses = set.probe_all().await;
        assert_eq!(statuses.len(), 2);
        let fake = statuses.iter().find(|(id, _)| id == "fake").unwrap();
        assert!(fake.1.ok, "the fake backend probes ok: {}", fake.1.detail);
    }
}
