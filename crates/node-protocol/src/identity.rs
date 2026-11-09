//! Ed25519 identities for both sides of the wire (§2.2).
//!
//! The node's private key never leaves the node; it is stored as JSON with
//! mode 0600 so no other local user can lift it. All binary values (keys,
//! signatures) are base64url **without padding** (§2).
//!
//! Deviation from the reference protocol: OpenBot stores the node key as
//! base64url PKCS#8 (WebCrypto's export format). We store the raw 32-byte
//! Ed25519 secret seed, which is what `ed25519-dalek` uses natively; the two
//! are trivially convertible but the files are not byte-identical.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::TryRngCore as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use swarmbot_contracts::{NodeId, PairResponse};

/// Errors around identity material: storage, encoding or a bad pinned key.
#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("identity file error: {0}")]
    Io(#[from] std::io::Error),

    #[error("identity file is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("the system RNG failed: {reason}")]
    Rng { reason: String },

    #[error("expected a 32-byte base64url key, got {len} bytes")]
    KeyLength { len: usize },

    #[error("not a valid Ed25519 public key: {reason}")]
    KeyEncoding { reason: String },

    #[error("not a valid node id: {0}")]
    InvalidNodeId(#[from] swarmbot_contracts::InvalidId),

    #[error("brain origin must not be empty")]
    InvalidOrigin,
}

/// Encode a public key as base64url without padding (§2).
pub fn public_key_base64url(key: &VerifyingKey) -> String {
    URL_SAFE_NO_PAD.encode(key.as_bytes())
}

/// Decode a base64url (no padding) public key. A wrong length or an invalid
/// curve point is a typed error, never a panic.
pub fn parse_public_key_base64url(encoded: &str) -> Result<VerifyingKey, IdentityError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|e| IdentityError::KeyEncoding {
            reason: e.to_string(),
        })?;
    let bytes: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| IdentityError::KeyLength { len: bytes.len() })?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| IdentityError::KeyEncoding {
        reason: e.to_string(),
    })
}

/// Fill `buf` from the operating system's CSPRNG. Keys are long-lived
/// secrets, so a deterministic or seeded RNG is never acceptable here.
fn fill_random(buf: &mut [u8]) -> Result<(), IdentityError> {
    rand::rngs::OsRng
        .try_fill_bytes(buf)
        .map_err(|e| IdentityError::Rng {
            reason: e.to_string(),
        })
}

fn secret_key_base64url(key: &SigningKey) -> String {
    URL_SAFE_NO_PAD.encode(key.to_bytes())
}

fn parse_secret_key_base64url(encoded: &str) -> Result<SigningKey, IdentityError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|e| IdentityError::KeyEncoding {
            reason: e.to_string(),
        })?;
    let bytes: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| IdentityError::KeyLength { len: bytes.len() })?;
    Ok(SigningKey::from_bytes(&bytes))
}

