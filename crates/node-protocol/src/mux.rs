//! The bidirectional peer: one WebSocket (or any text-frame [`Socket`])
//! carrying two virtual RPC endpoints in opposite directions (§4).
//!
//! Architecture: [`Peer::new`] spawns a single **driver task** that owns the
//! socket. Everything else — [`Peer::call`], [`Peer::call_stream`],
//! [`ChunkSink`] chunks from handler tasks — talks to the driver through a
//! command channel, so the socket has exactly one reader and one writer and
//! frames are serialized in one place. The driver implements the wire rules:
//!
//! * direction routing (§4.2, [`crate::wire::direction`]): requests are
//!   dispatched to the [`RpcHandler`]; responses/results/chunks settle calls
//!   this peer made; `Ack`/`Interrupt`/`Ping`/`Pong` are consumed by the
//!   control path;
//! * stream back-pressure (§4.6): [`ChunkSink::send`] emits one chunk frame
//!   and then **waits for the peer's Ack** before the handler may send the
//!   next one; this peer, as a receiver, Acks every chunk it forwards;
//! * interruption (§4.7): an inbound `Interrupt` fires a per-request
//!   [`CancellationToken`]; the handler's future is dropped (its `Drop`
//!   guards — killing a process group, say — run), and the driver answers
//!   with an error frame whose `data` holds an `Interrupt` reason;
//! * liveness (§3.3): `@effect/rpc/Ping` every [`PeerConfig::ping_interval`]
//!   (default 5 s), inbound `Ping` answered with `Pong`, and the socket is
//!   closed after [`PeerConfig::liveness_timeout`] (default 20 s) without any
//!   inbound frame — half-open detection;
//! * call deadlines (§4.7): [`Peer::call_with_id_deadline`] and
//!   [`Peer::call_stream_with_id_deadline`] stop waiting after a caller-chosen
//!   deadline. Expiry **withdraws** the call — [`Peer::abandon`] removes its
//!   pending entry so a late answer can neither settle it nor leak the
//!   bookkeeping, and the peer is sent `@effect/rpc/Interrupt` — and the call
//!   fails with [`ProtocolError::CallTimedOut`]. Liveness and a deadline are
//!   different failures on purpose: liveness means the *socket* went quiet, a
//!   deadline means the *work* did not finish while frames kept flowing.
//!
//!   **A deadline is not a cancellation guarantee.** It bounds how long this
//!   side waits and *asks* the peer to stop. Our own node answers an interrupt
//!   by dropping the handler future, which runs its `Drop` guards and kills the
//!   process group (§4.7, §5.1); a peer that ignores the frame may still be
//!   running the command when this side has given up, and nothing here can kill
//!   a process on another machine. The caller is told that in
//!   [`ProtocolError::CallTimedOut`]'s own doc and in the brain's tool error.
//! * two independent id spaces (§4.2): ids are allocated by the side that
//!   sends the request, so both directions may use `0,1,2,…` without
//!   colliding; inbound and outbound bookkeeping live in separate maps;
//! * the `node.hello` gate (§3.2): with [`PeerConfig::require_hello`] (a
//!   constructor flag — the node side sets it, the brain side does not) every
//!   inbound request is answered `NotAuthenticated` until the `node.hello`
//!   handler returned `Ok`.
//!
//! Borrowing note for maintainers: `tokio::select!` keeps the losing
//! branches' futures alive while a handler body runs, so the driver never
//! touches the socket inside `select!` — handlers queue rendered frames into
//! `State::outbound`, which is drained at the top of the next iteration.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

// Publicly re-exported so handler implementors can name the return type of
// [`RpcHandler::handle`] without depending on `futures` themselves.
pub use futures::future::BoxFuture;
use futures::{SinkExt as _, StreamExt as _};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until, timeout};

use crate::rpc::{NODE_HELLO, is_stream_method};
use crate::wire::{self, CauseError, Frame, Id, ProtocolError, TypedError};

/// A duplex text-frame transport. One frame in, one frame out; framing
/// (WebSocket, newline-delimited memory pipes, …) is the adapter's business.
///
/// **Cancellation safety:** [`Socket::recv_text`] is used inside
/// `tokio::select!`, so it is dropped and recreated between frames. It must
/// be cancellation-safe (losing a partial read must not lose data) — both
/// adapters in this crate are (`BufReader::read_line` and `StreamExt::next`
/// keep their state).
pub trait Socket: Send + 'static {
    /// Send one text frame.
    fn send_text(&mut self, text: &str) -> impl Future<Output = Result<(), ProtocolError>> + Send;

    /// Receive the next text frame. `Ok(None)` is a clean end of stream.
    fn recv_text(&mut self) -> impl Future<Output = Result<Option<String>, ProtocolError>> + Send;

    /// Best-effort close, carrying a WebSocket close code (§3.2: 4401, 4403,
    /// 4426, 4000, …). Non-WebSocket transports may ignore the code; failing
    /// to deliver a close frame is not an error — the socket is going away.
    fn close(
        &mut self,
        code: u16,
        reason: &str,
    ) -> impl Future<Output = Result<(), ProtocolError>> + Send {
        async move {
            let _ = (code, reason);
            Ok::<(), ProtocolError>(())
        }
    }
}

/// A cooperative-cancellation flag, one per in-flight request (§4.7).
///
/// This crate ships its own tiny token instead of depending on `tokio-util`:
/// the dependency list is fixed, and all the mux needs is "fire once, many
/// waiters, poll-safe".
#[derive(Clone, Debug)]
pub struct CancellationToken {
    inner: Arc<CancelInner>,
}

#[derive(Debug)]
struct CancelInner {
    state: watch::Sender<bool>,
    /// Keeps a receiver alive so `send` can never fail and `subscribe` always
    /// sees the latest value.
    _hold: watch::Receiver<bool>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    pub fn new() -> CancellationToken {
        let (state, hold) = watch::channel(false);
        CancellationToken {
            inner: Arc::new(CancelInner { state, _hold: hold }),
        }
    }

    /// Fire the token. Idempotent; every current and future waiter observes
    /// it (the value lives in the watch cell).
    pub fn cancel(&self) {
        let _ = self.inner.state.send(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.inner.state.borrow()
    }

    /// Resolves once the token is cancelled. Handlers `select!` their real
    /// work against this; the mux itself drops the handler's future when it
    /// fires, so `Drop` guards (kill the process group, close the file) run.
    pub async fn cancelled(&self) {
        let mut rx = self.inner.state.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                // The sender lives inside the same Arc, so this is
                // unreachable in practice; treat it as cancelled rather
                // than hanging forever.
                return;
            }
        }
    }
}

/// Where a streaming handler pushes its chunks (§4.6).
///
/// [`ChunkSink::send`] emits `{"chunk":true,…}` and **awaits the peer's
/// `@effect/rpc/Ack`** before returning, which is the back-pressure the
/// protocol mandates: at most one chunk per request is in flight. Cloning is
/// cheap (e.g. one task per output pipe of a shell run).
#[derive(Clone, Debug)]
pub struct ChunkSink {
    id: Id,
    cancel: CancellationToken,
    cmd: mpsc::UnboundedSender<Cmd>,
}

impl ChunkSink {
    /// The request id whose stream this sink feeds.
    pub fn id(&self) -> &Id {
        &self.id
    }

    /// The request's cancellation token (§4.7) — long-running work should
    /// `select!` against [`CancellationToken::cancelled`] so an `Interrupt`
    /// stops it promptly instead of only at the next await-point drop.
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Send one chunk and wait for the peer's Ack. Fails with
    /// [`ProtocolError::Interrupted`] when the call was interrupted while the
    /// chunk was in flight, [`ProtocolError::StreamGone`] when the request is
    /// no longer served, and [`ProtocolError::SocketClosed`] when the
    /// connection ended.
    pub async fn send(&self, value: Value) -> Result<(), ProtocolError> {
        let (ack, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Chunk {
                id: self.id.clone(),
                value,
                ack,
            })
            .map_err(|_| ProtocolError::SocketClosed)?;
        rx.await.map_err(|_| ProtocolError::SocketClosed)?
    }
}

/// The request handler of one peer (its "server" half).
///
/// Returns `Result<Value, TypedError>`: `Ok(Value::Null)` for void RPCs
/// (§4.4), `Err` for the typed refusals of §5.3. The future is dropped when
/// the call is interrupted, so cleanup belongs in `Drop` guards or in a
/// `select!` against [`ChunkSink::cancellation`].
///
/// The signature is the documented `async fn handle(&self, method, params,
/// tx)` desugared by hand into a boxed future (no `async-trait` in this
/// crate's dependency list), which keeps `Arc<dyn RpcHandler>` object-safe.
/// [`BoxFuture`] is re-exported here so implementors do not need the
/// `futures` crate themselves:
///
/// ```
/// use swarmbot_node_protocol::mux::BoxFuture;
/// use swarmbot_node_protocol::serde_json::Value;
/// use swarmbot_node_protocol::{ChunkSink, RpcHandler, TypedError};
///
/// struct MyHandler;
///
/// impl RpcHandler for MyHandler {
///     fn handle<'a>(
///         &'a self,
///         method: &'a str,
///         params: Value,
///         tx: Option<ChunkSink>,
///     ) -> BoxFuture<'a, Result<Value, TypedError>> {
///         Box::pin(async move {
///             let _ = (method, params, tx);
///             Ok(Value::Null)
///         })
///     }
/// }
/// ```
pub trait RpcHandler: Send + Sync + 'static {
    fn handle<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        tx: Option<ChunkSink>,
    ) -> BoxFuture<'a, Result<Value, TypedError>>;
}

