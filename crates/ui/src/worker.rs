//! The worker threads and the bridge back to the main (GPUI) thread.
//!
//! **Two** `std::thread`s, not one, and the reason is worth stating because it
//! looks like a deviation from "one worker thread": [`swarmbot_client::
//! LiveSubscription::next_event`] blocks until a frame arrives and exposes no
//! read timeout, so a single thread parked on the socket could not also service
//! a Stop button or a card answer promptly. Splitting into a *live* thread (pumps
//! `/ws/live`, owns reconnection) and an *rpc* thread (runs one blocking REST
//! call per [`Command`]) keeps both responsive while preserving the invariant
//! D7.2 actually cares about: no tokio, no I/O on the main thread, and the
//! workers hold no GPUI handle — they only push [`WorkerMsg`] onto an mpsc
//! channel. Both share the one [`BrainClient`]; its clones share the token and
//! cookie jar, so a `login()` on the rpc thread is seen by the live thread's
//! next reconnect.
//!
//! The main thread drains that channel on a GPUI foreground timer ([`spawn_pump`])
//! and applies each message through the reducer inside `Entity::update`, so every
//! state change lands as a GPUI notify on the UI thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use swarmbot_client::{BrainClient, ClientError};
use swarmbot_contracts::LiveCursor;

use crate::brain::{Brain, ClientBrain};
use crate::proto::{Command, CommandResult, ConnEvent, WorkerMsg};
use crate::reducer::apply_msg;
use crate::state::AppState;

/// How often the main thread drains the worker channel.
const POLL_INTERVAL: Duration = Duration::from_millis(40);
/// How often the animation ticker fires (spinners, countdowns).
const TICK_INTERVAL: Duration = Duration::from_millis(200);

/// Exponential reconnect backoff, deterministic (no jitter) so a test can assert
/// the exact sequence. `next()` doubles from `base` up to `cap`.
#[derive(Debug, Clone)]
pub struct Backoff {
    attempt: u32,
    base: Duration,
    cap: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(Duration::from_millis(250), Duration::from_secs(8))
    }
}

impl Backoff {
    pub fn new(base: Duration, cap: Duration) -> Self {
        Self {
            attempt: 0,
            base,
            cap,
        }
    }

    /// The delay before the next attempt, advancing the counter. Named
    /// `next_delay`, not `next`, so it cannot be confused with
    /// [`Iterator::next`] (this is not an iterator).
    pub fn next_delay(&mut self) -> Duration {
        // Double `base` once per prior attempt, saturating at `cap`. The shift
        // is bounded so a long outage cannot overflow the multiply.
        let exp = self.attempt.min(16);
        let mut delay = self.base;
        for _ in 0..exp {
            delay = delay.saturating_mul(2);
            if delay >= self.cap {
                delay = self.cap;
                break;
            }
        }
        self.attempt = self.attempt.saturating_add(1);
        delay.min(self.cap)
    }

    /// Reset after a successful connection.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// The number of `next()` calls so far.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

/// The worker side: owns the two thread handles and the command sender.
pub struct Worker {
    /// Clone this to send commands from any view.
    pub commands: Sender<Command>,
    shutdown: Arc<AtomicBool>,
    live: Option<JoinHandle<()>>,
    rpc: Option<JoinHandle<()>>,
}

impl Worker {
    /// Start the live and rpc threads against `brain`. `events` is drained by
    /// [`spawn_pump`] on the main thread.
    pub fn start(brain: Arc<ClientBrain>, events: Sender<WorkerMsg>) -> Self {
        let (commands_tx, commands_rx) = channel::<Command>();
        let shutdown = Arc::new(AtomicBool::new(false));

        let rpc_brain: Arc<dyn Brain> = brain.clone();
        let rpc_events = events.clone();
        let rpc_shutdown = Arc::clone(&shutdown);
        let rpc = std::thread::Builder::new()
            .name("swarmbot-ui-rpc".into())
            .spawn(move || rpc_loop(rpc_brain, commands_rx, rpc_events, rpc_shutdown))
            .ok();

        let client = brain.client().clone();
        let live_events = events;
        let live_shutdown = Arc::clone(&shutdown);
        let live = std::thread::Builder::new()
            .name("swarmbot-ui-live".into())
            .spawn(move || live_loop(client, live_events, live_shutdown))
            .ok();

        Self {
            commands: commands_tx,
            shutdown,
            live,
            rpc,
        }
    }

