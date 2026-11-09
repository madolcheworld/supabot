//! The control socket: dial out with a signed upgrade, serve until it closes,
//! and redial with backoff — unless the brain said this node is revoked.
//!
//! Three rules from the protocol document are load-bearing here:
//!
//! * **the node always dials out** (§3.1, decision D1.2), so NAT and firewalls
//!   are not a problem and the node never listens;
//! * **backoff is 250 ms → 30 s with ±50 % jitter, reset only after a
//!   connection stayed up 10 s** (§3.4). Resetting on every close would let a
//!   brain that flaps a socket turn into a reconnect storm;
//! * **close code 4401 stops redialing permanently** (§3.3): a revoked node
//!   must not keep knocking, and the operator has to be told, because the only
//!   fix is a re-pair.
//!
//! [`NodeSocket`] exists because [`swarmbot_node_protocol::WsSocket`] discards the peer's
//! close code — it answers `Ok(None)` for any `Close` frame — and 4401 is only
//! observable *from* that code. Everything else about it is the same adapter.

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use futures::{SinkExt as _, StreamExt as _};
use rand::Rng as _;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use swarmbot_node_protocol::mux::{Handler, Peer, PeerConfig, Socket};
use swarmbot_node_protocol::rpc::{self, EVENT_NODE_CONNECTED, Heartbeat, NodeEvent};
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::Message as WsMessage;
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::http::header::{
    HeaderName, HeaderValue,
};
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::protocol::CloseFrame;
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes;
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use swarmbot_node_protocol::tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use swarmbot_node_protocol::{CLOSE_REVOKED, PROTOCOL_VERSION, ProtocolError, unix_ms_now};

use crate::NODE_VERSION;
use crate::NodeError;
use crate::handlers::{NodeHandler, NodeRuntime, PeerLink};
use crate::identity::dial_url;
use crate::state::StateWriter;

/// The informational header of §3.1: `min-max` of the range this node speaks.
pub fn protocol_header() -> String {
    format!("{PROTOCOL_VERSION}-{PROTOCOL_VERSION}")
}

/// The close code a peer sent, or `0` when it closed without one (or never
/// closed at all). Zero is not a valid WebSocket close code, so it is a safe
/// "nothing recorded" marker.
pub const NO_CLOSE_CODE: u16 = 0;

/// A [`Socket`] over `tokio-tungstenite` that remembers the peer's close code.
pub struct NodeSocket {
    inner: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    close_code: Arc<AtomicU16>,
}

impl NodeSocket {
    /// The close code the peer sent, or [`NO_CLOSE_CODE`].
    pub fn close_code(&self) -> u16 {
        self.close_code.load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for NodeSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeSocket")
            .field("close_code", &self.close_code())
            .finish_non_exhaustive()
    }
}

impl Socket for NodeSocket {
    async fn send_text(&mut self, text: &str) -> Result<(), ProtocolError> {
        self.inner
            .send(WsMessage::Text(Utf8Bytes::from(text)))
            .await
            .map_err(|error| ProtocolError::ws(error.to_string()))
    }

    async fn recv_text(&mut self) -> Result<Option<String>, ProtocolError> {
        loop {
            match self.inner.next().await {
                Some(Ok(WsMessage::Text(text))) => return Ok(Some(text.as_str().to_string())),
                // Protocol-level pings are answered by tungstenite itself, and
                // the control socket is text-only (§4.1): everything else is
                // ignored rather than killing a live connection.
                Some(Ok(WsMessage::Ping(_)))
                | Some(Ok(WsMessage::Pong(_)))
                | Some(Ok(WsMessage::Binary(_)))
                | Some(Ok(WsMessage::Frame(_))) => continue,
                Some(Ok(WsMessage::Close(frame))) => {
                    // The one reason this adapter exists: 4401 has to survive.
                    if let Some(frame) = frame {
                        self.close_code.store(frame.code.into(), Ordering::SeqCst);
                        tracing::debug!(code = u16::from(frame.code), reason = %frame.reason, "the brain closed the control socket");
                    }
                    return Ok(None);
                }
                Some(Err(error)) => {
                    // A reset without a close handshake is an unclean end, not
                    // a protocol violation worth an error frame nobody reads.
                    return Err(ProtocolError::ws(error.to_string()));
                }
                None => return Ok(None),
            }
        }
    }

    async fn close(&mut self, code: u16, reason: &str) -> Result<(), ProtocolError> {
        let frame = CloseFrame {
            code: CloseCode::from(code),
            reason: Utf8Bytes::from(reason),
        };
        let _ = self.inner.send(WsMessage::Close(Some(frame))).await;
        let _ = self.inner.close(None).await;
        Ok(())
    }
}

