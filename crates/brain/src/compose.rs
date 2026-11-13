//! The brain's composition: `swarmbot_core::Swarm::new` with a **node dispatcher**
//! in front of the local executor.
//!
//! [`swarmbot_core::compose`] builds the machine side of a swarm — the path
//! policy, the ceilings, the audit log and a [`swarmbot_tools::LocalExecutor`] —
//! and hands that executor to the swarm as *the* way a tool call reaches a
//! machine. `LocalExecutor` runs any call it is given locally and only logs a
//! mismatched node id, so a brain composed that way would silently execute a
//! remote bot's `shell` call on the brain's own machine. That is the one thing
//! decision D1.1 ("the brain's machine is also a node, and a remote node runs its
//! own tools under its own policy") exists to prevent.
//!
//! `SwarmParts` is public precisely so a composition root can build it, so this
//! module mirrors `compose::parts_for` line for line and changes exactly one
//! thing: `nodes` is a [`NodeDispatcher`], which routes `nod_local` to the same
//! `LocalExecutor` core would have built and everything else across a socket.
//! Everything above that line — the services, the runners, the hub — is core's
//! `Swarm::new`, untouched.
//!
//! The duplication is the price of the seam, and [`tests`] below is what keeps the
//! two halves from drifting.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use tracing::info;

use swarmbot_contracts::config::{BrainConfig, Settings};
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::NodeId;
use swarmbot_contracts::node::NodeRoot;
use swarmbot_contracts::repo::Persistence;
use swarmbot_core::backends::BackendSet;
use swarmbot_core::swarm::{Swarm, SwarmParts};
use swarmbot_store::{JsonlMirror, MemoryStore, ThreadedStore};
use swarmbot_tools::{AuditLog, LocalExecutor, NodeExecutor, PathPolicy, PolicyEngine};

use crate::gateway::NodeGateway;
use crate::gateway::executor::NodeDispatcher;

