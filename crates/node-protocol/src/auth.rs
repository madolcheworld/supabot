//! The signed WebSocket upgrade and `node.hello` mutual authentication (§3).
//!
//! Two signatures make the control socket trustworthy in both directions:
//!
//! * the **node** signs every upgrade (`Authorization: SwarmBot-Node v1 …`),
//!   binding its node id, a timestamp, a fresh nonce and the brain origin
//!   together, so the brain knows *which* paired node dialed and a captured
//!   header cannot be replayed elsewhere or later;
//! * the **brain** signs `node.hello` with the node's id and the upgrade
//!   nonce, and the node verifies it against the key it **pinned** at
//!   pairing — a DNS or proxy hijack therefore cannot drive a node (§3.2).

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer, VerifyingKey};
use rand::TryRngCore as _;
use thiserror::Error;
use url::Url;

use swarmbot_contracts::{BRAIN_SIGN_PREFIX, NODE_SIGN_PREFIX};

/// Install rustls' process-wide crypto provider, once, and say whether one is
/// available (D9.25).
///
/// **Why this is a shared function and not a line in each TLS endpoint.** rustls
/// can choose a provider automatically only when exactly one of its
/// `aws-lc-rs`/`ring` features is enabled. In this workspace **both** are, because
/// `reqwest` and `tokio-tungstenite` each turn one on and Cargo features are
/// additive — so `ClientConfig::builder()` and `ServerConfig::builder()` *panic*
/// with "Could not automatically determine the process-level CryptoProvider"
/// instead of returning an error. That panic was found the hard way twice: once in
/// the brain's listener, and once in the node's `--brain-ca` connector — the second
/// only under the full gate, because a standalone test run had already installed a
/// provider earlier in the process. Two copies of this function would drift the
/// same way; both endpoints call this one.
///
/// A second call is not an error: whichever provider was installed first is the
/// process default and stays it, so `install_default`'s `Err` is ignored on
/// purpose. The provider named here is `aws_lc_rs`, the one this build exposes
/// (`rustls`'s `ring` module is compiled out).
///
/// Returns an error rather than panicking when no provider can be had, so a
/// caller can report it as a typed failure at start-up.
pub fn ensure_crypto_provider() -> Result<(), String> {
    let _ = rustls::crypto::CryptoProvider::install_default(
        rustls::crypto::aws_lc_rs::default_provider(),
    );
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        return Err(
            "no rustls crypto provider is installed, so TLS cannot start; this build installs \
             `aws-lc-rs` itself, so reaching this means another crate cleared it"
                .to_string(),
        );
    }
    Ok(())
}

/// Node revoked or unknown key: do not redial until re-paired (§3.2 table).
pub const CLOSE_REVOKED: u16 = 4401;
/// `node.hello` failed (§3.2 table).
pub const CLOSE_HELLO_FAILED: u16 = 4403;
/// No common protocol version (§3.2 table).
pub const CLOSE_NO_PROTOCOL: u16 = 4426;
/// A newer connection replaced this one; the brain asked us to go (§3.2).
pub const CLOSE_REPLACED: u16 = 4000;
/// A data-stream dial was refused: unknown id, expired token or wrong token
/// (§8). Distinct from [`CLOSE_REVOKED`] on purpose — a stream socket is a
/// different socket from the control one, and a node that mistook this for a
/// revocation would stop redialing its control socket and go silent for a bad
/// token.
pub const CLOSE_STREAM_REFUSED: u16 = 4405;

/// The auth scheme of the upgrade header (§3.1) — ours, not OpenBot's.
pub const AUTH_SCHEME: &str = "SwarmBot-Node v1";

/// Nonce size in bytes (§3.1: 16 random bytes, base64url on the wire).
pub const NONCE_BYTES: usize = 16;

/// Maximum accepted clock skew between `ts` and the brain's clock (§3.1).
pub const MAX_SKEW_MS: i64 = 60_000;

/// How long a nonce stays burned (§3.1: "not used in the last 2 min").
pub const NONCE_WINDOW_MS: i64 = 120_000;

/// Default bound on the replay cache: enough for a fleet of nodes redialing
/// in a storm, small enough that an attacker cannot grow it without limit.
pub const REPLAY_CACHE_CAPACITY: usize = 4096;

