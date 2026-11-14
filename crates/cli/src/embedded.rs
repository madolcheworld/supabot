//! The embedded brain: build it, boot it, own its lifetime, and expose what a
//! terminal front end needs through the [`Session`] port.
//!
//! This module is the seam's embedded half: [`Session for Embedded`] answers
//! every call by reaching into the in-process [`Swarm`], while
//! [`crate::remote::Remote`] answers the same calls over HTTP. The commands
//! themselves are written once, against the port.
//!
//! The stream helpers ([`drain_synced`], [`emit_event`], [`drain_quiescent`])
//! live here rather than in `render` because they are orchestration-aware:
//! they know what "the hub has caught up" means (the `Synchronized` marker,
//! then 250 ms of quiet) and every command needs the same answer. Both modes
//! feed them the same `UnboundedReceiver<LiveEvent>`, which is what makes their
//! output identical rather than merely similar.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::config::{BrainConfig, Settings};
use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::entry::Entry;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{
    BotId, ConversationId, EntryId, NodeId, RequestId, RoutineId, RuleId,
};
use swarmbot_contracts::live::LiveEvent;
use swarmbot_contracts::node::{NodeInfo, NodeRoot};
use swarmbot_contracts::request::{ApprovalRule, Decision, RuntimeRequest};
use swarmbot_contracts::routine::{Routine, RoutineRun};
use swarmbot_contracts::task::Task;
use swarmbot_contracts::turn::{DEFAULT_WINDOW_HOURS, UsageReport};
use swarmbot_core::compose;
use swarmbot_core::swarm::Swarm;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::remote::Backend;
use crate::render::{Printer, Roster, print_event};
use crate::session::{BotEdit, SendReport, Session, request_relevant_to};
use swarmbot_client::{SecretName, SecretWrite};

/// Everything [`Embedded::build`] needs: a resolved config and where to run.
pub struct BrainParams {
    pub config: BrainConfig,
    pub data_dir: PathBuf,
    pub roots: Vec<NodeRoot>,
    pub secrets: BTreeMap<String, String>,
    /// `true` for an in-memory store (`demo --offline` and the tests).
    pub offline: bool,
}

/// A brain this process owns, from `compose` to `shutdown`.
///
/// Cloning is cheap and shares one swarm: the idle waiter polls beside the event
/// stream and needs its own handle, exactly like a remote session does.
#[derive(Clone)]
pub struct Embedded {
    pub swarm: std::sync::Arc<Swarm>,
    /// Whether this handle booted the swarm (runners started, team provisioned)
    /// or only composed it. A read-only command (`bots ls`, `status`) must not
    /// provision a team into an empty data dir, so it composes without booting —
    /// and then `0 live runners` is a fact about *this process*, not about a
    /// team. [`Session::live_counts`] reports that distinction instead of letting
    /// the zero read as "the team has no runners".
    booted: bool,
}

impl Embedded {
    /// Compose the swarm; with `boot`, also recover, provision the team,
    /// start the runners and the expiry ticker. Read-only commands
    /// (`bots ls`, `transcript`, …) build *without* booting: listing the
    /// roster must not mutate the data dir by provisioning a team into it.
    pub fn build(params: BrainParams, boot: bool) -> Result<Self> {
        let composed = if params.offline {
            compose::memory(
                params.config,
                &params.data_dir,
                params.roots,
                &params.secrets,
            )
        } else {
            compose::durable(
                params.config,
                &params.data_dir,
                params.roots,
                &params.secrets,
            )
        }
        .map_err(|err: DomainError| anyhow!("{err}"))
        .context("cannot build the brain")?;
        if boot {
            let report = composed
                .boot()
                .map_err(|err: DomainError| anyhow!("{err}"))
                .context("cannot boot the brain")?;
            if !report.is_empty() {
                tracing::info!("{}", report.summary());
            }
            composed.start_expiry_ticker();
            // The embedded brain is a brain: a routine that only fires under
            // `swarmbot-brain` would make `swarmbot chat` a different product
            // from the daemon it talks to.
            composed.start_routine_ticker();
            composed.start_housekeeping_ticker();
        }
        Ok(Self {
            swarm: composed,
            booted: boot,
        })
    }

