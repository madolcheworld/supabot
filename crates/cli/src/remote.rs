//! The remote half of the seam: `--brain <url>`.
//!
//! [`Backend`] says where the orchestration lives; [`Remote`] is the half that
//! lives in another process. It owns a [`BrainClient`] (blocking REST) and the
//! live-event pump that turns the brain's `/ws/live` socket into the same
//! `UnboundedReceiver<LiveEvent>` the embedded hub hands out — which is why every
//! command body, and every line of `render.rs`, is shared by the two modes
//! instead of duplicated.
//!
//! Three rules shape this module, all of them honesty rules:
//!
//! * **a token is never printed, logged, formatted or embedded in an error.**
//!   [`Credentials`] keeps the value in a private field and hand-writes `Debug`;
//!   every message names the *source* (`--token`, `$SWARMBOT_TOKEN`, a file
//!   path), never the value.
//! * **a failure names the brain.** A refused connection says which URL it tried
//!   and that the brain may not be running; a 401 says the token is missing or
//!   wrong *and* lists where this CLI looked; a 501 repeats the brain's own
//!   sentence rather than paraphrasing it. See [`Remote::honest`].
//! * **nothing is emulated locally.** `demo` refuses `--brain` at the door
//!   ([`embedded_only`]), and `--team`/`--backend`/`--root` are refused rather
//!   than silently ignored ([`local_only_flag`]), because a flag that appears to
//!   apply and does not is a lie the user cannot see.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::Value;
use swarmbot_client::{BackendView, BrainClient, ClientError, Health, PairingCode, SessionAction};
use swarmbot_contracts::bot::{Bot, BotStatus};
use swarmbot_contracts::config::{BackendKind, Settings};
use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::entry::Entry;
use swarmbot_contracts::id::{BotId, ConversationId, EntryId, RequestId};
use swarmbot_contracts::live::{LiveCursor, LiveEvent};
use swarmbot_contracts::node::NodeInfo;
use swarmbot_contracts::request::{Decision, RuntimeRequest};
use swarmbot_contracts::task::{Task, TaskStatus};
use swarmbot_contracts::turn::UsageReport;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::embedded::{Embedded, short_conversation};
use crate::session::{SendReport, Session, request_relevant_to};

/// The environment variable that carries a token when `--token` is absent.
pub const TOKEN_ENV: &str = "SWARMBOT_TOKEN";

/// The file a brain writes its owner token to at first boot (0600).
pub const OWNER_TOKEN_FILE: &str = "owner-token";

/// Where the orchestration lives: in this process, or in a daemon this CLI talks
/// to over HTTP.
///
/// Commands that behave the same in both modes take a [`Session`] and are handed
/// one arm or the other; the arm decides nothing else. There is deliberately no
/// fallback between them — a `--brain` URL that cannot be reached is an error,
/// never a quiet in-process brain.
pub enum Backend {
    Embedded(Embedded),
    Remote(Remote),
}

/// Validate a `--brain` URL before anything dials it.
///
/// Only `http(s)` is accepted: the client derives the `ws://` origin for the live
/// socket itself, so a WebSocket URL here is almost always somebody pasting the
/// wrong half of a login line, and saying so beats a confusing handshake error.
pub fn parse_brain_url(url: &str) -> Result<()> {
    let parsed =
        url::Url::parse(url).map_err(|err| anyhow!("`{url}` is not a valid brain URL: {err}"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(()),
        "ws" | "wss" => {
            let http = if parsed.scheme() == "wss" {
                "https"
            } else {
                "http"
            };
            Err(anyhow!(
                "`{url}` is a WebSocket URL: --brain takes the brain's http(s) origin and derives \
                 the ws:// itself — did you mean `{http}{}`?",
                &url[parsed.scheme().len()..]
            ))
        }
        other => Err(anyhow!(
            "`{url}` is not an http(s) URL (its scheme is `{other}`) — a brain URL looks like \
             http://127.0.0.1:8787"
        )),
    }
}

/// Where a token came from, as a description. Never the token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenSource {
    /// `--token <TOKEN>`
    Flag,
    /// `$SWARMBOT_TOKEN`
    Env,
    /// `<data-dir>/owner-token`
    File(PathBuf),
    /// Typed at the terminal prompt, which does not echo it.
    Prompt,
}

impl TokenSource {
    pub fn describe(&self) -> String {
        match self {
            Self::Flag => "--token".to_string(),
            Self::Env => format!("${TOKEN_ENV}"),
            Self::File(path) => path.display().to_string(),
            Self::Prompt => "the terminal prompt".to_string(),
        }
    }
}

/// A resolved token plus the trail that led to it.
///
/// `Debug` is hand-written because the derived one would print the token into
/// any `expect_err`, log line or test-failure message — the exact leak the
/// redaction tests below exist to catch.
#[derive(Clone)]
pub struct Credentials {
    token: String,
    source: TokenSource,
    /// Every place looked at, in precedence order, as descriptions.
    looked: Vec<String>,
}

impl Credentials {
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn source(&self) -> &TokenSource {
        &self.source
    }

    /// The precedence list, for the message a 401 prints.
    pub fn looked(&self) -> &[String] {
        &self.looked
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("source", &self.source)
            .field("token", &"<redacted>")
            .field("looked", &self.looked)
            .finish()
    }
}

/// The token file this CLI would read for a data dir.
pub fn owner_token_file(data_dir: &Path) -> PathBuf {
    data_dir.join(OWNER_TOKEN_FILE)
}

/// Every place a token is looked for, in precedence order, as descriptions.
pub fn token_places(data_dir: &Path) -> Vec<String> {
    vec![
        "--token <TOKEN>".to_string(),
        format!("${TOKEN_ENV}"),
        owner_token_file(data_dir).display().to_string(),
        "a prompt on the terminal (only when stdin is one)".to_string(),
    ]
}

/// Resolve the brain's token: `--token`, then `$SWARMBOT_TOKEN`, then
/// `<data-dir>/owner-token` when that file exists, then a prompt.
///
/// The prompt is the last resort and only ever happens on a terminal: a CLI
/// running under a script, a pipe or CI must fail with one line saying where it
/// looked, not hang waiting for input nobody will type. `env` and `stdin_is_tty`
/// are parameters rather than reads of the process state, so the precedence is
/// testable without mutating the environment of a whole test binary.
pub fn resolve_token(
    flag: Option<&str>,
    env: Option<&str>,
    data_dir: &Path,
    stdin_is_tty: bool,
) -> Result<Credentials> {
    let places = token_places(data_dir);
    let found = |token: &str, source: TokenSource| -> Result<Credentials> {
        let token = token.trim();
        if token.is_empty() {
            return Err(anyhow!(
                "the brain token from {} is empty",
                source.describe()
            ));
        }
        Ok(Credentials {
            token: token.to_string(),
            source,
            looked: places.clone(),
        })
    };
    if let Some(token) = flag {
        return found(token, TokenSource::Flag);
    }
    if let Some(token) = env {
        return found(token, TokenSource::Env);
    }
    let file = owner_token_file(data_dir);
    match std::fs::read_to_string(&file) {
        Ok(token) => return found(&token, TokenSource::File(file)),
        // A missing file is the normal case; anything else (permissions, a
        // directory in the way) is worth saying, because "it looked and could
        // not read" is not the same as "there is no token here".
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(anyhow!(
                "cannot read the brain token {}: {err}",
                file.display()
            ));
        }
    }
    if stdin_is_tty {
        let token = prompt_token()?;
        return found(&token, TokenSource::Prompt);
    }
    Err(anyhow!(
        "no brain token: looked for {}, and stdin is not a terminal so it cannot ask — pass \
         --token <TOKEN>, set {TOKEN_ENV}, or point --data-dir at the brain's data directory \
         (which holds {OWNER_TOKEN_FILE})",
        places[..3].join(", then ")
    ))
}