/// Typed failures of upgrade/hello authentication. Each variant corresponds
/// to one step of the documented check order (§3.1), so a caller can tell
/// *why* a dial was refused and react (e.g. stop redialing on a revoked
/// node).
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum AuthError {
    #[error("malformed Authorization header: {reason}")]
    MalformedHeader { reason: String },

    #[error("node {node_id} is not paired")]
    UnknownNode { node_id: String },

    #[error("node {node_id} is revoked")]
    NodeRevoked { node_id: String },

    #[error(
        "timestamp {ts} is too far from the clock ({now}): limit is {} ms",
        MAX_SKEW_MS
    )]
    TimestampSkew { ts: i64, now: i64 },

    #[error("nonce was already used by node {node_id} within the replay window")]
    ReplayedNonce { node_id: String },

    #[error("signature of node {node_id} does not verify")]
    BadSignature { node_id: String },

    #[error("`{url}` is not a usable brain URL: {reason}")]
    BadOrigin { url: String, reason: String },

    #[error("the system RNG failed: {reason}")]
    Rng { reason: String },
}

/// What the brain's key store says about a node id (§3.1 step 2: "the node is
/// paired **and not revoked**" — two different facts, so two different
/// answers; a revoked node must not look like a never-paired one).
#[derive(Clone, Copy, Debug)]
pub enum KeyLookup {
    Unknown,
    Revoked,
    Active(VerifyingKey),
}

/// A verified upgrade: the identity the rest of the connection may trust.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedUpgrade {
    pub node_id: String,
    /// Unix milliseconds from the header (already skew-checked).
    pub ts: i64,
    /// base64url nonce; `node.hello` is signed over it, and it is now burned
    /// in the replay cache.
    pub nonce: String,
}

/// A fresh 16-byte nonce from the OS CSPRNG (§3.1: fresh per dial).
pub fn new_nonce() -> Result<[u8; NONCE_BYTES], AuthError> {
    let mut nonce = [0u8; NONCE_BYTES];
    rand::rngs::OsRng
        .try_fill_bytes(&mut nonce)
        .map_err(|e| AuthError::Rng {
            reason: e.to_string(),
        })?;
    Ok(nonce)
}

/// Current Unix time in milliseconds.
pub fn unix_ms_now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Build the upgrade header (§3.1) for explicit `ts`/`nonce` — the shape the
/// tests pin. Returns `(header_value, nonce_base64url)`; keep the nonce, the
/// brain signs `node.hello` over it.
///
/// The signed message is `swarmbot-node-v1|<nodeId>|<ts>|<nonce>|<brainOrigin>`
/// — our prefix, our field order, matching the reference layout byte for byte
/// modulo the prefix string.
pub fn make_upgrade_auth(
    node_id: &str,
    signing_key: &ed25519_dalek::SigningKey,
    brain_origin: &str,
    ts: i64,
    nonce: &[u8; NONCE_BYTES],
) -> (String, String) {
    let nonce = URL_SAFE_NO_PAD.encode(nonce);
    let message = format!("{NODE_SIGN_PREFIX}|{node_id}|{ts}|{nonce}|{brain_origin}");
    let signature = signing_key.sign(message.as_bytes());
    let sig = URL_SAFE_NO_PAD.encode(signature.to_bytes());
    (
        format!("{AUTH_SCHEME} id={node_id},ts={ts},nonce={nonce},sig={sig}"),
        nonce,
    )
}

/// [`make_upgrade_auth`] with a fresh nonce and the current time — what a
/// node calls on every (re)dial.
pub fn upgrade_auth(
    node_id: &str,
    signing_key: &ed25519_dalek::SigningKey,
    brain_origin: &str,
) -> Result<(String, String), AuthError> {
    let nonce = new_nonce()?;
    Ok(make_upgrade_auth(
        node_id,
        signing_key,
        brain_origin,
        unix_ms_now(),
        &nonce,
    ))
}

fn malformed(reason: impl Into<String>) -> AuthError {
    AuthError::MalformedHeader {
        reason: reason.into(),
    }
}