    /// Which half of the seam this is — the value a command matches on to decide
    /// nothing else, because both halves are the same [`Session`].
    pub fn backend(&self) -> Backend {
        Backend::Embedded(self.clone())
    }

    pub fn send(&self, conversation: &ConversationId, text: &str) -> Result<SendReport> {
        self.send_with(conversation, text, &[], None)
    }

    pub fn send_with(
        &self,
        conversation: &ConversationId,
        text: &str,
        addressed: &[BotId],
        reply_to: Option<&EntryId>,
    ) -> Result<SendReport> {
        let sent = self
            .swarm
            .orchestrator
            .send_owner_message(conversation, text, addressed, reply_to)
            .map_err(|err| anyhow!("{err}"))?;
        let queued = sent
            .queued
            .iter()
            .map(|id| {
                self.swarm
                    .store
                    .get_bot(id)
                    .ok()
                    .flatten()
                    .map(|bot| format!("@{}", bot.name))
                    // A delivery to a bot that vanished mid-send is still worth
                    // reporting; the id is the honest fallback.
                    .unwrap_or_else(|| id.to_string())
            })
            .collect();
        Ok(SendReport { queued })
    }

    pub fn subscribe(&self, conversation: Option<ConversationId>) -> UnboundedReceiver<LiveEvent> {
        self.swarm.hub.subscribe(None, conversation)
    }

    pub fn respond(
        &self,
        request: &RequestId,
        decision: Option<Decision>,
        answer: Option<Value>,
    ) -> Result<RuntimeRequest> {
        self.swarm
            .orchestrator
            .respond_request(request, decision, answer)
            .map_err(|err| anyhow!("{err}"))
    }

    pub fn interrupt(&self, conversation: &ConversationId) -> Result<bool> {
        self.swarm
            .orchestrator
            .interrupt(conversation)
            .map_err(|err| anyhow!("{err}"))
    }

    pub async fn shutdown(&self) {
        self.swarm.shutdown().await;
    }

    /// What is still moving — the honest answer a timed-out `run` prints
    /// instead of just giving up. Never empty: when nothing claims to be busy
    /// it says that, because an empty "still running:" list reads like a
    /// rendering bug rather than the truth.
    pub fn still_running(&self) -> Vec<String> {
        crate::session::still_running(self)
    }
}

impl Session for Embedded {
    fn describe(&self) -> String {
        "the in-process brain".to_string()
    }

    fn poll_interval(&self) -> Duration {
        // Reading this process's own store is microseconds; the in-process
        // waiter may therefore poll as often as `Swarm::wait_for_idle` did.
        Duration::from_millis(25)
    }

    fn bots(&self) -> Result<Vec<Bot>> {
        self.swarm.bots.list().map_err(|err| anyhow!("{err}"))
    }

    fn conversations(&self) -> Result<Vec<Conversation>> {
        self.swarm
            .conversations
            .list()
            .map_err(|err| anyhow!("{err}"))
    }

    fn tasks(&self) -> Result<Vec<Task>> {
        let mut tasks = Vec::new();
        for bot in self.bots()? {
            tasks.extend(
                self.swarm
                    .tasks
                    .list(&bot.id)
                    .map_err(|err| anyhow!("{err}"))?,
            );
        }
        Ok(tasks)
    }

    fn nodes(&self) -> Result<Vec<NodeInfo>> {
        self.swarm
            .store
            .list_nodes()
            .map_err(|err| anyhow!("{err}"))
    }

    fn usage(&self, since: Option<&str>) -> Result<UsageReport> {
        // The same window rule the brain's `GET /api/usage` applies, from the
        // same function: an embedded run and a remote one must not report
        // different totals for the same turns.
        let until = chrono::Utc::now();
        let since = match since {
            Some(raw) => chrono::DateTime::parse_from_rfc3339(raw.trim())
                .map(|parsed| parsed.with_timezone(&chrono::Utc))
                .map_err(|err| {
                    anyhow!(
                        "`--since` must be an RFC 3339 timestamp (e.g. 2026-01-31T00:00:00Z), \
                         got {raw:?}: {err}"
                    )
                })?,
            None => until - chrono::Duration::hours(DEFAULT_WINDOW_HOURS),
        };
        if since > until {
            return Err(anyhow!(
                "`--since` ({}) is in the future; the report would cover an empty window",
                since.to_rfc3339()
            ));
        }
        UsageReport::window(self.swarm.store.as_ref(), since, until).map_err(|err| anyhow!("{err}"))
    }

