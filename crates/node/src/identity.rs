//! The node's identity: its own Ed25519 key, the brain key it **pinned** at
//! pairing, and the `POST /node/pair` exchange that created both.
//!
//! Two invariants:
//!
//! * **the private key never leaves `<data>/identity.json`** (mode 0600). It is
//!   not in a log line, not in an error, not in `Debug` output — [`Identity`]
//!   implements `Debug` by hand and prints a fingerprint instead, and there is
//!   a test that fails if any spelling of the key ever reaches a formatter.
//!   There are no API keys in this crate at all: credentials stay on the brain
//!   (decision D1.5).
//! * **the brain key is pinned, never re-fetched.** `node.hello`'s `brainSig`
//!   is verified against the key stored at pairing, which is what makes a DNS
//!   or proxy hijack unable to drive this node (decision D1.3). A re-key is a
//!   re-pair.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use swarmbot_contracts::{PairRequest, PairResponse, ProtocolRange};
use url::Url;

use swarmbot_node_protocol::pairing::normalize_code;
use swarmbot_node_protocol::{
    NodeIdentity, brain_origin_from_url, public_key_base64url, upgrade_auth,
};

use crate::config::{NodeConfig, NodePaths, hostname};
use crate::{NODE_VERSION, NodeError};

/// The control socket path the node dials (§3.1).
pub const CONTROL_PATH: &str = "/node";
/// Where a data stream dials (§8): `/node/stream/<streamId>`.
pub const STREAM_PATH: &str = "/node/stream";
/// The pairing endpoint (§2.3).
pub const PAIR_PATH: &str = "/node/pair";

/// A loaded identity, with the keypair behind a private field.
///
/// `Debug` is hand-written: the derived one would print whatever
/// `ed25519_dalek` prints, and "whatever a dependency prints" is not an
/// invariant this crate is allowed to rely on for a secret. What is printed
/// instead — the node id and a fingerprint of the *public* halves — is enough
/// to tell two identities apart in a log.
pub struct Identity {
    inner: NodeIdentity,
}

impl Identity {
    /// Wrap an already-loaded protocol identity.
    pub fn new(inner: NodeIdentity) -> Self {
        Self { inner }
    }

    /// Build an identity from its parts without touching disk.
    ///
    /// This is the seam a test uses to make a node that is pinned to a known
    /// brain key (so a hello signature can be produced), and the seam an
    /// embedder would use if it kept keys somewhere other than
    /// `<data>/identity.json`. It performs no verification: the caller asserts
    /// that this brain key is the one to trust.
    pub fn from_parts(
        node_id: swarmbot_contracts::NodeId,
        keypair: SigningKey,
        brain_public_key: VerifyingKey,
        brain_origin: impl Into<String>,
    ) -> Result<Identity, NodeError> {
        let brain_origin = brain_origin.into();
        if brain_origin.is_empty() {
            return Err(NodeError::identity("the brain origin must not be empty"));
        }
        Ok(Self {
            inner: NodeIdentity {
                keypair,
                node_id,
                brain_public_key,
                brain_origin,
            },
        })
    }

    /// Load `<data>/identity.json`. A missing file is [`NodeError::NotPaired`]
    /// — the message says which command fixes it, because "no such file" does
    /// not.
    pub fn load(paths: &NodePaths) -> Result<Identity, NodeError> {
        let path = paths.identity();
        if !path.exists() {
            return Err(NodeError::NotPaired { path });
        }
        // A key file that other local users can read is a leaked key, even if
        // nothing has used it yet: tighten it and say so, rather than refusing
        // to start (refusing would leave the owner with a dead node and no
        // clue) and rather than staying quiet (a silently loose mode is how
        // the leak survives).
        enforce_private_mode(&path)?;
        let inner = NodeIdentity::load(&path)?;
        Ok(Self { inner })
    }

