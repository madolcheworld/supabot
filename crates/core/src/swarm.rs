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
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{info, warn};

use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::config::BrainConfig;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{BotId, ConversationId, NodeId, TaskId, TurnId};
use swarmbot_contracts::node::{Capabilities, NodeInfo, PolicyMap};
use swarmbot_contracts::repo::{Persistence, RecoveryReport};
use swarmbot_contracts::tool::Toolset;
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

/// Inputs describing a running turn so an external MCP server layer can
/// expose turn-scoped tools to `owns_tool_loop()` backends.
#[derive(Clone, Debug)]
pub struct McpTurnRequest {
    pub bot_id: BotId,
    pub conversation_id: ConversationId,
    pub turn_id: TurnId,
    pub task_id: Option<TaskId>,
    pub workspace: PathBuf,
    pub spool_dir: PathBuf,
    pub toolsets: Vec<Toolset>,
    pub node_caps: Option<Capabilities>,
    pub in_task: bool,
}

/// Per-turn MCP server specs and RAII cleanup guard.
///
/// Dropping this handle unregisters any turn-scoped MCP credentials/endpoints.
pub struct McpTurnRegistration {
    pub servers: Vec<swarmbot_llm::McpServerSpec>,
    cleanup: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl std::fmt::Debug for McpTurnRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTurnRegistration")
            .field("servers", &self.servers)
            .field("has_cleanup", &self.cleanup.is_some())
            .finish()
    }
}

impl Default for McpTurnRegistration {
    fn default() -> Self {
        Self::empty()
    }
}

impl McpTurnRegistration {
    pub fn empty() -> Self {
        Self {
            servers: Vec::new(),
            cleanup: None,
        }
    }

    pub fn new(
        servers: Vec<swarmbot_llm::McpServerSpec>,
        cleanup: impl FnOnce() + Send + Sync + 'static,
    ) -> Self {
        Self {
            servers,
            cleanup: Some(Box::new(cleanup)),
        }
    }
}

