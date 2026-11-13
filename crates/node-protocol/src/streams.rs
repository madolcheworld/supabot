//! Data streams (§7–§8): the tokens behind `/node/stream/:id`, and the framing
//! that rides the socket they open.
//!
//! Bytes never ride the control socket. Every data stream (a pty, a file
//! transfer) is a separate WebSocket the node dials with a **one-time** bearer
//! token that expires 30 s after minting. This module owns both halves that
//! belong on the wire:
//!
//! * **bookkeeping** — [`StreamBroker`]: mint, register, take-once, forget;
//! * **framing** — [`StreamFrame`]: what a byte chunk, an acknowledgement, a
//!   terminator and a refusal look like once the socket is open, and the caps
//!   that keep either side from buffering without bound ([`MAX_CHUNK_BYTES`],
//!   [`ACK_EVERY_BYTES`], [`MAX_IN_FLIGHT_BYTES`]).
//!
//! The endpoint that accepts the dial lives in the brain crate, and the dialer
//! lives in `crates/node` — this crate is the contract between them, so both
//! can be tested without a socket.
//!
//! Invariants:
//!
//! * a token authorizes **one** dial — `take` consumes it;
//! * a wrong token must not consume the right one (an attacker guessing
//!   tokens cannot deny the node its stream), so a mismatch leaves the entry
//!   registered until it expires;
//! * token comparison is constant-time (see [`crate::pairing::constant_time_eq`]);
//! * **a dial cannot choose its own framing.** The kind is fixed at
//!   registration by the brain ([`StreamKind`]), and `take` hands back the one
//!   that was granted — never a string the dialer supplied;
//! * **a frame is bounded before it is decoded.** A peer that announces a
//!   gigabyte chunk is refused on the length of its text, not by allocating
//!   the buffer and finding out (`StreamFrame::decode`).

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use rand::TryRngCore as _;
use serde::{Deserialize, Serialize};

use crate::pairing::constant_time_eq;
use crate::wire::TypedError;

/// Tokens expire 30 s after minting (§7) — long enough for the node to dial,
/// short enough that a leaked token is useless by the time it is found.
pub const STREAM_TOKEN_TTL: Duration = Duration::seconds(30);

/// A freshly minted stream grant, ready to hand to a node inside an RPC
/// payload (`{streamId, streamToken}`) and to [`StreamBroker::register`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamGrant {
    /// 96-bit random id, hex (§7) — appears in the URL path.
    pub stream_id: String,
    /// 128-bit random token, base64url without padding (§7) — the secret.
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

/// Typed failure of minting or framing. Every variant says which side is
/// wrong, because these messages end up in an operator's log across two
/// machines.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StreamError {
    #[error("the system RNG failed: {reason}")]
    Rng { reason: String },
    /// A chunk larger than [`MAX_CHUNK_BYTES`]. Refused when *building* the
    /// frame (the sender is wrong) and when decoding one (the peer is either
    /// buggy or hostile) — the second check is the one that matters: it runs on
    /// the text length, before any buffer is allocated.
    #[error("a stream chunk of {bytes} bytes exceeds the {max}-byte limit")]
    ChunkTooLarge { bytes: usize, max: usize },
    #[error("a stream frame is not valid JSON: {reason}")]
    NotJson { reason: String },
    #[error("a stream chunk is not base64: {reason}")]
    NotBase64 { reason: String },
    #[error("a stream frame could not be encoded: {reason}")]
    Encode { reason: String },
    #[error("`{raw}` is not a stream kind this build knows (pty, transfer)")]
    UnknownKind { raw: String },
}