/// How a peer serves requests: a shared, swappable handler object.
pub type Handler = Arc<dyn RpcHandler>;

/// Peer construction knobs. The defaults are the reference values of §3.3.
#[derive(Clone)]
pub struct PeerConfig {
    /// How often this side sends `@effect/rpc/Ping` (default 5 s).
    pub ping_interval: Duration,
    /// Close the socket after this long without **any** inbound frame
    /// (default 20 s, §3.3 half-open detection).
    pub liveness_timeout: Duration,
    /// Node-side gate (§3.2): answer every request with `NotAuthenticated`
    /// until the `node.hello` handler returned `Ok`. A constructor flag, not
    /// a runtime guess — the brain side leaves it `false`.
    pub require_hello: bool,
    /// The handler installed at construction; [`Peer::serve`] can replace it
    /// before traffic starts.
    pub handler: Option<Handler>,
}

// `Handler` is a trait object without Debug; the config should still print
// usefully in logs and test failures.
impl std::fmt::Debug for PeerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerConfig")
            .field("ping_interval", &self.ping_interval)
            .field("liveness_timeout", &self.liveness_timeout)
            .field("require_hello", &self.require_hello)
            .field("handler", &self.handler.as_ref().map(|_| "dyn RpcHandler"))
            .finish()
    }
}

impl Default for PeerConfig {
    fn default() -> Self {
        PeerConfig {
            ping_interval: Duration::from_secs(5),
            liveness_timeout: Duration::from_secs(20),
            require_hello: false,
            handler: None,
        }
    }
}

/// A handle to the driver task running one bidirectional connection.
///
/// Cheap to clone; every clone talks to the same driver. Dropping **all**
/// clones closes the connection: the driver holds only a weak sender, so the
/// command channel empties and the driver exits with [`Exit::HandlesGone`].
/// An explicit [`Peer::close`] is still the way to send a close code and
/// reason — dropping is the leak-proof fallback, not the polite goodbye.
/// The socket type parameter is kept so a `Peer<WsSocket<_>>` documents what
/// it runs on; the socket itself lives in the driver task.
pub struct Peer<S> {
    cmd: mpsc::UnboundedSender<Cmd>,
    next_id: Arc<AtomicI64>,
    handler_slot: Arc<RwLock<Option<Handler>>>,
    closed: watch::Receiver<bool>,
    _socket: PhantomData<fn() -> S>,
}

impl<S> Clone for Peer<S> {
    fn clone(&self) -> Self {
        Peer {
            cmd: self.cmd.clone(),
            next_id: Arc::clone(&self.next_id),
            handler_slot: Arc::clone(&self.handler_slot),
            closed: self.closed.clone(),
            _socket: PhantomData,
        }
    }
}

impl<S: Socket> Peer<S> {
    /// Start driving `socket`. Must be called inside a tokio runtime (it
    /// spawns the driver task). Nothing is sent until the first call.
    pub fn new(socket: S, config: PeerConfig) -> Peer<S> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (closed_tx, closed_rx) = watch::channel(false);
        let handler_slot = Arc::new(RwLock::new(config.handler.clone()));
        let driver = Driver {
            socket,
            cmd_rx,
            cmd_tx: cmd_tx.downgrade(),
            handler_slot: Arc::clone(&handler_slot),
            state: State::new(config.require_hello),
            ping_interval: config.ping_interval,
            liveness_timeout: config.liveness_timeout,
        };
        tokio::spawn(driver.run(closed_tx));
        Peer {
            cmd: cmd_tx,
            next_id: Arc::new(AtomicI64::new(0)),
            handler_slot,
            closed: closed_rx,
            _socket: PhantomData,
        }
    }

    /// Install (or replace) the request handler. Do this before the peer is
    /// expected to answer anything; inbound requests while no handler is set
    /// are answered with a defect, not silently dropped.
    pub fn serve(&self, handler: Handler) {
        // A poisoned lock only means a panic between lock and unlock; the
        // slot itself is consistent (AGENTS.md: recover with into_inner).
        *self.handler_slot.write().unwrap_or_else(|e| e.into_inner()) = Some(handler);
    }

    /// The next id of *this* direction's id space (§4.2: ids are allocated
    /// by the side that sends the request; the two spaces are independent).
    pub fn next_id(&self) -> Id {
        Id::Number(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// A unary RPC (§4.3/§4.4): resolve with the peer's `result`, or with
    /// [`ProtocolError::Remote`] carrying the peer's [`CauseError`].
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, ProtocolError> {
        self.call_with_id(self.next_id(), method, params, Vec::new())
            .await
    }

    /// [`Peer::call`] with a caller-chosen id and request headers (§4.10).
    /// The brain uses the headers to stamp [`crate::rpc::HEADER_NODE_ID`].
    ///
    /// This waits for as long as the peer takes. The liveness timeout is not a
    /// bound on it: a peer that keeps sending frames (pings, heartbeats) keeps
    /// the socket alive while never answering. Any path a human or a bot's turn
    /// is waiting on wants [`Peer::call_with_id_deadline`] instead.
    pub async fn call_with_id(
        &self,
        id: Id,
        method: &str,
        params: Value,
        headers: Vec<(String, String)>,
    ) -> Result<Value, ProtocolError> {
        let frame = Frame::Request {
            id: id.clone(),
            method: method.to_string(),
            params,
            headers,
        };
        let (resp, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Call { id, frame, resp })
            .map_err(|_| ProtocolError::SocketClosed)?;
        rx.await.map_err(|_| ProtocolError::SocketClosed)?
    }

    /// [`Peer::call_with_id`] with a deadline (§4.7).
    ///
    /// When the peer has not answered within `deadline` the call is
    /// **withdrawn**: its pending entry is removed so a late answer cannot
    /// settle it, the peer is sent `@effect/rpc/Interrupt`, and the call fails
    /// with [`ProtocolError::CallTimedOut`]. Withdrawing is what keeps the
    /// pending table from growing by one entry per hung call, and what makes a
    /// late answer a logged no-op instead of a delivery to nobody.
    ///
    /// A call that answers just after the deadline loses the race and is
    /// reported as a timeout: this side really did stop waiting, and its answer
    /// is discarded. The deadline is a bound on *waiting*, not on the peer's
    /// work — see the module docs for what that does and does not promise.
    pub async fn call_with_id_deadline(
        &self,
        id: Id,
        method: &str,
        params: Value,
        headers: Vec<(String, String)>,
        deadline: Duration,
    ) -> Result<Value, ProtocolError> {
        let frame = Frame::Request {
            id: id.clone(),
            method: method.to_string(),
            params,
            headers,
        };
        let (resp, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Call {
                id: id.clone(),
                frame,
                resp,
            })
            .map_err(|_| ProtocolError::SocketClosed)?;
        match timeout(deadline, rx).await {
            // A dropped sender means the driver went away without answering:
            // that is a closed socket, not a deadline.
            Ok(settled) => settled.map_err(|_| ProtocolError::SocketClosed)?,
            Err(_) => {
                // Withdraw *before* returning: the caller has stopped waiting,
                // so the bookkeeping must not outlive the wait.
                let _ = self.abandon(&id);
                Err(ProtocolError::CallTimedOut {
                    method: method.to_string(),
                    after: deadline,
                })
            }
        }
    }

    /// A streaming RPC (§4.6): every chunk value is forwarded to `out` (the
    /// driver Acks each chunk frame afterwards), and the call resolves when
    /// the peer ends the stream with a final result (`Ok(())`) or error.
    ///
    /// `out` must be drained concurrently — the driver awaits `out.send`, so
    /// a bounded channel nobody reads stalls the connection (that is the
    /// intended back-pressure, but it also stalls pings).
    pub async fn call_stream(
        &self,
        method: &str,
        params: Value,
        out: mpsc::Sender<Value>,
    ) -> Result<(), ProtocolError> {
        self.call_stream_with_id(self.next_id(), method, params, Vec::new(), out)
            .await
    }

    /// [`Peer::call_stream`] with a caller-chosen id and headers.
    pub async fn call_stream_with_id(
        &self,
        id: Id,
        method: &str,
        params: Value,
        headers: Vec<(String, String)>,
        out: mpsc::Sender<Value>,
    ) -> Result<(), ProtocolError> {
        let frame = Frame::Request {
            id: id.clone(),
            method: method.to_string(),
            params,
            headers,
        };
        let (resp, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Stream {
                id,
                frame,
                out,
                resp,
            })
            .map_err(|_| ProtocolError::SocketClosed)?;
        rx.await.map_err(|_| ProtocolError::SocketClosed)?
    }

    /// [`Peer::call_stream_with_id`] with a deadline (§4.7).
    ///
    /// The deadline covers the **whole stream**, not the gap between chunks: a
    /// stream that keeps dribbling chunks forever is exactly the shape a
    /// deadline exists to stop. Expiry withdraws the call like
    /// [`Peer::call_with_id_deadline`] does, which also drops the chunk sink, so
    /// a collector reading `out` finishes instead of waiting for a stream that
    /// will never end.
    pub async fn call_stream_with_id_deadline(
        &self,
        id: Id,
        method: &str,
        params: Value,
        headers: Vec<(String, String)>,
        out: mpsc::Sender<Value>,
        deadline: Duration,
    ) -> Result<(), ProtocolError> {
        let frame = Frame::Request {
            id: id.clone(),
            method: method.to_string(),
            params,
            headers,
        };
        let (resp, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Stream {
                id: id.clone(),
                frame,
                out,
                resp,
            })
            .map_err(|_| ProtocolError::SocketClosed)?;
        match timeout(deadline, rx).await {
            Ok(settled) => settled.map_err(|_| ProtocolError::SocketClosed)?,
            Err(_) => {
                let _ = self.abandon(&id);
                Err(ProtocolError::CallTimedOut {
                    method: method.to_string(),
                    after: deadline,
                })
            }
        }
    }

    /// Fire-and-forget RPC: sent as a normal request (this dialect has no
    /// id-less notifications), and whatever the peer answers is discarded.
    /// `Ok` means the frame was queued for sending.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), ProtocolError> {
        let frame = Frame::Request {
            id: self.next_id(),
            method: method.to_string(),
            params,
            headers: Vec::new(),
        };
        self.cmd
            .send(Cmd::Notify { frame })
            .map_err(|_| ProtocolError::SocketClosed)
    }

    /// Cancel a call this peer made (§4.7). The peer's server answers with an
    /// error frame whose `data` holds `Interrupt` reasons, which surfaces
    /// from the original `call`/`call_stream` as [`ProtocolError::Remote`].
    ///
    /// The pending entry is left in place on purpose: the peer still owes an
    /// answer, and that answer is what settles the call. Use [`Peer::abandon`]
    /// when this side has stopped waiting and no answer is wanted.
    pub async fn interrupt(&self, request_id: &Id) -> Result<(), ProtocolError> {
        self.cmd
            .send(Cmd::Interrupt {
                id: request_id.clone(),
            })
            .map_err(|_| ProtocolError::SocketClosed)
    }

    /// Withdraw a call this peer made: forget it and ask the peer to stop (§4.7).
    ///
    /// The driver removes the call's pending entry **first** and only then
    /// queues `@effect/rpc/Interrupt`, so an answer that arrives afterwards
    /// finds no entry, is logged and ignored, and cannot resurrect a call whose
    /// caller has gone. Dropping the entry also drops the stream sink, which is
    /// what lets a stream collector finish after a deadline.
    ///
    /// `Ok(())` even when nothing was pending: a call that settled first is a
    /// race, not an error, and turning it into one would make every deadline a
    /// coin flip. What this does *not* do is stop work on the peer — see the
    /// module docs on what an interrupt does and does not guarantee.
    pub fn abandon(&self, id: &Id) -> Result<(), ProtocolError> {
        self.cmd
            .send(Cmd::Abandon { id: id.clone() })
            .map_err(|_| ProtocolError::SocketClosed)
    }

    /// Close the connection with a WebSocket close code (§3.2) and wait for
    /// the driver to finish. Pending calls fail with
    /// [`ProtocolError::SocketClosed`]; running handlers are cancelled.
    pub async fn close(&self, code: u16, reason: &str) {
        let _ = self.cmd.send(Cmd::Close {
            code,
            reason: reason.to_string(),
        });
        self.closed().await;
    }

    /// Resolves when the connection has ended, however it ended. Mirrors the
    /// reference peer's `peer.closed`.
    pub async fn closed(&self) {
        let mut rx = self.closed.clone();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                // The driver dropped the sender: it is gone, so the
                // connection is closed even if the final `true` raced.
                return;
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }
}

