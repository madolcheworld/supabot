//! SwarmBot node protocol (v1): the wire between a brain and its nodes.
//!
//! One control socket per node, always dialed **out** by the node
//! (NAT-friendly), carrying a bidirectional JSON-RPC 2.0 mux in the Effect-RPC
//! dialect; byte-heavy data streams ride separate sockets under one-time
//! tokens. This crate is wire-compatible with the subset of OpenBot's node
//! protocol v1 described in `docs/node-protocol.md`, with SwarmBot's own
//! string prefixes ([`swarmbot_contracts::PAIRING_HASH_PREFIX`] & co.) and a
//! reduced method table: `node.hello`, `fs.*`, `shell.exec` on the node side;
//! `node.requestApproval`, `node.event`, `node.heartbeat` on the brain side.
//! Everything else in the reference table is answered with a typed
//! `PolicyRefused { retry: false }` — see [`rpc::unimplemented`].
//!
//! Module map:
//!
//! * [`identity`] — Ed25519 identities for node and brain, stored at mode
//!   0600, keys base64url without padding (§2).
//! * [`pairing`] — Crockford-base32 pairing codes, normalization, hashing,
//!   constant-time comparison (§2.1).
//! * [`auth`] — the signed upgrade header, the ordered verification with the
//!   bounded replay cache, `node.hello` signatures, brain-origin derivation
//!   and the close codes (§3).
//! * [`wire`] — the JSON-RPC 2.0 envelope: frames, type-preserving ids, the
//!   tagged error causes of §5.3, direction routing (§4).
//! * [`mux`] — [`mux::Peer`]: the bidirectional peer over any [`mux::Socket`],
//!   with streaming + Acks, interruption, ping/liveness, and the
//!   `NotAuthenticated` gate; [`mux::WsSocket`] is the real-WebSocket
//!   adapter.
//! * [`streams`] — one-time data-stream tokens ([`streams::StreamBroker`]) and
//!   the framing that rides the socket they open ([`streams::StreamFrame`],
//!   [`streams::StreamKind`]) (§7–§8).
//! * [`rpc`] — method names, payload structs and the unimplemented-method
//!   refusal table (§5).
//! * [`testing`] — in-memory sockets for tests of this crate and of `brain`/
//!   `node`.
//!
//! The dependency-heavy re-exports below (`serde_json`, `ed25519_dalek`,
//! `tokio_tungstenite`, `swarmbot_contracts`) exist so consumers — including
//! this crate's own integration tests — build payloads and keys against
//! exactly the versions this protocol was written with.

pub mod auth;
pub mod identity;
pub mod mux;
pub mod pairing;
pub mod rpc;
pub mod streams;
pub mod testing;
pub mod wire;

pub use auth::{
    AUTH_SCHEME, AuthError, CLOSE_HELLO_FAILED, CLOSE_NO_PROTOCOL, CLOSE_REPLACED, CLOSE_REVOKED,
    CLOSE_STREAM_REFUSED, KeyLookup, MAX_SKEW_MS, NONCE_BYTES, NONCE_WINDOW_MS, ReplayCache,
    VerifiedUpgrade, brain_origin_from_url, ensure_crypto_provider, make_upgrade_auth, new_nonce,
    sign_hello, unix_ms_now, upgrade_auth, verify_hello, verify_upgrade_auth,
};
pub use identity::{
    BrainIdentity, IdentityError, NodeIdentity, parse_public_key_base64url, public_key_base64url,
};
pub use mux::{
    BoxFuture, CancellationToken, ChunkSink, Handler, Peer, PeerConfig, RpcHandler, Socket,
    WsCallback, WsErrorResponse, WsRequest, WsResponse, WsSocket, accept_ws, accept_ws_hdr,
    connect_ws, connect_ws_with_headers,
};
pub use pairing::{
    CODE_ALPHABET, CODE_LEN, PairCode, PairError, constant_time_eq, format_display, hash_code,
    mint_code, normalize_code,
};
pub use rpc::{
    ApprovalAnswer, Encoding, FileKind, FileStat, FsEdit, FsEditResult, FsGlob, FsGlobResult,
    FsGrep, FsGrepResult, FsList, FsListResult, FsMkdir, FsMove, FsRead, FsReadResult, FsRemove,
    FsStat, FsWrite, FsWriteResult, GrepMatch, GrepOutputMode, Heartbeat, HelloParams, HelloPolicy,
    ListEntry, NodeEvent, NodeHello, OutputStream, PathCount, RequestApproval, ShellChunk,
    ShellExec, decode_params, is_stream_method, unimplemented,
};
pub use streams::{
    ACK_EVERY_BYTES, MAX_CHUNK_BYTES, MAX_IN_FLIGHT_BYTES, STREAM_TOKEN_TTL, StreamBroker,
    StreamError, StreamFrame, StreamGrant, StreamKind, chunk_sha256, mint_stream,
};
pub use wire::{
    CauseError, CauseReason, Direction, Frame, Id, ProtocolError, TypedError, direction, parse,
    render,
};

/// The node wire protocol version this build speaks (re-exported from
/// `swarmbot-contracts` so there is exactly one definition).
pub use swarmbot_contracts::PROTOCOL_VERSION;

pub use ed25519_dalek;
pub use serde_json;
pub use swarmbot_contracts;
pub use tokio_tungstenite;

#[cfg(test)]
mod sanity_tests {
    use super::*;

    #[test]
    fn the_protocol_constants_are_the_swarmbot_ones() {
        assert_eq!(PROTOCOL_VERSION, swarmbot_contracts::PROTOCOL_VERSION);
        assert_eq!(
            swarmbot_contracts::PAIRING_HASH_PREFIX,
            "swarmbot-pairing-v1"
        );
        assert_eq!(swarmbot_contracts::NODE_SIGN_PREFIX, "swarmbot-node-v1");
        assert_eq!(swarmbot_contracts::BRAIN_SIGN_PREFIX, "swarmbot-brain-v1");
        assert_eq!(AUTH_SCHEME, "SwarmBot-Node v1");
    }
}