    /// Ask both threads to stop and wait briefly for them. Called on quit; the
    /// spawned brain child (if any) is handled separately by the supervisor.
    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.commands.send(Command::Stop);
        for handle in [self.live.take(), self.rpc.take()].into_iter().flatten() {
            let _ = handle.join();
        }
    }
}

impl Drop for Worker {
    /// Best-effort signal on teardown (the window/app is going away): set the
    /// flag and send `Stop` so the threads unblock from their timeouts and exit.
    /// No join here — teardown must not block, and process exit reaps them.
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.commands.send(Command::Stop);
    }
}

/// Run one command against the brain, mapping it to its typed result. `Stop`
/// yields `None` (it is a control signal, not a call). Pure apart from the
/// blocking brain call, so a test can drive it with a scripted double.
pub fn run_command(brain: &dyn Brain, command: Command) -> Option<CommandResult> {
    let result = match command {
        Command::Login(token) => CommandResult::Login(brain.login(token.expose())),
        Command::Send {
            conversation,
            text,
            addressed,
            reply_to,
        } => CommandResult::Send(brain.send_message(
            &conversation,
            &text,
            &addressed,
            reply_to.as_ref(),
        )),
        Command::Interrupt(id) => CommandResult::Interrupt(brain.interrupt(&id)),
        Command::Respond {
            id,
            decision,
            answer,
        } => CommandResult::Respond(brain.respond_request(&id, decision, answer)),
        Command::CancelTask(id) => CommandResult::CancelTask(brain.cancel_task(&id)),
        Command::TaskDetail(id) => CommandResult::TaskDetail(brain.task(&id)),
        Command::CreateGroup {
            title,
            members,
            lead,
        } => CommandResult::CreateGroup(brain.create_group(&title, &members, lead.as_ref())),
        Command::UpdateGroup {
            id,
            title,
            members,
            lead,
        } => CommandResult::GroupUpdated(brain.update_group(
            &id,
            title.as_deref(),
            members.as_deref(),
            lead.as_ref(),
        )),
        Command::CreateBot(req) => CommandResult::CreateBot(brain.create_bot(&req)),
        Command::DeleteBot(id) => CommandResult::DeleteBot {
            result: brain.delete_bot(id.as_str()),
            bot: id,
        },
        Command::DeleteGroup(id) => CommandResult::DeleteGroup {
            result: brain.delete_conversation(&id),
            conversation: id,
        },
        Command::BotSession { id, action } => {
            CommandResult::BotSession(brain.bot_session(&id, action))
        }
        Command::Pairing => CommandResult::Pairing(brain.create_pairing_code()),
        Command::RevokeNode(id) => CommandResult::RevokeNode(brain.revoke_node(&id)),
        Command::FetchSettings => CommandResult::Settings(brain.settings()),
        Command::UpdateSettings(patch) => {
            CommandResult::UpdateSettings(brain.update_settings(&patch))
        }
        Command::FetchBackends => CommandResult::Backends(brain.backends()),
        Command::ProbeBackends => CommandResult::Probes(brain.probe_backends()),
        Command::SetBotBackend { bot, backend } => CommandResult::BotUpdated(
            brain
                .update_bot(bot.as_str(), &serde_json::json!({"backend": backend}))
                .map(|_| ()),
        ),
        Command::FetchModels(backend) => {
            CommandResult::Models(backend.clone(), brain.models(&backend))
        }
        Command::SetBotModel { bot, model } => CommandResult::BotUpdated(
            brain
                // An empty string is the brain's "clear the override" spelling.
                .update_bot(
                    bot.as_str(),
                    &serde_json::json!({"model": model.unwrap_or_default()}),
                )
                .map(|_| ()),
        ),
        Command::FetchNodes => CommandResult::Nodes(brain.nodes()),
        Command::FetchRoutines => CommandResult::Routines(brain.routines(None)),
        Command::CreateRoutine(req) => CommandResult::RoutineCreated(brain.create_routine(&req)),
        Command::PauseRoutine(id) => CommandResult::RoutineUpdated(brain.pause_routine(&id)),
        Command::ResumeRoutine(id) => CommandResult::RoutineUpdated(brain.resume_routine(&id)),
        Command::DeleteRoutine(id) => {
            CommandResult::RoutineDeleted(id.clone(), brain.delete_routine(&id))
        }
        Command::FetchRoutineRuns(id) => {
            CommandResult::RoutineRuns(id.clone(), brain.routine_runs(&id, Some(20)))
        }
        Command::Shutdown => CommandResult::Shutdown(brain.shutdown()),
        Command::Stop => return None,
    };
    Some(result)
}

