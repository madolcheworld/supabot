//! The composition root of the orchestration: everything wired, in the order
//! the dependencies allow.
//!
//! Construction order is not cosmetic. The cycle "messaging wakes a runner, a
//! runner calls messaging" is broken by [`RunnerMap`], which parks deliveries
//! for bots whose runner does not exist yet; that is why the map is created
//! first and runners are spawned last.
//!
//! A `Swarm` owns no listeners and no CLI: the brain crate mounts it on HTTP
//! and the CLI runs it in-process. That is the seam that keeps the orchestration
//! testable without a socket.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::task::JoinHandle;
use tracing::{info, warn};

use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::config::BrainConfig;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::NodeId;
use swarmbot_contracts::node::{NodeInfo, PolicyMap};
use swarmbot_contracts::repo::{Persistence, RecoveryReport};
use swarmbot_store::JsonlMirror;
use swarmbot_tools::{AuditLog, NodeExecutor, PathPolicy, PolicyEngine, ToolRegistry};

use crate::bot_tools::{BotToolEnv, register_bot_tools};

use crate::approval::{ApprovalBridge, ApprovalService};
use crate::backends::BackendSet;
use crate::bots::BotService;
use crate::bus::LiveHub;
use crate::conversations::ConversationService;
use crate::instructions::InstructionRenderer;
use crate::messaging::Messaging;
use crate::orchestrator::Orchestrator;
use crate::routines::RoutineScheduler;
use crate::runners::{BotRunner, RunnerEnv, RunnerMap};
use crate::settings::SettingsHandle;
use crate::tasks::BackgroundTasks;
use crate::transcript::Transcript;

/// A wired brain, without listeners.
pub struct Swarm {
    pub config: BrainConfig,
    pub store: Arc<dyn Persistence>,
    pub hub: Arc<LiveHub>,
    pub transcript: Arc<Transcript>,
    pub settings: Arc<SettingsHandle>,
    pub runners: Arc<RunnerMap>,
    pub backends: Arc<BackendSet>,
    /// Node tools (the seven built-ins) plus the bot tools, registered after
    /// the services they call exist.
    pub tools: Arc<ToolRegistry>,
    pub instructions: Arc<InstructionRenderer>,
    pub approval: Arc<ApprovalService>,
    pub gate: Arc<ApprovalBridge>,
    pub messaging: Arc<Messaging>,
    pub tasks: Arc<BackgroundTasks>,
    pub bots: Arc<BotService>,
    pub conversations: Arc<ConversationService>,
    pub orchestrator: Arc<Orchestrator>,
    pub routines: Arc<RoutineScheduler>,
    pub nodes: Arc<dyn NodeExecutor>,
    pub roots: Vec<swarmbot_contracts::node::NodeRoot>,
    pub data_dir: PathBuf,
    expiry: ExpirySlot,
    routine_ticker: ExpirySlot,
}

/// The machine-side pieces a swarm needs, built by the composition root
/// (`compose`) because they come from config and from the data dir.
pub struct SwarmParts {
    /// The filesystem boundary of the local node.
    pub policy: Arc<PathPolicy>,
    /// Ceilings: brain ∩ node, with the destructive-command floor.
    pub engine: Arc<PolicyEngine>,
    /// Optional JSONL audit log.
    pub audit: Option<Arc<AuditLog>>,
    /// Where a call aimed at a node actually runs.
    pub nodes: Arc<dyn NodeExecutor>,
    /// The local node's roots, as configured (workspaces first).
    pub roots: Vec<swarmbot_contracts::node::NodeRoot>,
}

