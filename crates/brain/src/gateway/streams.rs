//! The brain's half of a data stream (§8): mint a grant, wait for the node to
//! dial it, and check what arrived against what the node claimed.
//!
//! The control socket carries the *request*; this module carries the *bytes*.
//! A transfer is three steps in two places, and every step can fail on its own:
//!
//! 1. **open** — the brain mints a token, registers it with its kind, and keeps
//!    a ticket that resolves when bytes arrive. This happens *before* the RPC is
//!    sent, because a node on a fast link can dial before the brain has recorded
//!    what it is waiting for; an unregistered dial is refused, not queued.
//! 2. **dial** — the node opens `ws://<brain>/node/stream/<id>` with the token.
//!    `take_dial` consumes the token (single use) and hands back the kind the
//!    brain granted, so the node cannot choose its own framing.
//! 3. **complete** — the stream ends with `Done { sha256, size }`, or it does
//!    not end at all. Either way the ticket resolves with a `Result`, and the
//!    caller compares the digest with the one the node put in its RPC answer.
//!
//! Two of these three are the point of the design:
//!
//! * **A truncated stream is a failure, not a short file.** A socket that
//!   closes before `Done` resolves the ticket with an error naming how much
//!   arrived. Assembling whatever turned up would hand the owner a file that
//!   looks complete.
//! * **The claim and the payload are checked against each other.** The RPC's
//!   `FsDownloadResult` says what the node *thinks* it sent; the frames say what
//!   actually arrived. A node that lies, or a link that drops a frame, is a
//!   detected failure rather than a plausible-looking file.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::oneshot;
use tracing::{debug, warn};

use swarmbot_node_protocol::streams::{StreamGrant, StreamKind};
use swarmbot_node_protocol::wire::TypedError;
use swarmbot_node_protocol::{StreamBroker, StreamError, StreamFrame, chunk_sha256, mint_stream};

/// How long a minted grant waits for its dial before the ticket gives up.
///
/// The token itself expires in 30 s; this is the same budget plus the time a
/// whole transfer may legitimately take, measured from the *dial* rather than
/// from minting. A node that never dials must not leave a caller waiting on a
/// oneshot until the process ends.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(45);

/// How long a dialed stream may go without a frame before it is abandoned.
///
/// Larger than a slow disk needs and smaller than forever: a stalled transfer
/// holds memory, and the honest answer is an error naming the stall.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The largest transfer this build will assemble in memory (64 MiB).
///
/// A cap is the honest choice until the receiving side spills to a file: the
/// alternative is an unbounded allocation driven by the far end. [`crate::error`]
/// carries the refusal, and the node can still send anything smaller.
pub const MAX_TRANSFER_BYTES: u64 = 64 * 1024 * 1024;

/// A stream's bytes, as they arrived and as they were promised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    pub bytes: Vec<u8>,
    /// Hex SHA-256 of `bytes`, recomputed here — never copied from the `Done`
    /// frame without checking it first.
    pub sha256: String,
}

impl Received {
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// What a caller holds between "mint" and "the bytes are here".
///
/// Deliberately not `Clone`: a ticket is one wait, and the token inside it is a
/// secret that should not be duplicated by accident.
#[derive(Debug)]
pub struct StreamTicket {
    pub stream_id: String,
    /// The bearer the node must present. Handed to the node inside the RPC
    /// payload and nowhere else — never logged, never rendered.
    pub token: String,
    pub kind: StreamKind,
    rx: oneshot::Receiver<Result<Received, String>>,
}

impl StreamTicket {
    pub fn stream_id(&self) -> &str {
        &self.stream_id
    }
}

/// The brain's stream registry: grants awaiting a dial, and the tickets waiting
/// for bytes.
#[derive(Debug, Default)]
pub struct StreamHub {
    broker: StreamBroker,
    pending: Mutex<HashMap<String, Pending>>,
}

#[derive(Debug)]
struct Pending {
    kind: StreamKind,
    expires_at: DateTime<Utc>,
    /// `None` once the stream has been resolved, so a second resolution cannot
    /// overwrite the first answer.
    sink: Option<oneshot::Sender<Result<Received, String>>>,
    /// Lives **here**, not in the socket task, because the frames of one stream
    /// arrive on one socket but the rules that validate them must outlive a
    /// re-dial: an assembler rebuilt per frame would accept chunk 5 as if it
    /// were chunk 0.
    assembler: StreamAssembler,
}

impl StreamHub {
    pub fn new() -> StreamHub {
        StreamHub::default()
    }