/// Mint `{streamId (96-bit hex), streamToken (128-bit base64url)}` from the
/// OS CSPRNG, expiring in [`STREAM_TOKEN_TTL`]. Tokens are secrets, so a
/// failed RNG is a typed error — never a silently weak fallback.
pub fn mint_stream() -> Result<StreamGrant, StreamError> {
    let mut id_bytes = [0u8; 12]; // 96 bits
    let mut token_bytes = [0u8; 16]; // 128 bits
    let rng_error = |e: rand_core::OsError| StreamError::Rng {
        reason: e.to_string(),
    };
    rand::rngs::OsRng
        .try_fill_bytes(&mut id_bytes)
        .map_err(rng_error)?;
    rand::rngs::OsRng
        .try_fill_bytes(&mut token_bytes)
        .map_err(rng_error)?;
    Ok(StreamGrant {
        stream_id: hex::encode(id_bytes),
        token: URL_SAFE_NO_PAD.encode(token_bytes),
        expires_at: Utc::now() + STREAM_TOKEN_TTL,
    })
}

/// What a granted stream carries, and therefore how its frames are read.
///
/// The kind is fixed by the **brain** when it mints the grant, handed to the
/// node inside the RPC payload that asks for the stream, and handed back by
/// [`StreamBroker::take`] to the endpoint that accepts the dial. Nothing on the
/// wire can change it: a node that opens a `transfer` socket and sends pty
/// frames is refused at the first frame, not renegotiated into a different
/// stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    /// An interactive terminal: raw window bytes (and, later, resize events).
    /// **Not implemented** anywhere in this build — `pty.open` is still in the
    /// refused table — but the kind is named so a foreign node can be written
    /// against the same vocabulary.
    Pty,
    /// A file's bytes, in order, terminated by size and digest (§8).
    Transfer,
}

impl StreamKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pty => "pty",
            Self::Transfer => "transfer",
        }
    }

    /// Parse the string a config or an RPC payload spells. Unknown kinds are a
    /// typed error, never a default: guessing which framing a stream uses is
    /// how a transfer gets read as a terminal.
    pub fn parse(raw: &str) -> Result<Self, StreamError> {
        match raw {
            "pty" => Ok(Self::Pty),
            "transfer" => Ok(Self::Transfer),
            other => Err(StreamError::UnknownKind {
                raw: other.to_string(),
            }),
        }
    }
}

/// The largest chunk either side may put in one frame (1 MiB, §8).
///
/// Chosen so a frame is small enough to be decoded and written without a stall
/// on a slow disk, and large enough that a 100 MB transfer is ~100 frames
/// rather than 100 000.
pub const MAX_CHUNK_BYTES: usize = 1024 * 1024;

/// How much data may be un-acknowledged before the sender must wait (§8).
///
/// The receiver acks every [`ACK_EVERY_BYTES`]; the sender's window is twice
/// that, so one ack in flight never stalls a healthy stream.
pub const ACK_EVERY_BYTES: u64 = 1024 * 1024;
pub const MAX_IN_FLIGHT_BYTES: u64 = 2 * 1024 * 1024;

/// One frame of an open data stream, as it appears in a WebSocket **text**
/// frame (one JSON object per frame; the socket is text-only, the same rule the
/// control socket follows, and it is why chunks are base64 rather than binary).
///
/// `seq` counts chunks from zero per stream, so a bundled or reordered
/// transport cannot silently reassemble a file in the wrong order — the
/// receiver checks it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum StreamFrame {
    /// A chunk of the stream's bytes, base64 (standard alphabet, padded — this
    /// is not a secret, unlike a token, so the URL-safe alphabet would only add
    /// a conversion).
    Data { seq: u64, data: String },
    /// How many bytes the receiver has taken so far. Cumulative, not per-chunk,
    /// so a lost or duplicated ack cannot make the sender's window drift.
    Ack { bytes: u64 },
    /// The end of a `transfer`: how many bytes were sent and their SHA-256.
    /// The receiver verifies both before the file counts as received (§8).
    Done { sha256: String, size: u64 },
    /// A refusal from either side. Typed as a frame rather than a socket close
    /// so the reason survives: a close with 4400 tells the operator nothing.
    Error { code: String, reason: String },
}