/// Dial the brain's control socket with the signed upgrade header (§3.1).
///
/// Returns the socket plus the shared cell its close code will land in, so the
/// caller can read the code after the socket has been moved into a [`Peer`].
pub async fn connect(
    url: &str,
    authorization: &str,
) -> Result<(NodeSocket, Arc<AtomicU16>), NodeError> {
    let mut request = url
        .into_client_request()
        .map_err(|error| NodeError::Unreachable {
            url: url.to_string(),
            reason: error.to_string(),
        })?;
    for (name, value) in [
        ("Authorization", authorization),
        ("x-swarmbot-protocol", &protocol_header()),
    ] {
        let header =
            HeaderName::from_bytes(name.as_bytes()).map_err(|error| NodeError::Unreachable {
                url: url.to_string(),
                reason: format!("bad header name `{name}`: {error}"),
            })?;
        let value = HeaderValue::from_str(value).map_err(|error| NodeError::Unreachable {
            url: url.to_string(),
            // A header value that cannot be sent says nothing about the key
            // that produced it, and the signature is not in this string anyway.
            reason: format!("bad header value for `{name}`: {error}"),
        })?;
        request.headers_mut().insert(header, value);
    }
    let (stream, response) =
        connect_async(request)
            .await
            .map_err(|error| NodeError::Unreachable {
                url: url.to_string(),
                reason: error.to_string(),
            })?;
    // The URL is logged, never the Authorization header: it carries a signature
    // over a fresh nonce, which is not a secret worth keeping but is also not
    // anybody's business.
    tracing::debug!(url, status = %response.status(), "control socket connected");
    let close_code = Arc::new(AtomicU16::new(NO_CLOSE_CODE));
    Ok((
        NodeSocket {
            inner: stream,
            close_code: Arc::clone(&close_code),
        },
        close_code,
    ))
}

/// How one dial ended.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEnd {
    /// The socket closed. `close_code` is what the peer sent, if anything.
    Closed {
        close_code: Option<u16>,
        up_for: Duration,
    },
    /// The dial itself failed: refused, DNS, TLS, or a 401 without an upgrade.
    Failed { reason: String },
}

/// Why `serve` returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// SIGINT/SIGTERM, or the caller's token. A clean stop.
    Shutdown,
    /// The brain closed with 4401: this node is revoked. Do not redial until
    /// it is re-paired (§3.3).
    Revoked,
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopReason::Shutdown => f.write_str("stopped on request"),
            StopReason::Revoked => f.write_str(
                "the brain closed the control socket with 4401: this node is revoked and will \
                 not redial. Re-pair it (`swarmbot-node pair <brain-url> <code>`) to reconnect.",
            ),
        }
    }
}

/// Backoff shape (§3.4).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DialPlan {
    /// First retry delay.
    pub initial: Duration,
    /// Cap.
    pub max: Duration,
    /// Jitter fraction: `0.5` means ±50 %.
    pub jitter: f64,
    /// A connection that stayed up this long resets the backoff.
    pub reset_after: Duration,
}

impl Default for DialPlan {
    fn default() -> Self {
        DialPlan {
            initial: Duration::from_millis(250),
            max: Duration::from_secs(30),
            jitter: 0.5,
            reset_after: Duration::from_secs(10),
        }
    }
}

/// Exponential backoff with jitter. Split from the loop so the growth is
/// testable without a socket or a clock.
#[derive(Clone, Debug)]
pub struct Backoff {
    plan: DialPlan,
    attempt: u32,
}

impl Backoff {
    pub fn new(plan: DialPlan) -> Backoff {
        Backoff { plan, attempt: 0 }
    }

    /// How many retries have been scheduled so far.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The delay before the next dial: `initial * 2^attempt`, capped, then
    /// jittered by ±`plan.jitter`.
    ///
    /// Jitter is not cosmetic: a brain that restarts drops every node at once,
    /// and without it they all come back on the same tick.
    pub fn next_delay(&mut self) -> Duration {
        let factor = 1.0 - self.plan.jitter + rand::rng().random::<f64>() * 2.0 * self.plan.jitter;
        self.next_with(factor)
    }