/// Every header value must match `[A-Za-z0-9_-]+` (§3.1) — this keeps the
/// signed string unambiguous: none of the separators (`|`, `,`, `=`) can be
/// smuggled inside a value to forge a different message split.
fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Verify the upgrade header, in the documented order (§3.1):
///
/// 1. header shape (every value `[A-Za-z0-9_-]+`, `ts` decimal);
/// 2. the node is paired **and not revoked**;
/// 3. `|now − ts| ≤ 60 s`;
/// 4. the nonce was not used in the last 2 minutes;
/// 5. the signature.
///
/// Each step has its own error, and the order is load-bearing: a replayed
/// nonce from an unknown node must report the unknown node, not the replay.
/// The nonce is recorded **only after** the signature verifies — otherwise an
/// attacker who can observe a dial could burn the legitimate nonce first and
/// deny the node its connection.
pub fn verify_upgrade_auth(
    header: &str,
    brain_origin: &str,
    lookup: &dyn Fn(&str) -> KeyLookup,
    replay: &ReplayCache,
    now: i64,
) -> Result<VerifiedUpgrade, AuthError> {
    // 1. shape.
    let rest = header
        .strip_prefix(AUTH_SCHEME)
        .and_then(|rest| rest.strip_prefix(' '))
        .ok_or_else(|| malformed(format!("header must start with `{AUTH_SCHEME} `")))?;
    let mut parts = rest.split(',');
    let id_value = parts
        .next()
        .and_then(|p| p.strip_prefix("id="))
        .filter(|v| is_token(v))
        .ok_or_else(|| malformed("missing or malformed `id`"))?;
    let ts_value = parts
        .next()
        .and_then(|p| p.strip_prefix("ts="))
        .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| malformed("missing or non-decimal `ts`"))?;
    let nonce = parts
        .next()
        .and_then(|p| p.strip_prefix("nonce="))
        .filter(|v| is_token(v))
        .ok_or_else(|| malformed("missing or malformed `nonce`"))?;
    let sig = parts
        .next()
        .and_then(|p| p.strip_prefix("sig="))
        .filter(|v| is_token(v))
        .ok_or_else(|| malformed("missing or malformed `sig`"))?;
    if parts.next().is_some() {
        return Err(malformed("trailing header fields"));
    }
    let ts: i64 = ts_value
        .parse()
        .map_err(|_| malformed("`ts` is not a valid i64"))?;
    // The nonce must decode to exactly 16 bytes (§3.1) — checking here keeps
    // garbage out of the replay cache.
    let nonce_bytes = URL_SAFE_NO_PAD
        .decode(nonce)
        .map_err(|_| malformed("`nonce` is not base64url"))?;
    if nonce_bytes.len() != NONCE_BYTES {
        return Err(malformed(format!(
            "`nonce` must decode to {NONCE_BYTES} bytes, got {}",
            nonce_bytes.len()
        )));
    }
    let node_id = id_value.to_string();

    // 2. known and not revoked. Steps 2–5 run in this order and each has
    // its own error (see the fn docs for why the order is load-bearing).
    match lookup(&node_id) {
        KeyLookup::Unknown => Err(AuthError::UnknownNode { node_id }),
        KeyLookup::Revoked => Err(AuthError::NodeRevoked { node_id }),
        KeyLookup::Active(key) => {
            // 3. skew.
            if (now - ts).abs() > MAX_SKEW_MS {
                return Err(AuthError::TimestampSkew { ts, now });
            }
            // 4. nonce freshness — checked before the signature, but only
            // *recorded* after it (step 5), so a bad signature cannot burn a
            // good nonce.
            if replay.contains(nonce, now) {
                return Err(AuthError::ReplayedNonce { node_id });
            }
            // 5. signature over the canonical message.
            let message = format!("{NODE_SIGN_PREFIX}|{node_id}|{ts}|{nonce}|{brain_origin}");
            let signature = URL_SAFE_NO_PAD
                .decode(sig)
                .ok()
                .and_then(|bytes| Signature::from_slice(&bytes).ok())
                .ok_or_else(|| AuthError::BadSignature {
                    node_id: node_id.clone(),
                })?;
            key.verify_strict(message.as_bytes(), &signature)
                .map_err(|_| AuthError::BadSignature {
                    node_id: node_id.clone(),
                })?;
            replay.insert(nonce, now);
            Ok(VerifiedUpgrade {
                node_id,
                ts,
                nonce: nonce.to_string(),
            })
        }
    }
}

/// Bounded nonce memory (§3.1: "the nonce was not used in the last 2 min").
///
/// Entries expire after a window (default 2 minutes, twice the maximum clock
/// skew so an expired nonce can never become valid again), and the map is
/// capped (default 4096): when full, expired entries go first, then the
/// oldest. Uses a `std::sync::Mutex` because verification is synchronous —
/// never hold it across an await.
#[derive(Debug)]
pub struct ReplayCache {
    capacity: usize,
    window_ms: i64,
    seen: Mutex<HashMap<String, i64>>,
}