impl StreamFrame {
    /// A `Data` frame, refusing a chunk over [`MAX_CHUNK_BYTES`] *here* — the
    /// sender learns its bug at the call site instead of at the far end.
    pub fn data(seq: u64, chunk: &[u8]) -> Result<Self, StreamError> {
        if chunk.len() > MAX_CHUNK_BYTES {
            return Err(StreamError::ChunkTooLarge {
                bytes: chunk.len(),
                max: MAX_CHUNK_BYTES,
            });
        }
        Ok(Self::Data {
            seq,
            data: BASE64_STANDARD.encode(chunk),
        })
    }

    pub fn ack(bytes: u64) -> Self {
        Self::Ack { bytes }
    }

    pub fn done(sha256: impl Into<String>, size: u64) -> Self {
        Self::Done {
            sha256: sha256.into(),
            size,
        }
    }

    /// A refusal that names what went wrong, in a sentence a log can carry.
    pub fn error(code: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Error {
            code: code.into(),
            reason: reason.into(),
        }
    }

    /// The bytes of a `Data` frame; the typed error for any other frame.
    pub fn chunk(&self) -> Result<Vec<u8>, StreamError> {
        match self {
            Self::Data { data, .. } => {
                // The limit is checked twice on purpose: a peer controls `data`
                // and would otherwise choose how much memory we allocate.
                if data.len() > MAX_CHUNK_BYTES.div_ceil(3) * 4 {
                    return Err(StreamError::ChunkTooLarge {
                        bytes: data.len(),
                        max: MAX_CHUNK_BYTES,
                    });
                }
                BASE64_STANDARD
                    .decode(data)
                    .map_err(|error| StreamError::NotBase64 {
                        reason: error.to_string(),
                    })
            }
            other => Err(StreamError::NotBase64 {
                reason: format!("a `{}` frame carries no chunk", other.tag()),
            }),
        }
    }

    /// The tag, for error messages and tests. Never a wire value on its own.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Data { .. } => "data",
            Self::Ack { .. } => "ack",
            Self::Done { .. } => "done",
            Self::Error { .. } => "error",
        }
    }

    /// The frame as the one text frame a socket carries.
    pub fn encode(&self) -> Result<String, StreamError> {
        serde_json::to_string(self).map_err(|error| StreamError::Encode {
            reason: error.to_string(),
        })
    }

    /// A received text frame, decoded — with the chunk cap enforced on the
    /// **text** before anything is allocated.
    pub fn decode(text: &str) -> Result<Self, StreamError> {
        let frame: Self = serde_json::from_str(text).map_err(|error| StreamError::NotJson {
            reason: error.to_string(),
        })?;
        if let Self::Data { data, .. } = &frame
            && data.len() > MAX_CHUNK_BYTES.div_ceil(3) * 4
        {
            return Err(StreamError::ChunkTooLarge {
                bytes: data.len(),
                max: MAX_CHUNK_BYTES,
            });
        }
        Ok(frame)
    }
}

/// The digest a `transfer` terminates with (§8): hex SHA-256, lowercase.
pub fn chunk_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// One registered stream awaiting its dial.
#[derive(Clone, Debug)]
struct Entry {
    token: String,
    kind: StreamKind,
    expires_at: DateTime<Utc>,
}

/// The set of stream grants the brain has issued but not yet seen dialed.
///
/// `Clone` is not derived on purpose: one broker per brain, shared through
/// `Arc`, so a `take` on one handle is visible to all.
#[derive(Debug, Default)]
pub struct StreamBroker {
    entries: Mutex<HashMap<String, Entry>>,
}

impl StreamBroker {
    pub fn new() -> StreamBroker {
        StreamBroker::default()
    }

    /// Register a minted grant under its stream kind.
    ///
    /// The kind is returned by [`StreamBroker::take`] so the accepting endpoint
    /// knows what framing to expect (§8) **without trusting the dialer**: the
    /// value comes from the brain that minted the grant, not from the socket.
    /// It is an enum rather than a string for that reason — a string let a
    /// registration typo turn into a runtime surprise at the far end.
    pub fn register(&self, grant: &StreamGrant, kind: StreamKind) {
        let mut entries = self.lock();
        let now = Utc::now();
        entries.retain(|_, entry| entry.expires_at > now);
        entries.insert(
            grant.stream_id.clone(),
            Entry {
                token: grant.token.clone(),
                kind,
                expires_at: grant.expires_at,
            },
        );
    }