    /// Mint a grant for a node to dial, and wait for it.
    ///
    /// The ticket is created **before** the RPC that tells the node to dial, so a
    /// dial that arrives immediately still finds a registered id. Reversing that
    /// order is a race that fails on fast links and never on a developer's
    /// machine.
    pub fn open(&self, kind: StreamKind) -> Result<StreamTicket, StreamError> {
        let grant: StreamGrant = mint_stream()?;
        self.broker.register(&grant, kind);
        let (tx, rx) = oneshot::channel();
        let mut pending = self.pending();
        // Drop anything whose wait has aged out while we are here; the map is
        // small, and a stale entry is a ticket nobody can observe.
        let now = Utc::now();
        pending.retain(|_, entry| entry.expires_at > now);
        pending.insert(
            grant.stream_id.clone(),
            Pending {
                kind,
                expires_at: grant.expires_at + DIAL_TIMEOUT,
                sink: Some(tx),
                assembler: StreamAssembler::default(),
            },
        );
        Ok(StreamTicket {
            stream_id: grant.stream_id,
            token: grant.token,
            kind,
            rx,
        })
    }

    /// Validate a dial's bearer and consume it. Returns the kind the brain
    /// granted — not anything the dialer said.
    pub fn take_dial(&self, stream_id: &str, token: &str) -> Result<StreamKind, TypedError> {
        let kind = self.broker.take(stream_id, token)?;
        // Keep the pending entry: the socket still has to assemble the bytes and
        // only that socket may resolve the ticket.
        debug!(stream = %stream_id, kind = kind.as_str(), "a data stream was dialed");
        Ok(kind)
    }

    /// Resolve a ticket with what arrived. Called once by the socket task.
    ///
    /// A second call (a retried dial, a socket that ended after another already
    /// resolved it) finds no sink and does nothing: the first resolution wins,
    /// and the loser is logged rather than silently overwriting the answer.
    pub fn complete(&self, stream_id: &str, outcome: Result<Received, String>) {
        // Take the entry out, not just its sink: a resolved stream is over, and
        // leaving the row behind would make `len()` (and the memory it counts)
        // claim a stream is still in flight until its deadline passed.
        let entry = self.pending().remove(stream_id);
        match entry {
            Some(Pending { sink: Some(tx), .. }) => {
                // The caller may have stopped waiting (a timeout, a dropped
                // request); that is not an error worth failing over.
                let _ = tx.send(outcome);
            }
            Some(Pending { sink: None, .. }) | None => debug!(
                stream = %stream_id,
                "a data stream resolved twice; the first answer stands"
            ),
        }
    }

    /// Resolve a ticket that will never be answered, and forget the grant: the
    /// RPC that promised the stream failed, so nobody will dial.
    ///
    /// Without this the node could still dial a stream whose caller has gone,
    /// and the ticket would sit in the map until it aged out.
    pub fn abandon(&self, ticket: &StreamTicket, reason: impl Into<String>) {
        let reason = reason.into();
        self.broker.forget(&ticket.stream_id);
        self.complete(&ticket.stream_id, Err(reason));
    }

    /// Wait for a ticket's bytes, with the dial and idle budgets enforced.
    ///
    /// The timeout is the whole wait, not a per-frame gap: the socket task owns
    /// the per-frame idle check, and a caller that never sees an answer must not
    /// hang a request thread.
    pub async fn receive(&self, ticket: StreamTicket) -> Result<Received, String> {
        match tokio::time::timeout(DIAL_TIMEOUT + IDLE_TIMEOUT, ticket.rx).await {
            Ok(Ok(Ok(received))) => Ok(received),
            Ok(Ok(Err(reason))) => Err(reason),
            Ok(Err(_)) => Err("the stream ended without an answer".to_string()),
            Err(_) => Err(format!(
                "no data arrived within {:?}; the node never completed the stream",
                DIAL_TIMEOUT + IDLE_TIMEOUT
            )),
        }
    }