/// Ask for the token on the terminal, without echoing it.
///
/// Raw mode is restored whatever happens: a shell left in raw mode is a broken
/// shell, and "the token prompt failed" must not cost the user their terminal.
/// Nothing here prints the characters typed — not even as dots — because a
/// repaint of the line is one more place a token could land.
fn prompt_token() -> Result<String> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    eprint!("brain token (input is not echoed): ");
    std::io::stderr().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let mut token = String::new();
    let mut read = Ok(());
    loop {
        match crossterm::event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter => break,
                KeyCode::Backspace => {
                    token.pop();
                }
                KeyCode::Esc => {
                    read = Err(anyhow!("the token prompt was cancelled"));
                    break;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    read = Err(anyhow!("the token prompt was interrupted"));
                    break;
                }
                KeyCode::Char(character) => token.push(character),
                _ => {}
            },
            Ok(_) => {}
            Err(err) => {
                read = Err(anyhow!("cannot read the token from the terminal: {err}"));
                break;
            }
        }
    }
    let restored = crossterm::terminal::disable_raw_mode();
    // The newline the terminal never got in raw mode, so the next output does
    // not start halfway across the prompt line.
    eprintln!();
    read?;
    restored?;
    Ok(token)
}

/// How hard a dropped live socket is retried.
///
/// Bounded on purpose: a brain that is gone stays gone, and a CLI that retries
/// forever looks hung instead of broken.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Consecutive transport failures tolerated before the pump gives up.
    pub max_reconnects: u32,
    /// The first delay; it doubles per attempt.
    pub base: Duration,
    /// No delay ever exceeds this.
    pub cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_reconnects: 6,
            base: Duration::from_millis(100),
            cap: Duration::from_secs(2),
        }
    }
}

impl RetryPolicy {
    /// `base * 2^(attempt-1)`, capped: 100 ms, 200 ms, 400 ms … 2 s.
    pub fn delay(&self, attempt: u32) -> Duration {
        let shifts = attempt.saturating_sub(1).min(16);
        self.base.saturating_mul(1u32 << shifts).min(self.cap)
    }
}

/// One live pump's retry budget.
///
/// Split out from the pump so the rule is testable without a socket. The rule
/// that matters: a connection that *delivered events* was healthy and earns a
/// fresh budget, while one that dies without delivering anything does not. A
/// brain that accepts a handshake and drops it straight away would otherwise
/// become an endless reconnect loop at the base delay — a hung-looking CLI
/// instead of an error somebody can act on.
#[derive(Clone, Copy, Debug)]
struct Reconnects {
    policy: RetryPolicy,
    attempt: u32,
}

impl Reconnects {
    fn new(policy: RetryPolicy) -> Self {
        Self { policy, attempt: 0 }
    }

    /// This connection delivered at least one event: start the budget over.
    fn healthy(&mut self) {
        self.attempt = 0;
    }

    /// How long to wait before the next attempt, or `None` when the budget is
    /// spent.
    fn next_delay(&mut self) -> Option<Duration> {
        self.attempt += 1;
        if self.attempt > self.policy.max_reconnects {
            return None;
        }
        Some(self.policy.delay(self.attempt))
    }
}

/// A brain in another process, reached over HTTP.
///
/// Cloning is cheap and shares one session: the client's connection pool, its
/// cookie jar and its token, plus the slots a failing live socket reports into.
pub struct Remote {
    /// Boxed so `Backend`'s two variants are not 300 bytes apart: an embedded
    /// half is one `Arc`, and an enum sized for its largest variant is copied on
    /// every move of a value that is always handled by reference anyway.
    client: Box<BrainClient>,
    /// The normalized origin, e.g. `http://127.0.0.1:8791`.
    url: String,
    credentials: Credentials,
    /// Why a live pump stopped, when the reason is one the user must hear. A
    /// closed channel cannot carry an error, so the pump leaves it here.
    stream_error: Arc<Mutex<Option<String>>>,
    /// The newest cursor any pump of this handle saw, so a reconnect resumes
    /// instead of re-snapshotting from zero.
    cursor: Arc<Mutex<Option<LiveCursor>>>,
    retry: RetryPolicy,
}

impl Clone for Remote {
    fn clone(&self) -> Self {
        Self {
            client: Box::new((*self.client).clone()),
            url: self.url.clone(),
            credentials: self.credentials.clone(),
            stream_error: Arc::clone(&self.stream_error),
            cursor: Arc::clone(&self.cursor),
            retry: self.retry,
        }
    }
}

/// Hand-written: the token lives in `credentials`, and a derived `Debug` would
/// print it into the first `expect_err` that ever formats a `Remote`.
impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .field("token_source", &self.credentials.source)
            .finish_non_exhaustive()
    }
}

impl Remote {
    /// Dial a brain: validate the URL, resolve the token, then make one cheap
    /// authenticated call.
    ///
    /// The probe is not decoration. Without it a wrong token surfaces as "the
    /// event stream closed before it synchronized" twenty lines into a run, and a
    /// brain that is not running surfaces as a timeout. One `GET /api/health`
    /// turns both into the sentence they deserve, before anything streams.
    ///
    /// `login()` is deliberately not called: the brain accepts the bearer token
    /// on its own, and exchanging it for a session cookie would mint state this
    /// CLI does not need.
    ///
    /// Blocking: callers in an async context wrap it in `spawn_blocking`.
    pub fn connect(
        url: &str,
        flag_token: Option<&str>,
        data_dir: &Path,
        stdin_is_tty: bool,
    ) -> Result<Self> {
        parse_brain_url(url)?;
        let env_token = std::env::var(TOKEN_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty());
        let credentials = resolve_token(flag_token, env_token.as_deref(), data_dir, stdin_is_tty)?;
        let remote = Self::with_credentials(url, credentials)?;
        remote.health()?;
        Ok(remote)
    }