impl Swarm {
    /// Build on an existing store (tests, an embedded CLI, the brain).
    pub fn new(
        config: BrainConfig,
        data_dir: PathBuf,
        store: Arc<dyn Persistence>,
        backends: Arc<BackendSet>,
        parts: SwarmParts,
        mirror: Option<Arc<JsonlMirror>>,
    ) -> Result<Arc<Self>, DomainError> {
        config.validate()?;
        let hub = LiveHub::new(Arc::clone(&store));
        let transcript = Transcript::new(Arc::clone(&store), Arc::clone(&hub), mirror);
        let settings = SettingsHandle::new(Arc::clone(&store));
        let runners = RunnerMap::new();

        let approval = ApprovalService::new(
            Arc::clone(&store),
            Arc::clone(&hub),
            Arc::clone(&transcript),
            Arc::clone(&settings),
            Arc::clone(&runners),
        );
        let gate = ApprovalBridge::new(Arc::clone(&approval));
        // Tasks before messaging: a handoff is submitted to the task pump, which
        // is what decides where the work runs.
        let tasks = BackgroundTasks::new(
            Arc::clone(&store),
            Arc::clone(&hub),
            Arc::clone(&transcript),
            Arc::clone(&settings),
            Arc::clone(&runners),
        );
        let messaging = Messaging::new(
            Arc::clone(&store),
            Arc::clone(&transcript),
            Arc::clone(&settings),
            Arc::clone(&runners),
            Arc::clone(&tasks),
        );
        let orchestrator = Orchestrator::new(
            Arc::clone(&store),
            Arc::clone(&hub),
            Arc::clone(&transcript),
            Arc::clone(&settings),
            Arc::clone(&runners),
            Arc::clone(&messaging),
            Arc::clone(&tasks),
            Arc::clone(&approval),
        );
        let bots = BotService::new(
            Arc::clone(&store),
            Arc::clone(&hub),
            Arc::clone(&transcript),
            Arc::clone(&runners),
        );
        let conversations = ConversationService::new(
            Arc::clone(&store),
            Arc::clone(&hub),
            Arc::clone(&transcript),
            Arc::clone(&runners),
            Arc::clone(&tasks),
        );
        let instructions = InstructionRenderer::new(Arc::clone(&store));
        let routines = RoutineScheduler::new(Arc::clone(&store), Arc::clone(&runners));

        // Now the registry can hold both halves: the node tools (built from the
        // parts) and the bot tools (which call the services above).
        let mut registry =
            ToolRegistry::with_builtins(Arc::clone(&parts.policy), Arc::clone(&parts.engine));
        if let Some(audit) = parts.audit.clone() {
            registry = registry.with_audit(audit);
        }
        register_bot_tools(
            &mut registry,
            BotToolEnv {
                store: Arc::clone(&store),
                hub: Arc::clone(&hub),
                transcript: Arc::clone(&transcript),
                messaging: Arc::clone(&messaging),
                tasks: Arc::clone(&tasks),
                approval: Arc::clone(&approval),
                runners: Arc::clone(&runners),
            },
        );
        let tools = Arc::new(registry);

        Ok(Arc::new(Self {
            config,
            store,
            hub,
            transcript,
            settings,
            runners,
            backends,
            tools,
            instructions,
            approval,
            gate,
            messaging,
            tasks,
            bots,
            conversations,
            orchestrator,
            routines,
            nodes: parts.nodes,
            roots: parts.roots,
            data_dir,
            expiry: ExpirySlot(std::sync::Mutex::new(None)),
            routine_ticker: ExpirySlot(std::sync::Mutex::new(None)),
        }))
    }

    /// A swarm on a fresh in-memory store: tests, and `--offline` demos.
    pub fn memory(
        config: BrainConfig,
        backends: Arc<BackendSet>,
        parts: SwarmParts,
    ) -> Result<Arc<Self>, DomainError> {
        Self::new(
            config,
            PathBuf::from("data"),
            crate::testing::memory_store(),
            backends,
            parts,
            None,
        )
    }

    /// A durable swarm under `data_dir`, with the JSONL transcript mirror on.
    pub fn durable(
        config: BrainConfig,
        data_dir: &Path,
        backends: Arc<BackendSet>,
        parts: SwarmParts,
    ) -> Result<Arc<Self>, DomainError> {
        let store = crate::testing::sqlite_store(data_dir)?;
        let mirror = Arc::new(JsonlMirror::new(data_dir.join("transcripts")));
        Self::new(
            config,
            data_dir.to_path_buf(),
            store,
            backends,
            parts,
            Some(mirror),
        )
    }

    pub fn workspaces_root(&self) -> PathBuf {
        self.data_dir.join("workspaces")
    }

    /// Make sure the local node exists, so a bot with no node binding has one.
    pub fn ensure_local_node(&self) -> Result<NodeInfo, DomainError> {
        if let Some(node) = self.store.get_node(&NodeId::local())? {
            return Ok(node);
        }
        let workspace_root = self.workspaces_root();
        std::fs::create_dir_all(&workspace_root).map_err(|err| {
            DomainError::internal(format!("cannot create {workspace_root:?}: {err}"))
        })?;
        let node = NodeInfo::local(
            hostname(),
            self.roots.clone(),
            self.settings.get().safety.clone(),
        );
        let _ = workspace_root;
        self.store.insert_node(&node)?;
        self.hub.publish(crate::bus::Change::Node(node.clone()));
        Ok(node)
    }

    /// The team a config describes. Idempotent, so a restart does not duplicate.
    pub fn provision_team(&self) -> Result<Vec<Bot>, DomainError> {
        let bots = if let Some(team) = self.config.team.clone() {
            self.bots
                .provision_team(&team, &self.config.default_backend, &self.conversations)?
        } else {
            self.bots.list()?
        };
        let ws_root = self.workspaces_root();
        for bot in &bots {
            let _ = crate::compose::ensure_bot_workspace(&ws_root, &bot.name);
        }
        Ok(bots)
    }