    /// Feed one decoded frame to the stream's assembler, and say what the socket
    /// task should do next.
    ///
    /// The assembler lives in the pending entry, so the rules that span frames
    /// (chunk order, the running total, the digest at the end) actually span
    /// frames. This is the seam the socket task and the tests share: both call
    /// it with frames and get an [`Assembly`] back, so the socket task has no
    /// validation logic of its own to drift.
    pub fn assemble(&self, stream_id: &str, frame: StreamFrame) -> Assembly {
        let mut pending = self.pending();
        let Some(entry) = pending.get_mut(stream_id) else {
            return Assembly::fail(format!(
                "no stream `{stream_id}` is waiting for data (it was never opened, or it already finished)"
            ));
        };
        let kind = entry.kind;
        let outcome = entry.assembler.feed(kind, frame);
        match outcome {
            // Nothing to say yet: no ack boundary crossed.
            None => Assembly::Continue,
            Some(assembly) => assembly,
        }
    }

    pub fn len(&self) -> usize {
        let mut pending = self.pending();
        let now = Utc::now();
        pending.retain(|_, entry| entry.expires_at > now);
        pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Live grants awaiting a dial (the broker prunes expired ones itself).
    pub fn grants(&self) -> usize {
        self.broker.len()
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Accumulates one stream's bytes and enforces the framing rules.
#[derive(Debug, Default)]
pub struct StreamAssembler {
    bytes: Vec<u8>,
    next_seq: u64,
    acked: u64,
    failed: bool,
}

/// What the socket task should do after feeding one frame.
#[derive(Debug, PartialEq, Eq)]
pub enum Assembly {
    /// Keep reading.
    Continue,
    /// Send this ack, then keep reading.
    Ack(u64),
    /// The stream finished: send nothing more and resolve the ticket.
    Finished(Result<Received, String>),
    /// The stream broke: send this `Error` frame if the socket is still open,
    /// then close and resolve the ticket.
    Failed(String),
}

impl Assembly {
    fn fail(reason: String) -> Assembly {
        Assembly::Failed(reason)
    }

    /// Whether this outcome ends the stream.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Assembly::Finished(_) | Assembly::Failed(_))
    }
}

impl StreamAssembler {
    /// Take one frame. `None` means "keep going"; `Some(..)` ends the stream.
    ///
    /// Every rule this enforces is a way a transfer goes wrong quietly if it is
    /// not enforced: a reordered chunk reassembles a plausible but wrong file, a
    /// skipped chunk shortens it, and a `Done` that is believed without a digest
    /// check cannot tell either from a success.
    pub fn feed(&mut self, kind: StreamKind, frame: StreamFrame) -> Option<Assembly> {
        if self.failed {
            return Some(Assembly::Failed("the stream already failed".to_string()));
        }
        match (kind, frame) {
            (StreamKind::Transfer, frame @ StreamFrame::Data { seq, .. }) => {
                if seq != self.next_seq {
                    return self.fail(format!(
                        "expected chunk {expected}, got {seq}: the stream is out of order",
                        expected = self.next_seq
                    ));
                }
                let chunk = match frame.chunk() {
                    Ok(chunk) => chunk,
                    Err(error) => return self.fail(error.to_string()),
                };
                if self.bytes.len() as u64 + chunk.len() as u64 > MAX_TRANSFER_BYTES {
                    return self.fail(format!(
                        "the transfer passed the {MAX_TRANSFER_BYTES}-byte limit this build assembles"
                    ));
                }
                self.bytes.extend_from_slice(&chunk);
                self.next_seq += 1;
                let total = self.bytes.len() as u64;
                if total - self.acked >= swarmbot_node_protocol::ACK_EVERY_BYTES {
                    self.acked = total;
                    return Some(Assembly::Ack(total));
                }
                None
            }
            (StreamKind::Transfer, StreamFrame::Done { sha256, size }) => {
                let actual = chunk_sha256(&self.bytes);
                if size != self.bytes.len() as u64 {
                    return self.fail(format!(
                        "the node sent {size} bytes but {} arrived",
                        self.bytes.len()
                    ));
                }
                if !sha256.eq_ignore_ascii_case(&actual) {
                    return self.fail(format!(
                        "the digest does not match: the node said {sha256}, the bytes hash to {actual}"
                    ));
                }
                Some(Assembly::Finished(Ok(Received {
                    bytes: std::mem::take(&mut self.bytes),
                    sha256: actual,
                })))
            }
            (StreamKind::Transfer, StreamFrame::Error { code, reason }) => {
                self.failed = true;
                Some(Assembly::Finished(Err(format!("{code}: {reason}"))))
            }
            (kind, other) => self.fail(format!(
                "a `{}` stream cannot carry a `{}` frame",
                kind.as_str(),
                other.tag()
            )),
        }
    }

    fn fail(&mut self, reason: String) -> Option<Assembly> {
        self.failed = true;
        warn!(reason = %reason, "a data stream was refused");
        Some(Assembly::Failed(reason))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assembler() -> StreamAssembler {
        StreamAssembler::default()
    }

    #[test]
    fn a_transfer_assembles_in_order_and_verifies_its_digest() {
        let content = b"the node's bytes";
        let mut state = assembler();
        for (seq, chunk) in content.chunks(3).enumerate() {
            let frame = StreamFrame::data(seq as u64, chunk).expect("a small chunk");
            assert!(
                state.feed(StreamKind::Transfer, frame).is_none(),
                "no outcome until Done"
            );
        }
        let done = StreamFrame::done(chunk_sha256(content), content.len() as u64);
        match state
            .feed(StreamKind::Transfer, done)
            .expect("Done resolves the stream")
        {
            Assembly::Finished(Ok(received)) => {
                assert_eq!(received.bytes, content);
                assert_eq!(received.sha256, chunk_sha256(content));
            }
            other => panic!("expected a finished stream, got {other:?}"),
        }
    }

    #[test]
    fn an_out_of_order_chunk_is_a_detected_failure() {
        // Two chunks swapped reassemble into a file that is the right length and
        // the wrong content: the digest would catch it, but only if the order was
        // checked first, and the error is much more useful here.
        let mut state = assembler();
        assert!(
            state
                .feed(
                    StreamKind::Transfer,
                    StreamFrame::data(0, b"first").unwrap()
                )
                .is_none()
        );
        let outcome = state
            .feed(
                StreamKind::Transfer,
                StreamFrame::data(2, b"third").unwrap(),
            )
            .expect("a skipped chunk ends the stream");
        match outcome {
            Assembly::Failed(reason) => {
                assert!(reason.contains("expected chunk 1"), "{reason}");
                assert!(reason.contains("got 2"), "{reason}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_done_that_disagrees_with_what_arrived_is_refused() {
        let mut state = assembler();
        state.feed(StreamKind::Transfer, StreamFrame::data(0, b"abc").unwrap());
        // The size the node claims is wrong…
        let outcome = state
            .feed(
                StreamKind::Transfer,
                StreamFrame::done(chunk_sha256(b"abc"), 99),
            )
            .expect("a mismatched size ends the stream");
        assert!(
            matches!(outcome, Assembly::Failed(ref reason) if reason.contains("99 bytes")),
            "{outcome:?}"
        );

        // …and so is a digest that does not hash to its content. Both are
        // failures rather than a shorter or corrupt file.
        let mut state = assembler();
        state.feed(StreamKind::Transfer, StreamFrame::data(0, b"abc").unwrap());
        let outcome = state
            .feed(
                StreamKind::Transfer,
                StreamFrame::done(chunk_sha256(b"something else"), 3),
            )
            .expect("a mismatched digest ends the stream");
        assert!(
            matches!(outcome, Assembly::Failed(ref reason) if reason.contains("digest does not match")),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_frame_for_the_wrong_kind_is_refused() {
        // A `transfer` grant cannot carry a frame its framing does not define.
        // The kind comes from the grant, so this is not a negotiation.
        let mut state = assembler();
        let outcome = state
            .feed(StreamKind::Pty, StreamFrame::ack(10))
            .expect("an undefined frame ends the stream");
        assert!(
            matches!(outcome, Assembly::Failed(ref reason) if reason.contains("cannot carry")),
            "{outcome:?}"
        );
    }

    #[test]
    fn an_error_frame_ends_the_stream_with_its_reason() {
        let mut state = assembler();
        let outcome = state
            .feed(
                StreamKind::Transfer,
                StreamFrame::error("policy_refused", "the owner denied reading that path"),
            )
            .expect("an Error frame ends the stream");
        match outcome {
            Assembly::Finished(Err(reason)) => {
                assert!(reason.starts_with("policy_refused:"), "{reason}");
                assert!(reason.contains("denied"), "{reason}");
            }
            other => panic!("expected a finished-with-error stream, got {other:?}"),
        }
    }

    #[test]
    fn acks_are_sent_at_the_boundary_and_carry_the_running_total() {
        let chunk = vec![0u8; swarmbot_node_protocol::ACK_EVERY_BYTES as usize];
        let mut state = assembler();
        let first = state.feed(StreamKind::Transfer, StreamFrame::data(0, &chunk).unwrap());
        assert_eq!(
            first,
            Some(Assembly::Ack(swarmbot_node_protocol::ACK_EVERY_BYTES)),
            "one ack after the first MiB"
        );
        // A second MiB acks the cumulative total, not the chunk: a lost ack
        // cannot desynchronise the window.
        let second = state.feed(StreamKind::Transfer, StreamFrame::data(1, &chunk).unwrap());
        assert_eq!(
            second,
            Some(Assembly::Ack(2 * swarmbot_node_protocol::ACK_EVERY_BYTES))
        );
    }

    #[test]
    fn a_transfer_over_the_cap_is_refused_rather_than_allocated() {
        // The assembler is the last line of defence for memory: the far end
        // chooses how much it sends, so the cap lives here too.
        let mut state = assembler();
        state.bytes = vec![0u8; MAX_TRANSFER_BYTES as usize];
        let outcome = state
            .feed(
                StreamKind::Transfer,
                StreamFrame::data(0, b"one more").unwrap(),
            )
            .expect("passing the cap ends the stream");
        assert!(
            matches!(outcome, Assembly::Failed(ref reason) if reason.contains("limit")),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_ticket_resolves_once_and_a_second_dial_cannot_steal_the_answer() {
        let hub = StreamHub::new();
        let ticket = hub.open(StreamKind::Transfer).expect("a grant is minted");
        assert_eq!(hub.len(), 1);
        assert_eq!(hub.grants(), 1);
        assert_eq!(
            hub.take_dial(&ticket.stream_id, &ticket.token)
                .expect("the dial is allowed"),
            StreamKind::Transfer
        );
        // The token is single use…
        assert!(hub.take_dial(&ticket.stream_id, &ticket.token).is_err());
        // …and the ticket still belongs to the socket that took it.
        hub.complete(
            &ticket.stream_id,
            Ok(Received {
                bytes: b"x".to_vec(),
                sha256: chunk_sha256(b"x"),
            }),
        );
        // A late second resolution must not overwrite the first answer.
        hub.complete(&ticket.stream_id, Err("too late".to_string()));
        let received = hub.receive(ticket).await.expect("one answer");
        assert_eq!(received.bytes, b"x");
    }

    #[tokio::test]
    async fn abandoning_a_ticket_drops_the_grant_and_resolves_the_wait() {
        let hub = StreamHub::new();
        let ticket = hub.open(StreamKind::Transfer).expect("a grant is minted");
        let id = ticket.stream_id.clone();
        hub.abandon(&ticket, "the RPC to the node failed");
        let error = hub.receive(ticket).await.expect_err("no bytes");
        assert!(error.contains("RPC"), "{error}");
        // Nobody can dial a stream whose caller has gone.
        assert_eq!(hub.grants(), 0, "the grant was forgotten");
        assert!(hub.assemble(&id, StreamFrame::ack(1)).is_terminal());
    }

    #[test]
    fn a_frame_for_a_stream_nobody_opened_is_a_failure_not_a_panic() {
        let hub = StreamHub::new();
        let outcome = hub.assemble("000000000000000000000000", StreamFrame::ack(1));
        assert!(
            matches!(outcome, Assembly::Failed(ref reason) if reason.contains("no stream")),
            "{outcome:?}"
        );
    }
}