/// Write `contents` to `path`, then force mode 0600 (§2.2).
///
/// The file is created first and the permissions are set right after with
/// `std::fs::set_permissions`. On Windows there is no mode bits API; the file
/// keeps the ACL it inherits from its directory, so identity files belong in
/// a user-protected directory there.
fn write_private(path: &Path, contents: &str) -> Result<(), IdentityError> {
    std::fs::write(path, contents)?;
    set_private_permissions(path)
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<(), IdentityError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<(), IdentityError> {
    // Windows keeps the ACL inherited from the parent directory; there is
    // no portable mode-bits equivalent, and nothing to fail here.
    Ok(())
}

/// The node's persisted identity: its own Ed25519 key plus the brain material
/// it **pinned** at pairing time (§2.3). Pinning is what makes the signed
/// upgrade and `node.hello` meaningful — a DNS or proxy hijack cannot present
/// a different brain key.
#[derive(Clone, Debug)]
pub struct NodeIdentity {
    pub keypair: SigningKey,
    pub node_id: NodeId,
    pub brain_public_key: VerifyingKey,
    /// `scheme://host[:port]` — the origin every signature is bound to.
    pub brain_origin: String,
}

/// The JSON shape of a saved node identity (mode 0600).
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeIdentityFile {
    node_id: String,
    secret_key: String,
    brain_public_key: String,
    brain_origin: String,
}

impl NodeIdentity {
    /// A fresh keypair and node id, pinning the given brain key and origin.
    /// Use this once the brain's key is already known (e.g. re-keying); the
    /// first-time flow goes through [`NodeIdentity::from_pair_response`].
    pub fn generate(
        brain_public_key: VerifyingKey,
        brain_origin: impl Into<String>,
    ) -> Result<NodeIdentity, IdentityError> {
        let brain_origin = brain_origin.into();
        if brain_origin.is_empty() {
            return Err(IdentityError::InvalidOrigin);
        }
        Ok(NodeIdentity {
            keypair: SigningKey::from_bytes(&new_secret_seed()?),
            node_id: NodeId::new(),
            brain_public_key,
            brain_origin,
        })
    }

    /// Build the identity from a completed pairing (§2.3): our fresh keypair
    /// plus the `PairResponse` the brain returned, which we now pin.
    pub fn from_pair_response(
        keypair: SigningKey,
        response: &PairResponse,
    ) -> Result<NodeIdentity, IdentityError> {
        if response.brain_origin.is_empty() {
            return Err(IdentityError::InvalidOrigin);
        }
        Ok(NodeIdentity {
            keypair,
            node_id: NodeId::parse(response.node_id.as_str())?,
            brain_public_key: parse_public_key_base64url(&response.brain_public_key)?,
            brain_origin: response.brain_origin.clone(),
        })
    }

    /// A brand-new keypair, for the pairing request that precedes an
    /// identity.
    pub fn generate_keypair() -> Result<SigningKey, IdentityError> {
        Ok(SigningKey::from_bytes(&new_secret_seed()?))
    }

    pub fn public_key_base64url(&self) -> String {
        public_key_base64url(&self.keypair.verifying_key())
    }

    /// Save as JSON with mode 0600 (on Unix; see [`write_private`]).
    pub fn save(&self, path: &Path) -> Result<(), IdentityError> {
        let file = NodeIdentityFile {
            node_id: self.node_id.to_string(),
            secret_key: secret_key_base64url(&self.keypair),
            brain_public_key: public_key_base64url(&self.brain_public_key),
            brain_origin: self.brain_origin.clone(),
        };
        write_private(path, &serde_json::to_string(&file)?)
    }

    pub fn load(path: &Path) -> Result<NodeIdentity, IdentityError> {
        let file: NodeIdentityFile = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        Ok(NodeIdentity {
            keypair: parse_secret_key_base64url(&file.secret_key)?,
            node_id: NodeId::parse(file.node_id)?,
            brain_public_key: parse_public_key_base64url(&file.brain_public_key)?,
            brain_origin: file.brain_origin,
        })
    }
}

fn new_secret_seed() -> Result<[u8; 32], IdentityError> {
    let mut seed = [0u8; 32];
    fill_random(&mut seed)?;
    Ok(seed)
}

/// The brain's own Ed25519 identity: the key it signs `node.hello` with
/// (§3.2) and whose public half it hands out at pairing.
#[derive(Clone, Debug)]
pub struct BrainIdentity {
    pub keypair: SigningKey,
    /// The brain's public origin (`scheme://host[:port]`), bound into every
    /// signature so a relayed signature cannot be replayed against another
    /// origin.
    pub origin: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrainIdentityFile {
    secret_key: String,
    origin: String,
}

impl BrainIdentity {
    pub fn generate(origin: impl Into<String>) -> Result<BrainIdentity, IdentityError> {
        let origin = origin.into();
        if origin.is_empty() {
            return Err(IdentityError::InvalidOrigin);
        }
        Ok(BrainIdentity {
            keypair: SigningKey::from_bytes(&new_secret_seed()?),
            origin,
        })
    }

    pub fn new(
        keypair: SigningKey,
        origin: impl Into<String>,
    ) -> Result<BrainIdentity, IdentityError> {
        let origin = origin.into();
        if origin.is_empty() {
            return Err(IdentityError::InvalidOrigin);
        }
        Ok(BrainIdentity { keypair, origin })
    }

    pub fn public_key_base64url(&self) -> String {
        public_key_base64url(&self.keypair.verifying_key())
    }

    /// Save as JSON with mode 0600 (on Unix; see [`write_private`]).
    pub fn save(&self, path: &Path) -> Result<(), IdentityError> {
        let file = BrainIdentityFile {
            secret_key: secret_key_base64url(&self.keypair),
            origin: self.origin.clone(),
        };
        write_private(path, &serde_json::to_string(&file)?)
    }

    pub fn load(path: &Path) -> Result<BrainIdentity, IdentityError> {
        let file: BrainIdentityFile = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        Ok(BrainIdentity {
            keypair: parse_secret_key_base64url(&file.secret_key)?,
            origin: file.origin,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "swarmbot-identity-{tag}-{}.json",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn public_keys_round_trip_through_base64url() {
        let key = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let encoded = public_key_base64url(&key);
        // 32 bytes → 43 base64url chars, no padding (§2).
        assert_eq!(encoded.len(), 43);
        assert!(!encoded.contains('='));
        assert!(
            encoded
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(parse_public_key_base64url(&encoded).unwrap(), key);
    }

    #[test]
    fn wrong_length_keys_are_typed_errors() {
        let too_short = URL_SAFE_NO_PAD.encode([1u8; 16]);
        assert!(matches!(
            parse_public_key_base64url(&too_short),
            Err(IdentityError::KeyLength { len: 16 })
        ));
        let too_long = URL_SAFE_NO_PAD.encode([1u8; 64]);
        assert!(matches!(
            parse_public_key_base64url(&too_long),
            Err(IdentityError::KeyLength { len: 64 })
        ));
        assert!(matches!(
            parse_public_key_base64url("not base64!"),
            Err(IdentityError::KeyEncoding { .. })
        ));
    }

    #[test]
    fn node_identity_saves_and_loads_with_mode_0600() {
        let brain_key = SigningKey::from_bytes(&[3u8; 32]).verifying_key();
        let identity = NodeIdentity::generate(brain_key, "https://brain.example").unwrap();
        let path = temp_path("node");
        identity.save(&path).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "identity file must be 0600");
        }

        let loaded = NodeIdentity::load(&path).unwrap();
        assert_eq!(loaded.node_id, identity.node_id);
        assert_eq!(loaded.brain_origin, identity.brain_origin);
        assert_eq!(loaded.brain_public_key, identity.brain_public_key);
        assert_eq!(loaded.keypair.to_bytes(), identity.keypair.to_bytes());
        std::fs::remove_file(&path).unwrap();

        // A missing file is a typed error, not a panic.
        assert!(matches!(
            NodeIdentity::load(&path),
            Err(IdentityError::Io(_))
        ));
    }

    #[test]
    fn brain_identity_saves_and_loads_with_mode_0600() {
        let identity = BrainIdentity::generate("https://brain.example").unwrap();
        let path = temp_path("brain");
        identity.save(&path).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let loaded = BrainIdentity::load(&path).unwrap();
        assert_eq!(loaded.origin, identity.origin);
        assert_eq!(loaded.keypair.to_bytes(), identity.keypair.to_bytes());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn identities_from_pairing_pin_the_brain() {
        let brain = BrainIdentity::generate("https://brain.example").unwrap();
        let response = PairResponse {
            node_id: NodeId::new(),
            brain_public_key: brain.public_key_base64url(),
            brain_origin: "https://brain.example".into(),
        };
        let keypair = NodeIdentity::generate_keypair().unwrap();
        let identity = NodeIdentity::from_pair_response(keypair.clone(), &response).unwrap();
        assert_eq!(identity.node_id, response.node_id);
        assert_eq!(identity.brain_public_key, brain.keypair.verifying_key());

        // A truncated brain key in the response is a typed error.
        let bad = PairResponse {
            brain_public_key: "c2hvcnQ".into(),
            ..response
        };
        assert!(matches!(
            NodeIdentity::from_pair_response(keypair, &bad),
            Err(IdentityError::KeyLength { .. })
        ));
    }

    #[test]
    fn empty_origins_are_refused() {
        let key = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
        assert!(matches!(
            NodeIdentity::generate(key, ""),
            Err(IdentityError::InvalidOrigin)
        ));
        assert!(matches!(
            BrainIdentity::generate(""),
            Err(IdentityError::InvalidOrigin)
        ));
    }
}