impl Drop for McpTurnRegistration {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

/// Port implemented by `brain` (or tests) to expose scoped MCP servers per turn.
pub trait McpTurnProvider: Send + Sync {
    fn register_turn(&self, request: McpTurnRequest) -> Result<McpTurnRegistration, DomainError>;
}

pub type McpProviderSlot = Arc<RwLock<Option<Arc<dyn McpTurnProvider>>>>;

/// A wired brain, without listeners.
/// How often the housekeeping ticker runs.
pub const HOUSEKEEPING_TICK: Duration = Duration::from_secs(60);

/// How many live events the store keeps. A reconnecting client replays from
/// the log, so this is a generous ring, not a short history: 20k events is
/// several days of ordinary use. Its job is to make growth bounded, which it
/// was not — `events_log` gained one permanent row per published change.
pub const EVENT_LOG_KEEP: u64 = 20_000;

/// A routine run still `Running` after this long was cut off by a crash or a
/// deleted bot; no turn legitimately runs for six hours.
pub const ROUTINE_RUN_STALE_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

/// What one housekeeping pass did, so a test (and the log) can see it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HousekeepingReport {
    pub events_dropped: u64,
    pub routine_runs_reaped: usize,
}

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
    mcp_provider: McpProviderSlot,
    expiry: ExpirySlot,
    routine_ticker: ExpirySlot,
    housekeeping: ExpirySlot,
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
        let bots = BotService::new(Arc::clone(&store), Arc::clone(&hub), Arc::clone(&runners));
        let conversations = ConversationService::new(
            Arc::clone(&store),
            Arc::clone(&hub),
            Arc::clone(&transcript),
            Arc::clone(&runners),
            Arc::clone(&tasks),
        );
        // The skill library is `<data>/skills`: a directory per skill with a
        // `SKILL.md` in it. A brain with no library is not an error — a bot's
        // skills then render as names, with the honest note that the library
        // has no entry, instead of pretending there is a playbook behind them.
        let instructions =
            InstructionRenderer::with_skills_root(Arc::clone(&store), data_dir.join("skills"));
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
            mcp_provider: Arc::new(RwLock::new(None)),
            expiry: ExpirySlot(std::sync::Mutex::new(None)),
            routine_ticker: ExpirySlot(std::sync::Mutex::new(None)),
            housekeeping: ExpirySlot(std::sync::Mutex::new(None)),
        }))
    }

    /// Attach the per-turn MCP server provider used for `owns_tool_loop()`
    /// backends. Runners share the slot, so runners started beforehand see it.
    pub fn set_mcp_provider(&self, provider: Arc<dyn McpTurnProvider>) {
        *self.mcp_provider.write().unwrap_or_else(|e| e.into_inner()) = Some(provider);
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

    /// Provision `<data>/workspaces/<bot_name>` (including `.spool/` and
    /// `MEMORY.md`) so each bot's isolated workspace exists as soon as the bot
    /// is created.
    pub fn ensure_bot_workspace(&self, bot_name: &str) -> Result<PathBuf, DomainError> {
        crate::compose::ensure_bot_workspace(&self.workspaces_root(), bot_name)
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
        for bot in &bots {
            let _ = self.ensure_bot_workspace(&bot.name);
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
            mcp_provider: Arc::clone(&self.mcp_provider),
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
    ///
    /// Nothing called this before the review, which is why the whole routines
    /// feature — API, client, UI screen, scheduler, table — was dead code: the
    /// routes could create a routine and nothing ever fired it. `boot` calls it
    /// now; a caller that wants a still brain (`--no-ticker`) can stop it again.
    pub fn start_routine_ticker(&self) {
        let handle = self.routines.spawn_ticker();
        self.routine_ticker.set(handle);
    }

    /// Start the housekeeping ticker: bound the live-event log and mark routine
    /// runs a crash cut off.
    ///
    /// The event log is the only unbounded table in the schema — one permanent
    /// row per published change — and `LiveHub::truncate_log` existed with no
    /// caller, so a long-lived data dir grew without limit. The ring is what a
    /// reconnecting client replays from, so the bound is generous
    /// ([`EVENT_LOG_KEEP`]); it exists to stop unbounded growth, not to make
    /// history short.
    pub fn start_housekeeping_ticker(self: &Arc<Self>) {
        let swarm = Arc::clone(self);
        let handle = tokio::spawn(async move {
            let mut tick = tokio::time::interval(HOUSEKEEPING_TICK);
            // The first tick fires immediately; skipping it keeps boot snappy.
            tick.tick().await;
            loop {
                tick.tick().await;
                swarm.housekeep_once();
            }
        });
        self.housekeeping.set(handle);
    }

    /// One pass of the housekeeping ticker. Public so a test can run it
    /// without waiting on a timer.
    pub fn housekeep_once(&self) -> HousekeepingReport {
        self.housekeep(EVENT_LOG_KEEP)
    }

    /// One pass with an explicit ring size, so a test can prove the bound
    /// without publishing twenty thousand events first.
    pub fn housekeep(&self, keep: u64) -> HousekeepingReport {
        let mut report = HousekeepingReport::default();
        match self.hub.truncate_log(keep) {
            Ok(dropped) => report.events_dropped = dropped,
            Err(err) => warn!(error = %err, "cannot truncate the live-event log"),
        }
        match self
            .routines
            .reap_stale_runs(chrono::Utc::now(), ROUTINE_RUN_STALE_AFTER)
        {
            Ok(reaped) => report.routine_runs_reaped = reaped,
            Err(err) => warn!(error = %err, "cannot reap stale routine runs"),
        }
        report
    }

    /// Stop everything: runners finish their turn, nothing is left "running".
    pub async fn shutdown(&self) {
        self.expiry.stop();
        self.routine_ticker.stop();
        self.housekeeping.stop();
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
    ///
    /// "Idle" means four things at once, and the fourth is the one that is easy
    /// to forget: no runner is busy, nothing is queued, no task is pending, and
    /// **the store holds no turn still marked `Running`**. Without that last
    /// check there is a window at the start of every turn — the delivery has
    /// left the queue and the runner has not yet flipped itself to busy, or has
    /// written the turn and not yet counted an LLM call — in which all the
    /// in-memory signals read quiet while work is in flight. A caller then
    /// reports success and reads a transcript that is missing everything the
    /// turn was about to do. The store row is the authoritative answer because
    /// it is written before the turn does anything and closed when it ends.
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
            // A turn the store still calls `Running` is work in flight even when
            // every in-memory signal has already gone quiet. An unreadable store
            // counts as "not idle" rather than as idle: guessing quiet here is
            // what makes a caller report success on a half-written transcript.
            let turns_running = self
                .store
                .running_turns()
                .map(|turns| turns.len())
                .unwrap_or(1);
            if !busy
                && queued == 0
                && tasks_pending == 0
                && turns_running == 0
                && self.runners.parked_count() == 0
            {
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

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::config::BrainConfig;

    fn test_swarm() -> (Arc<Swarm>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut config = BrainConfig {
            default_backend: "fake".to_string(),
            backends: vec![swarmbot_contracts::config::BackendSpec::fake("fake")],
            ..BrainConfig::default()
        };
        config.team = Some(BrainConfig::demo_team("fake"));
        let swarm = crate::compose::memory(
            config,
            dir.path(),
            vec![],
            &std::collections::BTreeMap::new(),
        )
        .expect("swarm");
        swarm.provision_team().expect("provision");
        (swarm, dir)
    }

    #[tokio::test]
    async fn housekeeping_bounds_the_event_log() {
        // `LiveHub::truncate_log` existed with no caller, so `events_log` grew
        // one permanent row per published change for the life of the data dir.
        let (swarm, _dir) = test_swarm();
        let bot = swarm.bots.by_name("lead").expect("lead");
        for i in 0..20 {
            let _ =
                swarm
                    .transcript
                    .event(&bot.direct_conversation_id, "note", format!("entry {i}"));
        }
        // Count what the log holds through the port: `events_since` from the
        // epoch start is the whole ring.
        let epoch = swarm.store.epoch().expect("epoch");
        let from_start = swarmbot_contracts::live::LiveCursor { epoch, sequence: 0 };
        let before = swarm.store.events_since(from_start, 1000).expect("events");
        assert!(
            before.len() >= 20,
            "events were published: {}",
            before.len()
        );

        let report = swarm.housekeep(5);
        assert!(
            report.events_dropped > 0,
            "the log must shrink to the requested ring: {report:?}"
        );

        let after = swarm.store.events_since(from_start, 1000).expect("events");
        assert!(
            after.len() <= 5,
            "the ring is bounded: {} events left",
            after.len()
        );
        assert!(
            !after.is_empty(),
            "the newest events survive, so a reconnecting client can replay"
        );
    }

    #[tokio::test]
    async fn housekeeping_reaps_a_routine_run_a_crash_cut_off() {
        let (swarm, _dir) = test_swarm();
        let bot = swarm.bots.by_name("lead").expect("lead");
        // Fire it eight hours ago, so the run it started is older than the
        // six-hour staleness bound `housekeep_once` uses.
        let fired_at = chrono::Utc::now() - chrono::Duration::hours(8);
        let mut routine = swarm
            .routines
            .create_routine(
                bot.id.clone(),
                "Nightly",
                "check the build",
                swarmbot_contracts::routine::RoutineSchedule::Interval { every_secs: 60 },
                fired_at - chrono::Duration::minutes(1),
            )
            .expect("routine");
        routine.next_run_at = Some(fired_at - chrono::Duration::seconds(1));
        swarm.store.update_routine(&routine).expect("update");
        swarm.routines.fire_due(fired_at).expect("fire");

        let report = swarm.housekeep_once();
        assert_eq!(report.routine_runs_reaped, 1, "{report:?}");

        let runs = swarm.routines.list_runs(&routine.id, 5).expect("runs");
        assert_eq!(
            runs[0].status,
            swarmbot_contracts::routine::RunStatus::Failed,
            "a run no turn ever finished must not stay Running forever"
        );
    }

    /// A turn the store still calls `Running` keeps the swarm from reporting
    /// idle, even when every in-memory signal has gone quiet.
    ///
    /// This is the window that made a real end-to-end test fail under load with
    /// a transcript holding only the owner's message: the delivery had left the
    /// queue and the runner had not yet flipped itself busy (or had written the
    /// turn and not yet counted an LLM call), so `busy`, `queued`, `tasks` and
    /// `parked_count` all read "nothing happening" and the caller went ahead and
    /// asserted on a transcript the turn had not written yet. The store row is
    /// the one signal that is true from the moment the turn starts.
    #[tokio::test(start_paused = true)]
    async fn a_turn_the_store_calls_running_is_not_idle() {
        use swarmbot_contracts::turn::{Turn, TurnInitiator, TurnStatus};

        let (swarm, _dir) = test_swarm();
        let lead = swarm.bots.by_name("lead").expect("lead");

        // Nothing is queued, no runner is busy, no task is pending: every
        // in-memory signal says idle before this turn exists.
        assert!(
            swarm
                .wait_for_idle(std::time::Duration::from_millis(200))
                .await,
            "a quiet swarm with no turns is idle"
        );

        let mut turn = Turn::start(
            lead.id.clone(),
            lead.direct_conversation_id.clone(),
            TurnInitiator::Owner,
            None,
            None,
        );
        assert_eq!(turn.status, TurnStatus::Running, "a new turn is running");
        swarm.store.insert_turn(&turn).expect("the turn is stored");

        assert!(
            !swarm
                .wait_for_idle(std::time::Duration::from_millis(200))
                .await,
            "a turn still marked Running is work in flight, not idleness"
        );

        // Closing it is what lets the swarm report idle again.
        turn.status = TurnStatus::Completed;
        turn.ended_at = Some(chrono::Utc::now());
        swarm.store.update_turn(&turn).expect("the turn closes");
        assert!(
            swarm
                .wait_for_idle(std::time::Duration::from_millis(200))
                .await,
            "a closed turn leaves the swarm idle"
        );
    }
}