    /// Build a client for a resolved token. No probe: the caller decides when
    /// the first request happens.
    fn with_credentials(url: &str, credentials: Credentials) -> Result<Self> {
        let client = BrainClient::new(url)
            .map_err(|err| anyhow!("`{url}` cannot be used as a brain URL: {err}"))?
            .with_token(credentials.token());
        let url = client.base_url().to_string();
        Ok(Self {
            url,
            client: Box::new(client),
            credentials,
            stream_error: Arc::new(Mutex::new(None)),
            cursor: Arc::new(Mutex::new(None)),
            retry: RetryPolicy::default(),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Where the token in use came from — public information, and the thing a
    /// 401 message has to name.
    pub fn token_source(&self) -> &TokenSource {
        self.credentials.source()
    }

    pub fn health(&self) -> Result<Health> {
        self.client.health().map_err(|err| self.honest(&err))
    }

    pub fn settings(&self) -> Result<Settings> {
        self.client.settings().map_err(|err| self.honest(&err))
    }

    /// The brain's backends, already redacted by the brain: a key value never
    /// crosses the wire, only the name of the environment variable it lives in.
    pub fn backends(&self) -> Result<Vec<BackendView>> {
        self.client.backends().map_err(|err| self.honest(&err))
    }

    pub fn pairing_code(&self) -> Result<PairingCode> {
        self.client
            .create_pairing_code()
            .map_err(|err| self.honest(&err))
    }

    /// Ask the brain to stop, then watch it go.
    ///
    /// Watching is the honest half: `POST /api/shutdown` answers *before* the
    /// daemon exits (it has to, or the answer would never arrive), so "ok" alone
    /// would be a claim about a process nobody checked. The returned sentence
    /// says which of the two happened.
    ///
    /// Blocking: callers in an async context wrap it in `spawn_blocking`.
    pub fn shutdown_and_watch(&self, budget: Duration) -> Result<String> {
        self.client.shutdown().map_err(|err| self.honest(&err))?;
        let deadline = std::time::Instant::now() + budget;
        let poll = Duration::from_millis(250);
        loop {
            match self.client.health() {
                // Anything that is not an answer means the listener is gone.
                Err(ClientError::Transport { .. }) => {
                    return Ok(format!(
                        "the brain at {} stopped answering — it is down",
                        self.url
                    ));
                }
                Err(err) => return Err(self.honest(&err)),
                Ok(health) => {
                    if std::time::Instant::now() >= deadline {
                        return Ok(format!(
                            "the brain at {} acknowledged the shutdown but was still answering \
                             after {}s (its uptime was {}s) — it finishes the turns in flight first",
                            self.url,
                            budget.as_secs(),
                            health.uptime_secs
                        ));
                    }
                    std::thread::sleep(poll);
                }
            }
        }
    }

    /// One sentence per failure kind, always naming the brain.
    ///
    /// This is the function the honesty rules are about: the URL, what was
    /// tried, what to do next — and never a token value.
    pub fn honest(&self, err: &ClientError) -> anyhow::Error {
        let base = &self.url;
        match err {
            ClientError::Transport { url, reason } => anyhow!(
                "cannot reach the brain at {base} ({url}): {reason} — the brain may not be \
                 running there; start it with `swarmbot-brain --data-dir <dir>` and check the port"
            ),
            ClientError::Unauthorized => anyhow!(
                "the brain at {base} refused the token (401 unauthorized): it is missing or wrong \
                 — this CLI used {} and looks at {}",
                self.credentials.source().describe(),
                self.credentials.looked().join(", ")
            ),
            // The brain's own sentence, verbatim: it names the milestone that
            // will make this work, which no paraphrase would keep.
            ClientError::Http {
                status: 501,
                message,
                ..
            } => anyhow!("the brain at {base} does not implement this yet (501): {message}"),
            ClientError::Http {
                status,
                code,
                message,
            } => anyhow!("the brain at {base} refused the request: {status} {code}: {message}"),
            ClientError::Decode { reason } => anyhow!(
                "the brain at {base} sent something this CLI cannot read: {reason} — is that brain \
                 newer than this swarmbot?"
            ),
        }
    }

    /// The newest cursor a live pump of this handle has seen: what a reconnect
    /// resumes from. Read by the tests that pin that behaviour.
    #[cfg(test)]
    pub(crate) fn cursor(&self) -> Option<LiveCursor> {
        *self.cursor.lock().unwrap_or_else(|err| err.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn client(&self) -> &BrainClient {
        &self.client
    }

    /// A handle that skips the reachability probe, so a test can point one at a
    /// socket that refuses everything — which is exactly what the retry path
    /// needs and what [`Remote::connect`] would (correctly) refuse to build.
    #[cfg(test)]
    pub(crate) fn without_probe(url: &str, token: &str) -> Result<Self> {
        let credentials = resolve_token(Some(token), None, Path::new("."), false)?;
        Self::with_credentials(url, credentials)
    }

    #[cfg(test)]
    fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Leave the reason a pump stopped where [`Session::stream_error`] can find
    /// it. The first reason wins: it is the one that caused the rest.
    fn record(&self, err: &ClientError) {
        let message = self.honest(err).to_string();
        let mut slot = self
            .stream_error
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if slot.is_none() {
            *slot = Some(message);
        }
    }

    /// Read one subscription until it ends, reconnecting from its own cursor on
    /// a transport failure.
    ///
    /// Runs on a dedicated thread rather than a tokio task: `next_event` blocks
    /// in a socket read, and a blocked runtime worker is a stalled renderer.
    fn pump(&self, tx: UnboundedSender<LiveEvent>, conversation: Option<ConversationId>) {
        let mut cursor: Option<LiveCursor> = None;
        let mut retries = Reconnects::new(self.retry);
        'reconnect: loop {
            let mut live = match self.client.live(cursor, conversation.as_ref()) {
                Ok(live) => live,
                Err(err) => {
                    if !self.retry_or_stop(&err, &mut retries) {
                        return;
                    }
                    continue 'reconnect;
                }
            };
            // Whether this connection ever delivered an event decides if the
            // retry budget starts over when it drops; see [`Reconnects`].
            let mut healthy = false;
            loop {
                match live.next_event() {
                    Ok(Some(event)) => {
                        healthy = true;
                        cursor = live.cursor().or(cursor);
                        self.remember(cursor);
                        // The reader is gone (the command finished): stop rather
                        // than buffer events nobody will ever see.
                        if tx.send(event).is_err() {
                            live.close();
                            return;
                        }
                    }
                    // A clean close is the brain's own "going away" frame: there
                    // is nothing to resume, and redialling a port nobody serves
                    // any more would only spin. The command reports the ended
                    // stream itself.
                    Ok(None) => {
                        live.close();
                        return;
                    }
                    Err(err) => {
                        // The cursor of a socket that just died is the point of
                        // the whole exercise: resuming with it replays the gap
                        // instead of silently losing it.
                        cursor = live.cursor().or(cursor);
                        self.remember(cursor);
                        live.close();
                        let resume = cursor
                            .map(|cursor| cursor.to_string())
                            .unwrap_or_else(|| "<no cursor yet>".to_string());
                        tracing::warn!(
                            "the live socket dropped ({err}); reconnecting from cursor {resume}"
                        );
                        if healthy {
                            retries.healthy();
                        }
                        if !self.retry_or_stop(&err, &mut retries) {
                            return;
                        }
                        continue 'reconnect;
                    }
                }
            }
        }
    }

    fn remember(&self, cursor: Option<LiveCursor>) {
        if let Some(cursor) = cursor {
            *self.cursor.lock().unwrap_or_else(|err| err.into_inner()) = Some(cursor);
        }
    }

    /// Decide whether a failed socket is worth another attempt, and sleep the
    /// backoff when it is. Returns `false` when the pump must stop, having first
    /// recorded the reason so the command can report it.
    ///
    /// Only a transport failure is retried: a 401 does not start working on the
    /// fourth try, and a frame this build cannot decode means the brain and the
    /// CLI disagree about the protocol.
    fn retry_or_stop(&self, err: &ClientError, retries: &mut Reconnects) -> bool {
        if !matches!(err, ClientError::Transport { .. }) {
            self.record(err);
            return false;
        }
        match retries.next_delay() {
            Some(delay) => {
                std::thread::sleep(delay);
                true
            }
            None => {
                self.record(err);
                false
            }
        }
    }

    /// The `delegated:` lines and the queued-bot note need names; the wire
    /// carries ids.
    fn name_of(&self, bots: &[Bot], id: &BotId) -> String {
        bots.iter()
            .find(|bot| &bot.id == id)
            .map(|bot| format!("@{}", bot.name))
            .unwrap_or_else(|| "@unknown".to_string())
    }
}

impl Session for Remote {
    fn describe(&self) -> String {
        format!("the brain at {}", self.url)
    }

    fn poll_interval(&self) -> Duration {
        // Ten times slower than the embedded poll: every probe is two HTTP round
        // trips, and a run's events are rendering beside it.
        Duration::from_millis(250)
    }

    fn idle_confirmations(&self) -> usize {
        // The brain publishes `Working` from the runner that picks a delivery
        // up, which is after `POST /messages` answers. One idle reading in that
        // window would end a run before the reply it came for; two readings,
        // 250 ms apart, do not.
        2
    }

    fn bots(&self) -> Result<Vec<Bot>> {
        self.client.bots().map_err(|err| self.honest(&err))
    }

    fn conversations(&self) -> Result<Vec<Conversation>> {
        self.client.conversations().map_err(|err| self.honest(&err))
    }

    fn tasks(&self) -> Result<Vec<Task>> {
        self.client.tasks(None).map_err(|err| self.honest(&err))
    }

    fn nodes(&self) -> Result<Vec<NodeInfo>> {
        self.client.nodes().map_err(|err| self.honest(&err))
    }

    fn usage(&self, since: Option<&str>) -> Result<UsageReport> {
        self.client.usage(since).map_err(|err| self.honest(&err))
    }

    fn tail(&self, conversation: &ConversationId, limit: u32) -> Result<Vec<Entry>> {
        self.client
            .tail(conversation, limit)
            .map_err(|err| self.honest(&err))
    }

    fn send(&self, conversation: &ConversationId, text: &str) -> Result<SendReport> {
        // No `addressed` list: the brain parses the mentions itself, which is
        // what the embedded half does too, so routing cannot differ by mode.
        self.send_with(conversation, text, &[], None)
    }

    fn send_with(
        &self,
        conversation: &ConversationId,
        text: &str,
        addressed: &[BotId],
        reply_to: Option<&EntryId>,
    ) -> Result<SendReport> {
        let sent = self
            .client
            .send_message(conversation, text, addressed, reply_to)
            .map_err(|err| self.honest(&err))?;
        let queued = if sent.queued.is_empty() {
            Vec::new()
        } else {
            // A roster lookup that fails must not turn a delivered message into
            // an error: the raw ids are the honest fallback.
            let bots = self.client.bots().unwrap_or_default();
            sent.queued
                .iter()
                .map(|id| {
                    if bots.is_empty() {
                        id.to_string()
                    } else {
                        self.name_of(&bots, id)
                    }
                })
                .collect()
        };
        Ok(SendReport { queued })
    }

    fn subscribe(
        &self,
        conversation: Option<ConversationId>,
    ) -> Result<UnboundedReceiver<LiveEvent>> {
        let (tx, rx) = unbounded_channel();
        let pump = self.clone();
        let spawned = std::thread::Builder::new()
            .name("swarmbot-live".to_string())
            .spawn(move || pump.pump(tx, conversation));
        match spawned {
            Ok(_) => Ok(rx),
            Err(err) => Err(anyhow!(
                "cannot start the live-event reader for {}: {err}",
                self.describe()
            )),
        }
    }

    fn respond(
        &self,
        request: &RequestId,
        decision: Option<Decision>,
        answer: Option<Value>,
    ) -> Result<RuntimeRequest> {
        self.client
            .respond_request(request, decision, answer)
            .map_err(|err| self.honest(&err))
    }

    fn interrupt(&self, conversation: &ConversationId) -> Result<bool> {
        self.client
            .interrupt(conversation)
            .map_err(|err| self.honest(&err))
    }

    fn pending_in(&self, conversation: &ConversationId) -> Result<Vec<RuntimeRequest>> {
        let pending: Vec<RuntimeRequest> = self
            .client
            .pending_requests()
            .map_err(|err| self.honest(&err))?
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
        Ok(pending
            .into_iter()
            .filter(|request| {
                request_relevant_to(request, conversation, &conversations, &bots, &tasks, &[])
            })
            .collect())
    }

    fn new_session(&self, bot: &Bot) -> Result<()> {
        self.client
            .bot_session(&bot.name, SessionAction::New)
            .map_err(|err| self.honest(&err))?;
        Ok(())
    }

    fn busy(&self) -> Result<Vec<String>> {
        let mut lines = Vec::new();
        for bot in self.bots()? {
            match &bot.status {
                BotStatus::Working {
                    conversation_id,
                    activity,
                } => lines.push(format!(
                    "@{} is mid-turn in {} ({activity})",
                    bot.name,
                    short_conversation(conversation_id)
                )),
                BotStatus::Queued { count } => lines.push(format!(
                    "@{name} has {count} queued delivery(ies)",
                    name = bot.name
                )),
                // Idle, disabled and errored are not "moving": an errored
                // backend is a *stopped* bot, and calling it busy would make
                // every run after a backend failure time out.
                _ => {}
            }
        }
        for task in self.tasks()? {
            match task.status {
                TaskStatus::Queued => lines.push(format!("task \"{}\" is queued", task.title)),
                TaskStatus::Running => lines.push(format!("task \"{}\" is running", task.title)),
                _ => {}
            }
        }
        if lines.is_empty() {
            // Only asked once nothing else moves: a request waiting on a human
            // *is* work in progress, and saying so beats a timeout that lists
            // nothing. It costs a third round trip, which is why it comes last.
            for request in self
                .client
                .pending_requests()
                .map_err(|err| self.honest(&err))?
            {
                lines.push(format!(
                    "request \"{}\" is waiting for an answer",
                    request.title()
                ));
            }
        }
        Ok(lines)
    }

    fn delegation_lines(&self, conversation: &ConversationId) -> Result<Vec<String>> {
        let bots = self.bots()?;
        Ok(self
            .tasks()?
            .into_iter()
            .filter(|task| &task.origin == conversation)
            .map(|task| crate::demo::task_line(&task, &|id| self.name_of(&bots, id)))
            .collect())
    }

    fn stream_error(&self) -> Option<String> {
        self.stream_error
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }
}

// --- the remote-only commands ---------------------------------------------

/// `config show` against a brain: **its** settings and **its** backends, not a
/// local file. A `config.toml` on this machine says nothing about a daemon that
/// may run elsewhere, so printing one would be the quiet kind of lie.
///
/// Note what is *not* here: whether an `api_key_env` variable is set. That is a
/// fact about the brain's environment and this CLI cannot see it — printing
/// "(set)" from here would be a guess dressed as a check.
pub fn config_show_text(
    url: &str,
    token_source: &TokenSource,
    health: &Health,
    settings: &Settings,
    backends: &[BackendView],
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "brain: {url} (version {}, uptime {}s, live epoch {})\n",
        health.version, health.uptime_secs, health.epoch
    ));
    out.push_str(&format!(
        "settings: bot_messaging={} max_hops={} max_tool_rounds={} tasks_per_bot={} \
         approval_expiry={}s auto_approve_read_only={}\n",
        settings.bot_messaging,
        settings.max_hops,
        settings.max_tool_rounds,
        settings.max_running_tasks_per_bot,
        settings.approval_expiry_secs,
        settings.auto_approve_read_only,
    ));
    out.push_str(&crate::config::budget_line(settings));
    out.push_str("backends:\n");
    if backends.is_empty() {
        out.push_str("  (none — only the built-in `fake` will resolve)\n");
    }
    for backend in backends {
        let mut line = format!(
            "  {:<12} kind={:<14} model={}",
            backend.id,
            kind_label(backend.kind),
            backend.model.as_deref().unwrap_or("-")
        );
        if let Some(base_url) = &backend.base_url {
            line.push_str(&format!("  base_url={base_url}"));
        }
        if let Some(env) = &backend.api_key_env {
            line.push_str(&format!("  api_key_env={env} (on the brain's host)"));
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&format!(
        "token: taken from {} (its value is never printed)\n",
        token_source.describe()
    ));
    out
}

/// The wire name of a backend kind — the same spelling `config show` prints for
/// an in-process brain, so the two outputs read alike.
fn kind_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::OpenAiCompat => "open_ai_compat",
        BackendKind::ClaudeCli => "claude_cli",
        BackendKind::Acp => "acp",
        BackendKind::Fake => "fake",
    }
}

/// The lines `nodes pair` prints: the code, its expiry, and the exact command
/// the node operator runs on the other machine.
///
/// The brain stores a pairing code as a hash and shows it once, so this output is
/// the only place it exists — which is why the command is spelled out in full
/// rather than described.
pub fn pairing_lines(url: &str, pairing: &PairingCode) -> Vec<String> {
    let mut lines = vec![
        format!("pairing code: {}", pairing.code),
        format!("expires: {}", expiry_text(&pairing.expires_at)),
        "on that machine, run:".to_string(),
        format!("  swarmbot-node pair {url} {}", pairing.code),
        // Minting is not pairing: whether a code can be *redeemed* depends on
        // the brain's node gateway, which this CLI cannot see from here. The
        // note says how to tell, rather than asserting a milestone that may have
        // landed by the time somebody reads it.
        "note: minting a code is not pairing — if `swarmbot-node pair` answers 501, this brain's \
         node gateway is not built yet and the code cannot be redeemed"
            .to_string(),
    ];
    if pairing.node_id.is_some() {
        // Not reachable today (the brain always answers null at creation), but a
        // code that is already redeemed must never look unused.
        lines.push("note: this code has already been redeemed by a node".to_string());
    }
    lines
}

/// An RFC 3339 expiry as text, plus how long that is from now when it parses.
///
/// The raw timestamp is always printed: one this build cannot parse is still the
/// brain's truth, and inventing a relative time from a guess would not be.
fn expiry_text(expires_at: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(expires_at) {
        Ok(at) => {
            let seconds = at.signed_duration_since(chrono::Utc::now()).num_seconds();
            if seconds <= 0 {
                format!("{expires_at} (expired)")
            } else if seconds >= 3600 {
                format!(
                    "{expires_at} (in {}h {:02}m)",
                    seconds / 3600,
                    (seconds % 3600) / 60
                )
            } else if seconds >= 60 {
                format!("{expires_at} (in {}m {:02}s)", seconds / 60, seconds % 60)
            } else {
                format!("{expires_at} (in {seconds}s)")
            }
        }
        Err(_) => expires_at.to_string(),
    }
}

/// `demo` is a scripted in-process brain. Pointing it at a daemon would either
/// lie (running the script locally while the user believes it ran remotely) or
/// fail somewhere confusing, so it refuses at the door, in one line.
pub fn embedded_only(command: &str, url: &str) -> anyhow::Error {
    anyhow!(
        "`{command}` runs a scripted brain in this process and cannot run against the brain at \
         {url} — drop --brain to run it"
    )
}

/// A command that only means something with a daemon to talk to.
pub fn needs_brain(command: &str, because: &str) -> anyhow::Error {
    anyhow!("`{command}` needs a running brain: pass --brain <URL> ({because})")
}

/// `--team`, `--backend` and `--root` configure a brain this CLI builds. A
/// remote brain was configured when *it* started, so accepting them silently
/// would be a lie about what they did.
pub fn local_only_flag(flag: &str, url: &str) -> anyhow::Error {
    anyhow!(
        "`{flag}` configures an in-process brain and does nothing against the brain at {url} — \
         start that brain with the flag instead, or drop --brain to use it here"
    )
}

/// `--token` is a credential for a daemon. Without `--brain` there is no daemon
/// and nothing to present it to, so accepting it would imply an authentication
/// step that never happened.
pub fn token_without_brain() -> anyhow::Error {
    anyhow!(
        "`--token` only applies to `--brain <URL>`: without it this CLI runs the orchestration \
         in-process, where there is no brain to authenticate to (the token was not used)"
    )
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use swarmbot_client::testing::StubBrain;

    use super::*;
    use crate::render::{Printer, Roster, Theme};
    use crate::session::drain_synced_honest;
    use crate::tmp::TempDir;

    /// A token that must never appear in any output, error or `Debug`.
    const SECRET: &str = "tok_super-secret-value";

    /// Wait for a condition with a deadline, so a broken pump fails the test
    /// instead of hanging the suite.
    fn wait_until(mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(
                std::time::Instant::now() < deadline,
                "the live pump never finished"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    // --- URLs -----------------------------------------------------------------

    #[test]
    fn a_bad_url_is_refused_before_anything_dials_it() {
        for good in ["http://127.0.0.1:8787", "https://brain.example.com"] {
            assert!(parse_brain_url(good).is_ok(), "{good}");
        }
        assert!(parse_brain_url("not a url").is_err());
        assert!(parse_brain_url("ftp://example.invalid").is_err());

        let ws = parse_brain_url("ws://127.0.0.1:8787").expect_err("a ws URL is the wrong half");
        let text = ws.to_string();
        assert!(text.contains("WebSocket URL"), "{text}");
        assert!(text.contains("http://127.0.0.1:8787"), "{text}");

        let wss = parse_brain_url("wss://brain.example.com").expect_err("wss");
        assert!(
            wss.to_string().contains("https://brain.example.com"),
            "{wss}"
        );

        let other = parse_brain_url("ftp://brain").expect_err("ftp");
        assert!(other.to_string().contains("ftp"), "{other}");
    }

    // --- tokens ----------------------------------------------------------------

    #[test]
    fn the_token_precedence_is_flag_then_env_then_file() {
        let dir = TempDir::new("token").expect("temp");
        std::fs::write(dir.path().join(OWNER_TOKEN_FILE), "file-token\n").expect("write");

        let flag = resolve_token(Some("flag-token"), Some("env-token"), dir.path(), false)
            .expect("--token wins");
        assert_eq!(flag.token(), "flag-token");
        assert_eq!(flag.source(), &TokenSource::Flag);

        let env = resolve_token(None, Some("env-token"), dir.path(), false).expect("env next");
        assert_eq!(env.token(), "env-token");
        assert_eq!(env.source(), &TokenSource::Env);

        let file = resolve_token(None, None, dir.path(), false).expect("the file next");
        assert_eq!(
            file.token(),
            "file-token",
            "the trailing newline is trimmed"
        );
        assert_eq!(
            file.source(),
            &TokenSource::File(owner_token_file(dir.path()))
        );

        // An empty flag does not fall through to the file: an explicitly empty
        // `--token` is a mistake worth reporting, not a request to guess.
        let empty = resolve_token(Some("   "), None, dir.path(), false).expect_err("empty");
        assert!(empty.to_string().contains("empty"), "{empty}");

        // A data dir with no token file falls through to the prompt — which a
        // non-terminal refuses, one line saying where it looked.
        let bare = TempDir::new("token-bare").expect("temp");
        assert!(resolve_token(None, None, bare.path(), false).is_err());
    }

    #[test]
    fn without_a_token_and_without_a_terminal_it_says_where_it_looked() {
        let dir = TempDir::new("token-none").expect("temp");
        let err = resolve_token(None, None, dir.path(), false).expect_err("no token");
        let text = err.to_string();
        // One line, naming every place it looked — including the exact file
        // path, which is the one the user can fix.
        assert!(!text.contains('\n'), "{text}");
        assert!(text.contains("--token"), "{text}");
        assert!(text.contains(TOKEN_ENV), "{text}");
        assert!(
            text.contains(&owner_token_file(dir.path()).display().to_string()),
            "{text}"
        );
        assert!(text.contains("not a terminal"), "{text}");
    }

    #[test]
    fn an_unreadable_token_file_is_reported_not_skipped() {
        let dir = TempDir::new("token-dir").expect("temp");
        // A directory where the file should be: reading it fails with something
        // other than NotFound, and the user must hear that instead of being told
        // there is no token here.
        std::fs::create_dir(owner_token_file(dir.path())).expect("mkdir");
        let err = resolve_token(None, None, dir.path(), false).expect_err("unreadable");
        assert!(
            err.to_string().contains("cannot read the brain token"),
            "{err}"
        );
    }

    #[test]
    fn the_credentials_debug_impl_redacts_the_token() {
        let dir = TempDir::new("token-debug").expect("temp");
        let credentials = resolve_token(Some(SECRET), None, dir.path(), false).expect("token");
        let debug = format!("{credentials:?}");
        assert!(!debug.contains(SECRET), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(debug.contains("--token"), "the source is public: {debug}");
    }

    #[test]
    fn a_remote_handle_redacts_its_token_in_debug_and_in_errors() {
        let stub = StubBrain::start();
        let remote = Remote::connect(&stub.url(), Some(SECRET), Path::new("."), false)
            .expect("the stub answers health");
        assert!(!format!("{remote:?}").contains(SECRET), "{remote:?}");
        assert_eq!(remote.token_source(), &TokenSource::Flag);

        // The 401 message names the source and the places looked at — never the
        // value that was refused.
        let text = format!("{:#}", remote.honest(&ClientError::Unauthorized));
        assert!(text.contains("401"), "{text}");
        assert!(text.contains("missing or wrong"), "{text}");
        assert!(text.contains("--token"), "{text}");
        assert!(text.contains(TOKEN_ENV), "{text}");
        assert!(text.contains(OWNER_TOKEN_FILE), "{text}");
        assert!(!text.contains(SECRET), "{text}");
    }

    // --- honest failures -------------------------------------------------------

    #[test]
    fn a_refused_connection_names_the_url_and_the_likely_cause() {
        // Bind a loopback port, note it, and let it go: nothing listens there
        // any more, so the connect is refused. No test traffic leaves 127.0.0.1.
        let addr = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr")
        };
        let url = format!("http://{addr}");
        let err = Remote::connect(&url, Some("tok"), Path::new("."), false).expect_err("refused");
        let text = format!("{err:#}");
        assert!(text.contains(&url), "{text}");
        assert!(text.contains("may not be running"), "{text}");
        assert!(text.contains("swarmbot-brain"), "{text}");
    }

    #[test]
    fn a_501_repeats_the_brains_own_sentence() {
        let stub = StubBrain::start();
        let remote = Remote::without_probe(&stub.url(), "tok").expect("builds");
        let err = remote.honest(&ClientError::Http {
            status: 501,
            code: "not_implemented".to_string(),
            message: "node pairing arrives in milestone M6".to_string(),
        });
        let text = format!("{err:#}");
        assert!(
            text.contains("node pairing arrives in milestone M6"),
            "{text}"
        );
        assert!(text.contains("501"), "{text}");
        assert!(text.contains(remote.url()), "{text}");
    }

    #[test]
    fn other_statuses_keep_their_code_and_a_decode_failure_says_what_it_was() {
        let stub = StubBrain::start();
        let remote = Remote::without_probe(&stub.url(), "tok").expect("builds");
        let conflict = remote.honest(&ClientError::Http {
            status: 409,
            code: "conflict".to_string(),
            message: "a bot is already called lead".to_string(),
        });
        assert!(conflict.to_string().contains("409 conflict"), "{conflict}");
        assert!(
            conflict
                .to_string()
                .contains("a bot is already called lead"),
            "{conflict}"
        );

        let decode = remote.honest(&ClientError::Decode {
            reason: "bad live frame `<html>`".to_string(),
        });
        assert!(decode.to_string().contains("cannot read"), "{decode}");
        assert!(decode.to_string().contains("bad live frame"), "{decode}");
    }

    #[test]
    fn a_404_keeps_the_brains_own_not_found_message() {
        // The stub serves every route this CLI calls, so the mapping is checked
        // where it lives: the brain's sentence survives verbatim, with its
        // status, instead of being flattened into "not found".
        let stub = StubBrain::start();
        let remote = Remote::without_probe(&stub.url(), "tok").expect("builds");
        let err = remote.honest(&ClientError::Http {
            status: 404,
            code: "not_found".to_string(),
            message: "bot not found: scout".to_string(),
        });
        let text = err.to_string();
        assert!(text.contains("404 not_found"), "{text}");
        assert!(text.contains("bot not found: scout"), "{text}");
        assert!(text.contains(remote.url()), "{text}");
    }

    #[test]
    fn the_embedded_only_and_brain_only_refusals_are_one_honest_line() {
        let text = embedded_only("demo", "http://127.0.0.1:8791").to_string();
        assert!(text.contains("demo"), "{text}");
        assert!(text.contains("http://127.0.0.1:8791"), "{text}");
        assert!(text.contains("drop --brain"), "{text}");
        assert!(!text.contains('\n'), "{text}");

        let pair = needs_brain(
            "nodes pair",
            "a code is minted by the daemon a node connects to",
        );
        assert!(pair.to_string().contains("--brain <URL>"), "{pair}");

        let text = local_only_flag("--team", "http://127.0.0.1:8791").to_string();
        assert!(text.contains("--team"), "{text}");
        assert!(text.contains("does nothing against the brain"), "{text}");
    }

    // --- the live pump ----------------------------------------------------------

    /// A loopback listener that accepts and immediately drops every connection:
    /// the socket dies mid-handshake, which the client reports as a transport
    /// failure — the case the reconnect path exists for.
    struct DroppingListener {
        url: String,
        hits: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl DroppingListener {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            listener.set_nonblocking(true).expect("nonblocking");
            let addr = listener.local_addr().expect("addr");
            let hits = Arc::new(AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let (hits, stop) = (Arc::clone(&hits), Arc::clone(&stop));
                std::thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((stream, _peer)) => {
                                hits.fetch_add(1, Ordering::SeqCst);
                                drop(stream);
                            }
                            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(2));
                            }
                            Err(_) => return,
                        }
                    }
                })
            };
            Self {
                url: format!("http://{addr}"),
                hits,
                stop,
                thread: Some(thread),
            }
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    impl Drop for DroppingListener {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[test]
    fn a_socket_that_keeps_dropping_is_retried_a_bounded_number_of_times() {
        let listener = DroppingListener::start();
        let remote = Remote::without_probe(&listener.url, "tok")
            .expect("builds")
            .with_retry(RetryPolicy {
                max_reconnects: 2,
                base: Duration::from_millis(1),
                cap: Duration::from_millis(2),
            });
        let rx = remote.subscribe(None).expect("the pump starts");

        // The pump stops after 1 + max_reconnects attempts and leaves the reason
        // where the command reports it — one honest line, not an endless retry.
        wait_until(|| remote.stream_error().is_some());
        drop(rx);
        let reason = remote.stream_error().expect("a recorded reason");
        assert!(reason.contains(&listener.url), "{reason}");
        assert!(reason.contains("may not be running"), "{reason}");
        assert_eq!(
            remote.cursor(),
            None,
            "no event ever arrived, so there is nothing to resume from"
        );
        assert!(listener.hits() >= 3, "it retried: {} hits", listener.hits());
        assert!(
            listener.hits() <= 8,
            "and it stopped retrying: {} hits",
            listener.hits()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_subscription_resumes_from_its_own_cursor() {
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), Path::new("."), false).expect("connects");
        let mut rx = remote.subscribe(None).expect("the pump starts");

        // The stub sends three canned frames and closes cleanly: the pump ends
        // and the channel closes, which is the "the brain went away" case.
        let mut events = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                Ok(Some(event)) => events.push(event),
                Ok(None) => break,
                Err(_) => panic!("the pump never delivered its canned frames"),
            }
        }
        assert_eq!(events.len(), 3, "snapshot, entry upsert, synchronized");
        assert!(matches!(events[0], LiveEvent::Snapshot(_)), "{events:?}");
        assert!(
            matches!(events[2], LiveEvent::Synchronized { .. }),
            "{events:?}"
        );
        // A clean close is not a failure: nothing is recorded, and the command
        // reports the ended stream in its own words.
        assert_eq!(remote.stream_error(), None);

        // The cursor the dropped subscription saw is what a resume dials with.
        let cursor = remote.cursor().expect("every canned frame carries one");
        assert_eq!(cursor, LiveCursor::new(3, 3));
        let mut live = remote
            .client()
            .live(Some(cursor), None)
            .expect("redials the stub");
        while let Ok(Some(_)) = live.next_event() {}
        live.close();
        let recorded = stub.last_request("GET", "/ws/live").expect("recorded");
        assert_eq!(recorded.query, "cursor=3%3A3", "the resume is on the wire");
        assert_eq!(recorded.auth.as_deref(), Some("Bearer tok"));
    }

    /// The pump's events reach `render.rs` through exactly the path the embedded
    /// hub uses, so a remote stream reads like a local one.
    #[tokio::test(flavor = "multi_thread")]
    async fn pumped_events_render_through_the_shared_printer_path() {
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), Path::new("."), false).expect("connects");
        let mut rx = remote.subscribe(None).expect("the pump starts");
        let mut buffer = Vec::new();
        let mut roster = Roster::new(Vec::new(), Vec::new(), None);
        {
            let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
            // `with_snapshot = true` is what `chat` does: the snapshot's tail is
            // the history a chat opens with.
            drain_synced_honest(&remote, &mut rx, &mut printer, &mut roster, false, true)
                .await
                .expect("synchronizes");
            printer.finish().expect("finishes");
        }
        let out = String::from_utf8(buffer).expect("utf8");
        assert!(out.contains("you › canned entry"), "{out}");
        assert!(out.contains("@stub › stub reply"), "{out}");
        // The roster came from the snapshot, which is why the handle resolved
        // instead of printing `@unknown`.
        assert_eq!(
            roster.handle(&BotId::from_suffix("000000000000000000000000000000b1")),
            "@stub"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stream_that_cannot_open_reports_the_reason_not_a_closed_channel() {
        let listener = DroppingListener::start();
        let remote = Remote::without_probe(&listener.url, "tok")
            .expect("builds")
            .with_retry(RetryPolicy {
                max_reconnects: 0,
                base: Duration::from_millis(1),
                cap: Duration::from_millis(1),
            });
        let mut rx = remote.subscribe(None).expect("the pump starts");
        let mut buffer = Vec::new();
        let mut roster = Roster::new(Vec::new(), Vec::new(), None);
        let mut printer = Printer::new(&mut buffer, Theme::plain(), false);
        let err = drain_synced_honest(&remote, &mut rx, &mut printer, &mut roster, false, false)
            .await
            .expect_err("the socket never opens");
        let text = format!("{err:#}");
        assert!(text.contains(&listener.url), "{text}");
        assert!(text.contains("may not be running"), "{text}");
        assert!(
            !text.contains("closed before it synchronized"),
            "the recorded reason replaces the useless one: {text}"
        );
    }

    // --- remote command text ------------------------------------------------------

    #[test]
    fn config_show_renders_the_brains_own_settings_and_backends() {
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some(SECRET), Path::new("."), false).expect("connects");
        let health = remote.health().expect("health");
        let settings = remote.settings().expect("settings");
        let backends = remote.backends().expect("backends");
        let text = config_show_text(
            remote.url(),
            remote.token_source(),
            &health,
            &settings,
            &backends,
        );
        assert!(text.contains(&format!("brain: {}", stub.url())), "{text}");
        assert!(text.contains("version 0.1.0"), "{text}");
        assert!(text.contains("live epoch 7"), "{text}");
        assert!(text.contains("bot_messaging=true"), "{text}");
        assert!(text.contains("max_hops=8"), "{text}");
        assert!(text.contains("kind=fake"), "{text}");
        assert!(text.contains("token: taken from --token"), "{text}");
        // The token is used, never shown — not even as a source's value.
        assert!(!text.contains(SECRET), "{text}");
    }

    #[test]
    fn pairing_prints_the_code_the_expiry_and_the_exact_node_command() {
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), Path::new("."), false).expect("connects");
        let pairing = remote.pairing_code().expect("a code");
        let lines = pairing_lines(remote.url(), &pairing);
        let joined = lines.join("\n");
        assert!(joined.contains("pairing code: ABCD-EFGH"), "{joined}");
        assert!(
            joined.contains(&format!("swarmbot-node pair {} ABCD-EFGH", remote.url())),
            "{joined}"
        );
        // The stub's expiry is in the past, and "expired" is the honest reading
        // of a timestamp that has already passed.
        assert!(joined.contains("expires: 2026-01-01T00:00:00Z"), "{joined}");
        assert!(joined.contains("(expired)"), "{joined}");
        // The output must not imply that redeeming the code works: whether it
        // does is the brain's node gateway's business, not this CLI's.
        assert!(joined.contains("minting a code is not pairing"), "{joined}");
        assert!(
            stub.last_request("POST", "/api/nodes/pairing-codes")
                .is_some(),
            "the code really was minted by the brain"
        );
    }

    #[test]
    fn an_unparsable_expiry_is_printed_verbatim_rather_than_guessed() {
        let pairing = PairingCode {
            code: "ABCD-EFGH".to_string(),
            expires_at: "soon".to_string(),
            node_id: None,
        };
        let lines = pairing_lines("http://127.0.0.1:1", &pairing);
        assert!(
            lines.iter().any(|line| line == "expires: soon"),
            "{lines:?}"
        );
    }

    #[test]
    fn a_shutdown_that_is_acknowledged_but_not_finished_says_so() {
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), Path::new("."), false).expect("connects");
        // The stub never exits, so the honest answer is "it acknowledged and was
        // still answering" — never "it is down".
        let line = remote
            .shutdown_and_watch(Duration::from_millis(300))
            .expect("shutdown");
        assert!(line.contains("acknowledged the shutdown"), "{line}");
        assert!(line.contains(&stub.url()), "{line}");
        assert!(!line.contains("it is down"), "{line}");
        assert!(stub.last_request("POST", "/api/shutdown").is_some());
    }

    // --- the session half ----------------------------------------------------------

    #[test]
    fn a_remote_session_serves_the_roster_and_settles_a_request() {
        let stub = StubBrain::start();
        let remote =
            Remote::connect(&stub.url(), Some("tok"), Path::new("."), false).expect("connects");
        assert_eq!(remote.describe(), format!("the brain at {}", stub.url()));
        assert_eq!(remote.poll_interval(), Duration::from_millis(250));

        let bots = Session::bots(&remote).expect("bots");
        assert_eq!(bots.len(), 1);
        assert_eq!(bots[0].name, "stub");
        let conversations = Session::conversations(&remote).expect("conversations");
        assert_eq!(conversations.len(), 1);
        assert_eq!(Session::nodes(&remote).expect("nodes").len(), 1);
        assert_eq!(Session::tasks(&remote).expect("tasks").len(), 1);

        let conversation = conversations[0].id.clone();
        assert_eq!(
            Session::tail(&remote, &conversation, 10)
                .expect("tail")
                .len(),
            1
        );

        // The stub's canned task belongs to this conversation, so it is the
        // `delegated:` line — and the busy probe sees it as queued work, which
        // is why a remote `run` against the stub times out instead of idling.
        let lines = Session::delegation_lines(&remote, &conversation).expect("lines");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("\"stub task\""), "{lines:?}");
        assert_eq!(
            Session::busy(&remote).expect("busy"),
            vec!["task \"stub task\" is queued".to_string()]
        );

        let pending = Session::pending_in(&remote, &conversation).expect("pending");
        assert_eq!(pending.len(), 1);
        let settled = Session::respond(&remote, &pending[0].id, Some(Decision::AllowOnce), None)
            .expect("answered");
        assert_eq!(settled.decision, Some(Decision::AllowOnce));
        let path = format!("/api/requests/{}/respond", pending[0].id);
        let recorded = stub.last_request("POST", &path).expect("recorded");
        assert_eq!(recorded.auth.as_deref(), Some("Bearer tok"));

        // Every request carries the token in a header, never in the URL.
        for request in stub.requests() {
            assert!(
                !request.path.contains("tok") && !request.query.contains("tok"),
                "a token never travels in a URL: {request:?}"
            );
        }

        assert!(Session::interrupt(&remote, &conversation).expect("interrupt"));
        let sent = Session::send(&remote, &conversation, "@stub status?").expect("send");
        assert!(sent.queued.is_empty());
        let recorded = stub
            .last_request(
                "POST",
                &format!("/api/conversations/{conversation}/messages"),
            )
            .expect("recorded");
        assert!(recorded.body.contains("@stub status?"), "{recorded:?}");
        assert!(
            recorded.body.contains("\"addressed\""),
            "the body carries an empty addressed list; the brain parses the mentions: {recorded:?}"
        );
        Session::new_session(&remote, &bots[0]).expect("a fresh session");
    }

    #[test]
    fn a_connection_that_delivered_events_earns_a_fresh_retry_budget() {
        let fast = RetryPolicy {
            max_reconnects: 2,
            base: Duration::from_millis(1),
            cap: Duration::from_millis(2),
        };
        let mut retries = Reconnects::new(fast);
        assert_eq!(retries.next_delay(), Some(Duration::from_millis(1)));
        assert_eq!(retries.next_delay(), Some(Duration::from_millis(2)));
        assert_eq!(retries.next_delay(), None, "the budget is spent");

        // A healthy connection resets it, so a chat that runs for hours survives
        // more than two drops in total …
        retries.healthy();
        assert_eq!(retries.next_delay(), Some(Duration::from_millis(1)));

        // … while a socket that never delivers anything keeps spending the same
        // budget, and the pump stops instead of looping forever.
        let mut stuck = Reconnects::new(RetryPolicy {
            max_reconnects: 1,
            base: Duration::from_millis(1),
            cap: Duration::from_millis(1),
        });
        assert_eq!(stuck.next_delay(), Some(Duration::from_millis(1)));
        assert_eq!(stuck.next_delay(), None);
        stuck.healthy();
        assert_eq!(
            stuck.next_delay(),
            Some(Duration::from_millis(1)),
            "and a later healthy connection still earns a fresh budget"
        );
    }

    #[test]
    fn the_retry_backoff_is_exponential_and_capped() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.delay(0), Duration::from_millis(100), "never zero");
        assert_eq!(policy.delay(1), Duration::from_millis(100));
        assert_eq!(policy.delay(2), Duration::from_millis(200));
        assert_eq!(policy.delay(3), Duration::from_millis(400));
        assert_eq!(policy.delay(20), Duration::from_secs(2), "capped");
        // Bounded in both directions: at most six retries, and never more than a
        // couple of seconds of silence each.
        assert!(policy.max_reconnects <= 10);
        assert!(policy.cap <= Duration::from_secs(5));
    }
}
