//! Nodes: the machines that execute tools ("the hands").
//!
//! Two invariants shape every type here:
//!
//! * a node enforces its **own** policy — the brain can ask, never raise a
//!   ceiling;
//! * capabilities are **declared** by the node and **approved** by the owner,
//!   and the effective set is the intersection, so a newly declared capability
//!   stays off until somebody says yes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::id::NodeId;

/// Where a node runs relative to the brain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// In-process with the brain; no socket, no pairing.
    Local,
    /// A separate daemon that dialed out to the brain.
    Remote,
}

/// Access mode of one root directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootMode {
    ReadOnly,
    ReadWrite,
}

/// One directory a node will act inside.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRoot {
    pub id: String,
    pub path: String,
    pub mode: RootMode,
}

impl NodeRoot {
    pub fn read_write(id: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            path: path.into(),
            mode: RootMode::ReadWrite,
        }
    }

    pub fn read_only(id: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            path: path.into(),
            mode: RootMode::ReadOnly,
        }
    }
}

/// The strongest thing a node will do without asking, per surface.
///
/// Ordered `Never < Ask < Always`; taking the minimum of two ceilings is how
/// "the brain cannot raise a node's ceiling" is implemented.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyCeiling {
    /// Refuse outright.
    Never,
    /// Ask the owner (through the brain) every time. Asking is the default:
    /// a ceiling nobody set must not turn into permission nobody gave.
    #[default]
    Ask,
    /// Run without asking.
    Always,
}

impl PolicyCeiling {
    /// The stricter of two ceilings.
    pub fn min(self, other: Self) -> Self {
        if self <= other { self } else { other }
    }

    pub fn allows_auto(self) -> bool {
        matches!(self, Self::Always)
    }

    pub fn refuses(self) -> bool {
        matches!(self, Self::Never)
    }
}

/// A policy surface: what kind of action a ceiling governs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    Shell,
    ReadFile,
    WriteFile,
    EditFile,
    Computer,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::ReadFile => "read_file",
            Self::WriteFile => "write_file",
            Self::EditFile => "edit_file",
            Self::Computer => "computer",
        }
    }
}

impl std::fmt::Display for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-surface ceilings. Missing surfaces fall back to [`PolicyCeiling::Ask`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyMap {
    #[serde(default)]
    pub surfaces: BTreeMap<Surface, PolicyCeiling>,
    pub default: PolicyCeiling,
}

impl Default for PolicyMap {
    fn default() -> Self {
        Self {
            surfaces: BTreeMap::new(),
            default: PolicyCeiling::Ask,
        }
    }
}

impl PolicyMap {
    /// The ceiling for one surface: an explicit entry, else the default.
    /// A per-surface entry may be stricter *or* looser than the default —
    /// it is the node (or its owner) who sets it, never a remote brain.
    pub fn get(&self, surface: Surface) -> PolicyCeiling {
        self.surfaces.get(&surface).copied().unwrap_or(self.default)
    }

    pub fn set(&mut self, surface: Surface, ceiling: PolicyCeiling) {
        self.surfaces.insert(surface, ceiling);
    }

    /// One ceiling for every surface (used by `--ceiling never`).
    pub fn uniform(ceiling: PolicyCeiling) -> Self {
        Self {
            surfaces: BTreeMap::new(),
            default: ceiling,
        }
    }
}

/// Versioned capability names, e.g. `fs@1`, `shell@1`, `pty@1`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Capabilities(Vec<String>);

impl Capabilities {
    pub fn new(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(names.into_iter().map(Into::into).collect())
    }

    /// A bare name means `@1`.
    pub fn has(&self, name: &str) -> bool {
        let full = if name.contains('@') {
            name.to_string()
        } else {
            format!("{name}@1")
        };
        self.0.iter().any(|c| *c == full || *c == name)
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// declared ∩ approved — the set the brain actually uses.
    pub fn effective(&self, approved: &Capabilities) -> Capabilities {
        Capabilities(
            self.0
                .iter()
                .filter(|c| approved.0.contains(c))
                .cloned()
                .collect(),
        )
    }
}

/// A node as the roster knows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: NodeId,
    pub label: String,
    pub kind: NodeKind,
    pub hostname: String,
    pub platform: String,
    pub arch: String,
    /// The daemon's own version string.
    pub version: String,
    pub protocol: u32,
    /// What the node says it can do.
    pub declared: Capabilities,
    /// What the owner turned on.
    pub approved: Capabilities,
    pub roots: Vec<NodeRoot>,
    pub policy: PolicyMap,
    pub online: bool,
    pub paired_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    /// True once the node's key was revoked: it may not redial.
    pub revoked: bool,
}

impl NodeInfo {
    /// declared ∩ approved.
    pub fn capabilities(&self) -> Capabilities {
        self.declared.effective(&self.approved)
    }

    pub fn can(&self, capability: &str) -> bool {
        self.online && !self.revoked && self.capabilities().has(capability)
    }

    pub fn local(label: impl Into<String>, roots: Vec<NodeRoot>, policy: PolicyMap) -> Self {
        let now = chrono::Utc::now();
        Self {
            id: NodeId::local(),
            label: label.into(),
            kind: NodeKind::Local,
            hostname: "brain".into(),
            platform: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            protocol: crate::PROTOCOL_VERSION,
            declared: Capabilities::new(["fs@1", "shell@1"]),
            approved: Capabilities::new(["fs@1", "shell@1"]),
            roots,
            policy,
            online: true,
            paired_at: now,
            last_seen_at: Some(now),
            revoked: false,
        }
    }
}

/// A pending pairing code (the record the brain keeps, never the code itself).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairingRecord {
    /// `sha256("<prefix>|<code>")`, hex. The plain code is shown once.
    pub code_hash: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub used: bool,
    pub node_id: Option<NodeId>,
}