    /// Write `<data>/identity.json` at mode 0600.
    pub fn save(&self, paths: &NodePaths) -> Result<(), NodeError> {
        if let Some(parent) = paths.identity().parent() {
            std::fs::create_dir_all(parent).map_err(|source| NodeError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        self.inner.save(&paths.identity())?;
        enforce_private_mode(&paths.identity())?;
        Ok(())
    }

    pub fn node_id(&self) -> &swarmbot_contracts::NodeId {
        &self.inner.node_id
    }

    /// The origin every signature is bound to: `scheme://host[:port]`.
    pub fn brain_origin(&self) -> &str {
        &self.inner.brain_origin
    }

    /// The pinned brain key. `node.hello` is verified against exactly this.
    pub fn brain_public_key(&self) -> &VerifyingKey {
        &self.inner.brain_public_key
    }

    /// The signing key. Deliberately the only accessor: keeping it behind a
    /// method (rather than a public field on a `Debug` type) is what makes the
    /// redaction test meaningful.
    pub fn signing_key(&self) -> &SigningKey {
        &self.inner.keypair
    }

    pub fn public_key_base64url(&self) -> String {
        public_key_base64url(&self.inner.keypair.verifying_key())
    }

    /// `sha256(<public key>)` truncated to 16 hex chars — safe to log, and
    /// enough to notice that a node re-paired.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.inner.keypair.verifying_key())
    }

    /// The pinned brain key's fingerprint.
    pub fn brain_fingerprint(&self) -> String {
        fingerprint(&self.inner.brain_public_key)
    }

    /// Build this dial's `Authorization` header (§3.1). Returns
    /// `(header, nonce)`; **keep the nonce** — `node.hello`'s `brainSig` is
    /// over it, so the handler needs the same value the header carried.
    pub fn upgrade(&self) -> Result<(String, String), NodeError> {
        let (header, nonce) = upgrade_auth(
            self.inner.node_id.as_str(),
            &self.inner.keypair,
            &self.inner.brain_origin,
        )?;
        Ok((header, nonce))
    }
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("node_id", &self.inner.node_id.to_string())
            .field("brain_origin", &self.inner.brain_origin)
            .field(
                "node_key",
                &format_args!(
                    "ed25519:{}",
                    fingerprint(&self.inner.keypair.verifying_key())
                ),
            )
            .field(
                "brain_key",
                &format_args!("ed25519:{}", fingerprint(&self.inner.brain_public_key)),
            )
            // Named explicitly so a reader of a debug line knows the secret
            // exists and was withheld, rather than wondering if it is absent.
            .field("secret_key", &format_args!("<redacted>"))
            .finish()
    }
}

/// `sha256` of a public key, first 16 hex chars. Public material, so logging
/// it is fine; short, so a log line stays readable.
fn fingerprint(key: &VerifyingKey) -> String {
    let digest = Sha256::digest(key.as_bytes());
    hex::encode(&digest[..8])
}