    /// Validate a dial (`Authorization: Bearer <token>` on
    /// `/node/stream/:id`) and **consume** the token: single use (§7).
    ///
    /// Returns the registered stream kind. Unknown ids, expired grants and
    /// wrong tokens are all typed [`TypedError::StreamRefused`] — the dialer
    /// gets "no", never a hang or a panic, and cannot distinguish the three
    /// cases beyond the reason text.
    pub fn take(&self, stream_id: &str, token: &str) -> Result<StreamKind, TypedError> {
        let refused = |reason: &str| {
            Err(TypedError::stream_refused(
                stream_id.to_string(),
                reason.to_string(),
            ))
        };
        let mut entries = self.lock();
        let now = Utc::now();
        let Some(entry) = entries.get(stream_id) else {
            return refused("unknown stream id");
        };
        if entry.expires_at <= now {
            entries.remove(stream_id);
            return refused("stream token expired");
        }
        if !constant_time_eq(&entry.token, token) {
            // Deliberately does NOT remove the entry: a wrong guess must not
            // burn the legitimate token.
            return refused("wrong stream token");
        }
        // The only moment the entry can vanish between the checks and here is
        // another thread's `take` for the same id — in which case the honest
        // answer is "you lost the race", not a fabricated kind.
        match entries.remove(stream_id) {
            Some(entry) => Ok(entry.kind),
            None => refused("the stream token was already used"),
        }
    }

    /// Drop a grant without consuming it (the RPC that promised the stream
    /// failed, so nobody will dial). Returns whether one was registered.
    pub fn forget(&self, stream_id: &str) -> bool {
        self.lock().remove(stream_id).is_some()
    }

