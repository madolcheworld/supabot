//! Composition: a config and a data directory in, a wired [`Swarm`] out.
//!
//! Both entry points (the brain daemon and the CLI's embedded mode) come through
//! here, so "how a brain is built" has exactly one answer. The order matters and
//! is the reason this module exists: the safety policy is read from the store,
//! the registry is built from the policy, and the swarm builds the registry's
//! bot tools from its own services — none of which a caller should have to know.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::info;

use swarmbot_contracts::config::{BrainConfig, Settings};
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::NodeId;
use swarmbot_contracts::node::NodeRoot;
use swarmbot_contracts::repo::Persistence;
use swarmbot_store::{JsonlMirror, MemoryStore, SqliteStore};
use swarmbot_tools::{AuditLog, LocalExecutor, PathPolicy, PolicyEngine};

use crate::backends::BackendSet;
use crate::swarm::{Swarm, SwarmParts};

/// A brain on disk: SQLite, the JSONL transcript mirror and an audit log.
pub fn durable(
    config: BrainConfig,
    data_dir: &Path,
    extra_roots: Vec<NodeRoot>,
    secrets: &BTreeMap<String, String>,
) -> Result<Arc<Swarm>, DomainError> {
    std::fs::create_dir_all(data_dir).map_err(|err| {
        DomainError::internal(format!("cannot create {}: {err}", data_dir.display()))
    })?;
    let store: Arc<dyn Persistence> =
        Arc::new(SqliteStore::open(&data_dir.join("swarmbot.sqlite"))?);
    let mirror = Arc::new(JsonlMirror::new(data_dir.join("transcripts")));
    // A brand-new data dir has no stored settings, and `settings()` answers with
    // the built-in defaults in that case — which would silently throw away the
    // `[settings]` the operator wrote (their safety ceilings above all). Seed it
    // once, while the store is still empty; after that an owner's PATCH wins and
    // a restart must not overwrite it.
    let fresh = store.list_bots()?.is_empty() && store.list_conversations()?.is_empty();
    if fresh {
        store.put_settings(&config.settings)?;
    }
    let settings = store.settings().unwrap_or_default();
    let backends = BackendSet::from_config(&config, secrets)?;
    let parts = parts_for(&config, data_dir, &settings, store.clone(), extra_roots)?;
    let swarm = Swarm::new(
        config,
        data_dir.to_path_buf(),
        store,
        backends,
        parts,
        Some(mirror),
    )?;
    info!(data_dir = %data_dir.display(), "brain state loaded");
    Ok(swarm)
}

/// A brain in memory: tests, and `swarmbot demo --offline`.
pub fn memory(
    config: BrainConfig,
    data_dir: &Path,
    extra_roots: Vec<NodeRoot>,
    secrets: &BTreeMap<String, String>,
) -> Result<Arc<Swarm>, DomainError> {
    let store: Arc<dyn Persistence> = Arc::new(MemoryStore::default());
    // An in-memory store is always fresh, so the config is the only word there
    // is: seeding it is what makes `--offline`, the CLI's embedded mode and the
    // tests honour the same `[settings]` a durable brain would.
    store.put_settings(&config.settings)?;
    let settings = config.settings.clone();
    let backends = BackendSet::from_config(&config, secrets)?;
    let parts = parts_for(&config, data_dir, &settings, store.clone(), extra_roots)?;
    Swarm::new(config, data_dir.to_path_buf(), store, backends, parts, None)
}