fn rpc_loop(
    brain: Arc<dyn Brain>,
    commands: Receiver<Command>,
    events: Sender<WorkerMsg>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        // A timeout lets the loop notice `shutdown` even when idle, so quitting
        // does not hang on a blocked `recv`.
        match commands.recv_timeout(Duration::from_millis(200)) {
            Ok(Command::Stop) => break,
            Ok(command) => {
                if let Some(result) = run_command(&*brain, command)
                    && events.send(WorkerMsg::Result(result)).is_err()
                {
                    break; // main thread gone
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn live_loop(client: BrainClient, events: Sender<WorkerMsg>, shutdown: Arc<AtomicBool>) {
    let mut backoff = Backoff::default();
    let mut cursor: Option<LiveCursor> = None;

    while !shutdown.load(Ordering::SeqCst) {
        let _ = events.send(WorkerMsg::Conn(ConnEvent::Connecting));
        match client.live(cursor, None) {
            Ok(mut subscription) => {
                backoff.reset();
                let _ = events.send(WorkerMsg::Conn(ConnEvent::Established));
                // Opportunistic health read to populate version/uptime; a
                // failure here is not fatal, the socket is what matters.
                let _ = events.send(WorkerMsg::Result(CommandResult::Health(client.health())));

                loop {
                    if shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    match subscription.next_event() {
                        Ok(Some(event)) => {
                            if let Some(c) = event.cursor() {
                                cursor = Some(c);
                            }
                            if events.send(WorkerMsg::Live(event)).is_err() {
                                subscription.close();
                                return;
                            }
                        }
                        Ok(None) => break, // clean close; reconnect below
                        Err(err) => {
                            let _ = events.send(WorkerMsg::Conn(classify(&err)));
                            break;
                        }
                    }
                }
                if let Some(c) = subscription.cursor() {
                    cursor = Some(c);
                }
                subscription.close();
            }
            Err(err) => {
                let _ = events.send(WorkerMsg::Conn(classify(&err)));
            }
        }

        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let delay = backoff.next_delay();
        let _ = events.send(WorkerMsg::Conn(ConnEvent::Reconnecting {
            attempt: backoff.attempt(),
        }));
        // Sleep in slices so a shutdown request is honoured promptly.
        let mut waited = Duration::ZERO;
        while waited < delay && !shutdown.load(Ordering::SeqCst) {
            let step = delay.min(Duration::from_millis(100));
            std::thread::sleep(step);
            waited += step;
        }
    }
    let _ = events.send(WorkerMsg::Conn(ConnEvent::Offline {
        reason: "shutting down".to_string(),
    }));
}

/// Turn a client error into the connection event the header should show: a 401
/// is a login problem, everything else is "offline, retrying".
fn classify(err: &ClientError) -> ConnEvent {
    match err {
        ClientError::Unauthorized => ConnEvent::NeedsLogin {
            reason: "the brain needs a login token".to_string(),
        },
        other => ConnEvent::Offline {
            reason: other.to_string(),
        },
    }
}

// --- main-thread bridges ---------------------------------------------------

use gpui::{App, AsyncApp, Entity};

/// Drain the worker channel on a foreground timer and apply each message through
/// the reducer inside `Entity::update`. Runs until the app is dropped.
pub fn spawn_pump(cx: &mut App, state: Entity<AppState>, events: Receiver<WorkerMsg>) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            cx.background_executor().timer(POLL_INTERVAL).await;
            let _ = cx.update(|app| {
                while let Ok(msg) = events.try_recv() {
                    state.update(app, |state, cx| {
                        apply_msg(state, msg);
                        cx.notify();
                    });
                }
            });
        }
    })
    .detach();
}