    fn tail(&self, conversation: &ConversationId, limit: u32) -> Result<Vec<Entry>> {
        self.swarm
            .conversations
            .tail(conversation, limit)
            .map_err(|err| anyhow!("{err}"))
    }

    fn send(&self, conversation: &ConversationId, text: &str) -> Result<SendReport> {
        Embedded::send(self, conversation, text)
    }

    fn send_with(
        &self,
        conversation: &ConversationId,
        text: &str,
        addressed: &[BotId],
        reply_to: Option<&EntryId>,
    ) -> Result<SendReport> {
        Embedded::send_with(self, conversation, text, addressed, reply_to)
    }

    fn subscribe(
        &self,
        conversation: Option<ConversationId>,
    ) -> Result<UnboundedReceiver<LiveEvent>> {
        Ok(Embedded::subscribe(self, conversation))
    }

    fn respond(
        &self,
        request: &RequestId,
        decision: Option<Decision>,
        answer: Option<Value>,
    ) -> Result<RuntimeRequest> {
        Embedded::respond(self, request, decision, answer)
    }

    fn interrupt(&self, conversation: &ConversationId) -> Result<bool> {
        Embedded::interrupt(self, conversation)
    }

    fn pending_in(&self, conversation: &ConversationId) -> Result<Vec<RuntimeRequest>> {
        let pending: Vec<RuntimeRequest> = self
            .swarm
            .store
            .pending_requests()
            .map_err(|err| anyhow!("{err}"))?
            .into_iter()
            .filter(|request| request.is_pending())
            .collect();
        if pending.is_empty()
            || pending
                .iter()
                .all(|request| request.conversation_id == *conversation)
        {
            return Ok(pending);
        }
        let conversations = self.conversations()?;
        let bots = self.bots()?;
        let tasks = self.tasks()?;
        let mut turns = Vec::new();
        for request in &pending {
            if request.conversation_id != *conversation
                && request.task_id.is_none()
                && let Some(turn_id) = &request.turn_id
                && let Ok(Some(turn)) = self.swarm.store.get_turn(turn_id)
            {
                turns.push(turn);
            }
        }
        Ok(pending
            .into_iter()
            .filter(|request| {
                request_relevant_to(request, conversation, &conversations, &bots, &tasks, &turns)
            })
            .collect())
    }

    fn new_session(&self, bot: &Bot) -> Result<()> {
        self.swarm
            .bots
            .new_session(&bot.id)
            .map_err(|err| anyhow!("{err}"))
    }

    fn busy(&self) -> Result<Vec<String>> {
        // The same question `Swarm::wait_for_idle` asks, spelled out as the
        // lines a human reads: mid-turn bots, waiting deliveries, deliveries
        // parked for a runner that does not exist yet, unfinished tasks and
        // unanswered requests. Empty means idle, in both modes.
        let mut lines = Vec::new();
        for bot in self
            .swarm
            .store
            .list_bots()
            .map_err(|err| anyhow!("{err}"))?
        {
            if self.swarm.runners.is_busy(&bot.id) {
                let where_ = self
                    .swarm
                    .runners
                    .active_turn(&bot.id)
                    .map(|id| format!(" in {}", short_conversation(&id)))
                    .unwrap_or_default();
                lines.push(format!("@{} is mid-turn{where_}", bot.name));
            }
            let queued = self
                .swarm
                .store
                .queued_deliveries(&bot.id)
                .map_err(|err| anyhow!("{err}"))?
                .len();
            if queued > 0 {
                lines.push(format!(
                    "@{name} has {queued} queued delivery(ies)",
                    name = bot.name
                ));
            }
        }
        let parked = self.swarm.runners.parked_count();
        if parked > 0 {
            lines.push(format!(
                "{parked} delivery(ies) are parked waiting for a runner that does not exist yet"
            ));
        }
        for task in self
            .swarm
            .store
            .queued_tasks()
            .map_err(|err| anyhow!("{err}"))?
        {
            lines.push(format!("task \"{}\" is queued", task.title));
        }
        for task in self
            .swarm
            .store
            .running_tasks()
            .map_err(|err| anyhow!("{err}"))?
        {
            lines.push(format!("task \"{}\" is running", task.title));
        }
        for request in self
            .swarm
            .store
            .pending_requests()
            .map_err(|err| anyhow!("{err}"))?
        {
            lines.push(format!(
                "request \"{}\" is waiting for an answer",
                request.title()
            ));
        }
        Ok(lines)
    }

