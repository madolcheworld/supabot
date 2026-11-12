//! The port every command talks through: **one brain, in this process or behind
//! HTTP**.
//!
//! [`Session`] is the whole seam. [`crate::embedded::Embedded`] implements it by
//! calling the orchestration directly; [`crate::remote::Remote`] implements it by
//! calling the brain's REST API and its `/ws/live` socket. Every command body —
//! `run`, `chat`, `transcript`, `bots ls`, `tasks ls`, `nodes ls` — is written
//! once against this trait, which is what makes the two modes render identically
//! instead of "almost identically": there is one `run_once`, one chat loop, one
//! `render.rs`.
//!
//! Two rules shape the trait:
//!
//! * **every method is blocking and synchronous.** The HTTP client is blocking
//!   on purpose (see `swarmbot-client`'s crate docs), and the in-process half is
//!   synchronous anyway; the async callers wrap the calls that can take a while
//!   in `spawn_blocking`. A trait with async methods would need `async_trait`,
//!   which this crate does not depend on.
//! * **a failure is a value that says what failed.** Nothing here panics, and
//!   nothing here invents a reassuring answer: [`Session::busy`] returns an
//!   error when the brain cannot be asked, so a run reports "cannot reach the
//!   brain" instead of "the team went idle".
//!
//! [`Session::subscribe`] hands back the same `UnboundedReceiver<LiveEvent>` the
//! embedded hub does, so the stream helpers in [`crate::embedded`]
//! (`drain_synced`, `emit_event`, `drain_quiescent`) work for both modes
//! unchanged.

use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::Value;
use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::entry::Entry;
use swarmbot_contracts::id::{BotId, ConversationId, EntryId, RequestId};
use swarmbot_contracts::live::LiveEvent;
use swarmbot_contracts::node::NodeInfo;
use swarmbot_contracts::request::{Decision, RuntimeRequest};
use swarmbot_contracts::task::Task;
use swarmbot_contracts::turn::Turn;
use tokio::sync::mpsc::UnboundedReceiver;

/// What one sent message did, in the only fact a front end prints.
///
/// Both halves resolve the queued bots to `@handle`s themselves: an in-process
/// brain reads its own store, a remote one asks `/api/bots`. A caller that had
/// to do it would need a third lookup path per mode.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SendReport {
    /// Bots that were busy elsewhere: their copy of the message waits.
    pub queued: Vec<String>,
}

/// One brain, reached one way.
///
/// `Clone` is required because the idle waiter runs beside the event stream and
/// needs its own handle; both implementations are cheap handles (`Arc<Swarm>`,
/// and a client that shares its connection pool and token).
pub trait Session: Clone + Send + Sync + 'static {
    /// Which brain this is, for messages that must say so
    /// ("cannot ask the brain at http://127.0.0.1:8791 what is running").
    fn describe(&self) -> String;

    /// How often the idle waiter may ask. An in-process brain reads its own
    /// store, which is microseconds; a remote brain answers over HTTP, so it is
    /// polled less often.
    fn poll_interval(&self) -> Duration;

    /// How many *consecutive* idle readings confirm idleness.
    ///
    /// One is enough when the state comes from this process's own store: a send
    /// queues its deliveries synchronously, so the poll right after it already
    /// sees the work. A remote brain publishes `Working` from the runner task
    /// that picks the delivery up, which happens *after* `POST /messages`
    /// answers — so a single poll in that window would report "idle" for a team
    /// that is about to answer, and the run would exit before the reply arrived.
    /// Requiring two readings closes the window; it costs one extra interval at
    /// the end of a run.
    fn idle_confirmations(&self) -> usize {
        1
    }

    fn bots(&self) -> Result<Vec<Bot>>;

    fn conversations(&self) -> Result<Vec<Conversation>>;

    /// Every task the brain knows, in its own order.
    fn tasks(&self) -> Result<Vec<Task>>;

    fn nodes(&self) -> Result<Vec<NodeInfo>>;

    /// The newest `limit` entries of one conversation, ascending.
    fn tail(&self, conversation: &ConversationId, limit: u32) -> Result<Vec<Entry>>;

    /// Post the owner's message. Mentions are parsed by the brain in both
    /// modes, so neither half sends an `addressed` list of its own.
    fn send(&self, conversation: &ConversationId, text: &str) -> Result<SendReport>;

    /// Post the owner's message with explicit addressing and/or reply target.
    fn send_with(
        &self,
        conversation: &ConversationId,
        text: &str,
        addressed: &[BotId],
        reply_to: Option<&EntryId>,
    ) -> Result<SendReport> {
        let _ = (addressed, reply_to);
        self.send(conversation, text)
    }

    /// The live event stream, optionally narrowed to one conversation.
    ///
    /// The receiver ends when the brain closes the stream; the *reason* a
    /// stream ended badly is reported through [`Session::stream_error`],
    /// because a closed channel cannot carry one.
    fn subscribe(
        &self,
        conversation: Option<ConversationId>,
    ) -> Result<UnboundedReceiver<LiveEvent>>;

    /// Answer a runtime request (an approval decision, or a free-text answer).
    fn respond(
        &self,
        request: &RequestId,
        decision: Option<Decision>,
        answer: Option<Value>,
    ) -> Result<RuntimeRequest>;

    /// Stop the running turn of one conversation; `false` means nothing was
    /// running, which is an answer, not a failure.
    fn interrupt(&self, conversation: &ConversationId) -> Result<bool>;

    /// The requests of one conversation that are still waiting for an answer.
    fn pending_in(&self, conversation: &ConversationId) -> Result<Vec<RuntimeRequest>>;

    /// Drop a bot's session history (the REPL's `/new`).
    fn new_session(&self, bot: &Bot) -> Result<()>;

    /// What is still moving: mid-turn bots, waiting deliveries, unfinished
    /// tasks, unanswered requests. **Empty means the team is idle.**
    fn busy(&self) -> Result<Vec<String>>;

    /// The `delegated:` lines of one conversation, read from the tasks that
    /// belong to it — the same sentence in both modes.
    fn delegation_lines(&self, conversation: &ConversationId) -> Result<Vec<String>>;

    /// Why the event stream ended, when the reason is one the user must hear
    /// (a token that stopped working mid-run, a frame this build cannot
    /// decode). `None` when the stream is fine or ended cleanly.
    fn stream_error(&self) -> Option<String> {
        None
    }
}