    /// [`Backoff::next_delay`] with an explicit jitter factor in `[0, 2]`, so a test
    /// can pin the sequence instead of asserting on a range.
    pub fn next_with(&mut self, factor: f64) -> Duration {
        let base = self
            .plan
            .initial
            .saturating_mul(2u32.saturating_pow(self.attempt))
            .min(self.plan.max);
        self.attempt = self.attempt.saturating_add(1);
        let factor = factor.clamp(0.0, 2.0);
        base.mul_f64(factor).min(self.plan.max)
    }

    /// Reset the growth after a connection that stayed up long enough to count
    /// as "it worked" (§3.4). A socket that closed after 200 ms does not reset
    /// anything, which is what keeps a flapping brain from causing a storm.
    pub fn note_uptime(&mut self, up_for: Duration) {
        if up_for >= self.plan.reset_after {
            self.attempt = 0;
        }
    }
}

/// What to do after one dial ended.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Next {
    Retry(Duration),
    Stop(StopReason),
}

/// The redial decision, as a pure function so a test can pin it.
pub fn decide(close_code: Option<u16>, backoff: &mut Backoff) -> Next {
    if close_code == Some(CLOSE_REVOKED) {
        return Next::Stop(StopReason::Revoked);
    }
    Next::Retry(backoff.next_delay())
}

/// What `serve` needs beyond the runtime.
#[derive(Default)]
pub struct ServeOptions {
    pub plan: DialPlan,
    /// The live-state file; `None` in tests that must not touch a data dir.
    pub state: Option<StateWriter>,
}