    fn delegation_lines(&self, conversation: &ConversationId) -> Result<Vec<String>> {
        Ok(crate::demo::delegation_lines(&self.swarm, conversation))
    }

    // --- the owner's write surface -------------------------------------------

    fn rules(&self) -> Result<Vec<ApprovalRule>> {
        self.swarm.approval.rules().map_err(|err| anyhow!("{err}"))
    }

    fn delete_rule(&self, id: &RuleId) -> Result<()> {
        // The brain's route refuses a rule that is not there instead of a silent
        // no-op; the embedded half keeps the same promise, or the two modes
        // would disagree about what "deleted" means.
        if !self.rules()?.iter().any(|rule| &rule.id == id) {
            return Err(anyhow!(
                "there is no approval rule {id} — `swarmbot rules ls` lists the saved ones"
            ));
        }
        self.swarm
            .approval
            .delete_rule(id)
            .map_err(|err| anyhow!("{err}"))
    }

    fn pending(&self) -> Result<Vec<RuntimeRequest>> {
        self.swarm
            .approval
            .pending()
            .map_err(|err| anyhow!("{err}"))
    }

    fn settings(&self) -> Result<Settings> {
        Ok(self.swarm.settings.get())
    }

    fn update_settings(&self, patch: &Value) -> Result<Settings> {
        let merged = crate::config::merge_settings(&self.swarm.settings.get(), patch)
            .map_err(|err| anyhow!("{err}"))?;
        self.swarm
            .settings
            .update(|slot| *slot = merged)
            .map_err(|err| anyhow!("{err}"))
    }

    fn routines(&self) -> Result<Vec<Routine>> {
        self.swarm
            .routines
            .list_routines()
            .map_err(|err| anyhow!("{err}"))
    }

    fn pause_routine(&self, id: &RoutineId) -> Result<Routine> {
        self.swarm
            .routines
            .pause_routine(id)
            .map_err(|err| anyhow!("{err}"))
    }

    fn resume_routine(&self, id: &RoutineId) -> Result<Routine> {
        self.swarm
            .routines
            .resume_routine(id, chrono::Utc::now())
            .map_err(|err| anyhow!("{err}"))
    }

    fn delete_routine(&self, id: &RoutineId) -> Result<()> {
        self.swarm
            .routines
            .delete_routine(id)
            .map_err(|err| anyhow!("{err}"))
    }

    fn routine_runs(&self, id: &RoutineId, limit: u32) -> Result<Vec<RoutineRun>> {
        self.swarm
            .routines
            .list_runs(id, limit.clamp(1, 200))
            .map_err(|err| anyhow!("{err}"))
    }

    fn revoke_node(&self, id: &NodeId) -> Result<()> {
        // The brain's route refuses the local node *and* closes a connected
        // socket for a remote one. In this process the only node is the local
        // one, and that refusal is the honest answer: revoking it would leave
        // every tool call failing with `node_unavailable` and no way back.
        if id.is_local() {
            return Err(anyhow!(
                "the local node cannot be revoked: it runs inside this process — with --brain, \
                 `nodes revoke` cuts a remote machine off"
            ));
        }
        // No gateway lives in this process, so a foreign node cannot be paired
        // here at all; a row for one would be a leftover from a daemon's data
        // dir. Say that instead of writing a revocation nothing can enforce.
        Err(anyhow!(
            "this in-process brain has no node gateway, so the only node it can hold is its own \
             local one — run `swarmbot nodes revoke` with --brain to cut a paired machine off"
        ))
    }