/// Wait until [`Session::busy`] comes back empty, or the budget runs out.
///
/// This is the remote-capable twin of `Swarm::wait_for_idle`: the same question,
/// asked through the port instead of through the store. The poll runs on the
/// blocking pool so a slow HTTP answer never stalls the event rendering beside
/// it, and the deadline uses `tokio::time` so a test can pause time.
pub async fn wait_for_idle<S: Session>(session: &S, budget: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + budget;
    let interval = session.poll_interval();
    let needed = session.idle_confirmations().max(1);
    let mut idle_in_a_row = 0;
    loop {
        let probe = session.clone();
        let busy = tokio::task::spawn_blocking(move || probe.busy())
            .await
            .map_err(|err| anyhow!("the idle check stopped: {err}"))?
            .map_err(|err| anyhow!("cannot ask {} what is running: {err}", session.describe()))?;
        if busy.is_empty() {
            idle_in_a_row += 1;
            if idle_in_a_row >= needed {
                return Ok(true);
            }
        } else {
            idle_in_a_row = 0;
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(interval).await;
    }
}

/// What a timed-out command prints: the busy lines, or one honest line saying
/// that nothing claims to be busy — because "TIMED OUT — still running: " with
/// an empty list would read like a rendering bug rather than the truth (the idle
/// check itself never answered in time).
pub fn still_running<S: Session>(session: &S) -> Vec<String> {
    match session.busy() {
        Ok(lines) if !lines.is_empty() => lines,
        Ok(_) => vec!["nothing reports as busy (the idle check itself timed out)".to_string()],
        Err(err) => vec![format!(
            "cannot ask {} what is running: {err}",
            session.describe()
        )],
    }
}

/// `crate::embedded::drain_synced`, but a stream that died for a reason the
/// session knows about reports *that* reason.
///
/// Without this, a remote brain that refuses the token answers "the event stream
/// closed before it synchronized" — true, and useless: the fact that matters is
/// the 401 the socket saw, which a closed channel cannot carry.
pub async fn drain_synced_honest<S: Session, W: std::io::Write>(
    session: &S,
    rx: &mut UnboundedReceiver<LiveEvent>,
    printer: &mut crate::render::Printer<W>,
    roster: &mut crate::render::Roster,
    json: bool,
    with_snapshot: bool,
) -> Result<()> {
    crate::embedded::drain_synced(rx, printer, roster, json, with_snapshot)
        .await
        .map_err(|err| honest_stream_error(session, err))
}

/// Replace a generic stream failure with the session's own reason, when it has
/// one.
pub fn honest_stream_error<S: Session>(session: &S, err: anyhow::Error) -> anyhow::Error {
    match session.stream_error() {
        Some(reason) => anyhow!("{reason}"),
        None => err,
    }
}

/// Whether a runtime request belongs to `focus`.
///
/// A direct conversation only matches requests created in `focus` (or tied to
/// a task whose `origin` is `focus`). A group conversation also surfaces:
/// - requests tied to a task whose `origin` is that group (so cards raised in
///   delegated DM turns appear inline in the group), and
/// - requests raised in a member's direct conversation when no task belonging
///   to a different origin claims them.
pub(crate) fn request_relevant_to(
    request: &RuntimeRequest,
    focus: &ConversationId,
    conversations: &[Conversation],
    bots: &[Bot],
    tasks: &[Task],
    turns: &[Turn],
) -> bool {
    if request.conversation_id == *focus {
        return true;
    }
    let task_id = request.task_id.as_ref().or_else(|| {
        request.turn_id.as_ref().and_then(|turn_id| {
            turns
                .iter()
                .find(|turn| &turn.id == turn_id)
                .and_then(|turn| turn.task_id.as_ref())
        })
    });
    if let Some(task_id) = task_id
        && let Some(task) = tasks.iter().find(|task| &task.id == task_id)
    {
        return task.origin == *focus;
    }
    let Some(group) = conversations
        .iter()
        .find(|c| &c.id == focus && c.kind.is_group())
    else {
        return false;
    };
    if !group.kind.participants().contains(&request.bot_id) {
        return false;
    }
    bots.iter()
        .find(|bot| bot.id == request.bot_id)
        .is_some_and(|bot| bot.direct_conversation_id == request.conversation_id)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A session that answers from a script, so the waiter's own logic
    /// (deadline, interval, error path) is testable without a brain — and
    /// without a socket, which is the rule this crate's tests keep.
    #[derive(Clone)]
    struct Scripted {
        answers: Arc<Mutex<VecDeque<Result<Vec<String>>>>>,
        stream_error: Option<String>,
        confirmations: usize,
    }

    impl Scripted {
        fn new(answers: Vec<Result<Vec<String>>>) -> Self {
            Self {
                answers: Arc::new(Mutex::new(answers.into())),
                stream_error: None,
                confirmations: 1,
            }
        }

        fn with_stream_error(mut self, reason: &str) -> Self {
            self.stream_error = Some(reason.to_string());
            self
        }

        fn with_confirmations(mut self, confirmations: usize) -> Self {
            self.confirmations = confirmations;
            self
        }

        /// The next scripted answer; an exhausted script reports idle rather
        /// than spinning the waiter forever.
        fn next_answer(&self) -> Result<Vec<String>> {
            let mut answers = self.answers.lock().unwrap_or_else(|err| err.into_inner());
            answers.pop_front().unwrap_or_else(|| Ok(Vec::new()))
        }
    }

    impl Session for Scripted {
        fn describe(&self) -> String {
            "the scripted brain".to_string()
        }
        fn poll_interval(&self) -> Duration {
            Duration::from_millis(1)
        }
        fn idle_confirmations(&self) -> usize {
            self.confirmations
        }
        fn bots(&self) -> Result<Vec<Bot>> {
            Ok(Vec::new())
        }
        fn conversations(&self) -> Result<Vec<Conversation>> {
            Ok(Vec::new())
        }
        fn tasks(&self) -> Result<Vec<Task>> {
            Ok(Vec::new())
        }
        fn nodes(&self) -> Result<Vec<NodeInfo>> {
            Ok(Vec::new())
        }
        fn tail(&self, _: &ConversationId, _: u32) -> Result<Vec<Entry>> {
            Ok(Vec::new())
        }
        fn send(&self, _: &ConversationId, _: &str) -> Result<SendReport> {
            Ok(SendReport::default())
        }
        fn subscribe(&self, _: Option<ConversationId>) -> Result<UnboundedReceiver<LiveEvent>> {
            let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
            Ok(rx)
        }
        fn respond(
            &self,
            _: &RequestId,
            _: Option<Decision>,
            _: Option<Value>,
        ) -> Result<RuntimeRequest> {
            Err(anyhow!("the scripted session answers no requests"))
        }
        fn interrupt(&self, _: &ConversationId) -> Result<bool> {
            Ok(false)
        }
        fn pending_in(&self, _: &ConversationId) -> Result<Vec<RuntimeRequest>> {
            Ok(Vec::new())
        }
        fn new_session(&self, _: &Bot) -> Result<()> {
            Ok(())
        }
        fn busy(&self) -> Result<Vec<String>> {
            self.next_answer()
        }
        fn delegation_lines(&self, _: &ConversationId) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn stream_error(&self) -> Option<String> {
            self.stream_error.clone()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_waiter_returns_as_soon_as_nothing_is_busy() {
        let session = Scripted::new(vec![
            Ok(vec!["@lead is mid-turn".to_string()]),
            Ok(vec!["@lead is mid-turn".to_string()]),
            Ok(Vec::new()),
        ]);
        let idle = wait_for_idle(&session, Duration::from_secs(30))
            .await
            .expect("waits");
        assert!(idle, "the third poll reported nothing busy");
    }

    #[tokio::test(start_paused = true)]
    async fn the_waiter_gives_up_when_the_budget_runs_out() {
        let session = Scripted::new(vec![
            Ok(vec!["task \"x\" is queued".to_string()]),
            Ok(vec!["task \"x\" is queued".to_string()]),
            Ok(vec!["task \"x\" is queued".to_string()]),
            Ok(vec!["task \"x\" is queued".to_string()]),
        ]);
        let idle = wait_for_idle(&session, Duration::from_millis(3))
            .await
            .expect("waits");
        assert!(
            !idle,
            "a team that stays busy inside the budget is a timeout"
        );
    }

    /// The rule a remote session needs: one idle reading right after a send can
    /// be a bot that has not picked the delivery up yet, so it takes two in a
    /// row — and a busy reading in between restarts the count.
    #[tokio::test(start_paused = true)]
    async fn an_idle_reading_has_to_repeat_before_the_waiter_believes_it() {
        let session = Scripted::new(vec![
            Ok(Vec::new()),
            Ok(vec!["@lead is mid-turn".to_string()]),
            Ok(Vec::new()),
            Ok(Vec::new()),
        ])
        .with_confirmations(2);
        let idle = wait_for_idle(&session, Duration::from_secs(30))
            .await
            .expect("waits");
        assert!(idle, "two consecutive idle readings confirm");
        // Three polls were enough for the first pair? No: the busy reading in
        // between reset the count, so all four were consumed.
        assert!(
            session.next_answer().expect("script exhausted").is_empty(),
            "the waiter really did poll four times"
        );

        let one = Scripted::new(vec![Ok(Vec::new())]).with_confirmations(1);
        assert!(
            wait_for_idle(&one, Duration::from_secs(30))
                .await
                .expect("waits"),
            "the embedded default believes a single reading"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_poll_is_an_error_not_a_timeout() {
        // The honest answer to "the brain cannot be reached" is that error,
        // never "the team is still running": a run must not blame the team for
        // a dead socket.
        let session = Scripted::new(vec![Err(anyhow!("connection refused"))]);
        let err = wait_for_idle(&session, Duration::from_secs(30))
            .await
            .expect_err("the poll failed");
        let text = format!("{err:#}");
        assert!(text.contains("cannot ask the scripted brain"), "{text}");
        assert!(text.contains("connection refused"), "{text}");
    }

    #[test]
    fn still_running_says_so_when_nothing_claims_to_be_busy() {
        let session = Scripted::new(vec![Ok(Vec::new())]);
        let lines = still_running(&session);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("nothing reports as busy"), "{lines:?}");
    }

    #[test]
    fn still_running_reports_a_failed_probe_as_the_failure() {
        let session = Scripted::new(vec![Err(anyhow!("the brain is gone"))]);
        let lines = still_running(&session);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("cannot ask"), "{lines:?}");
        assert!(lines[0].contains("the brain is gone"), "{lines:?}");
    }

    #[test]
    fn a_known_stream_reason_replaces_the_generic_one() {
        let generic = || anyhow!("the event stream closed before it synchronized");
        let quiet = Scripted::new(Vec::new());
        assert_eq!(
            honest_stream_error(&quiet, generic()).to_string(),
            "the event stream closed before it synchronized",
            "with no reason recorded the original error survives"
        );
        let refused = quiet.with_stream_error("the brain refused the token (401)");
        assert_eq!(
            honest_stream_error(&refused, generic()).to_string(),
            "the brain refused the token (401)"
        );
    }
}
