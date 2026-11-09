//! Data-stream tokens (§7): the bookkeeping behind `/node/stream/:id`.
//!
//! Bytes never ride the control socket. Every data stream (a pty, a tunnel, a
//! file transfer) is a separate WebSocket the node dials with a **one-time**
//! bearer token that expires 30 s after minting. The socket endpoint itself
//! lives in the brain crate; this module is the exact bookkeeping it needs:
//! mint, register, take-once, forget.
//!
//! Invariants:
//!
//! * a token authorizes **one** dial — `take` consumes it;
//! * a wrong token must not consume the right one (an attacker guessing
//!   tokens cannot deny the node its stream), so a mismatch leaves the entry
//!   registered until it expires;
//! * token comparison is constant-time (see [`crate::pairing::constant_time_eq`]).

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use rand::TryRngCore as _;

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

/// Typed failure of token minting (the OS entropy source broke).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StreamError {
    #[error("the system RNG failed: {reason}")]
    Rng { reason: String },
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

/// One registered stream awaiting its dial.
#[derive(Clone, Debug)]
struct Entry {
    token: String,
    kind: String,
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

    /// Register a minted grant under its stream kind (`pty`, `transfer`, …).
    /// The kind is returned by [`StreamBroker::take`] so the stream socket
    /// knows what framing to expect (§7 table) without trusting the dialer.
    pub fn register(&self, grant: &StreamGrant, kind: impl Into<String>) {
        let mut entries = self.lock();
        let now = Utc::now();
        entries.retain(|_, entry| entry.expires_at > now);
        entries.insert(
            grant.stream_id.clone(),
            Entry {
                token: grant.token.clone(),
                kind: kind.into(),
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
    pub fn take(&self, stream_id: &str, token: &str) -> Result<String, TypedError> {
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
        let entry = entries.remove(stream_id).unwrap_or_else(|| Entry {
            token: String::new(),
            kind: String::new(),
            expires_at: now,
        });
        Ok(entry.kind)
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
        broker.register(&grant, "pty");
        assert_eq!(broker.len(), 1);
        assert_eq!(broker.take(&grant.stream_id, &grant.token).unwrap(), "pty");
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
        broker.register(&grant, "transfer");
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
        broker.register(&grant, "tcp");
        let err = broker
            .take(&grant.stream_id, "AAAAAAAAAAAAAAAAAAAAAA")
            .unwrap_err();
        assert!(
            matches!(err, TypedError::StreamRefused { reason, .. } if reason == "wrong stream token")
        );
        // The legitimate dial still works: guessing attacks must not be able
        // to deny the node its stream.
        assert_eq!(broker.take(&grant.stream_id, &grant.token).unwrap(), "tcp");
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
        broker.register(&grant, "pty");
        assert!(broker.forget(&grant.stream_id));
        assert!(!broker.forget(&grant.stream_id));
        assert!(broker.take(&grant.stream_id, &grant.token).is_err());
    }
}