/// Dial, serve, redial — until shutdown or a 4401.
pub async fn serve(
    runtime: Arc<NodeRuntime>,
    shutdown: CancellationToken,
    options: ServeOptions,
) -> Result<StopReason, NodeError> {
    let url = dial_url(&runtime.config, &runtime.identity)?;
    let ServeOptions { plan, state } = options;
    let mut backoff = Backoff::new(plan);
    loop {
        let end = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(StopReason::Shutdown),
            result = session(Arc::clone(&runtime), &url, state.as_ref()) => result,
        };
        let (close_code, up_for, reason) = match end {
            Ok(SessionEnd::Closed { close_code, up_for }) => (close_code, up_for, None),
            Ok(SessionEnd::Failed { reason }) => (None, Duration::ZERO, Some(reason.clone())),
            Err(error) => (None, Duration::ZERO, Some(error.to_string())),
        };
        backoff.note_uptime(up_for);
        if let Some(state) = state.as_ref()
            && let Err(error) = state.update(|snapshot| {
                snapshot.connected = false;
                snapshot.active_invocations = 0;
                snapshot.last_error = reason.clone().or_else(|| {
                    close_code.map(|code| format!("the brain closed the socket with code {code}"))
                });
            })
        {
            tracing::warn!(%error, "could not record the disconnect in the state file");
        }
        match decide(close_code, &mut backoff) {
            Next::Stop(reason) => {
                if reason == StopReason::Revoked {
                    // Said on stderr through tracing at error level: this is
                    // the one stop an operator has to act on.
                    tracing::error!(url = %url, "{reason}");
                }
                return Ok(reason);
            }
            Next::Retry(delay) => {
                tracing::info!(
                    url = %url,
                    attempt = backoff.attempt(),
                    retry_in_ms = delay.as_millis() as u64,
                    last_error = reason.as_deref().unwrap_or("the socket closed"),
                    "redialing"
                );
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => return Ok(StopReason::Shutdown),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

/// One connection: dial with a fresh signed upgrade, serve until the socket
/// ends, then report how it ended.
pub async fn session(
    runtime: Arc<NodeRuntime>,
    url: &str,
    state: Option<&StateWriter>,
) -> Result<SessionEnd, NodeError> {
    let started = Instant::now();
    // A fresh nonce per dial (§3.1): the brain burns it in its replay cache, so
    // reusing one would be refused as a replay.
    let (authorization, nonce) = runtime.identity.upgrade()?;
    let (socket, close_code) = match connect(url, &authorization).await {
        Ok(pair) => pair,
        Err(error) => {
            return Ok(SessionEnd::Failed {
                reason: error.to_string(),
            });
        }
    };
    let handler = NodeHandler::new(Arc::clone(&runtime), nonce);
    let peer = Peer::new(
        socket,
        PeerConfig {
            // The *brain* calls `node.hello`; the node must refuse everything
            // else until `brainSig` verified against the pinned key. This flag
            // is the mux's half of that rule and the handler's `is_authenticated`
            // check is the other half — both, on purpose.
            require_hello: true,
            handler: Some(Arc::clone(&handler) as Handler),
            ..PeerConfig::default()
        },
    );
    handler.attach(PeerLink::shared(peer.clone()));

    if let Some(state) = state
        && let Err(error) = state.update(|snapshot| {
            snapshot.connected = false;
            snapshot.last_error = None;
        })
    {
        tracing::warn!(%error, "could not record the dial in the state file");
    }

    // Three background tasks, all of which end with the connection:
    //
    // * `announce` sends `node.connected` **once the hello verified**, not once
    //   the socket opened — an event that says "connected" for a peer that
    //   could not authenticate would be a lie the brain publishes to its UI;
    // * `mark_connected` records the same fact in the state file `status` reads;
    // * `heartbeat` sends `node.heartbeat` on its own period.
    let announce = {
        let handler = Arc::clone(&handler);
        let peer = peer.clone();
        let label = runtime.config.label.clone();
        let capabilities = runtime.config.capabilities();
        tokio::spawn(async move {
            handler.wait_authenticated().await;
            let event = NodeEvent {
                event_type: EVENT_NODE_CONNECTED.to_string(),
                data: json!({
                    "label": label,
                    "version": NODE_VERSION,
                    "capabilities": capabilities.as_slice(),
                    "roots": handler.runtime().roots(),
                }),
            };
            if let Err(error) = peer.notify(rpc::NODE_EVENT, json!(event)).await {
                tracing::debug!(%error, "could not announce node.connected");
            }
        })
    };
    let mark_connected = {
        let handler = Arc::clone(&handler);
        let state = state.cloned();
        tokio::spawn(async move {
            handler.wait_authenticated().await;
            let Some(state) = state else { return };
            if let Err(error) = state.update(|snapshot| {
                snapshot.connected = true;
                snapshot.connected_at = Some(chrono::Utc::now());
                snapshot.last_error = None;
            }) {
                tracing::warn!(%error, "could not record the connection in the state file");
            }
        })
    };
    let heartbeat = {
        let handler = Arc::clone(&handler);
        let peer = peer.clone();
        let secs = runtime.config.heartbeat_secs;
        tokio::spawn(async move { heartbeat_loop(peer, handler, secs).await })
    };

    // Serve until the socket ends or the daemon is asked to stop.
    tokio::select! {
        _ = peer.closed() => {}
        _ = runtime.stopping.cancelled() => {
            // Graceful: stop accepting new calls, kill any process group still
            // registered (the mux also cancels in-flight handlers, whose Drop
            // guards kill their own groups), then close 1000 (§3.3).
            runtime.shutdown();
        }
    }
    // Whatever woke us, the socket is going away: the handler must not be able
    // to ask for an approval on a dead link, the driver must stop, and the
    // background tasks must not outlive the connection they belong to.
    handler.detach();
    peer.close(1000, "node stopped serving this connection")
        .await;
    announce.abort();
    mark_connected.abort();
    heartbeat.abort();

    let code = close_code.load(Ordering::SeqCst);
    let up_for = started.elapsed();
    tracing::info!(
        url,
        close_code = code,
        up_for_ms = up_for.as_millis() as u64,
        "the control socket ended"
    );
    Ok(SessionEnd::Closed {
        close_code: (code != NO_CLOSE_CODE).then_some(code),
        up_for,
    })
}

/// `node.heartbeat` every `secs` seconds, with the number of invocations this
/// node is running (§6). Fire-and-forget: a heartbeat nobody answers is the
/// liveness timeout's problem, not this task's.
async fn heartbeat_loop<S: Socket>(peer: Peer<S>, handler: Arc<NodeHandler>, secs: u64) {
    let period = Duration::from_secs(secs.max(1));
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires immediately; consume it so a fresh connection does
    // not heartbeat at t=0.
    tick.tick().await;
    loop {
        tick.tick().await;
        let beat = Heartbeat {
            at: unix_ms_now(),
            active_invocations: handler.active_invocations(),
            load: load_average(),
            free_mem_bytes: free_mem_bytes(),
        };
        if peer.notify(rpc::NODE_HEARTBEAT, json!(beat)).await.is_err() {
            return;
        }
    }
}

/// The 1-minute load average, or `None` where the OS does not publish one.
///
/// Reads `/proc/loadavg` synchronously: it is a few dozen bytes of a procfs
/// file, and the alternative (a blocking-pool round trip every 15 s) costs
/// more than the read.
pub fn load_average() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    text.split_whitespace().next()?.parse::<f64>().ok()
}

/// Available memory in bytes, or `None` where `/proc/meminfo` does not exist.
pub fn free_mem_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_plan_is_the_documented_one() {
        let plan = DialPlan::default();
        assert_eq!(plan.initial, Duration::from_millis(250));
        assert_eq!(plan.max, Duration::from_secs(30));
        assert_eq!(plan.jitter, 0.5);
        assert_eq!(plan.reset_after, Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_connection_retries_with_a_growing_delay() {
        // Paused clock: the delays below are "waited" instantly, so this test
        // asserts the sequence rather than the wall time.
        let mut backoff = Backoff::new(DialPlan::default());
        let delays: Vec<Duration> = (0..12).map(|_| backoff.next_with(1.0)).collect();
        assert_eq!(delays[0], Duration::from_millis(250));
        assert_eq!(delays[1], Duration::from_millis(500));
        assert_eq!(delays[2], Duration::from_secs(1));
        assert_eq!(delays[3], Duration::from_secs(2));
        for pair in delays.windows(2) {
            assert!(pair[0] <= pair[1], "delays must not shrink: {delays:?}");
        }
        // ...and they stop growing at the cap.
        assert_eq!(delays[11], Duration::from_secs(30));
        assert!(delays.iter().all(|d| *d <= Duration::from_secs(30)));
        // A real sleep under the paused clock resolves without wall time.
        tokio::time::sleep(delays[0]).await;
    }

    #[test]
    fn jitter_stays_inside_plus_or_minus_fifty_percent() {
        let plan = DialPlan::default();
        for attempt in 0..8 {
            let base = plan.initial.saturating_mul(2u32.pow(attempt)).min(plan.max);
            let mut backoff = Backoff::new(plan);
            for _ in 0..attempt {
                backoff.next_delay();
            }
            for _ in 0..200 {
                let delay = backoff.next_delay();
                let low = base.mul_f64(0.5);
                let high = base.mul_f64(1.5).min(plan.max);
                assert!(
                    delay >= low && delay <= high,
                    "attempt {attempt}: {delay:?} outside [{low:?}, {high:?}]"
                );
                backoff = Backoff::new(plan);
                for _ in 0..attempt {
                    backoff.next_delay();
                }
            }
        }
    }

    #[test]
    fn the_backoff_resets_only_after_a_connection_that_lasted() {
        let plan = DialPlan::default();
        let mut backoff = Backoff::new(plan);
        for _ in 0..5 {
            backoff.next_delay();
        }
        assert_eq!(backoff.attempt(), 5);
        // A socket that died at once does not reset anything.
        backoff.note_uptime(Duration::from_millis(200));
        assert_eq!(backoff.attempt(), 5);
        backoff.note_uptime(Duration::from_secs(9));
        assert_eq!(backoff.attempt(), 5, "9 s is not the 10 s the rule names");
        backoff.note_uptime(Duration::from_secs(10));
        assert_eq!(backoff.attempt(), 0);
        assert_eq!(backoff.next_with(1.0), Duration::from_millis(250));
    }

    #[test]
    fn a_4401_close_stops_everything_else_retries() {
        let plan = DialPlan::default();
        let mut backoff = Backoff::new(plan);
        assert_eq!(
            decide(Some(CLOSE_REVOKED), &mut backoff),
            Next::Stop(StopReason::Revoked)
        );
        // Every other code — including 4403 (hello failed) and 4000 (replaced)
        // — is a retry: those are conditions the next dial can fix.
        for code in [Some(4403u16), Some(4426), Some(4000), Some(1000), None] {
            assert!(
                matches!(decide(code, &mut backoff), Next::Retry(_)),
                "{code:?} must retry"
            );
        }
        assert!(StopReason::Revoked.to_string().contains("4401"));
    }

    #[test]
    fn the_protocol_header_names_the_range_this_build_speaks() {
        assert_eq!(
            protocol_header(),
            format!("{PROTOCOL_VERSION}-{PROTOCOL_VERSION}")
        );
        assert_eq!(protocol_header(), "1-1");
    }

    #[test]
    fn load_and_memory_are_either_real_or_absent() {
        // Both are optional by contract: a platform without /proc reports None
        // rather than a made-up zero.
        if let Some(load) = load_average() {
            assert!(load >= 0.0 && load.is_finite(), "{load}");
        }
        if let Some(free) = free_mem_bytes() {
            assert!(free > 0, "{free}");
        }
        if std::path::Path::new("/proc/meminfo").exists() {
            assert!(free_mem_bytes().is_some(), "Linux must report memory");
            assert!(load_average().is_some(), "Linux must report load");
        }
    }
}
