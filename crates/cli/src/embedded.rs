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
use swarmbot_contracts::config::BrainConfig;
use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::entry::Entry;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{ConversationId, RequestId};
use swarmbot_contracts::live::LiveEvent;
use swarmbot_contracts::node::{NodeInfo, NodeRoot};
use swarmbot_contracts::request::{Decision, RuntimeRequest};
use swarmbot_contracts::task::Task;
use swarmbot_core::compose;
use swarmbot_core::swarm::Swarm;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::remote::Backend;
use crate::render::{Printer, Roster, print_event};
use crate::session::{SendReport, Session};

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
        }
        Ok(Self { swarm: composed })
    }

    /// Which half of the seam this is — the value a command matches on to decide
    /// nothing else, because both halves are the same [`Session`].
    pub fn backend(&self) -> Backend {
        Backend::Embedded(self.clone())
    }

    pub fn send(&self, conversation: &ConversationId, text: &str) -> Result<SendReport> {
        let sent = self
            .swarm
            .send(conversation, text)
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

    fn tail(&self, conversation: &ConversationId, limit: u32) -> Result<Vec<Entry>> {
        self.swarm
            .conversations
            .tail(conversation, limit)
            .map_err(|err| anyhow!("{err}"))
    }

    fn send(&self, conversation: &ConversationId, text: &str) -> Result<SendReport> {
        Embedded::send(self, conversation, text)
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
        Ok(self
            .swarm
            .store
            .requests_in(conversation)
            .map_err(|err| anyhow!("{err}"))?
            .into_iter()
            .filter(|request| request.is_pending())
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