/// A ticker that advances the spinner phase and expires a stale toast, and
/// re-renders — but only while something time-based is actually on screen (a
/// busy bot, a pending card's countdown, a recent resync, or a toast inside its
/// window), so an idle window stays cheap on llvmpipe.
pub fn spawn_ticker(cx: &mut App, state: Entity<AppState>) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            cx.background_executor().timer(TICK_INTERVAL).await;
            let _ = cx.update(|app| {
                state.update(app, |state, cx| {
                    // The one place a toast is dropped: the reducer records when
                    // it was set, and this tick is the clock that acts on it.
                    let expired = state.expire_toast(Instant::now());
                    let animate = should_animate(state);
                    if animate {
                        state.spinner_phase = state.spinner_phase.wrapping_add(1);
                    }
                    if expired || animate {
                        cx.notify();
                    }
                });
            });
        }
    })
    .detach();
}

/// Whether a periodic redraw would change anything the user can see.
fn should_animate(state: &AppState) -> bool {
    if state.any_busy() {
        return true;
    }
    // A toast is on a clock: the tick that expires it has to run even when
    // nothing else is moving, so the message cannot outlive its cause.
    if state.toast.is_some() {
        return true;
    }
    // A pending card with an expiry needs its countdown to tick.
    if state.requests.iter().any(|r| r.is_pending()) {
        return true;
    }
    // "resynced" is shown for a few seconds after a resync.
    if let Some(at) = state.resynced_at {
        return at.elapsed() < Duration::from_secs(4);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::scripted::ScriptedBrain;
    use swarmbot_contracts::{Decision, RequestId, RequestStatus};

    #[test]
    fn backoff_doubles_then_caps_and_resets() {
        let mut backoff = Backoff::new(Duration::from_millis(100), Duration::from_secs(1));
        assert_eq!(backoff.next_delay(), Duration::from_millis(100));
        assert_eq!(backoff.next_delay(), Duration::from_millis(200));
        assert_eq!(backoff.next_delay(), Duration::from_millis(400));
        assert_eq!(backoff.next_delay(), Duration::from_millis(800));
        assert_eq!(backoff.next_delay(), Duration::from_secs(1), "capped");
        assert_eq!(backoff.next_delay(), Duration::from_secs(1), "stays capped");
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(100));
    }

    #[test]
    fn a_card_answer_is_dispatched_to_the_client() {
        let brain = ScriptedBrain::new();
        let id = RequestId::from_suffix("r1");
        let result = run_command(
            &brain,
            Command::Respond {
                id: id.clone(),
                decision: Some(Decision::AllowOnce),
                answer: None,
            },
        )
        .expect("a respond command yields a result");
        match result {
            CommandResult::Respond(Ok(request)) => {
                assert_eq!(request.id, id);
                assert_eq!(request.status, RequestStatus::Answered);
                assert_eq!(request.decision, Some(Decision::AllowOnce));
            }
            other => panic!("expected Respond(Ok), got {other:?}"),
        }
        assert!(
            brain.calls().iter().any(|c| c.name == "respond_request"),
            "the client was actually called"
        );
    }

    #[test]
    fn a_client_error_is_returned_as_a_value_not_a_panic() {
        let brain = ScriptedBrain::new();
        *brain.fail_with.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(ClientError::Unauthorized);
        let result = run_command(&brain, Command::FetchSettings).expect("a result");
        assert!(
            matches!(
                result,
                CommandResult::Settings(Err(ClientError::Unauthorized))
            ),
            "the 401 is preserved for the reducer to raise the login prompt"
        );
    }

    #[test]
    fn stop_is_a_control_signal_with_no_result() {
        let brain = ScriptedBrain::new();
        assert!(run_command(&brain, Command::Stop).is_none());
    }

    /// The ticker is the only clock a toast has. If a toast stopped counting as
    /// "something on screen", the tick would stop and the message would sit
    /// there forever — the bug this wiring exists to prevent.
    #[test]
    fn a_visible_toast_keeps_the_ticker_running_until_it_expires() {
        use crate::state::TOAST_TTL;

        let mut state = AppState::new("http://brain.test");
        assert!(
            !should_animate(&state),
            "a quiet, idle window is not redrawn"
        );

        let set_at = Instant::now();
        state.set_toast("settings saved", set_at);
        assert!(
            should_animate(&state),
            "a toast is time-bounded, so the tick that expires it must keep running"
        );

        // Once the window is over the tick clears it and the ticker can rest.
        assert!(state.expire_toast(set_at + TOAST_TTL));
        assert!(!should_animate(&state));
    }
}