    /// A runner per bot that does not have one yet.
    pub fn ensure_runners(&self) -> Result<usize, DomainError> {
        let env = self.runner_env();
        let mut started = 0;
        for bot in self.store.list_bots()? {
            if matches!(bot.status, swarmbot_contracts::bot::BotStatus::Disabled) {
                continue;
            }
            if self.runners.get(&bot.id).is_some() {
                continue;
            }
            BotRunner::spawn(bot.id.clone(), env.clone());
            started += 1;
        }
        if started > 0 {
            info!(started, "bot runners started");
        }
        Ok(started)
    }

    pub fn runner_env(&self) -> RunnerEnv {
        RunnerEnv {
            store: Arc::clone(&self.store),
            hub: Arc::clone(&self.hub),
            transcript: Arc::clone(&self.transcript),
            backends: Arc::clone(&self.backends),
            tools: Arc::clone(&self.tools),
            instructions: Arc::clone(&self.instructions),
            settings: Arc::clone(&self.settings),
            approval: Arc::clone(&self.gate) as Arc<dyn swarmbot_tools::ApprovalGate>,
            nodes: Arc::clone(&self.nodes),
            runners: Arc::clone(&self.runners),
            workspaces_root: self.workspaces_root(),
            hooks: Arc::clone(&self.orchestrator) as Arc<dyn crate::runners::RunnerHooks>,
        }
    }

    /// Boot: recover what a restart cut off, make sure the team and its runners
    /// exist, start the expiry ticker, and let queued tasks run.
    pub fn boot(self: &Arc<Self>) -> Result<RecoveryReport, DomainError> {
        self.ensure_local_node()?;
        let report = self.orchestrator.boot()?;
        self.provision_team()?;
        self.ensure_runners()?;
        if let Err(err) = self.tasks.pump() {
            warn!(error = %err, "cannot start queued tasks at boot");
        }
        Ok(report)
    }

    /// Start the task that expires unattended requests. Idempotent: a second
    /// call replaces the first ticker.
    pub fn start_expiry_ticker(&self) {
        let handle = self.approval.spawn_expiry_ticker();
        self.expiry.set(handle);
    }

    /// Start the task that checks and fires scheduled routines. Idempotent:
    /// a second call replaces the first ticker.
    pub fn start_routine_ticker(&self) {
        let handle = self.routines.spawn_ticker();
        self.routine_ticker.set(handle);
    }

    /// Stop everything: runners finish their turn, nothing is left "running".
    pub async fn shutdown(&self) {
        self.expiry.stop();
        self.routine_ticker.stop();
        self.orchestrator.shutdown().await;
        info!("swarm stopped");
    }

    /// Send a message as the owner.
    pub fn send(
        &self,
        conversation: &swarmbot_contracts::id::ConversationId,
        text: &str,
    ) -> Result<crate::orchestrator::Sent, DomainError> {
        self.orchestrator
            .send_owner_message(conversation, text, &[], None)
    }

    /// Wait until no bot is busy, or the budget runs out. Tests and the CLI's
    /// one-shot mode use this instead of sleeping on a guess.
    pub async fn wait_for_idle(&self, budget: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let busy = self
                .store
                .list_bots()
                .unwrap_or_default()
                .iter()
                .any(|bot| self.runners.is_busy(&bot.id));
            let queued = self
                .store
                .list_bots()
                .unwrap_or_default()
                .iter()
                .filter_map(|bot| self.store.queued_deliveries(&bot.id).ok())
                .map(|deliveries| deliveries.len())
                .sum::<usize>();
            let tasks_pending = self
                .store
                .queued_tasks()
                .map(|tasks| tasks.len())
                .unwrap_or(0);
            if !busy && queued == 0 && tasks_pending == 0 && self.runners.parked_count() == 0 {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
}

/// The expiry ticker's handle, kept behind a mutex-free cell so `shutdown` can
/// stop it. One swarm has at most one ticker.
struct ExpirySlot(std::sync::Mutex<Option<JoinHandle<()>>>);

impl ExpirySlot {
    fn set(&self, handle: JoinHandle<()>) {
        if let Some(previous) = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(handle)
        {
            previous.abort();
        }
    }

    fn stop(&self) {
        if let Some(handle) = self.0.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "this machine".to_string())
}

/// The policy a node starts with: the brain's own ceilings.
pub fn default_node_policy(settings: &swarmbot_contracts::config::Settings) -> PolicyMap {
    settings.safety.clone()
}