/// Force mode 0600 on a private file. On non-Unix platforms there are no mode
/// bits; the file keeps its directory's ACL and there is nothing to do.
#[cfg(unix)]
fn enforce_private_mode(path: &Path) -> Result<(), NodeError> {
    use std::os::unix::fs::PermissionsExt as _;
    let metadata = std::fs::metadata(path).map_err(|source| NodeError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o600 {
        tracing::warn!(
            path = %path.display(),
            mode = format!("{mode:o}"),
            "the identity file was not 0600; tightening it"
        );
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| NodeError::Io {
                path: path.to_path_buf(),
                source,
            },
        )?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn enforce_private_mode(_path: &Path) -> Result<(), NodeError> {
    Ok(())
}

/// Load the identity or tell the operator exactly how to get one. `run` and
/// `status` both go through here so neither can start with a half-identity.
pub fn ensure_paired(paths: &NodePaths) -> Result<Identity, NodeError> {
    Identity::load(paths)
}

/// What pairing produced: the identity, now on disk, and the node id to print.
#[derive(Debug)]
pub struct PairOutcome {
    pub identity: Identity,
    pub node_id: String,
    /// Where the identity was written.
    pub identity_path: std::path::PathBuf,
}

/// `POST /node/pair` (§2.3): generate a keypair, send the code plus the public
/// half and this machine's facts, pin what comes back.
///
/// A refused code is [`NodeError::PairingRejected`] carrying **the brain's own
/// reason** — expired, already used, unknown — because the node cannot tell
/// those apart by itself and the operator must not be left guessing. Nothing is
/// written to disk on a refusal: a half-paired identity would make the next
/// `run` fail with a signature error instead of "not paired".
pub async fn pair(
    brain_url: &str,
    code: &str,
    config: &NodeConfig,
) -> Result<PairOutcome, NodeError> {
    let paths = config.paths();
    // Normalize first: `abcd efgh`, `ABCD-EFGH` and `abcd-efgh` are one code
    // (§2.1), and refusing a look-alike `I`/`O` here would be worse than
    // mapping it the way the brain does.
    let code = normalize_code(code).map_err(|error| NodeError::config(error.to_string()))?;
    let keypair = NodeIdentity::generate_keypair()?;
    let request = PairRequest {
        code: code.clone(),
        public_key: URL_SAFE_NO_PAD.encode(keypair.verifying_key().as_bytes()),
        hostname: hostname(),
        platform: std::env::consts::OS.to_string(),
        arch: Some(std::env::consts::ARCH.to_string()),
        node_version: Some(NODE_VERSION.to_string()),
        protocol: Some(ProtocolRange::current()),
    };

    let endpoint = pair_endpoint(brain_url)?;
    let response = post_pair(&endpoint, &request).await?;
    let identity = Identity::new(NodeIdentity::from_pair_response(keypair, &response)?);

    // Create the directories *before* saving, so the very first `run` (and the
    // audit log's first line) has somewhere to live.
    let workspace = config.workspace();
    paths.ensure_dirs(&workspace)?;
    identity.save(&paths)?;
    remember_brain_url(config, brain_url);

    Ok(PairOutcome {
        node_id: identity.node_id().to_string(),
        identity_path: paths.identity(),
        identity,
    })
}

/// Remember the brain we paired with, so `swarmbot-node run` needs no `--brain`.
/// A failure here is a warning, not an error: the identity is already on disk
/// and pairing succeeded, so failing the command would make the operator think
/// it did not.
fn remember_brain_url(config: &NodeConfig, brain_url: &str) {
    match NodeConfig::load(&config.data_dir) {
        Ok(mut stored) => {
            if stored.brain_url.as_deref() == Some(brain_url) {
                return;
            }
            stored.brain_url = Some(brain_url.to_string());
            if let Err(error) = stored.save() {
                tracing::warn!(%error, "paired, but could not remember the brain URL in node.toml");
            }
        }
        Err(error) => {
            tracing::warn!(%error, "paired, but could not read node.toml to remember the brain URL");
        }
    }
}

/// `POST` the pairing request and turn any non-2xx into the brain's reason.
async fn post_pair(endpoint: &str, request: &PairRequest) -> Result<PairResponse, NodeError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| NodeError::Unreachable {
            url: endpoint.to_string(),
            reason: error.to_string(),
        })?;
    let response = client
        .post(endpoint)
        .json(request)
        .send()
        .await
        .map_err(|error| NodeError::Unreachable {
            url: endpoint.to_string(),
            reason: error.to_string(),
        })?;
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(NodeError::PairingRejected {
            status,
            reason: reason_from_body(status, &body),
        });
    }
    serde_json::from_str::<PairResponse>(&body).map_err(|error| {
        NodeError::Protocol(format!(
            "the brain answered {status} at {endpoint} with a body that is not a PairResponse: {error}"
        ))
    })
}