/// A brain on disk: SQLite, the JSONL transcript mirror, an audit log, and the
/// dispatcher that makes a remote node reachable.
pub fn durable(
    config: BrainConfig,
    data_dir: &Path,
    extra_roots: Vec<NodeRoot>,
    secrets: &BTreeMap<String, String>,
    gateway: Arc<NodeGateway>,
) -> Result<Arc<Swarm>, DomainError> {
    std::fs::create_dir_all(data_dir).map_err(|error| {
        DomainError::internal(format!("cannot create {}: {error}", data_dir.display()))
    })?;
    // `ThreadedStore`, not `SqliteStore`: a port call is synchronous, so with
    // one `Mutex<Connection>` behind `busy_timeout = 5000` a single contended
    // write parked a tokio worker for up to five seconds and serialised every
    // other caller behind it. The wrapper gives the writer its own OS thread
    // and the readers their own, so a contended write no longer blocks a read
    // or a different lane. What it does **not** do is un-park a caller that
    // invokes the port inline on an async task — that still costs that task a
    // blocking call, and freeing it needs `spawn_blocking` (which needs tokio
    // inside `store`). See `store::threaded`'s module docs for the full limit.
    let store: Arc<dyn Persistence> =
        Arc::new(ThreadedStore::open(&data_dir.join("swarmbot.sqlite"))?);
    let mirror = Arc::new(JsonlMirror::new(data_dir.join("transcripts")));
    // A brand-new data dir has no stored settings, and `settings()` answers with
    // the built-in defaults in that case — which would silently throw away the
    // `[settings]` the operator wrote (their safety ceilings above all). Seed it
    // once while the store is empty; checking nodes too prevents overwriting
    // settings after a boot that had `team = None`.
    let fresh = store.list_bots()?.is_empty()
        && store.list_conversations()?.is_empty()
        && store.list_nodes()?.is_empty();
    if fresh {
        store.put_settings(&config.settings)?;
    }
    let settings = store.settings().unwrap_or_default();
    let backends = BackendSet::from_config(&config, secrets)?;
    let parts = parts(
        data_dir,
        &settings,
        extra_roots,
        gateway,
        Arc::clone(&store),
    )?;
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

/// A brain in memory: tests, and `--offline` demos that must leave no file
/// behind. Same wiring, no SQLite and no transcript mirror.
pub fn memory(
    config: BrainConfig,
    data_dir: &Path,
    extra_roots: Vec<NodeRoot>,
    secrets: &BTreeMap<String, String>,
    gateway: Arc<NodeGateway>,
) -> Result<Arc<Swarm>, DomainError> {
    let store: Arc<dyn Persistence> = Arc::new(MemoryStore::default());
    // An in-memory store is always fresh, so seed `config.settings` before
    // building `SwarmParts` so `PolicyEngine` honours configured ceilings too.
    store.put_settings(&config.settings)?;
    let settings = config.settings.clone();
    let backends = BackendSet::from_config(&config, secrets)?;
    let parts = parts(
        data_dir,
        &settings,
        extra_roots,
        gateway,
        Arc::clone(&store),
    )?;
    Swarm::new(config, data_dir.to_path_buf(), store, backends, parts, None)
}

/// The machine side of the swarm, with the dispatcher in front.
///
/// Mirrors `swarmbot_core::compose::parts_for`; see the module docs for the one
/// difference and for why it matters.
pub fn parts(
    data_dir: &Path,
    settings: &Settings,
    extra_roots: Vec<NodeRoot>,
    gateway: Arc<NodeGateway>,
    store: Arc<dyn Persistence>,
) -> Result<SwarmParts, DomainError> {
    let workspaces = data_dir.join("workspaces");
    std::fs::create_dir_all(&workspaces).map_err(|error| {
        DomainError::internal(format!("cannot create {}: {error}", workspaces.display()))
    })?;

    // The local node's roots: the bot workspaces, plus whatever the operator
    // pointed at with `--root`. Everything outside is refused by the path policy,
    // not merely discouraged.
    let mut roots = vec![NodeRoot::read_write(
        "work",
        workspaces.to_string_lossy().to_string(),
    )];
    roots.extend(extra_roots);

    let policy =
        Arc::new(PathPolicy::with_default_deny(roots.clone()).with_workspaces_root(&workspaces));
    // The local node adopts the brain's ceilings: there is no second operator here
    // to be stricter than.
    let engine = Arc::new(PolicyEngine::from_settings(
        settings,
        settings.safety.clone(),
    ));
    let audit = Arc::new(AuditLog::for_node(data_dir, &NodeId::local()));
    let local: Arc<dyn NodeExecutor> =
        Arc::new(LocalExecutor::new(workspaces, Arc::clone(&policy)));
    // The one line this module exists for: `nod_local` runs here, anything else
    // has to be a connected node.
    let nodes: Arc<dyn NodeExecutor> = NodeDispatcher::new(local, gateway, store);

    Ok(SwarmParts {
        policy,
        engine,
        audit: Some(audit),
        nodes,
        roots,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::Invocation;
    use swarmbot_contracts::id::{BotId, ConversationId};
    use swarmbot_contracts::node::{PolicyCeiling, Surface};
    use swarmbot_tools::NodeCall;

    fn settings() -> Settings {
        Settings::default()
    }

    #[tokio::test]
    async fn the_parts_match_what_core_composes_apart_from_the_executor() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let extra = vec![NodeRoot::read_only(
            "project",
            dir.path().to_string_lossy().to_string(),
        )];
        let store: Arc<dyn Persistence> = Arc::new(MemoryStore::default());
        let mine = parts(
            dir.path(),
            &settings(),
            extra.clone(),
            NodeGateway::new("http://127.0.0.1:1"),
            Arc::clone(&store),
        )
        .expect("parts");

        let config = BrainConfig::default();
        let theirs = swarmbot_core::compose::memory(
            config.clone(),
            dir.path(),
            extra.clone(),
            &BTreeMap::new(),
        )
        .expect("a core-composed swarm");

        // Roots, in the same order, with the same modes: this is the boundary a
        // bot's file tools are allowed to touch.
        assert_eq!(mine.roots, theirs.roots);
        assert_eq!(mine.roots[0].id, "work");
        assert_eq!(mine.roots.len(), 2, "the workspaces root plus --root");

        // The policy refuses what is outside those roots, and allows what is in.
        let inside = dir.path().join("workspaces/x.txt");
        assert!(mine.policy.resolve(&inside, dir.path(), false).is_ok());
        assert!(
            mine.policy
                .resolve(Path::new("/etc/passwd"), dir.path(), false)
                .is_err()
        );

        // Per-bot workspace isolation: alice cannot touch bob's workspace
        let alice_dir = dir.path().join("workspaces/alice");
        let bob_dir = dir.path().join("workspaces/bob");
        std::fs::create_dir_all(&alice_dir).expect("mkdir alice");
        std::fs::create_dir_all(&bob_dir).expect("mkdir bob");
        assert!(
            mine.policy
                .resolve(Path::new("file.txt"), &alice_dir, false)
                .is_ok()
        );
        let sibling_err = mine
            .policy
            .resolve(&bob_dir.join("file.txt"), &alice_dir, false)
            .expect_err("sibling workspace is refused");
        assert!(matches!(
            sibling_err,
            swarmbot_contracts::ToolError::PathDenied { reason, .. } if reason == "outside bot workspace"
        ));

        // The engine carries the brain's ceilings, unchanged.
        assert_eq!(
            mine.engine.ceiling(Surface::ReadFile),
            PolicyCeiling::Always,
            "reading inside the roots is free by default"
        );
        assert_eq!(mine.engine.ceiling(Surface::Shell), PolicyCeiling::Ask);
        assert!(mine.audit.is_some(), "the JSONL audit log is on");

        // And the difference: a call addressed to another node is not run here.
        let remote = NodeId::new();
        // A composition test drives the port directly, so it says who is calling.
        let caller = Invocation::new(BotId::new(), ConversationId::new());
        let error = mine
            .nodes
            .exec(
                &remote,
                &caller,
                NodeCall::FsStat {
                    path: ".".to_string(),
                },
            )
            .await
            .expect_err("no such node is connected");
        assert!(error.to_string().contains("not connected"), "{error}");
        // The local id still runs locally, through the same executor core builds.
        let local = mine
            .nodes
            .exec(
                &NodeId::local(),
                &caller,
                NodeCall::FsStat {
                    path: dir.path().to_string_lossy().to_string(),
                },
            )
            .await
            .expect("a local stat runs");
        assert!(!local.is_error, "{}", local.text_only());
        drop(theirs);
    }

    #[tokio::test]
    async fn a_memory_brain_and_a_durable_brain_both_compose() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let gateway = NodeGateway::new("http://127.0.0.1:1");
        let config = BrainConfig::default();
        let swarm = memory(
            config.clone(),
            dir.path(),
            vec![],
            &BTreeMap::new(),
            Arc::clone(&gateway),
        )
        .expect("a memory swarm");
        assert_eq!(swarm.runners.len(), 0, "nothing is provisioned before boot");
        swarm.boot().expect("boots");
        assert_eq!(swarm.runners.len(), 0, "the default config has no team");
        swarm.shutdown().await;

        let swarm = durable(config, dir.path(), vec![], &BTreeMap::new(), gateway)
            .expect("a durable swarm");
        swarm.boot().expect("boots");
        assert!(dir.path().join("swarmbot.sqlite").exists());
        assert!(dir.path().join("workspaces").is_dir());
        swarm.shutdown().await;
    }

    #[tokio::test]
    async fn memory_compose_seeds_configured_settings_and_safety_ceilings() {
        use swarmbot_contracts::{BotId, ConversationId, Invocation, ToolCallId, ToolCallRequest};
        use swarmbot_tools::{ApprovalGate, ToolContext};

        let dir = tempfile::tempdir().expect("a temp dir");
        let gateway = NodeGateway::new("http://127.0.0.1:1");
        let mut config = BrainConfig::default();
        config.settings.bot_messaging = false;
        config.settings.max_hops = 3;
        config
            .settings
            .safety
            .set(Surface::Shell, PolicyCeiling::Never);

        let swarm =
            memory(config, dir.path(), vec![], &BTreeMap::new(), gateway).expect("a memory swarm");
        let stored = swarm.settings.get();
        assert!(!stored.bot_messaging);
        assert_eq!(stored.max_hops, 3);
        assert_eq!(stored.safety.get(Surface::Shell), PolicyCeiling::Never);

        let workspace = dir.path().join("workspaces/test");
        let spool = workspace.join(".spool");
        std::fs::create_dir_all(&spool).expect("mkdir");
        let ctx = ToolContext::local(
            Invocation::new(BotId::from_suffix("test"), ConversationId::from_suffix("c")),
            workspace,
            Arc::clone(&swarm.gate) as Arc<dyn ApprovalGate>,
            Arc::clone(&swarm.nodes),
            spool,
        );
        let result = swarm
            .tools
            .dispatch(
                &ctx,
                ToolCallRequest {
                    id: ToolCallId::new(),
                    name: "shell".into(),
                    args: serde_json::json!({"command": "echo should-not-run"}),
                },
            )
            .await;
        assert!(
            result.is_error,
            "shell must be blocked by PolicyCeiling::Never"
        );
        assert!(
            swarm.store.pending_requests().expect("requests").is_empty(),
            "Never ceiling refuses without queuing an approval request"
        );
        swarm.shutdown().await;
    }
}