/// The machine side of a swarm: filesystem boundary, ceilings, audit and the
/// local executor.
fn parts_for(
    config: &BrainConfig,
    data_dir: &Path,
    settings: &Settings,
    store: Arc<dyn Persistence>,
    extra_roots: Vec<NodeRoot>,
) -> Result<SwarmParts, DomainError> {
    let workspaces = workspaces_root(config, data_dir);
    std::fs::create_dir_all(&workspaces).map_err(|err| {
        DomainError::internal(format!("cannot create {}: {err}", workspaces.display()))
    })?;

    // The local node's roots: the bot workspaces, plus whatever the operator
    // pointed at with `--root`. Everything outside is refused by the path
    // policy, not merely discouraged.
    let mut roots = vec![NodeRoot::read_write(
        "work",
        workspaces.to_string_lossy().to_string(),
    )];
    roots.extend(extra_roots);

    let policy =
        Arc::new(PathPolicy::with_default_deny(roots.clone()).with_workspaces_root(&workspaces));
    // The local node adopts the brain's ceilings: there is no second operator
    // here to be stricter than.
    let engine = Arc::new(PolicyEngine::from_settings(
        settings,
        settings.safety.clone(),
    ));
    let audit = Arc::new(AuditLog::for_node(data_dir, &NodeId::local()));
    let nodes: Arc<dyn swarmbot_tools::NodeExecutor> =
        Arc::new(LocalExecutor::new(workspaces, Arc::clone(&policy)));
    let _ = store;

    Ok(SwarmParts {
        policy,
        engine,
        audit: Some(audit),
        nodes,
        roots,
    })
}

/// Where each bot's files live: `<data>/workspaces/<bot name>`.
pub fn workspaces_root(config: &BrainConfig, data_dir: &Path) -> PathBuf {
    let _ = config;
    data_dir.join("workspaces")
}