impl PairingRecord {
    pub fn is_live(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        !self.used && now < self.expires_at
    }
}

/// `POST /node/pair` request body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRequest {
    pub code: String,
    /// base64url, no padding: the raw 32-byte Ed25519 public key.
    pub public_key: String,
    pub hostname: String,
    pub platform: String,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub node_version: Option<String>,
    #[serde(default)]
    pub protocol: Option<ProtocolRange>,
}

/// `POST /node/pair` response body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairResponse {
    pub node_id: NodeId,
    pub brain_public_key: String,
    pub brain_origin: String,
}

/// A protocol version range, so brain and node can negotiate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolRange {
    pub min: u32,
    pub max: u32,
}

impl ProtocolRange {
    pub fn current() -> Self {
        Self {
            min: crate::PROTOCOL_VERSION,
            max: crate::PROTOCOL_VERSION,
        }
    }

    /// The highest version both sides speak, if any.
    pub fn negotiate(self, other: ProtocolRange) -> Option<u32> {
        let low = self.min.max(other.min);
        let high = self.max.min(other.max);
        (low <= high).then_some(high)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceilings_order_and_combine() {
        assert!(PolicyCeiling::Never < PolicyCeiling::Ask);
        assert!(PolicyCeiling::Ask < PolicyCeiling::Always);
        assert_eq!(
            PolicyCeiling::Always.min(PolicyCeiling::Ask),
            PolicyCeiling::Ask
        );
        assert!(PolicyCeiling::Always.allows_auto());
        assert!(PolicyCeiling::Never.refuses());
        assert_eq!(PolicyCeiling::default(), PolicyCeiling::Ask);
    }

    #[test]
    fn policy_map_falls_back_to_its_default() {
        let mut map = PolicyMap::default();
        assert_eq!(map.get(Surface::Shell), PolicyCeiling::Ask);
        map.set(Surface::Shell, PolicyCeiling::Always);
        assert_eq!(map.get(Surface::Shell), PolicyCeiling::Always);
        // A surface entry may also be stricter than the default.
        map.set(Surface::WriteFile, PolicyCeiling::Never);
        assert_eq!(map.get(Surface::WriteFile), PolicyCeiling::Never);
        assert_eq!(map.get(Surface::ReadFile), PolicyCeiling::Ask);
        // `uniform` sets the default for every surface not listed.
        let locked = PolicyMap::uniform(PolicyCeiling::Never);
        assert_eq!(locked.get(Surface::Shell), PolicyCeiling::Never);
    }

    #[test]
    fn capabilities_intersect() {
        let declared = Capabilities::new(["fs@1", "shell@1", "pty@1"]);
        let approved = Capabilities::new(["fs@1", "shell@1"]);
        let effective = declared.effective(&approved);
        assert!(effective.has("fs"));
        assert!(effective.has("shell@1"));
        assert!(!effective.has("pty@1"));
    }

    #[test]
    fn local_node_is_online_and_capable() {
        let node = NodeInfo::local(
            "this machine",
            vec![NodeRoot::read_write("work", "/tmp/work")],
            PolicyMap::default(),
        );
        assert!(node.id.is_local());
        assert!(node.can("shell"));
        assert!(!node.can("desktop"));
        let mut offline = node.clone();
        offline.online = false;
        assert!(!offline.can("shell"));
    }

    #[test]
    fn protocol_negotiation_picks_the_highest_common() {
        let a = ProtocolRange { min: 1, max: 2 };
        let b = ProtocolRange { min: 2, max: 3 };
        assert_eq!(a.negotiate(b), Some(2));
        let c = ProtocolRange { min: 5, max: 6 };
        assert_eq!(a.negotiate(c), None);
        assert_eq!(
            ProtocolRange::current().negotiate(ProtocolRange::current()),
            Some(crate::PROTOCOL_VERSION)
        );
    }

    #[test]
    fn pairing_records_expire() {
        let now = chrono::Utc::now();
        let record = PairingRecord {
            code_hash: "aa".into(),
            created_at: now,
            expires_at: now + chrono::Duration::minutes(10),
            used: false,
            node_id: None,
        };
        assert!(record.is_live(now));
        assert!(!record.is_live(now + chrono::Duration::minutes(11)));
        let used = PairingRecord {
            used: true,
            ..record
        };
        assert!(!used.is_live(now));
    }
}