impl Default for ReplayCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Drop the oldest entry to make room (bounded memory beats perfect recall —
/// the expiry window already limits what a replay can do).
fn evict_oldest(seen: &mut HashMap<String, i64>) {
    if let Some(oldest) = seen
        .iter()
        .min_by_key(|(_, at)| *at)
        .map(|(k, _)| k.clone())
    {
        seen.remove(&oldest);
    }
}

impl ReplayCache {
    pub fn new() -> ReplayCache {
        ReplayCache::with_limits(REPLAY_CACHE_CAPACITY, NONCE_WINDOW_MS)
    }

    pub fn with_limits(capacity: usize, window_ms: i64) -> ReplayCache {
        ReplayCache {
            capacity: capacity.max(1),
            window_ms,
            seen: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, i64>> {
        // A poisoned lock only means a panic between lock and unlock; the map
        // itself is still consistent, so recover instead of propagating.
        self.seen.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True when the nonce was seen within the window. Prunes expired entries
    /// on the way, so the cache does not accumulate garbage.
    pub fn contains(&self, nonce: &str, now_ms: i64) -> bool {
        let mut seen = self.lock();
        seen.retain(|_, at| now_ms - *at < self.window_ms);
        seen.contains_key(nonce)
    }

    /// Record a nonce. When at capacity, expired entries are dropped first;
    /// if it is still full, the oldest entry is evicted.
    pub fn insert(&self, nonce: &str, now_ms: i64) {
        let mut seen = self.lock();
        seen.retain(|_, at| now_ms - *at < self.window_ms);
        if seen.len() >= self.capacity && !seen.contains_key(nonce) {
            evict_oldest(&mut seen);
        }
        seen.insert(nonce.to_string(), now_ms);
    }

    /// Combined "was it fresh? then burn it": true when the nonce was unused
    /// (and is now recorded), false when it was a replay.
    pub fn check_and_insert(&self, nonce: &str, now_ms: i64) -> bool {
        let mut seen = self.lock();
        seen.retain(|_, at| now_ms - *at < self.window_ms);
        if seen.contains_key(nonce) {
            return false;
        }
        if seen.len() >= self.capacity {
            evict_oldest(&mut seen);
        }
        seen.insert(nonce.to_string(), now_ms);
        true
    }

    /// Number of live entries (after pruning).
    pub fn len(&self) -> usize {
        let mut seen = self.lock();
        seen.retain(|_, at| unix_ms_now() - *at < self.window_ms);
        seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Sign the `node.hello` challenge (§3.2): the brain proves to *this* node
/// that it holds the pinned key, over a message bound to the node id, the
/// connection's upgrade nonce and the brain origin. Returns base64url.
pub fn sign_hello(
    brain_key: &ed25519_dalek::SigningKey,
    node_id: &str,
    nonce: &str,
    brain_origin: &str,
) -> String {
    let message = format!("{BRAIN_SIGN_PREFIX}|{node_id}|{nonce}|{brain_origin}");
    let signature = brain_key.sign(message.as_bytes());
    URL_SAFE_NO_PAD.encode(signature.to_bytes())
}

/// Verify a `node.hello` signature against the **pinned** brain key (§3.2).
/// The node calls this before it answers anything else on the connection.
pub fn verify_hello(
    brain_key: &VerifyingKey,
    node_id: &str,
    nonce: &str,
    brain_origin: &str,
    signature_base64url: &str,
) -> Result<(), AuthError> {
    let message = format!("{BRAIN_SIGN_PREFIX}|{node_id}|{nonce}|{brain_origin}");
    let signature = URL_SAFE_NO_PAD
        .decode(signature_base64url)
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
        .ok_or_else(|| AuthError::BadSignature {
            node_id: node_id.to_string(),
        })?;
    brain_key
        .verify_strict(message.as_bytes(), &signature)
        .map_err(|_| AuthError::BadSignature {
            node_id: node_id.to_string(),
        })
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

/// Derive the brain origin that both signatures are bound to (§2.3, §3.1):
/// `ws` → `http`, `wss` → `https`, keep `scheme://host[:port]`, drop path,
/// query and credentials. `http`/`https` inputs pass through normalized so a
/// browser-side URL works too.
pub fn brain_origin_from_url(url: &str) -> Result<String, AuthError> {
    let bad = |reason: &str| AuthError::BadOrigin {
        url: url.to_string(),
        reason: reason.to_string(),
    };
    let parsed = Url::parse(url).map_err(|e| bad(&e.to_string()))?;
    let scheme = match parsed.scheme() {
        "ws" => "http",
        "wss" => "https",
        "http" => "http",
        "https" => "https",
        other => return Err(bad(&format!("unsupported scheme `{other}`"))),
    };
    let host = parsed.host_str().ok_or_else(|| bad("missing host"))?;
    if host.is_empty() {
        return Err(bad("empty host"));
    }
    // `Url::port()` is Some only for an explicitly written port; a port that
    // is the default of the *mapped* scheme is dropped so origins compare
    // equal regardless of how the URL was spelled.
    let origin = match parsed.port() {
        Some(port) if default_port(scheme) != Some(port) => {
            format!("{scheme}://{host}:{port}")
        }
        _ => format!("{scheme}://{host}"),
    };
    Ok(origin)
}

#[cfg(test)]
mod tests {

    /// The provider must be installable **before** any rustls config is built, and
    /// asking for it twice must be fine.
    ///
    /// This is a regression test with a specific history: `ClientConfig::builder()`
    /// and `ServerConfig::builder()` **panic** in this workspace because two
    /// providers' features are on at once (`reqwest` and `tokio-tungstenite` each
    /// enable one, and Cargo features are additive). The brain's listener was fixed
    /// first; the node's `--brain-ca` connector still panicked, and it only showed
    /// up under the **full gate** — a standalone test run had already installed a
    /// provider earlier in the same process, so the panic needed a fresh process to
    /// reproduce. That is exactly the shape of bug a unit test can pin cheaply, so
    /// here it is: call the helper, then build a config from *each* side.
    #[test]
    fn the_crypto_provider_is_installed_and_a_config_can_be_built_from_both_sides() {
        ensure_crypto_provider().expect("the provider installs");
        // Calling it again is the normal case (brain, node, and a test in one
        // process) and must not fail.
        ensure_crypto_provider().expect("a second install is a no-op");
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "the process has a provider, so the builders below cannot panic"
        );
        // The two builders that used to panic. Building them is the assertion:
        // reaching the next line at all is the proof.
        let _client = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        // A server config needs a certificate; the *builder chain* is what matters
        // here, so `builder()` alone is called and the missing-cert error (if any)
        // is irrelevant — the panic happened in `builder()`, not in `with_single_cert`.
        let _server = rustls::ServerConfig::builder().with_no_client_auth();
    }
    use super::*;
    use ed25519_dalek::SigningKey;

    const ORIGIN: &str = "https://brain.example";
    const NODE: &str = "nod_0189fbd3a4d37f0e9c0e6a3f8f1c9d2e";

    fn keys() -> (SigningKey, VerifyingKey) {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let verifying = signing.verifying_key();
        (signing, verifying)
    }

    fn active(key: VerifyingKey) -> impl Fn(&str) -> KeyLookup {
        move |id: &str| {
            if id == NODE {
                KeyLookup::Active(key)
            } else {
                KeyLookup::Unknown
            }
        }
    }

    fn header(signing: &SigningKey, ts: i64, nonce: &[u8; NONCE_BYTES]) -> (String, String) {
        make_upgrade_auth(NODE, signing, ORIGIN, ts, nonce)
    }

    #[test]
    fn a_valid_header_verifies() {
        let (signing, verifying) = keys();
        let now = unix_ms_now();
        let nonce = new_nonce().unwrap();
        let (header, nonce_b64) = header(&signing, now, &nonce);
        assert!(header.starts_with("SwarmBot-Node v1 id="));
        let replay = ReplayCache::new();
        let verified =
            verify_upgrade_auth(&header, ORIGIN, &active(verifying), &replay, now).unwrap();
        assert_eq!(verified.node_id, NODE);
        assert_eq!(verified.ts, now);
        assert_eq!(verified.nonce, nonce_b64);
        // The nonce is burned only now.
        assert!(replay.contains(&nonce_b64, now));
    }

    #[test]
    fn wrong_signature_unknown_and_revoked_each_have_their_own_error() {
        let (signing, verifying) = keys();
        let other = SigningKey::from_bytes(&[8u8; 32]);
        let now = unix_ms_now();
        let nonce = new_nonce().unwrap();
        let replay = ReplayCache::new();

        // Signed by the wrong key (attacker with a valid-looking node id).
        let (bad, _) = header(&other, now, &nonce);
        assert_eq!(
            verify_upgrade_auth(&bad, ORIGIN, &active(verifying), &replay, now),
            Err(AuthError::BadSignature {
                node_id: NODE.into()
            })
        );

        // Unknown node.
        let (header, _) = header(&signing, now, &nonce);
        assert_eq!(
            verify_upgrade_auth(&header, ORIGIN, &|_| KeyLookup::Unknown, &replay, now),
            Err(AuthError::UnknownNode {
                node_id: NODE.into()
            })
        );

        // Revoked node.
        assert_eq!(
            verify_upgrade_auth(&header, ORIGIN, &|_| KeyLookup::Revoked, &replay, now),
            Err(AuthError::NodeRevoked {
                node_id: NODE.into()
            })
        );
    }

    #[test]
    fn clock_skew_beyond_sixty_seconds_is_refused() {
        let (signing, verifying) = keys();
        let now = unix_ms_now();
        let replay = ReplayCache::new();

        let (old, _) = header(&signing, now - (MAX_SKEW_MS + 1000), &new_nonce().unwrap());
        assert!(matches!(
            verify_upgrade_auth(&old, ORIGIN, &active(verifying), &replay, now),
            Err(AuthError::TimestampSkew { .. })
        ));

        // Future skew is refused too: |now − ts|, not now − ts.
        let (future, _) = header(&signing, now + (MAX_SKEW_MS + 1000), &new_nonce().unwrap());
        assert!(matches!(
            verify_upgrade_auth(&future, ORIGIN, &active(verifying), &replay, now),
            Err(AuthError::TimestampSkew { .. })
        ));

        // Exactly at the limit is fine.
        let (edge, _) = header(&signing, now - MAX_SKEW_MS, &new_nonce().unwrap());
        assert!(verify_upgrade_auth(&edge, ORIGIN, &active(verifying), &replay, now).is_ok());
    }

    #[test]
    fn a_replayed_nonce_is_refused() {
        let (signing, verifying) = keys();
        let now = unix_ms_now();
        let nonce = new_nonce().unwrap();
        let (header, _) = header(&signing, now, &nonce);
        let replay = ReplayCache::new();
        assert!(verify_upgrade_auth(&header, ORIGIN, &active(verifying), &replay, now).is_ok());
        // The very same header again (within the window, still in skew).
        assert_eq!(
            verify_upgrade_auth(&header, ORIGIN, &active(verifying), &replay, now + 1000),
            Err(AuthError::ReplayedNonce {
                node_id: NODE.into()
            })
        );
        // After the window the nonce is forgotten — but by then the ts is far
        // out of skew, which is exactly why the window is 2× the skew limit.
        assert!(!replay.contains(&URL_SAFE_NO_PAD.encode(nonce), now + NONCE_WINDOW_MS + 1));
    }

    #[test]
    fn checks_run_in_the_documented_order() {
        let (signing, verifying) = keys();
        let now = unix_ms_now();
        let replay = ReplayCache::new();

        // Burn a nonce with a fully valid header first.
        let nonce = new_nonce().unwrap();
        let (valid, nonce_b64) = header(&signing, now, &nonce);
        assert!(verify_upgrade_auth(&valid, ORIGIN, &active(verifying), &replay, now).is_ok());

        // Unknown node + replayed nonce → the unknown node is reported, not
        // the replay (step 2 before step 4).
        let other_key = SigningKey::from_bytes(&[42u8; 32]);
        let (unknown_replay, _) = make_upgrade_auth("nod_other", &other_key, ORIGIN, now, &nonce);
        assert_eq!(
            verify_upgrade_auth(&unknown_replay, ORIGIN, &active(verifying), &replay, now),
            Err(AuthError::UnknownNode {
                node_id: "nod_other".into()
            })
        );

        // Revoked + replayed nonce + bad skew → revoked wins (step 2 first).
        assert_eq!(
            verify_upgrade_auth(
                &valid,
                ORIGIN,
                &|_| KeyLookup::Revoked,
                &replay,
                now + 999_999
            ),
            Err(AuthError::NodeRevoked {
                node_id: NODE.into()
            })
        );

        // Skew + replayed nonce → skew wins (step 3 before step 4).
        assert_eq!(
            verify_upgrade_auth(&valid, ORIGIN, &active(verifying), &replay, now + 999_999),
            Err(AuthError::TimestampSkew {
                ts: now,
                now: now + 999_999,
            })
        );

        // Replayed nonce + tampered signature → replay wins (step 4 before 5):
        // the replay check must not depend on the attacker being able to sign.
        let last = valid.chars().last().unwrap_or('A');
        let replacement = if last == 'A' { 'B' } else { 'A' };
        let tampered = format!("{}{}", &valid[..valid.len() - 1], replacement);
        assert_ne!(tampered, valid);
        assert_eq!(
            verify_upgrade_auth(&tampered, ORIGIN, &active(verifying), &replay, now),
            Err(AuthError::ReplayedNonce {
                node_id: NODE.into()
            })
        );
        let _ = nonce_b64;
    }

    #[test]
    fn a_nonce_burned_by_a_failed_signature_stays_usable() {
        let (signing, verifying) = keys();
        let impostor = SigningKey::from_bytes(&[11u8; 32]);
        let now = unix_ms_now();
        let nonce = new_nonce().unwrap();
        let replay = ReplayCache::new();

        // An attacker replays the shape with a bad signature; it must NOT burn
        // the nonce.
        let (bad, _) = header(&impostor, now, &nonce);
        assert!(matches!(
            verify_upgrade_auth(&bad, ORIGIN, &active(verifying), &replay, now),
            Err(AuthError::BadSignature { .. })
        ));
        let nonce_b64 = URL_SAFE_NO_PAD.encode(nonce);
        assert!(!replay.contains(&nonce_b64, now));

        // The legitimate node now dials with the very same nonce and wins.
        let (good, _) = header(&signing, now, &nonce);
        assert!(verify_upgrade_auth(&good, ORIGIN, &active(verifying), &replay, now).is_ok());
    }

    #[test]
    fn malformed_headers_are_refused_by_shape_alone() {
        let (signing, verifying) = keys();
        let now = unix_ms_now();
        let nonce = new_nonce().unwrap();
        let replay = ReplayCache::new();
        let (valid, _) = header(&signing, now, &nonce);

        for broken in [
            "",
            "Bearer something",
            "OpenBot-Node v1 id=x,ts=1,nonce=y,sig=z", // the other product's scheme
            "SwarmBot-Node v1",
            "SwarmBot-Node v1 ",
            "SwarmBot-Node v2 id=a,ts=1,nonce=b,sig=c",
            &valid.replace("ts=", "T="),
            &valid.replace(",nonce=", ", nonce="), // space breaks the token
            &format!("{valid},extra=1"),
            &valid.replace("id=", "id=!"),
        ] {
            assert!(
                matches!(
                    verify_upgrade_auth(broken, ORIGIN, &active(verifying), &replay, now),
                    Err(AuthError::MalformedHeader { .. })
                ),
                "expected malformed: {broken:?}"
            );
        }

        // ts must be decimal digits (no sign, no float).
        for bad_ts in ["-1", "1.5", "0x10", "abc"] {
            let header = valid.replace(&format!("ts={now}"), &format!("ts={bad_ts}"));
            assert!(
                matches!(
                    verify_upgrade_auth(&header, ORIGIN, &active(verifying), &replay, now),
                    Err(AuthError::MalformedHeader { .. })
                ),
                "expected malformed ts: {bad_ts}"
            );
        }

        // A nonce that is not 16 bytes is a shape error: it is rejected
        // before the signature is even looked at.
        let short_nonce = URL_SAFE_NO_PAD.encode([1u8, 2, 3]);
        let short = format!(
            "{AUTH_SCHEME} id={NODE},ts={now},nonce={short_nonce},sig=AAAAAAAAAAAAAAAAAAAAAA"
        );
        assert!(matches!(
            verify_upgrade_auth(&short, ORIGIN, &active(verifying), &replay, now),
            Err(AuthError::MalformedHeader { .. })
        ));
    }

    #[test]
    fn replay_cache_expires_and_evicts() {
        let now = unix_ms_now();
        let cache = ReplayCache::with_limits(2, 1_000);
        assert!(cache.check_and_insert("a", now));
        assert!(!cache.check_and_insert("a", now + 500)); // still inside the window
        assert!(cache.check_and_insert("a", now + 1_001)); // window passed

        // Capacity: the oldest entry is evicted when full.
        let cache = ReplayCache::with_limits(2, 60_000);
        assert!(cache.check_and_insert("old", now - 30_000));
        assert!(cache.check_and_insert("mid", now - 20_000));
        assert!(cache.check_and_insert("new", now - 10_000)); // evicts "old"
        assert!(cache.check_and_insert("old", now - 5_000)); // usable again, evicts "mid"
        assert!(!cache.check_and_insert("new", now)); // still remembered
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn hello_signatures_round_trip_and_reject_tampering() {
        let brain = SigningKey::from_bytes(&[5u8; 32]);
        let pinned = brain.verifying_key();
        let nonce = "AAAAAAAAAAAAAAAAAAAAAA";
        let sig = sign_hello(&brain, NODE, nonce, ORIGIN);
        assert!(verify_hello(&pinned, NODE, nonce, ORIGIN, &sig).is_ok());

        // A different nonce, node id or origin must not verify: the message
        // binds all of them.
        assert!(verify_hello(&pinned, NODE, "BBBBBBBBBBBBBBBBBBBBBB", ORIGIN, &sig).is_err());
        assert!(verify_hello(&pinned, "nod_other", nonce, ORIGIN, &sig).is_err());
        assert!(verify_hello(&pinned, NODE, nonce, "https://evil.example", &sig).is_err());

        // A different brain key must not verify (this is the hijack case).
        let evil = SigningKey::from_bytes(&[6u8; 32]).verifying_key();
        assert!(matches!(
            verify_hello(&evil, NODE, nonce, ORIGIN, &sig),
            Err(AuthError::BadSignature { .. })
        ));

        // Garbage signatures are typed errors.
        assert!(verify_hello(&pinned, NODE, nonce, ORIGIN, "not-base64!").is_err());
        assert!(
            verify_hello(
                &pinned,
                NODE,
                nonce,
                ORIGIN,
                &URL_SAFE_NO_PAD.encode([0u8; 10])
            )
            .is_err()
        );
    }

    #[test]
    fn origins_map_ws_wss_and_strip_the_rest() {
        assert_eq!(
            brain_origin_from_url("ws://localhost:8080/node").unwrap(),
            "http://localhost:8080"
        );
        assert_eq!(
            brain_origin_from_url("wss://brain.example/x?y=1#z").unwrap(),
            "https://brain.example"
        );
        assert_eq!(
            brain_origin_from_url("wss://brain.example:8443").unwrap(),
            "https://brain.example:8443"
        );
        // Default ports of the mapped scheme disappear.
        assert_eq!(
            brain_origin_from_url("ws://brain.example:80").unwrap(),
            "http://brain.example"
        );
        assert_eq!(
            brain_origin_from_url("wss://brain.example:443").unwrap(),
            "https://brain.example"
        );
        // Credentials are dropped.
        assert_eq!(
            brain_origin_from_url("wss://user:pass@brain.example/secret").unwrap(),
            "https://brain.example"
        );
        // http/https pass through.
        assert_eq!(
            brain_origin_from_url("https://brain.example/deep/path").unwrap(),
            "https://brain.example"
        );
        // Anything else is a typed error.
        assert!(matches!(
            brain_origin_from_url("ftp://brain.example"),
            Err(AuthError::BadOrigin { .. })
        ));
        assert!(matches!(
            brain_origin_from_url("not a url"),
            Err(AuthError::BadOrigin { .. })
        ));
    }

    #[test]
    fn the_upgrade_convenience_helper_produces_verifiable_headers() {
        let (signing, verifying) = keys();
        let replay = ReplayCache::new();
        let (header, nonce) = upgrade_auth(NODE, &signing, ORIGIN).unwrap();
        let verified =
            verify_upgrade_auth(&header, ORIGIN, &active(verifying), &replay, unix_ms_now())
                .unwrap();
        assert_eq!(verified.nonce, nonce);
    }

    #[test]
    fn close_codes_match_the_protocol_table() {
        assert_eq!(CLOSE_REVOKED, 4401);
        assert_eq!(CLOSE_HELLO_FAILED, 4403);
        assert_eq!(CLOSE_NO_PROTOCOL, 4426);
        assert_eq!(CLOSE_REPLACED, 4000);
        assert_eq!(CLOSE_STREAM_REFUSED, 4405);
    }
}