    fn edit_bot(&self, name: &str, edit: &BotEdit) -> Result<Bot> {
        let mut bot = self
            .swarm
            .bots
            .by_name(name)
            .map_err(|err| anyhow!("{err}"))?;
        // The same fields the brain's `PATCH /api/bots/{id}` touches, and the
        // same empty-string rule for the model, so the two modes cannot drift.
        if let Some(backend) = &edit.backend {
            bot.backend = backend.clone();
        }
        if let Some(model) = &edit.model {
            let trimmed = model.trim();
            bot.model = (!trimmed.is_empty()).then(|| trimmed.to_string());
        }
        if let Some(skills) = &edit.skills {
            bot.skills = skills.clone();
        }
        let saved = self
            .swarm
            .bots
            .update(&bot)
            .map_err(|err| anyhow!("{err}"))?;
        // `update` does not touch `status`; enabling/disabling is its own writer
        // (`set_enabled`) because it also starts or stops the bot's runner.
        match edit.enabled {
            Some(enabled) => self
                .swarm
                .bots
                .set_enabled(&saved.id, enabled)
                .map_err(|err| anyhow!("{err}")),
            None => Ok(saved),
        }
    }

    fn entries_before(
        &self,
        conversation: &ConversationId,
        before_seq: u64,
        limit: u32,
    ) -> Result<Vec<Entry>> {
        self.swarm
            .store
            .entries_before(conversation, before_seq, limit)
            .map_err(|err| anyhow!("{err}"))
    }

    fn export_transcript(&self, conversation: &ConversationId) -> Result<String> {
        crate::admin::export_ndjson(self.swarm.store.as_ref(), conversation)
    }

    fn secrets(&self) -> Result<Vec<SecretName>> {
        crate::admin::list_secrets(&self.swarm.data_dir, &self.swarm.config)
    }

    fn set_secret(&self, name: &str, value: &str) -> Result<SecretWrite> {
        crate::admin::write_secret(&self.swarm.data_dir, name, Some(value))
    }

    fn delete_secret(&self, name: &str) -> Result<SecretWrite> {
        crate::admin::write_secret(&self.swarm.data_dir, name, None)
    }

    fn live_counts(&self) -> Option<crate::session::LiveCounts> {
        let store = self.swarm.store.as_ref();
        // Each of these is a `Result` on purpose in the store; a status command
        // must not fail because one counter could not be read, so a failed read
        // counts as zero *and* the caller is told by the count being absent…
        // which it cannot be here. So: a store error makes the whole block
        // `None`, and `status` prints "not readable" for those lines rather than
        // inventing zeros.
        let turns_running = store.running_turns().ok()?.len() as u64;
        let mut deliveries_queued = 0u64;
        for bot in store.list_bots().ok()? {
            deliveries_queued += store.queued_deliveries(&bot.id).ok()?.len() as u64;
        }
        Some(crate::session::LiveCounts {
            turns_running,
            deliveries_queued,
            deliveries_parked: self.swarm.runners.parked_count() as u64,
            runners: self.swarm.runners.len() as u64,
            booted: self.booted,
        })
    }
}