/// Ensure a bot's workspace directory, `.spool` directory, and initial `MEMORY.md` exist.
pub fn ensure_bot_workspace(
    workspaces_root: &Path,
    bot_name: &str,
) -> Result<PathBuf, DomainError> {
    let workspace = workspaces_root.join(bot_name);
    std::fs::create_dir_all(&workspace)
        .map_err(|err| DomainError::internal(format!("cannot create the workspace: {err}")))?;
    let spool_dir = workspace.join(".spool");
    let _ = std::fs::create_dir_all(&spool_dir);
    let memory_file = workspace.join("MEMORY.md");
    if !memory_file.exists() {
        let _ = std::fs::write(&memory_file, "");
    }
    Ok(workspace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::config::BackendSpec;
    use swarmbot_contracts::node::PolicyCeiling;

    fn config() -> BrainConfig {
        BrainConfig {
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        }
    }

    #[tokio::test]
    async fn a_memory_swarm_boots_and_provisions_the_demo_team() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let swarm = memory(config(), dir.path(), vec![], &BTreeMap::new()).expect("a swarm");
        let report = swarm.boot().expect("boot");
        assert!(report.is_empty(), "a fresh brain has nothing to recover");

        let bots = swarm.bots.list().expect("bots");
        assert_eq!(bots.len(), 4);
        assert!(bots.iter().any(|b| b.name == "lead"));
        let groups = swarm.conversations.list().expect("conversations");
        assert!(groups.iter().any(|c| c.title.as_deref() == Some("Team")));

        // Every bot has a runner, and the local node exists.
        assert_eq!(swarm.runners.len(), 4);
        let node = swarm
            .store
            .get_node(&NodeId::local())
            .expect("node lookup")
            .expect("a local node");
        assert!(node.can("shell"));
        assert_eq!(node.roots.len(), 1, "the workspaces root");

        // Every provisioned bot has its isolated workspace with MEMORY.md and .spool/
        for bot in &bots {
            let ws = dir.path().join("workspaces").join(&bot.name);
            assert!(ws.is_dir(), "workspace for @{} exists", bot.name);
            assert!(
                ws.join(".spool").is_dir(),
                ".spool for @{} exists",
                bot.name
            );
            assert!(
                ws.join("MEMORY.md").is_file(),
                "MEMORY.md for @{} exists",
                bot.name
            );
        }

        swarm.shutdown().await;
    }

    #[tokio::test]
    async fn a_durable_swarm_survives_being_reopened() {
        let dir = tempfile::tempdir().expect("a temp dir");
        {
            let swarm = durable(config(), dir.path(), vec![], &BTreeMap::new()).expect("a swarm");
            swarm.boot().expect("boot");
            let lead = swarm.bots.by_name("lead").expect("the lead");
            swarm
                .send(&lead.direct_conversation_id, "remember this")
                .expect("sent");
        }
        let reopened = durable(config(), dir.path(), vec![], &BTreeMap::new()).expect("reopen");
        reopened.boot().expect("boot");
        let lead = reopened.bots.by_name("lead").expect("the lead");
        let entries = reopened
            .conversations
            .tail(&lead.direct_conversation_id, 10)
            .expect("entries");
        assert!(
            entries
                .iter()
                .any(|e| e.kind.text() == Some("remember this"))
        );
        // The JSONL mirror holds the same transcript.
        let mirrored = JsonlMirror::new(dir.path().join("transcripts"))
            .read(&lead.direct_conversation_id)
            .expect("mirror");
        assert!(
            mirrored
                .iter()
                .any(|e| e.kind.text() == Some("remember this"))
        );
    }

    #[tokio::test]
    async fn extra_roots_widen_the_local_node() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("create");
        let roots = vec![NodeRoot::read_only(
            "project",
            project.to_string_lossy().to_string(),
        )];
        let swarm = memory(config(), dir.path(), roots, &BTreeMap::new()).expect("a swarm");
        swarm.boot().expect("boot");
        let node = swarm
            .store
            .get_node(&NodeId::local())
            .expect("lookup")
            .expect("node");
        assert_eq!(node.roots.len(), 2);
        assert!(
            node.roots.iter().any(
                |r| r.id == "project" && r.mode == swarmbot_contracts::node::RootMode::ReadOnly
            )
        );
        swarm.shutdown().await;
    }

    #[tokio::test]
    async fn configured_settings_reach_the_swarm_and_its_policy() {
        // The bug this pins: `memory` used `Settings::default()`, so an operator's
        // `[settings]` — including safety ceilings — was silently ignored.
        let mut cfg = config();
        cfg.settings.max_hops = 3;
        cfg.settings.bot_messaging = false;
        cfg.settings.safety.set(
            swarmbot_contracts::node::Surface::Shell,
            PolicyCeiling::Always,
        );
        let dir = tempfile::tempdir().expect("a temp dir");
        let swarm = memory(cfg, dir.path(), vec![], &BTreeMap::new()).expect("a swarm");
        let settings = swarm.settings.get();
        assert_eq!(settings.max_hops, 3);
        assert!(!settings.bot_messaging, "the kill switch is the operator's");
        assert_eq!(
            settings
                .safety
                .get(swarmbot_contracts::node::Surface::Shell),
            PolicyCeiling::Always
        );
        swarm.boot().expect("boot");
        assert_eq!(swarm.settings.get().max_hops, 3, "boot does not reset them");
        swarm.shutdown().await;
    }

    #[tokio::test]
    async fn a_durable_brain_seeds_settings_once_and_never_overwrites_an_owner() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut cfg = config();
        cfg.settings.max_hops = 4;
        {
            let swarm =
                durable(cfg.clone(), dir.path(), vec![], &BTreeMap::new()).expect("a swarm");
            swarm.boot().expect("boot");
            assert_eq!(swarm.settings.get().max_hops, 4, "seeded from the config");
            // The owner changes their mind through the API.
            swarm.settings.update(|s| s.max_hops = 2).expect("update");
            swarm.shutdown().await;
        }
        // A restart with the *old* config must not undo the owner's change.
        let reopened = durable(cfg, dir.path(), vec![], &BTreeMap::new()).expect("reopen");
        reopened.boot().expect("boot");
        assert_eq!(
            reopened.settings.get().max_hops,
            2,
            "an owner's saved settings survive a restart"
        );
        reopened.shutdown().await;
    }

    #[test]
    fn an_unconfigured_backend_fails_the_build_not_the_first_turn() {
        let mut broken = config();
        broken.default_backend = "inferhub".into();
        broken.backends = vec![BackendSpec::fake("fake")];
        let dir = tempfile::tempdir().expect("a temp dir");
        // `unwrap_err` would need `Swarm: Debug`; match instead.
        let err = match memory(broken, dir.path(), vec![], &BTreeMap::new()) {
            Err(err) => err,
            Ok(_) => panic!("a config whose default backend is missing must not build"),
        };
        assert_eq!(err.code(), "invalid_input");
        assert!(err.to_string().contains("default_backend"), "{err}");
    }
}