    /// Live entries (prunes expired ones first).
    pub fn len(&self) -> usize {
        let mut entries = self.lock();
        let now = Utc::now();
        entries.retain(|_, entry| entry.expires_at > now);
        entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        // A poisoned lock leaves the map consistent; recover instead of
        // taking the whole brain down (AGENTS.md: poisoned mutexes are
        // recovered with into_inner).
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_tokens_have_the_documented_shapes() {
        let grant = mint_stream().unwrap();
        // 96-bit hex id.
        assert_eq!(grant.stream_id.len(), 24);
        assert!(grant.stream_id.bytes().all(|b| b.is_ascii_hexdigit()));
        // 128-bit base64url token without padding: 22 chars.
        assert_eq!(grant.token.len(), 22);
        assert!(!grant.token.contains('='));
        assert_eq!(URL_SAFE_NO_PAD.decode(&grant.token).unwrap().len(), 16);
        // Expires 30 s out (§7), give or take scheduling.
        let ttl = grant.expires_at - Utc::now();
        assert!(ttl > Duration::seconds(25) && ttl <= STREAM_TOKEN_TTL);
        assert_ne!(grant.stream_id, mint_stream().unwrap().stream_id);
        assert_ne!(grant.token, mint_stream().unwrap().token);
    }

    #[test]
    fn a_token_is_single_use() {
        let broker = StreamBroker::new();
        let grant = mint_stream().unwrap();
        broker.register(&grant, StreamKind::Pty);
        assert_eq!(broker.len(), 1);
        assert_eq!(
            broker.take(&grant.stream_id, &grant.token).unwrap(),
            StreamKind::Pty
        );
        // The second dial with the same token is refused — and now even the
        // correct token is too, because the first take consumed the entry.
        let err = broker.take(&grant.stream_id, &grant.token).unwrap_err();
        assert_eq!(err.tag(), "StreamRefused");
        assert!(
            matches!(err, TypedError::StreamRefused { reason, .. } if reason == "unknown stream id")
        );
        assert!(broker.is_empty());
    }

    #[test]
    fn an_expired_token_is_refused() {
        let broker = StreamBroker::new();
        let grant = StreamGrant {
            expires_at: Utc::now() - Duration::seconds(1),
            ..mint_stream().unwrap()
        };
        broker.register(&grant, StreamKind::Transfer);
        let err = broker.take(&grant.stream_id, &grant.token).unwrap_err();
        assert!(
            matches!(err, TypedError::StreamRefused { reason, .. } if reason == "stream token expired")
        );
        // The expired entry is gone, not lingering.
        assert!(broker.is_empty());
    }

    #[test]
    fn a_wrong_token_is_refused_without_burning_the_right_one() {
        let broker = StreamBroker::new();
        let grant = mint_stream().unwrap();
        broker.register(&grant, StreamKind::Transfer);
        let err = broker
            .take(&grant.stream_id, "AAAAAAAAAAAAAAAAAAAAAA")
            .unwrap_err();
        assert!(
            matches!(err, TypedError::StreamRefused { reason, .. } if reason == "wrong stream token")
        );
        // The legitimate dial still works: guessing attacks must not be able
        // to deny the node its stream.
        assert_eq!(
            broker.take(&grant.stream_id, &grant.token).unwrap(),
            StreamKind::Transfer
        );
    }

    #[test]
    fn a_data_frame_round_trips_and_keeps_its_sequence() {
        let payload = b"the quick brown fox\x00\xff";
        let frame = StreamFrame::data(7, payload).unwrap();
        assert_eq!(frame.tag(), "data");
        let wire = frame.encode().unwrap();
        // One JSON object, so one WebSocket text frame carries it whole.
        assert!(wire.starts_with('{'), "{wire}");
        let back = StreamFrame::decode(&wire).unwrap();
        assert_eq!(back, frame);
        assert_eq!(back.chunk().unwrap(), payload);
        assert!(matches!(back, StreamFrame::Data { seq: 7, .. }));
    }

    #[test]
    fn a_chunk_over_the_cap_is_refused_by_both_sides() {
        // Building it: the sender learns at the call site.
        let too_big = vec![0u8; MAX_CHUNK_BYTES + 1];
        let err = StreamFrame::data(0, &too_big).unwrap_err();
        assert!(
            matches!(err, StreamError::ChunkTooLarge { max, .. } if max == MAX_CHUNK_BYTES),
            "{err}"
        );
        // Exactly the cap is fine — the limit is inclusive.
        assert!(StreamFrame::data(0, &vec![0u8; MAX_CHUNK_BYTES]).is_ok());

        // Decoding it: a peer cannot choose how much memory we allocate. The
        // check runs on the *text* length, before base64 decoding.
        let huge = "A".repeat(MAX_CHUNK_BYTES.div_ceil(3) * 4 + 4);
        let text = format!("{{\"t\":\"data\",\"seq\":0,\"data\":\"{huge}\"}}");
        let err = StreamFrame::decode(&text).unwrap_err();
        assert!(
            matches!(err, StreamError::ChunkTooLarge { .. }),
            "a hostile chunk is refused before decoding: {err}"
        );
    }

    #[test]
    fn every_frame_kind_has_a_wire_shape_a_foreign_node_can_write() {
        // These strings are the contract documented in node-protocol.md §8: a
        // node in another language has to be able to produce them by hand.
        let ack = StreamFrame::ack(2 * 1024 * 1024);
        assert_eq!(ack.encode().unwrap(), r#"{"t":"ack","bytes":2097152}"#);
        let done = StreamFrame::done("abc123", 4096);
        assert_eq!(
            done.encode().unwrap(),
            r#"{"t":"done","sha256":"abc123","size":4096}"#
        );
        let error = StreamFrame::error("policy_refused", "the owner denied it");
        assert_eq!(
            error.encode().unwrap(),
            r#"{"t":"error","code":"policy_refused","reason":"the owner denied it"}"#
        );
        // And the tag is not a wire value of its own: decoding needs `t`.
        assert!(StreamFrame::decode(r#"{"t":"nope"}"#).is_err());
    }

    #[test]
    fn a_done_frame_carries_the_digest_of_what_was_sent() {
        // The receiver's only defence against a truncated or reordered
        // transfer: the terminator names the whole content, not the last chunk.
        let content = b"hello, node";
        let digest = chunk_sha256(content);
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        let done = StreamFrame::done(digest.clone(), content.len() as u64);
        let back = StreamFrame::decode(&done.encode().unwrap()).unwrap();
        match back {
            StreamFrame::Done { sha256, size } => {
                assert_eq!(sha256, digest);
                assert_eq!(size, 11);
            }
            other => panic!("expected a done frame, got {other:?}"),
        }
        // A digest of different bytes differs — the assertion the receiver
        // makes, stated once here so the framing cannot drift from sha2.
        assert_ne!(chunk_sha256(b"hello, nodules"), digest);
    }

    #[test]
    fn a_non_data_frame_has_no_chunk_and_says_so() {
        let err = StreamFrame::ack(10).chunk().unwrap_err();
        assert!(
            matches!(err, StreamError::NotBase64 { ref reason } if reason.contains("ack")),
            "{err}"
        );
    }

    #[test]
    fn malformed_text_is_a_typed_error_not_a_panic() {
        let err = StreamFrame::decode("not json").unwrap_err();
        assert!(matches!(err, StreamError::NotJson { .. }), "{err}");
        // A frame with a known tag and a wrong field type is still JSON, so it
        // fails at serde rather than at the tag — either way, typed.
        assert!(StreamFrame::decode(r#"{"t":"ack","bytes":"lots"}"#).is_err());
    }

    #[test]
    fn a_stream_kind_survives_the_round_trip_and_an_unknown_one_is_refused() {
        assert_eq!(StreamKind::parse("pty").unwrap(), StreamKind::Pty);
        assert_eq!(StreamKind::parse("transfer").unwrap(), StreamKind::Transfer);
        assert_eq!(StreamKind::Transfer.as_str(), "transfer");
        assert_eq!(StreamKind::Pty.as_str(), "pty");
        // An unknown kind is a refusal, never a default: guessing the framing
        // is how a transfer gets read as a terminal.
        let err = StreamKind::parse("tcp").unwrap_err();
        assert!(
            matches!(err, StreamError::UnknownKind { ref raw } if raw == "tcp"),
            "{err}"
        );
        // The serde spelling is the same string, so an RPC payload cannot
        // disagree with `parse` about what a kind is called.
        assert_eq!(
            serde_json::to_string(&StreamKind::Transfer).unwrap(),
            "\"transfer\""
        );
    }

    #[test]
    fn the_granted_kind_is_the_one_the_endpoint_reads() {
        // The whole point of the typed kind: the dialer cannot pick its own
        // framing. `take` returns what `register` was given.
        let broker = StreamBroker::new();
        let grant = mint_stream().unwrap();
        broker.register(&grant, StreamKind::Transfer);
        assert_eq!(
            broker.take(&grant.stream_id, &grant.token).unwrap(),
            StreamKind::Transfer
        );
    }

    #[test]
    fn unknown_ids_are_refused() {
        let broker = StreamBroker::new();
        let err = broker.take("000000000000000000000000", "x").unwrap_err();
        assert!(
            matches!(err, TypedError::StreamRefused { reason, .. } if reason == "unknown stream id")
        );
    }

    #[test]
    fn forget_drops_a_grant() {
        let broker = StreamBroker::new();
        let grant = mint_stream().unwrap();
        broker.register(&grant, StreamKind::Pty);
        assert!(broker.forget(&grant.stream_id));
        assert!(!broker.forget(&grant.stream_id));
        assert!(broker.take(&grant.stream_id, &grant.token).is_err());
    }
}