/// A command from a handle (or a handler task) to the driver.
enum Cmd {
    Call {
        id: Id,
        frame: Frame,
        resp: oneshot::Sender<Result<Value, ProtocolError>>,
    },
    Stream {
        id: Id,
        frame: Frame,
        out: mpsc::Sender<Value>,
        resp: oneshot::Sender<Result<(), ProtocolError>>,
    },
    Notify {
        frame: Frame,
    },
    /// A handler chunk, with the waiter that the peer's Ack will settle.
    Chunk {
        id: Id,
        value: Value,
        ack: oneshot::Sender<Result<(), ProtocolError>>,
    },
    Interrupt {
        id: Id,
    },
    /// Withdraw a call this side made: forget its pending entry, then send
    /// `@effect/rpc/Interrupt` for it (§4.7). One command, so the two cannot be
    /// reordered against an answer that is already on its way.
    Abandon {
        id: Id,
    },
    /// A handler task's terminal outcome; the driver turns it into the
    /// response/error frame (keeping exactly one writer).
    Served {
        id: Id,
        outcome: ServedOutcome,
    },
    Close {
        code: u16,
        reason: String,
    },
}

enum ServedOutcome {
    Ok(Value),
    Err(TypedError),
    Interrupted,
}

/// Bookkeeping for a call *we* made (client half).
enum Pending {
    Unary(oneshot::Sender<Result<Value, ProtocolError>>),
    Stream {
        out: mpsc::Sender<Value>,
        resp: oneshot::Sender<Result<(), ProtocolError>>,
    },
}

/// Bookkeeping for a request *we* are serving (server half).
struct InFlight {
    method: String,
    cancel: CancellationToken,
    /// The waiter of the chunk currently in flight (§4.6: at most one).
    ack: Option<oneshot::Sender<Result<(), ProtocolError>>>,
}

/// Everything the select-loop mutates, kept away from the socket borrow.
struct State {
    /// Calls we made, by our own id space.
    pending: HashMap<Id, Pending>,
    /// Requests we are serving, by the peer's id space (§4.2: independent).
    in_flight: HashMap<Id, InFlight>,
    /// §3.2 gate: refuse everything until `node.hello` succeeded.
    gated: bool,
    /// Frames to write at the top of the next loop iteration (see the
    /// borrowing note in the module docs).
    outbound: VecDeque<String>,
}

impl State {
    fn new(gated: bool) -> State {
        State {
            pending: HashMap::new(),
            in_flight: HashMap::new(),
            gated,
            outbound: VecDeque::new(),
        }
    }

    fn queue(&mut self, frame: &Frame) {
        self.outbound.push_back(wire::render(frame));
    }

    fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Call { id, frame, resp } => {
                self.pending.insert(id, Pending::Unary(resp));
                self.queue(&frame);
            }
            Cmd::Stream {
                id,
                frame,
                out,
                resp,
            } => {
                self.pending.insert(id, Pending::Stream { out, resp });
                self.queue(&frame);
            }
            Cmd::Notify { frame } => self.queue(&frame),
            Cmd::Chunk { id, value, ack } => {
                let Some(entry) = self.in_flight.get_mut(&id) else {
                    let _ = ack.send(Err(ProtocolError::StreamGone {
                        request_id: id.to_string(),
                        reason: "the request is no longer served".into(),
                    }));
                    return;
                };
                // §4.6 keeps one chunk per request in flight; a second waiter
                // would mean a handler violated that, so fail the stale one
                // instead of leaking it.
                if let Some(stale) = entry.ack.take() {
                    let _ = stale.send(Err(ProtocolError::StreamGone {
                        request_id: id.to_string(),
                        reason: "superseded by another chunk".into(),
                    }));
                }
                entry.ack = Some(ack);
                self.queue(&Frame::Chunk {
                    id,
                    result: vec![value],
                });
            }
            Cmd::Interrupt { id } => {
                self.queue(&Frame::Interrupt { request_id: id });
            }
            Cmd::Abandon { id } => {
                // Forget first, then ask the peer to stop: a reply that lands
                // after this finds no pending entry, so it is logged and
                // ignored instead of being delivered to a caller that has
                // stopped waiting — and the dropped oneshot sender cannot panic
                // the driver.
                self.pending.remove(&id);
                self.queue(&Frame::Interrupt { request_id: id });
            }
            Cmd::Served { id, outcome } => {
                let Some(entry) = self.in_flight.remove(&id) else {
                    tracing::debug!(request_id = %id, "handler finished for an untracked request (ignored)");
                    return;
                };
                let hello_ok =
                    entry.method == NODE_HELLO && matches!(outcome, ServedOutcome::Ok(_));
                let frame = match outcome {
                    ServedOutcome::Ok(value) => Frame::Response { id, result: value },
                    ServedOutcome::Err(error) => Frame::Error {
                        id,
                        error: CauseError::fail(error),
                    },
                    // §4.7: the server answers an interrupt with an error
                    // frame whose data contains Interrupt reasons.
                    ServedOutcome::Interrupted => Frame::Error {
                        id,
                        error: CauseError::interrupted(),
                    },
                };
                if hello_ok {
                    // §3.2: the brain proved it holds the pinned key; only
                    // now may this node work for it.
                    self.gated = false;
                    tracing::debug!("node.hello verified; authentication gate lifted");
                }
                self.queue(&frame);
            }
            // Handled in the driver loop itself (it breaks out and closes
            // the socket during shutdown).
            Cmd::Close { .. } => {}
        }
    }

    async fn on_frame(
        &mut self,
        frame: Frame,
        cmd_tx: &mpsc::UnboundedSender<Cmd>,
        handler_slot: &Arc<RwLock<Option<Handler>>>,
    ) {
        match frame {
            Frame::Request {
                id, method, params, ..
            } => {
                self.on_request(id, method, params, cmd_tx, handler_slot);
            }
            Frame::Response { id, result } => match self.pending.remove(&id) {
                Some(Pending::Unary(resp)) => {
                    let _ = resp.send(Ok(result));
                }
                // §4.6: the end of a stream is a normal result frame
                // (`result: null` for void streams).
                Some(Pending::Stream { resp, .. }) => {
                    let _ = resp.send(Ok(()));
                }
                None => {
                    tracing::debug!(request_id = %id, "response for a settled or unknown id (ignored)")
                }
            },
            Frame::Error { id, error } => match self.pending.remove(&id) {
                Some(Pending::Unary(resp)) => {
                    let _ = resp.send(Err(ProtocolError::remote(error)));
                }
                Some(Pending::Stream { resp, .. }) => {
                    let _ = resp.send(Err(ProtocolError::remote(error)));
                }
                None => {
                    tracing::debug!(request_id = %id, "error for a settled or unknown id (ignored)")
                }
            },
            Frame::Chunk { id, result } => self.on_chunk(id, result).await,
            Frame::Ack { request_id } => {
                // §4.6: the Ack releases the next chunk of the stream we are
                // producing. An Ack whose requestId JSON type does not match
                // the request id finds nothing here — and the stream stalls,
                // exactly as the protocol doc warns.
                if let Some(entry) = self.in_flight.get_mut(&request_id) {
                    if let Some(ack) = entry.ack.take() {
                        let _ = ack.send(Ok(()));
                    }
                } else {
                    tracing::debug!(request_id = %request_id, "Ack for an untracked request (ignored)");
                }
            }
            Frame::Interrupt { request_id } => {
                if let Some(entry) = self.in_flight.get_mut(&request_id) {
                    // Cancel *before* failing a chunk in flight: the handler's
                    // `select!` is biased toward the token, so it reports
                    // Interrupted deterministically; its future is then
                    // dropped and Drop guards run (kill the process group),
                    // and the terminal frame carries Interrupt reasons (§4.7).
                    entry.cancel.cancel();
                    if let Some(ack) = entry.ack.take() {
                        let _ = ack.send(Err(ProtocolError::Interrupted));
                    }
                } else {
                    tracing::debug!(request_id = %request_id, "Interrupt for an untracked request (ignored)");
                }
            }
            // §4.9: answer Ping with Pong. A Pong only counts as liveness.
            Frame::Ping => self.queue(&Frame::Pong),
            Frame::Pong => {}
        }
    }

    fn on_request(
        &mut self,
        id: Id,
        method: String,
        params: Value,
        cmd_tx: &mpsc::UnboundedSender<Cmd>,
        handler_slot: &Arc<RwLock<Option<Handler>>>,
    ) {
        // §3.2: until node.hello succeeded, a gated (node-side) peer answers
        // every request NotAuthenticated — a hijack cannot drive the node.
        if self.gated && method != NODE_HELLO {
            self.queue(&Frame::Error {
                id,
                error: CauseError::fail(TypedError::not_authenticated(
                    "node.hello has not completed on this connection",
                )),
            });
            return;
        }
        let handler = handler_slot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(handler) = handler else {
            self.queue(&Frame::Error {
                id,
                error: CauseError::die("peer has no RPC handler installed"),
            });
            return;
        };
        let cancel = CancellationToken::new();
        // Only streaming methods get a ChunkSink (§5.1: shell.exec in our
        // subset); whether a call streams is schema knowledge, decided here
        // rather than guessed by the handler.
        let sink = is_stream_method(&method).then(|| ChunkSink {
            id: id.clone(),
            cancel: cancel.clone(),
            cmd: cmd_tx.clone(),
        });
        if self
            .in_flight
            .insert(
                id.clone(),
                InFlight {
                    method: method.clone(),
                    cancel: cancel.clone(),
                    ack: None,
                },
            )
            .is_some()
        {
            tracing::warn!(request_id = %id, method, "peer reused an in-flight request id; replacing the entry");
        }
        let cmd_tx = cmd_tx.clone();
        tokio::spawn(async move {
            // §4.7: the per-request CancellationToken is how an Interrupt
            // reaches the work. `biased` makes the cancel win a race against
            // a just-finished handler, so an interrupted call never answers
            // with a late success.
            let outcome = tokio::select! {
                biased;
                _ = cancel.cancelled() => ServedOutcome::Interrupted,
                result = handler.handle(&method, params, sink) => match result {
                    Ok(value) => ServedOutcome::Ok(value),
                    Err(error) => ServedOutcome::Err(error),
                },
            };
            let _ = cmd_tx.send(Cmd::Served { id, outcome });
        });
    }

    async fn on_chunk(&mut self, id: Id, result: Vec<Value>) {
        // Clone the consumer handle so no borrow of `self` is held across
        // the awaits below.
        let out = match self.pending.get(&id) {
            Some(Pending::Stream { out, .. }) => Some(out.clone()),
            Some(Pending::Unary(_)) => {
                // A unary call receiving chunks: not our dialect, but
                // Ack them so a streaming peer is not stalled forever and
                // drop the values.
                tracing::warn!(request_id = %id, "chunks arrived for a unary call; values dropped");
                None
            }
            None => {
                // Unknown id: do NOT Ack — Acking frames we cannot attribute
                // would hide bugs on both sides.
                tracing::debug!(request_id = %id, "chunk for an unknown id (ignored, not acked)");
                return;
            }
        };
        if let Some(out) = out {
            for value in result {
                if out.send(value).await.is_err() {
                    // The consumer is gone: end the call and stop Acking, so
                    // the producer notices instead of streaming into the void.
                    if let Some(Pending::Stream { resp, .. }) = self.pending.remove(&id) {
                        let _ = resp.send(Err(ProtocolError::ConsumerGone));
                    }
                    return;
                }
            }
        }
        // §4.6: after every chunk the receiver must send an Ack — with the
        // same id type the request used (Id preserves it).
        self.queue(&Frame::Ack { request_id: id });
    }
}

/// Why the driver stopped.
enum Exit {
    /// Every `Peer` handle (and every ChunkSink) was dropped.
    HandlesGone,
    /// The transport reported a clean end of stream.
    PeerEof,
    Read(ProtocolError),
    Write(ProtocolError),
    /// No inbound frame within the liveness timeout (§3.3).
    Liveness,
    /// [`Peer::close`] was called.
    Requested {
        code: u16,
        reason: String,
    },
}

struct Driver<S> {
    socket: S,
    cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    /// A **weak** sender, on purpose. The driver needs a sender to hand to the
    /// tasks it spawns for inbound requests, but if it held a strong one the
    /// command channel would never empty and `Exit::HandlesGone` would be
    /// unreachable: dropping every `Peer` would park the driver forever with the
    /// socket still open — one leaked task and socket per disconnected node.
    cmd_tx: mpsc::WeakUnboundedSender<Cmd>,
    handler_slot: Arc<RwLock<Option<Handler>>>,
    state: State,
    ping_interval: Duration,
    liveness_timeout: Duration,
}

impl<S: Socket> Driver<S> {
    async fn run(self, closed_tx: watch::Sender<bool>) {
        // Destructure so the select below borrows `socket`/`cmd_rx` while the
        // handlers only touch the other locals (see the module docs).
        let Driver {
            mut socket,
            mut cmd_rx,
            cmd_tx,
            handler_slot,
            mut state,
            ping_interval,
            liveness_timeout,
        } = self;

        let mut deadline = Instant::now() + liveness_timeout;
        let mut ping = interval(ping_interval);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // The first tick fires immediately; consume it so we do not ping at
        // t=0 of a fresh connection.
        ping.tick().await;

        let exit = loop {
            // Write everything queued by the previous iteration first: while
            // the select runs, `socket` is borrowed by `recv_text`.
            let mut write_error = None;
            while let Some(text) = state.outbound.pop_front() {
                if let Err(error) = socket.send_text(&text).await {
                    tracing::debug!(%error, "frame write failed");
                    write_error = Some(error);
                    break;
                }
            }
            if let Some(error) = write_error {
                break Exit::Write(error);
            }

            let mut exit = None;
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => match maybe_cmd {
                    // All handles and sinks are gone: nothing can ever talk
                    // to this connection again.
                    None => exit = Some(Exit::HandlesGone),
                    Some(Cmd::Close { code, reason }) => exit = Some(Exit::Requested { code, reason }),
                    Some(cmd) => state.on_cmd(cmd),
                },
                inbound = socket.recv_text() => match inbound {
                    Ok(Some(text)) => {
                        // §3.3: any decoded frame counts as liveness.
                        deadline = Instant::now() + liveness_timeout;
                        match wire::parse(&text) {
                            // An unparseable frame has no id we could answer
                            // under this dialect, so log and ignore it rather
                            // than killing a live connection.
                            Err(error) => tracing::warn!(%error, "ignoring an unparseable frame"),
                            Ok(frame) => match cmd_tx.upgrade() {
                                Some(cmd_tx) => {
                                    state.on_frame(frame, &cmd_tx, &handler_slot).await
                                }
                                // Every handle is gone: nobody can answer this
                                // frame, so close instead of leaking the socket.
                                None => exit = Some(Exit::HandlesGone),
                            },
                        }
                    }
                    Ok(None) => exit = Some(Exit::PeerEof),
                    Err(error) => {
                        tracing::debug!(%error, "socket read failed");
                        exit = Some(Exit::Read(error));
                    }
                },
                _ = ping.tick() => state.queue(&Frame::Ping),
                _ = sleep_until(deadline) => {
                    tracing::warn!(
                        timeout_secs = liveness_timeout.as_secs(),
                        "no inbound frame within the liveness timeout; closing the control socket (§3.3)"
                    );
                    exit = Some(Exit::Liveness);
                }
            }
            if let Some(exit) = exit {
                break exit;
            }
        };