/// The distinguishing tail of an id, for one-line messages: ids are long and
/// share a prefix, so the end is the part a human can match against a roster.
pub(crate) fn short_conversation(id: &ConversationId) -> String {
    let raw = id.as_str();
    let tail: String = raw
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

/// Route one live event to the printer (prose) or to the JSON sink.
pub fn emit_event<W: Write>(
    printer: &mut Printer<W>,
    roster: &mut Roster,
    event: &LiveEvent,
    json: bool,
    with_snapshot: bool,
) -> Result<()> {
    if json {
        roster.observe_event(event);
        if let LiveEvent::EntryUpsert { entry, .. } = event
            && !roster.should_show(entry)
        {
            return Ok(());
        }
        printer.json_event(event)
    } else {
        print_event(printer, roster, event, with_snapshot)
    }
}

/// Drain a fresh subscription through its `Synchronized` marker. The marker is
/// the hub's promise that everything persisted before the subscribe call has
/// been delivered — sending *before* seeing it could race the snapshot.
pub async fn drain_synced<W: Write>(
    rx: &mut UnboundedReceiver<LiveEvent>,
    printer: &mut Printer<W>,
    roster: &mut Roster,
    json: bool,
    with_snapshot: bool,
) -> Result<()> {
    let budget = Duration::from_secs(10);
    loop {
        match tokio::time::timeout(budget, rx.recv()).await {
            Ok(Some(event)) => {
                let synced = matches!(event, LiveEvent::Synchronized { .. });
                emit_event(printer, roster, &event, json, with_snapshot)?;
                if synced {
                    return Ok(());
                }
            }
            Ok(None) => return Err(anyhow!("the event stream closed before it synchronized")),
            Err(_) => {
                return Err(anyhow!(
                    "the event stream did not synchronize within {}s",
                    budget.as_secs()
                ));
            }
        }
    }
}

/// After the team went idle the hub's forwarder may still be a few events
/// behind; drain until the channel has been quiet for `quiet_for`. This is a
/// deadline, not a sleep-on-a-guess: the loop ends either way.
pub async fn drain_quiescent<W: Write>(
    rx: &mut UnboundedReceiver<LiveEvent>,
    printer: &mut Printer<W>,
    roster: &mut Roster,
    json: bool,
    quiet_for: Duration,
) -> Result<()> {
    loop {
        match tokio::time::timeout(quiet_for, rx.recv()).await {
            Ok(Some(event)) => emit_event(printer, roster, &event, json, false)?,
            Ok(None) => return Ok(()),
            Err(_) => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Theme;
    use crate::tmp::TempDir;

    fn config() -> BrainConfig {
        BrainConfig {
            team: Some(BrainConfig::demo_team("fake")),
            ..BrainConfig::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_embedded_brain_boots_sends_and_shuts_down() {
        let dir = TempDir::new("embedded").expect("temp");
        let embedded = Embedded::build(
            BrainParams {
                config: config(),
                data_dir: dir.path().to_path_buf(),
                roots: Vec::new(),
                secrets: BTreeMap::new(),
                offline: true,
            },
            true,
        )
        .expect("builds");
        // The seam hands back the same swarm.
        assert!(matches!(embedded.backend(), Backend::Embedded(_)));

        let lead = embedded.swarm.bots.by_name("lead").expect("the lead");
        let mut rx = embedded.subscribe(Some(lead.direct_conversation_id.clone()));
        let mut buffer = Vec::new();
        let mut roster = Roster::new(Vec::new(), Vec::new(), None);
        {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            drain_synced(&mut rx, &mut printer, &mut roster, false, false)
                .await
                .expect("synchronizes");
            embedded
                .send(&lead.direct_conversation_id, "hello there")
                .expect("sends");
            assert!(embedded.swarm.wait_for_idle(Duration::from_secs(20)).await);
            drain_quiescent(
                &mut rx,
                &mut printer,
                &mut roster,
                false,
                Duration::from_millis(200),
            )
            .await
            .expect("drains");
            printer.finish().expect("finishes");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("you › hello there"), "{out}");
        // The fake backend answered (empty queue reply is still an answer).
        assert!(out.contains("@lead ›"), "{out}");

        embedded
            .interrupt(&lead.direct_conversation_id)
            .expect("interrupts");
        embedded.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unbooted_brain_does_not_provision_a_team() {
        let dir = TempDir::new("unbooted").expect("temp");
        let embedded = Embedded::build(
            BrainParams {
                config: config(),
                data_dir: dir.path().to_path_buf(),
                roots: Vec::new(),
                secrets: BTreeMap::new(),
                offline: true,
            },
            false,
        )
        .expect("builds");
        assert!(
            embedded.swarm.bots.list().expect("bots").is_empty(),
            "a read-only command must not create the team as a side effect"
        );
        // The runners map is reachable and empty; nothing was spawned.
        assert_eq!(embedded.swarm.runners.len(), 0);
    }

    #[test]
    fn still_running_reports_something_when_nothing_runs() {
        // No swarm needed for the shape check: an idle brain says so honestly.
        let dir = TempDir::new("idle").expect("temp");
        let embedded = Embedded::build(
            BrainParams {
                config: config(),
                data_dir: dir.path().to_path_buf(),
                roots: Vec::new(),
                secrets: BTreeMap::new(),
                offline: true,
            },
            false,
        )
        .expect("builds");
        let lines = embedded.still_running();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("nothing reports as busy"), "{lines:?}");
    }
}