/// The brain's own explanation, if the body carries one. The documented
/// envelope is `{"error":{"code":…,"message":…}}`; other shapes are accepted
/// because a proxy or an older brain may answer differently, and falling back
/// to the raw body beats saying "unknown error" when the reason is right there.
fn reason_from_body(status: u16, body: &str) -> String {
    const MAX: usize = 400;
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        for pointer in ["/error/message", "/message", "/reason", "/error"] {
            if let Some(text) = value.pointer(pointer).and_then(Value::as_str)
                && !text.trim().is_empty()
            {
                return clip(text.trim(), MAX);
            }
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        format!("HTTP {status} with an empty body")
    } else {
        clip(trimmed, MAX)
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

/// The control-socket URL to dial for a brain URL (§3.1).
///
/// `http`→`ws`, `https`→`wss`, `ws`/`wss` pass through; the path becomes
/// `/node` unless the URL already ends in it, so both `https://brain.example`
/// and `ws://brain.example/node` work. Query and credentials are dropped: the
/// upgrade is authenticated by the signature, never by a token in a URL
/// (decision D1.3).
pub fn control_socket_url(brain_url: &str) -> Result<String, NodeError> {
    rewrite_url(brain_url, "ws", "wss", CONTROL_PATH)
}

/// The data-stream socket for one granted stream (§8): the same rewrite rules
/// as [`control_socket_url`], with the stream id appended.
///
/// The token is **not** part of this URL — it rides the `Authorization` header,
/// because a URL is written to every access log and the token is a credential.
pub fn stream_socket_url(brain_url: &str, stream_id: &str) -> Result<String, NodeError> {
    if stream_id.trim().is_empty() {
        return Err(NodeError::config(
            "a data stream needs a stream id; the brain's grant carried none".to_string(),
        ));
    }
    rewrite_url(
        brain_url,
        "ws",
        "wss",
        &format!("{STREAM_PATH}/{stream_id}"),
    )
}

/// The pairing endpoint for a brain URL (§2.3): same rules as
/// [`control_socket_url`], but the scheme stays `http(s)` because pairing is a
/// plain POST, not an upgrade.
pub fn pair_endpoint(brain_url: &str) -> Result<String, NodeError> {
    rewrite_url(brain_url, "http", "https", PAIR_PATH)
}

/// Map a brain URL onto one of this crate's fixed paths. A `ws`/`http` URL
/// becomes `plain`, a `wss`/`https` one becomes `secure`, so the caller decides
/// whether the result is a socket or an HTTP endpoint.
fn rewrite_url(
    brain_url: &str,
    plain: &str,
    secure: &str,
    path: &str,
) -> Result<String, NodeError> {
    let mut parsed = Url::parse(brain_url).map_err(|error| {
        NodeError::config(format!("`{brain_url}` is not a usable brain URL: {error}"))
    })?;
    let scheme = match parsed.scheme() {
        "http" | "ws" => plain.to_string(),
        "https" | "wss" => secure.to_string(),
        other => {
            return Err(NodeError::config(format!(
                "`{brain_url}` has scheme `{other}`; expected one of http, https, ws, wss"
            )));
        }
    };
    let already = parsed.path().trim_end_matches('/') == path.trim_end_matches('/');
    // `set_scheme` only fails for a URL that cannot be a base, and the scheme
    // match above already excluded those, so there is no error text to report.
    parsed.set_scheme(&scheme).map_err(|()| {
        NodeError::config(format!(
            "cannot rewrite `{brain_url}` to the scheme `{scheme}`"
        ))
    })?;
    if !already {
        parsed.set_path(path);
    }
    parsed.set_query(None);
    parsed.set_fragment(None);
    // `set_username`/`set_password` fail on cannot-be-a-base URLs, which the
    // scheme check above already excluded; ignoring the result would hide a
    // credential left in a URL, so drop it explicitly and report if we cannot.
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    Ok(parsed.to_string())
}

/// The URL to dial, given the config and the pinned identity: `--brain` wins,
/// else the origin recorded at pairing is turned back into a socket URL.
pub fn dial_url(config: &NodeConfig, identity: &Identity) -> Result<String, NodeError> {
    match &config.brain_url {
        Some(url) if !url.trim().is_empty() => control_socket_url(url),
        _ => {
            // `brain_origin_from_url` normalizes `http(s)`; the origin is
            // already an origin, so this only validates it before the scheme
            // is flipped to `ws(s)`.
            let origin = brain_origin_from_url(identity.brain_origin())
                .map_err(|error| NodeError::config(error.to_string()))?;
            control_socket_url(&origin)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::NodeId;

    fn temp_paths() -> (tempfile::TempDir, NodePaths) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(dir.path());
        (dir, paths)
    }

    fn paired(dir: &Path) -> Identity {
        let brain = ed25519_dalek::SigningKey::from_bytes(&[4u8; 32]);
        let response = PairResponse {
            node_id: NodeId::new(),
            brain_public_key: URL_SAFE_NO_PAD.encode(brain.verifying_key().as_bytes()),
            brain_origin: "https://brain.example".into(),
        };
        let keypair = NodeIdentity::generate_keypair().expect("keypair");
        let identity =
            Identity::new(NodeIdentity::from_pair_response(keypair, &response).expect("identity"));
        let paths = NodePaths::new(dir);
        identity.save(&paths).expect("saves");
        identity
    }

    #[test]
    fn an_identity_saves_at_0600_and_loads_back() {
        let (dir, paths) = temp_paths();
        let identity = paired(dir.path());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(paths.identity())
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the key file must be 0600");
        }
        let loaded = Identity::load(&paths).expect("loads");
        assert_eq!(loaded.node_id(), identity.node_id());
        assert_eq!(loaded.brain_origin(), identity.brain_origin());
        assert_eq!(
            loaded.signing_key().to_bytes(),
            identity.signing_key().to_bytes()
        );
        assert_eq!(loaded.fingerprint(), identity.fingerprint());
    }

    #[test]
    fn a_loose_mode_is_tightened_on_load() {
        let (dir, paths) = temp_paths();
        paired(dir.path());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(paths.identity(), std::fs::Permissions::from_mode(0o644))
                .expect("loosen");
            Identity::load(&paths).expect("loads");
            let mode = std::fs::metadata(paths.identity())
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "load must tighten a loose key file");
        }
    }

    #[test]
    fn a_missing_identity_says_how_to_get_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(dir.path());
        let error = ensure_paired(&paths).expect_err("not paired");
        assert!(matches!(error, NodeError::NotPaired { .. }), "{error:?}");
        assert!(error.to_string().contains("swarmbot-node pair"), "{error}");
    }

    #[test]
    fn debug_prints_a_fingerprint_and_never_the_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = paired(dir.path());
        let rendered = format!("{identity:?}");
        assert!(rendered.contains(identity.node_id().as_str()), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(rendered.contains(&identity.fingerprint()), "{rendered}");
        // Neither spelling of the secret may appear.
        let secret_b64 = URL_SAFE_NO_PAD.encode(identity.signing_key().to_bytes());
        let secret_hex = hex::encode(identity.signing_key().to_bytes());
        assert!(!rendered.contains(&secret_b64), "{rendered}");
        assert!(!rendered.contains(&secret_hex), "{rendered}");
        // Nor may the whole seed appear through the public key (a different
        // value, but the same class of leak).
        assert!(
            !rendered.contains(&identity.public_key_base64url()),
            "{rendered}"
        );
    }

    #[test]
    fn upgrade_headers_carry_the_node_id_and_no_secret() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = paired(dir.path());
        let (header, nonce) = identity.upgrade().expect("header");
        assert!(header.starts_with("SwarmBot-Node v1 id="), "{header}");
        assert!(header.contains(identity.node_id().as_str()), "{header}");
        assert!(!header.contains(&hex::encode(identity.signing_key().to_bytes())));
        assert_eq!(URL_SAFE_NO_PAD.decode(&nonce).expect("decodes").len(), 16);
        // A fresh nonce per dial: two calls must not repeat one.
        let (_, other) = identity.upgrade().expect("header");
        assert_ne!(nonce, other, "nonces must be fresh per dial");
    }

    #[test]
    fn brain_urls_map_onto_the_two_endpoints() {
        assert_eq!(
            control_socket_url("http://127.0.0.1:8787").expect("maps"),
            "ws://127.0.0.1:8787/node"
        );
        assert_eq!(
            control_socket_url("https://brain.example").expect("maps"),
            "wss://brain.example/node"
        );
        assert_eq!(
            control_socket_url("wss://brain.example/some/base").expect("maps"),
            "wss://brain.example/node",
            "a path that is not /node is replaced, not appended to"
        );
        assert_eq!(
            control_socket_url("ws://brain.example/node").expect("maps"),
            "ws://brain.example/node"
        );
        assert_eq!(
            pair_endpoint("wss://brain.example").expect("maps"),
            "https://brain.example/node/pair"
        );
        assert_eq!(
            pair_endpoint("http://127.0.0.1:1/node/pair").expect("maps"),
            "http://127.0.0.1:1/node/pair"
        );
        // A credential in the URL is dropped: the signature authenticates the
        // dial, and a token in a URL is exactly what D1.3 forbids.
        let mapped = control_socket_url("https://user:pass@brain.example").expect("maps");
        assert!(!mapped.contains("pass"), "{mapped}");
        assert_eq!(mapped, "wss://brain.example/node");
        assert!(control_socket_url("ftp://brain.example").is_err());
        assert!(control_socket_url("not a url").is_err());
    }

    #[test]
    fn the_dial_url_prefers_the_config_and_falls_back_to_the_pin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = paired(dir.path());
        let mut config = NodeConfig::new_in(dir.path());
        // No brain URL configured: the pinned origin decides.
        assert_eq!(
            dial_url(&config, &identity).expect("derives"),
            "wss://brain.example/node"
        );
        config.brain_url = Some("http://127.0.0.1:9".into());
        assert_eq!(
            dial_url(&config, &identity).expect("derives"),
            "ws://127.0.0.1:9/node",
            "--brain wins: an owner may dial through a proxy"
        );
    }

    #[test]
    fn a_refused_pairing_carries_the_brains_own_reason() {
        assert_eq!(
            reason_from_body(
                410,
                r#"{"error":{"code":"gone","message":"pairing code expired"}}"#
            ),
            "pairing code expired"
        );
        assert_eq!(
            reason_from_body(409, r#"{"message":"code already used"}"#),
            "code already used"
        );
        assert_eq!(reason_from_body(502, "bad gateway"), "bad gateway");
        assert_eq!(reason_from_body(500, "  "), "HTTP 500 with an empty body");
        let long = "x".repeat(1000);
        assert!(reason_from_body(500, &long).chars().count() <= 401);
    }
}