        // §3.3: RPC streams do not survive a disconnect — cancel running
        // handlers (their Drop guards kill process groups) and fail waiting
        // callers with a typed error instead of leaving them hanging.
        for (_, entry) in state.in_flight.drain() {
            entry.cancel.cancel();
        }
        let failure = |exit: &Exit| match exit {
            Exit::Liveness => ProtocolError::LivenessTimeout,
            _ => ProtocolError::SocketClosed,
        };
        for (_, pending) in state.pending.drain() {
            match pending {
                Pending::Unary(resp) => {
                    let _ = resp.send(Err(failure(&exit)));
                }
                Pending::Stream { resp, .. } => {
                    let _ = resp.send(Err(failure(&exit)));
                }
            }
        }
        match &exit {
            Exit::Requested { code, reason } => {
                let _ = socket.close(*code, reason).await;
            }
            Exit::Liveness => {
                let _ = socket.close(1001, "liveness timeout").await;
            }
            Exit::HandlesGone => {
                let _ = socket.close(1000, "peer dropped").await;
            }
            // The socket is already broken; a best-effort close frame costs
            // nothing and helps a healthy peer see a clean end.
            Exit::PeerEof => {
                let _ = socket.close(1000, "connection ended").await;
            }
            Exit::Read(error) | Exit::Write(error) => {
                tracing::debug!(%error, "control socket ended after a transport error");
                let _ = socket.close(1000, "connection ended").await;
            }
        }
        let _ = closed_tx.send(true);
    }
}

// --- the WebSocket adapter ---------------------------------------------------

use tokio_tungstenite::tungstenite::Message as WsMessage;
pub use tokio_tungstenite::tungstenite::handshake::server::{
    Callback as WsCallback, ErrorResponse as WsErrorResponse, Request as WsRequest,
    Response as WsResponse,
};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// The [`Socket`] adapter over a `tokio-tungstenite` stream.
///
/// Generic over anything that sinks/streams tungstenite `Message`s, so it
/// fits both `connect_async` clients and server-side upgrades (including
/// axum's, as long as the tungstenite versions agree). WebSocket-level
/// ping/pong is handled by tungstenite itself; liveness here runs on the
/// application-level `@effect/rpc/Ping` frames of §3.3.
pub struct WsSocket<S> {
    inner: S,
}

// The wrapped stream usually lacks Debug; a stable placeholder is enough for
// error messages and assertions.
impl<S> std::fmt::Debug for WsSocket<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WsSocket(..)")
    }
}

impl<S> WsSocket<S> {
    pub fn new(inner: S) -> WsSocket<S> {
        WsSocket { inner }
    }
}

impl<S, E> Socket for WsSocket<S>
where
    S: futures::Stream<Item = Result<WsMessage, E>>
        + futures::Sink<WsMessage, Error = E>
        + Unpin
        + Send
        + 'static,
    E: std::fmt::Display + Send + 'static,
{
    async fn send_text(&mut self, text: &str) -> Result<(), ProtocolError> {
        self.inner
            .send(WsMessage::Text(Utf8Bytes::from(text)))
            .await
            .map_err(|e| ProtocolError::ws(e.to_string()))
    }

    async fn recv_text(&mut self) -> Result<Option<String>, ProtocolError> {
        loop {
            match self.inner.next().await {
                Some(Ok(WsMessage::Text(text))) => return Ok(Some(text.as_str().to_string())),
                // Protocol-level pings are answered by tungstenite itself;
                // the control socket is text-only (§4.1), so everything else
                // is ignored rather than killing the connection.
                Some(Ok(WsMessage::Ping(_)))
                | Some(Ok(WsMessage::Pong(_)))
                | Some(Ok(WsMessage::Binary(_)))
                | Some(Ok(WsMessage::Frame(_))) => continue,
                Some(Ok(WsMessage::Close(_))) => return Ok(None),
                Some(Err(error)) => return Err(ProtocolError::ws(error.to_string())),
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
        let _ = self.inner.close().await;
        Ok(())
    }
}

/// Dial the brain's control socket with the signed upgrade header (§3.1).
/// This is what a node calls after [`crate::auth::upgrade_auth`].
pub async fn connect_ws(
    url: &str,
    authorization: &str,
) -> Result<WsSocket<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>, ProtocolError> {
    connect_ws_with_headers(url, &[("Authorization", authorization)]).await
}

/// [`connect_ws`] with arbitrary extra headers (e.g. the informational
/// `x-swarmbot-protocol: 1-1`).
pub async fn connect_ws_with_headers(
    url: &str,
    headers: &[(&str, &str)],
) -> Result<WsSocket<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>, ProtocolError> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::header::{HeaderName, HeaderValue};

    let mut request = url
        .into_client_request()
        .map_err(|e| ProtocolError::ws(e.to_string()))?;
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| ProtocolError::ws(format!("bad header name `{name}`: {e}")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|e| ProtocolError::ws(format!("bad header value for `{name}`: {e}")))?;
        request.headers_mut().insert(name, value);
    }
    let (stream, response) = connect_async(request)
        .await
        .map_err(|e| ProtocolError::ws(e.to_string()))?;
    tracing::debug!(url, status = %response.status(), "control socket connected");
    Ok(WsSocket::new(stream))
}

/// Accept a WebSocket upgrade on an already-established transport (the plain
/// case: no header inspection).
pub async fn accept_ws<S>(stream: S) -> Result<WsSocket<WebSocketStream<S>>, ProtocolError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio_tungstenite::accept_async(stream)
        .await
        .map(WsSocket::new)
        .map_err(|e| ProtocolError::ws(e.to_string()))
}

/// Accept a WebSocket upgrade with a handshake callback — the place where the
/// brain reads the `Authorization` header and answers `401` without upgrading
/// when [`crate::auth::verify_upgrade_auth`] fails (§3.1). The callback type
/// is tungstenite's [`WsCallback`].
pub async fn accept_ws_hdr<S, C>(
    stream: S,
    callback: C,
) -> Result<WsSocket<WebSocketStream<S>>, ProtocolError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    C: WsCallback + Unpin,
{
    tokio_tungstenite::accept_hdr_async(stream, callback)
        .await
        .map(WsSocket::new)
        .map_err(|e| ProtocolError::ws(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{NODE_HELLO, SHELL_EXEC};
    use crate::testing::{ScriptSocket, duplex_pair};
    use crate::wire::CauseReason;
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    /// Long timers, so a test only ends when its assertions are done — the
    /// liveness test is the one place that *wants* the timeout to fire.
    fn quiet_cfg() -> PeerConfig {
        PeerConfig {
            ping_interval: Duration::from_secs(3600),
            liveness_timeout: Duration::from_secs(3600),
            ..Default::default()
        }
    }

    struct Echo {
        side: &'static str,
    }

    impl RpcHandler for Echo {
        fn handle<'a>(
            &'a self,
            method: &'a str,
            params: Value,
            _tx: Option<ChunkSink>,
        ) -> BoxFuture<'a, Result<Value, TypedError>> {
            Box::pin(
                async move { Ok(json!({ "side": self.side, "method": method, "params": params })) },
            )
        }
    }

    /// Streams five chunks for `shell.exec` (the only registered stream
    /// method), each one gated on the peer's Ack by `ChunkSink::send`.
    struct Counter;

    impl RpcHandler for Counter {
        fn handle<'a>(
            &'a self,
            method: &'a str,
            _params: Value,
            tx: Option<ChunkSink>,
        ) -> BoxFuture<'a, Result<Value, TypedError>> {
            Box::pin(async move {
                assert_eq!(method, SHELL_EXEC);
                let sink = tx.expect("stream methods get a sink");
                for i in 0..5 {
                    sink.send(json!(i))
                        .await
                        .map_err(|e| TypedError::policy_refused(method, e.to_string(), false))?;
                }
                Ok(Value::Null)
            })
        }
    }

    struct Sleeper {
        finished: Arc<AtomicBool>,
    }

    impl RpcHandler for Sleeper {
        fn handle<'a>(
            &'a self,
            _method: &'a str,
            _params: Value,
            _tx: Option<ChunkSink>,
        ) -> BoxFuture<'a, Result<Value, TypedError>> {
            let finished = Arc::clone(&self.finished);
            Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                finished.store(true, AtomicOrdering::SeqCst);
                Ok(json!("late"))
            })
        }
    }

    /// Emits one chunk, then would sleep far longer than the test allows.
    struct ChunkyThenSleep;

    impl RpcHandler for ChunkyThenSleep {
        fn handle<'a>(
            &'a self,
            _method: &'a str,
            _params: Value,
            tx: Option<ChunkSink>,
        ) -> BoxFuture<'a, Result<Value, TypedError>> {
            Box::pin(async move {
                let sink = tx.expect("stream methods get a sink");
                sink.send(json!("c0"))
                    .await
                    .map_err(|e| TypedError::policy_refused("shell.exec", e.to_string(), false))?;
                tokio::time::sleep(Duration::from_secs(30)).await;
                sink.send(json!("c1"))
                    .await
                    .map_err(|e| TypedError::policy_refused("shell.exec", e.to_string(), false))?;
                Ok(Value::Null)
            })
        }
    }

    /// The node-side hello gate: succeeds only when params say so.
    struct GateHandler;

    impl RpcHandler for GateHandler {
        fn handle<'a>(
            &'a self,
            method: &'a str,
            params: Value,
            _tx: Option<ChunkSink>,
        ) -> BoxFuture<'a, Result<Value, TypedError>> {
            Box::pin(async move {
                match method {
                    NODE_HELLO if params.get("ok") == Some(&json!(true)) => {
                        Ok(json!({ "nodeId": "nod_x", "brainVerified": true }))
                    }
                    NODE_HELLO => Err(TypedError::not_authenticated("bad brain signature")),
                    "fs.stat" => Ok(json!({ "kind": "file" })),
                    _ => Err(TypedError::policy_refused(method, "unknown method", false)),
                }
            })
        }
    }

    /// Logs every frame in both directions of one socket half.
    #[derive(Clone)]
    struct FrameLog(Arc<Mutex<Vec<(char, String)>>>);

    struct Recording<S> {
        inner: S,
        log: FrameLog,
    }

    impl<S: Socket> Socket for Recording<S> {
        async fn send_text(&mut self, text: &str) -> Result<(), ProtocolError> {
            self.log.0.lock().unwrap().push(('o', text.to_string()));
            self.inner.send_text(text).await
        }

        async fn recv_text(&mut self) -> Result<Option<String>, ProtocolError> {
            let received = self.inner.recv_text().await;
            if let Ok(Some(text)) = &received {
                self.log.0.lock().unwrap().push(('i', text.clone()));
            }
            received
        }
    }

    /// The regression test for a leaked connection: the driver used to hold a
    /// strong sender, so the command channel never emptied, `HandlesGone` was
    /// unreachable, and dropping every `Peer` parked the driver forever with the
    /// socket open — one leaked task and socket per disconnected node.
    ///
    /// Real time on purpose: the liveness timeout here is an hour away, so the
    /// only thing that can close this socket is the driver noticing that nobody
    /// is left to answer.
    #[tokio::test]
    async fn dropping_every_handle_closes_the_connection() {
        let (sock_a, sock_b) = duplex_pair(64 * 1024);
        let a = Peer::new(
            sock_a,
            PeerConfig {
                handler: Some(Arc::new(Echo { side: "a" })),
                ..quiet_cfg()
            },
        );
        let b = Peer::new(
            sock_b,
            PeerConfig {
                handler: Some(Arc::new(Echo { side: "b" })),
                ..quiet_cfg()
            },
        );
        // Prove the pair works before breaking one side on purpose.
        let echoed = a.call("from.a", json!(1)).await.expect("a round trip");
        assert_eq!(echoed["side"], json!("b"));

        drop(a);

        // b's next frame is what a's driver would have to answer; with a weak
        // sender it closes instead of parking, and b learns the peer is gone.
        let _ = b.notify("are you there", json!(null)).await;
        tokio::time::timeout(Duration::from_secs(5), b.closed())
            .await
            .expect(
                "b still thinks the connection is open after every handle of a was dropped \
                 (the driver must not hold a strong command sender)",
            );
        assert!(b.is_closed());
    }

    #[tokio::test(start_paused = true)]
    async fn requests_flow_in_both_directions_with_independent_ids() {
        let (sock_a, sock_b) = duplex_pair(64 * 1024);
        let a = Peer::new(
            sock_a,
            PeerConfig {
                handler: Some(Arc::new(Echo { side: "a" })),
                ..quiet_cfg()
            },
        );
        let b = Peer::new(
            sock_b,
            PeerConfig {
                handler: Some(Arc::new(Echo { side: "b" })),
                ..quiet_cfg()
            },
        );
        // Both peers allocate ids from their own space, so both first calls
        // are id 0. If the mux confused directions, the two id-0 requests
        // would cross and the answers would not match their callers.
        let (ra, rb) = tokio::join!(a.call("from.a", json!(1)), b.call("from.b", json!(2)));
        let ra = ra.unwrap();
        let rb = rb.unwrap();
        assert_eq!(ra["side"], json!("b"), "a's call must be served by b");
        assert_eq!(ra["method"], json!("from.a"));
        assert_eq!(ra["params"], json!(1));
        assert_eq!(rb["side"], json!("a"), "b's call must be served by a");
        assert_eq!(rb["method"], json!("from.b"));
        assert_eq!(rb["params"], json!(2));
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_acks_every_chunk_before_the_next_one() {
        let (server_sock, client_sock) = duplex_pair(64 * 1024);
        let log = FrameLog(Arc::new(Mutex::new(Vec::new())));
        let server = Peer::new(
            Recording {
                inner: server_sock,
                log: log.clone(),
            },
            PeerConfig {
                handler: Some(Arc::new(Counter)),
                ..quiet_cfg()
            },
        );
        let client = Peer::new(client_sock, quiet_cfg());

        let (out_tx, mut out_rx) = mpsc::channel(16);
        let call = tokio::spawn({
            let client = client.clone();
            async move { client.call_stream(SHELL_EXEC, Value::Null, out_tx).await }
        });
        let mut values = Vec::new();
        // `out` is dropped by the driver once the stream settles, so this
        // loop ends with the stream.
        while let Some(value) = out_rx.recv().await {
            values.push(value);
        }
        call.await.unwrap().unwrap();
        assert_eq!(values, (0..5).map(|i| json!(i)).collect::<Vec<_>>());

        // The server-side frame log must show strict alternation:
        // chunk out → Ack in → chunk out → … — never two chunks in flight
        // (§4.6 back-pressure), and one Ack per chunk.
        let frames: Vec<(char, Frame)> = log
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(dir, text)| (*dir, wire::parse(text).unwrap()))
            .collect();
        let mut expected = vec![(
            'i',
            Frame::Request {
                id: Id::Number(0),
                method: SHELL_EXEC.into(),
                params: Value::Null,
                headers: vec![],
            },
        )];
        for i in 0..5 {
            expected.push((
                'o',
                Frame::Chunk {
                    id: Id::Number(0),
                    result: vec![json!(i)],
                },
            ));
            expected.push((
                'i',
                Frame::Ack {
                    request_id: Id::Number(0),
                },
            ));
        }
        expected.push((
            'o',
            Frame::Response {
                id: Id::Number(0),
                result: Value::Null,
            },
        ));
        assert_eq!(frames, expected);
        drop(server);
    }

    #[tokio::test(start_paused = true)]
    async fn interrupt_cancels_a_sleeping_handler() {
        let finished = Arc::new(AtomicBool::new(false));
        let (server_sock, client_sock) = duplex_pair(64 * 1024);
        let server = Peer::new(
            server_sock,
            PeerConfig {
                handler: Some(Arc::new(Sleeper {
                    finished: finished.clone(),
                })),
                ..quiet_cfg()
            },
        );
        let client = Peer::new(client_sock, quiet_cfg());
        let start = tokio::time::Instant::now();

        let id = client.next_id();
        let call = tokio::spawn({
            let client = client.clone();
            let id = id.clone();
            async move {
                client
                    .call_with_id(id, "slow", Value::Null, Vec::new())
                    .await
            }
        });
        // Let the request land and the handler start its 60 s sleep.
        tokio::time::advance(Duration::from_secs(1)).await;
        client.interrupt(&id).await.unwrap();

        match call.await.unwrap() {
            Err(ProtocolError::Remote { error }) => {
                // §4.7: the server answers with an error frame whose data
                // contains an Interrupt reason.
                assert!(error.is_interrupted(), "{error:?}");
                assert_eq!(error.data, vec![CauseReason::Interrupt]);
            }
            other => panic!("expected an interrupted remote error, got {other:?}"),
        }
        assert!(
            !finished.load(AtomicOrdering::SeqCst),
            "the handler future must be dropped, not completed"
        );
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the call must not have waited out the handler's sleep"
        );
        drop((server, client));
    }

    #[tokio::test(start_paused = true)]
    async fn interrupt_ends_a_stream_with_an_interrupt_cause() {
        let (server_sock, client_sock) = duplex_pair(64 * 1024);
        let server = Peer::new(
            server_sock,
            PeerConfig {
                handler: Some(Arc::new(ChunkyThenSleep)),
                ..quiet_cfg()
            },
        );
        let client = Peer::new(client_sock, quiet_cfg());

        let (out_tx, mut out_rx) = mpsc::channel(8);
        let id = client.next_id();
        let call = tokio::spawn({
            let client = client.clone();
            let id = id.clone();
            async move {
                client
                    .call_stream_with_id(id, SHELL_EXEC, Value::Null, Vec::new(), out_tx)
                    .await
            }
        });
        // Receive the first chunk, then cancel the run mid-sleep.
        assert_eq!(out_rx.recv().await, Some(json!("c0")));
        client.interrupt(&id).await.unwrap();
        let result = call.await.unwrap();
        match result {
            Err(ProtocolError::Remote { error }) => assert!(error.is_interrupted(), "{error:?}"),
            other => panic!("expected an interrupted remote error, got {other:?}"),
        }
        // The consumer channel is closed once the call settled.
        assert_eq!(out_rx.recv().await, None);
        drop((server, client));
    }

    /// The defect this pins: `call_with_id` awaited a oneshot, so a peer that
    /// kept the socket alive (pings) while never answering its handler held the
    /// caller forever — the liveness timeout never fires on a live socket.
    #[tokio::test(start_paused = true)]
    async fn a_call_that_is_never_answered_fails_at_its_deadline_and_is_interrupted() {
        let (sock, mut raw) = duplex_pair(64 * 1024);
        let peer = Peer::new(sock, quiet_cfg());
        let id = peer.next_id();
        let deadline = Duration::from_secs(5);
        let call = tokio::spawn({
            let peer = peer.clone();
            let id = id.clone();
            async move {
                peer.call_with_id_deadline(
                    id,
                    "fs.read",
                    json!({ "path": "a.txt" }),
                    Vec::new(),
                    deadline,
                )
                .await
            }
        });

        // The request is on the wire; this far end is a node that never answers.
        let request = raw
            .recv_text()
            .await
            .expect("a frame")
            .expect("the request");
        assert!(matches!(
            wire::parse(&request).expect("parses"),
            Frame::Request { .. }
        ));

        // A deadline bounds the wait; it does not fail fast.
        tokio::time::advance(deadline - Duration::from_secs(1)).await;
        assert!(!call.is_finished(), "the call gave up before its deadline");

        tokio::time::advance(Duration::from_secs(2)).await;
        match call.await.expect("the call task finishes") {
            Err(ProtocolError::CallTimedOut { method, after }) => {
                assert_eq!(method, "fs.read");
                assert_eq!(after, deadline);
            }
            other => panic!("expected a deadline failure, got {other:?}"),
        }

        // §4.7: the peer is *asked to stop*, not merely abandoned.
        let interrupt = raw
            .recv_text()
            .await
            .expect("a frame")
            .expect("the interrupt");
        match wire::parse(&interrupt).expect("parses") {
            Frame::Interrupt { request_id } => assert_eq!(request_id, id),
            other => panic!("expected an Interrupt frame, got {other:?}"),
        }
        drop(peer);
    }

    /// A late answer must not panic the driver or poison the connection.
    ///
    /// What this can *observe* is limited, and the name says so rather than
    /// promising the stronger claim: a unary caller already holds its
    /// `CallTimedOut` and its oneshot receiver is gone, so "the answer was
    /// delivered anyway" has no API to show up in. What is checked here is the
    /// half that is visible — the frame is ignored without killing the socket,
    /// and the next call on the same connection succeeds. That the pending entry
    /// is actually removed is pinned where it *is* observable:
    /// `a_stream_that_stops_answering_fails_at_its_deadline`, where a leaked
    /// entry would keep the chunk sink alive and deliver a late chunk.
    #[tokio::test(start_paused = true)]
    async fn a_late_answer_after_a_timeout_does_not_panic_or_break_the_connection() {
        let (sock, mut raw) = duplex_pair(64 * 1024);
        let peer = Peer::new(sock, quiet_cfg());
        let id = peer.next_id();
        let call = tokio::spawn({
            let peer = peer.clone();
            let id = id.clone();
            async move {
                peer.call_with_id_deadline(
                    id,
                    "fs.read",
                    json!({ "path": "a.txt" }),
                    Vec::new(),
                    Duration::from_secs(5),
                )
                .await
            }
        });
        raw.recv_text()
            .await
            .expect("a frame")
            .expect("the request");
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(matches!(
            call.await.expect("the call task finishes"),
            Err(ProtocolError::CallTimedOut { .. })
        ));
        // Reading the interrupt proves the driver processed the withdrawal, so
        // the answer below really is a late answer to a settled id.
        let interrupt = raw
            .recv_text()
            .await
            .expect("a frame")
            .expect("the interrupt");
        assert!(matches!(
            wire::parse(&interrupt).expect("parses"),
            Frame::Interrupt { .. }
        ));

        // The node answers the withdrawn id anyway.
        raw.send_text(&wire::render(&Frame::Response {
            id: id.clone(),
            result: json!("late"),
        }))
        .await
        .expect("the late answer is written");

        // The connection still works: a fresh call is answered normally. If the
        // late frame had settled (or panicked) the driver, this would fail.
        let second = peer.next_id();
        let later = tokio::spawn({
            let peer = peer.clone();
            let id = second.clone();
            async move {
                peer.call_with_id_deadline(
                    id,
                    "fs.stat",
                    json!({ "path": "b.txt" }),
                    Vec::new(),
                    Duration::from_secs(5),
                )
                .await
            }
        });
        let request = raw
            .recv_text()
            .await
            .expect("a frame")
            .expect("the second request");
        match wire::parse(&request).expect("parses") {
            Frame::Request { id: seen, .. } => assert_eq!(seen, second),
            other => panic!("expected the second request, got {other:?}"),
        }
        raw.send_text(&wire::render(&Frame::Response {
            id: second,
            result: json!({ "ok": true }),
        }))
        .await
        .expect("the answer is written");
        let answered = later
            .await
            .expect("the second call task finishes")
            .expect("a call that answers in time still succeeds");
        assert_eq!(answered["ok"], json!(true));
        assert!(!peer.is_closed(), "a late answer must not kill the socket");
        drop(peer);
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_answered_before_its_deadline_still_succeeds() {
        let (a, b) = duplex_pair(64 * 1024);
        let peer = Peer::new(a, quiet_cfg());
        let far = Peer::new(
            b,
            PeerConfig {
                handler: Some(Arc::new(Echo { side: "far" })),
                ..quiet_cfg()
            },
        );
        let id = peer.next_id();
        let value = peer
            .call_with_id_deadline(
                id,
                "fs.stat",
                json!({ "path": "a.txt" }),
                Vec::new(),
                Duration::from_secs(5),
            )
            .await
            .expect("answered well inside its deadline");
        assert_eq!(value["side"], json!("far"));
        assert_eq!(value["method"], json!("fs.stat"));
        drop(far);
    }

    /// The deadline covers the whole stream, and withdrawing it releases the
    /// chunk sink — otherwise a collector would wait for a stream that never
    /// ends, which is the same hang in a different place.
    #[tokio::test(start_paused = true)]
    async fn a_stream_that_stops_answering_fails_at_its_deadline() {
        let (sock, mut raw) = duplex_pair(64 * 1024);
        let peer = Peer::new(sock, quiet_cfg());
        let (tx, mut rx) = mpsc::channel(8);
        let id = peer.next_id();
        let call = tokio::spawn({
            let peer = peer.clone();
            let id = id.clone();
            async move {
                peer.call_stream_with_id_deadline(
                    id,
                    SHELL_EXEC,
                    Value::Null,
                    Vec::new(),
                    tx,
                    Duration::from_secs(5),
                )
                .await
            }
        });
        let request = raw
            .recv_text()
            .await
            .expect("a frame")
            .expect("the request");
        assert!(matches!(
            wire::parse(&request).expect("parses"),
            Frame::Request { .. }
        ));

        // One chunk, Acknowledged, and then the node goes quiet mid-stream.
        raw.send_text(&wire::render(&Frame::Chunk {
            id: id.clone(),
            result: vec![json!("c0")],
        }))
        .await
        .expect("the chunk is written");
        assert_eq!(rx.recv().await, Some(json!("c0")));
        let ack = raw.recv_text().await.expect("a frame").expect("the ack");
        assert!(matches!(
            wire::parse(&ack).expect("parses"),
            Frame::Ack { .. }
        ));

        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(matches!(
            call.await.expect("the call task finishes"),
            Err(ProtocolError::CallTimedOut { .. })
        ));

        // The node keeps streaming into the withdrawn call. Nothing of it may
        // reach the collector: with the pending entry left in the table the
        // driver still holds the sink, forwards this chunk, and `rx` yields it
        // — a call the caller has given up on, resurrected with data. The bound
        // is well under this test's liveness timeout, so a chunk can only arrive
        // through the withdrawal's removal, never through the socket dying.
        raw.send_text(&wire::render(&Frame::Chunk {
            id: id.clone(),
            result: vec![json!("late")],
        }))
        .await
        .expect("the late chunk is written");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), rx.recv()).await,
            Ok(None),
            "a late chunk must not reach a withdrawn stream (and withdrawing it must drop its sink)"
        );
        let interrupt = raw
            .recv_text()
            .await
            .expect("a frame")
            .expect("the interrupt");
        assert!(matches!(
            wire::parse(&interrupt).expect("parses"),
            Frame::Interrupt { .. }
        ));
        drop(peer);
    }

    #[tokio::test(start_paused = true)]
    async fn ping_is_answered_with_pong() {
        let socket = ScriptSocket::scripted([wire::render(&Frame::Ping)]);
        let log = socket.sent_handle();
        let peer = Peer::new(socket, quiet_cfg());
        // Bounded wait for the driver to write the Pong (§4.9).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let frames = loop {
            let frames = log.frames();
            if !frames.is_empty() {
                break frames;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no Pong was ever sent"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(wire::parse(&frames[0]).unwrap(), Frame::Pong);
        drop(peer);
    }

    #[tokio::test(start_paused = true)]
    async fn the_ping_interval_emits_application_pings() {
        let socket = ScriptSocket::scripted(Vec::<String>::new());
        let log = socket.sent_handle();
        let config = PeerConfig {
            ping_interval: Duration::from_millis(50),
            liveness_timeout: Duration::from_secs(3600),
            ..Default::default()
        };
        let peer = Peer::new(socket, config);
        // With paused time, sleeping lets the runtime run the interval.
        tokio::time::sleep(Duration::from_millis(130)).await;
        let pings = log
            .frames()
            .iter()
            .filter(|text| *text == &wire::render(&Frame::Ping))
            .count();
        assert!(pings >= 2, "expected periodic pings, got {pings}");
        drop(peer);
    }

    #[tokio::test(start_paused = true)]
    async fn a_gated_peer_refuses_everything_until_hello_succeeds() {
        let (node_sock, mut raw) = duplex_pair(64 * 1024);
        let node = Peer::new(
            node_sock,
            PeerConfig {
                require_hello: true,
                handler: Some(Arc::new(GateHandler)),
                ..quiet_cfg()
            },
        );

        async fn rpc(
            raw: &mut crate::testing::LineSocket<tokio::io::DuplexStream>,
            frame: &Frame,
        ) -> Frame {
            raw.send_text(&wire::render(frame)).await.unwrap();
            let text = raw.recv_text().await.unwrap().expect("a frame");
            wire::parse(&text).unwrap()
        }

        // 1. Before hello: NotAuthenticated (§3.2), whatever the method.
        let denied = rpc(
            &mut raw,
            &Frame::Request {
                id: Id::Number(0),
                method: "fs.stat".into(),
                params: json!({ "path": "/x" }),
                headers: vec![],
            },
        )
        .await;
        match denied {
            Frame::Error { id, error } => {
                assert_eq!(id, Id::Number(0));
                assert!(matches!(
                    error.first_fail(),
                    Some(TypedError::NotAuthenticated { .. })
                ));
            }
            other => panic!("expected NotAuthenticated, got {other:?}"),
        }

        // 2. A failing node.hello keeps the gate closed.
        let failed_hello = rpc(
            &mut raw,
            &Frame::Request {
                id: Id::Number(1),
                method: NODE_HELLO.into(),
                params: json!({ "ok": false }),
                headers: vec![],
            },
        )
        .await;
        match failed_hello {
            Frame::Error { id, error } => {
                assert_eq!(id, Id::Number(1));
                assert!(matches!(
                    error.first_fail(),
                    Some(TypedError::NotAuthenticated { .. })
                ));
            }
            other => panic!("expected the handler's NotAuthenticated, got {other:?}"),
        }
        let still_denied = rpc(
            &mut raw,
            &Frame::Request {
                id: Id::Number(2),
                method: "fs.stat".into(),
                params: Value::Null,
                headers: vec![],
            },
        )
        .await;
        assert!(matches!(
            still_denied,
            Frame::Error { ref error, .. } if matches!(error.first_fail(), Some(TypedError::NotAuthenticated { .. }))
        ));

        // 3. A successful node.hello lifts the gate for good.
        let hello = rpc(
            &mut raw,
            &Frame::Request {
                id: Id::Number(3),
                method: NODE_HELLO.into(),
                params: json!({ "ok": true }),
                headers: vec![],
            },
        )
        .await;
        assert_eq!(
            hello,
            Frame::Response {
                id: Id::Number(3),
                result: json!({ "nodeId": "nod_x", "brainVerified": true }),
            }
        );
        let stat = rpc(
            &mut raw,
            &Frame::Request {
                id: Id::Number(4),
                method: "fs.stat".into(),
                params: Value::Null,
                headers: vec![],
            },
        )
        .await;
        assert_eq!(
            stat,
            Frame::Response {
                id: Id::Number(4),
                result: json!({ "kind": "file" }),
            }
        );

        // 4. Pings are answered even while gated (liveness is not auth).
        drop(node);
    }

    #[tokio::test(start_paused = true)]
    async fn liveness_timeout_closes_a_silent_socket() {
        let (sock, silent) = duplex_pair(1024);
        let peer = Peer::new(
            sock,
            PeerConfig {
                ping_interval: Duration::from_secs(60),
                liveness_timeout: Duration::from_secs(20),
                ..Default::default()
            },
        );
        let start = tokio::time::Instant::now();
        let caller = tokio::spawn({
            let peer = peer.clone();
            async move { peer.call("any", Value::Null).await }
        });
        // The other end never writes a single frame: after 20 s of silence
        // the driver must close the socket (§3.3) and fail the pending call.
        let error = caller.await.unwrap().unwrap_err();
        assert!(
            matches!(error, ProtocolError::LivenessTimeout),
            "expected LivenessTimeout, got {error:?}"
        );
        peer.closed().await;
        assert!(peer.is_closed());
        assert!(start.elapsed() >= Duration::from_secs(20));
        drop(silent);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_socket_fails_pending_calls() {
        let (sock, mut raw) = duplex_pair(4096);
        let peer = Peer::new(sock, quiet_cfg());
        let caller = tokio::spawn({
            let peer = peer.clone();
            async move { peer.call("never.answered", Value::Null).await }
        });
        // Let the request go out, then hang up: in-flight calls must fail
        // instead of hanging (§3.3).
        raw.recv_text().await.unwrap().expect("the request frame");
        drop(raw);
        let error = caller.await.unwrap().unwrap_err();
        assert!(
            matches!(error, ProtocolError::SocketClosed),
            "expected SocketClosed, got {error:?}"
        );
        peer.closed().await;
    }

    #[tokio::test(start_paused = true)]
    async fn requests_without_a_handler_get_a_defect_and_serve_fixes_it() {
        let (sock, mut raw) = duplex_pair(4096);
        let peer = Peer::new(sock, quiet_cfg());

        async fn rpc(
            raw: &mut crate::testing::LineSocket<tokio::io::DuplexStream>,
            frame: &Frame,
        ) -> Frame {
            raw.send_text(&wire::render(frame)).await.unwrap();
            let text = raw.recv_text().await.unwrap().expect("a frame");
            wire::parse(&text).unwrap()
        }

        let denied = rpc(
            &mut raw,
            &Frame::Request {
                id: Id::Number(0),
                method: "x".into(),
                params: Value::Null,
                headers: vec![],
            },
        )
        .await;
        match denied {
            // No handler is a defect on our side, never a silent drop.
            Frame::Error { error, .. } => assert!(
                matches!(error.data.first(), Some(CauseReason::Die(_))),
                "{error:?}"
            ),
            other => panic!("expected a defect, got {other:?}"),
        }

        // serve() reaches the running driver.
        peer.serve(Arc::new(Echo { side: "late" }));
        let answered = rpc(
            &mut raw,
            &Frame::Request {
                id: Id::Number(1),
                method: "x".into(),
                params: Value::Null,
                headers: vec![],
            },
        )
        .await;
        match answered {
            Frame::Response { id, result } => {
                assert_eq!(id, Id::Number(1));
                assert_eq!(result["side"], json!("late"));
            }
            other => panic!("expected a response, got {other:?}"),
        }
        drop(peer);
    }

    #[tokio::test(start_paused = true)]
    async fn notify_sends_a_request_and_ignores_the_answer() {
        // The scripted peer answers id 0 immediately; a notify must not care.
        let socket = ScriptSocket::scripted([wire::render(&Frame::Response {
            id: Id::Number(0),
            result: json!("ignored"),
        })]);
        let log = socket.sent_handle();
        let peer = Peer::new(socket, quiet_cfg());
        peer.notify("node.event", json!({ "type": "audit" }))
            .await
            .unwrap();
        // One beat for the driver to flush the queued frame.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let frames = log.frames();
        assert_eq!(frames.len(), 1, "{frames:?}");
        match wire::parse(&frames[0]).unwrap() {
            Frame::Request {
                id,
                method,
                params,
                headers,
            } => {
                assert_eq!(id, Id::Number(0));
                assert_eq!(method, "node.event");
                assert_eq!(params, json!({ "type": "audit" }));
                assert!(headers.is_empty());
            }
            other => panic!("expected a request frame, got {other:?}"),
        }
        drop(peer);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unparseable_frame_does_not_kill_the_connection() {
        let (sock, mut raw) = duplex_pair(4096);
        let peer = Peer::new(
            sock,
            PeerConfig {
                handler: Some(Arc::new(Echo { side: "a" })),
                ..quiet_cfg()
            },
        );
        // Garbage first (logged and ignored: it has no id to answer under),
        // then a valid request that must still be served.
        raw.send_text("this is not json").await.unwrap();
        raw.send_text("[{\"jsonrpc\":\"2.0\"}]").await.unwrap();
        raw.send_text(&wire::render(&Frame::Request {
            id: Id::Number(0),
            method: "x".into(),
            params: Value::Null,
            headers: vec![],
        }))
        .await
        .unwrap();
        let text = raw.recv_text().await.unwrap().expect("a frame");
        match wire::parse(&text).unwrap() {
            Frame::Response { id, result } => {
                assert_eq!(id, Id::Number(0));
                assert_eq!(result["side"], json!("a"));
            }
            other => panic!("expected a response, got {other:?}"),
        }
        drop(peer);
    }

    #[test]
    fn cancellation_tokens_fire_once_and_for_all_waiters() {
        // A sync smoke test of the token semantics the mux relies on.
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        let clone = token.clone();
        token.cancel();
        assert!(clone.is_cancelled());
        clone.cancel(); // idempotent
        assert!(token.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_wakes_waiters() {
        let token = CancellationToken::new();
        let waiter = tokio::spawn({
            let token = token.clone();
            async move { token.cancelled().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        token.cancel();
        waiter.await.unwrap();
        // An already-cancelled token resolves immediately, even for a waiter
        // that arrives later.
        let pre = CancellationToken::new();
        pre.cancel();
        pre.cancelled().await;
        token.cancelled().await;
    }
}
