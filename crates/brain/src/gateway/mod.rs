//! The node gateway: which nodes are connected, and how the orchestration
//! reaches them.
//!
//! Three pieces, and the reason each is separate:
//!
//! * [`NodeGateway`] — the live roster of connected nodes plus the two facts a
//!   dial has to be checked against: the brain origin both signatures are bound
//!   to, and the nonce replay cache. It exists *before* the swarm does, because
//!   the swarm's [`swarmbot_tools::NodeExecutor`] needs something to look a peer
//!   up in, and a peer needs the swarm to answer approvals. Breaking that cycle
//!   with an empty registry that is filled in later is what keeps `compose` a
//!   one-shot build.
//! * [`socket`] — the three-method [`node_protocol::mux::Socket`] adapter over
//!   axum's WebSocket.
//! * [`session`] — one connected node: the signed upgrade, `node.hello`, the
//!   presence row, and the brain-served methods.
//! * [`executor`] — the [`swarmbot_tools::NodeExecutor`] that routes a call to the
//!   local machine or across a socket.
//!
//! Nothing here trusts a node id from a payload: the id a connection is served
//! under comes from the verified upgrade header, and every frame the brain sends
//! is stamped with `x-swarmbot-node-id` so the node can check who it is talking
//! to (the node stamps nothing under that prefix itself).

pub mod executor;
pub mod session;
pub mod socket;
pub mod streams;
pub mod wire;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use chrono::{DateTime, Utc};
use tracing::{debug, info};

use swarmbot_contracts::id::{ConversationId, NodeId};
use swarmbot_contracts::node::Surface;
use swarmbot_contracts::request::Decision;
use swarmbot_node_protocol::auth::ReplayCache;
use swarmbot_node_protocol::mux::{Peer, Socket};

use self::socket::GatewaySocket;
use self::streams::StreamHub;

/// The mux peer driving one node's control socket.
pub type NodePeer = Peer<GatewaySocket>;

/// A node that is connected right now.
///
/// Generic over the socket only so the registry's own rules (one live socket per
/// node id, generation-guarded unregistration) can be tested over an in-memory
/// pair; production code always means `GatewaySocket`, which is the default.
pub struct Connected<S: Socket = GatewaySocket> {
    pub node_id: NodeId,
    pub peer: Peer<S>,
    /// Monotonic per registration: a session that ends must only unregister
    /// *itself*, never the connection that replaced it.
    pub generation: u64,
    pub connected_at: DateTime<Utc>,
    /// The protocol version `node.hello` negotiated.
    pub protocol: u32,
    /// The node's hostname, replaced by the owner's `--label` when
    /// `node.connected` carries one.
    pub label: String,
}

/// How long an owner's decision stays reusable for a node's repeat ask.
///
/// The window is short on purpose: it exists to cover the milliseconds between
/// the brain's own card and the node's ask for the same action, not to become a
/// standing allowance. Anything older is asked again.
pub const RECENT_DECISION_WINDOW: chrono::Duration = chrono::Duration::seconds(120);

/// How many decisions are remembered. A single owner answering cards cannot
/// outrun this, and the oldest entries are the least useful.
const RECENT_DECISION_CAPACITY: usize = 64;

/// One owner decision, remembered so a node's repeat ask for the identical
/// action does not need a second card.
#[derive(Clone, Debug)]
pub struct RecordedDecision {
    pub surface: Surface,
    pub target: String,
    pub decision: Decision,
    pub at: DateTime<Utc>,
    /// Where the decision was made, so reusing it can be written down there.
    pub conversation: ConversationId,
}

// Hand-written rather than derived: `#[derive(Clone)]` would add a `S: Clone`
// bound, and the socket lives inside the driver task — only the `Peer` handle,
// which is cheap to clone, is held here.
impl<S: Socket> Clone for Connected<S> {
    fn clone(&self) -> Self {
        Self {
            node_id: self.node_id.clone(),
            peer: self.peer.clone(),
            generation: self.generation,
            connected_at: self.connected_at,
            protocol: self.protocol,
            label: self.label.clone(),
        }
    }
}

/// The gateway's live state.
pub struct NodeGateway<S: Socket = GatewaySocket> {
    /// The origin both signatures are bound to (§2.3). A node signs over the
    /// origin it *pinned at pairing*, so this has to be the same string the
    /// pairing response carried — not the host the node happened to dial.
    origin: String,
    replay: ReplayCache,
    nodes: RwLock<HashMap<NodeId, Connected<S>>>,
    generations: AtomicU64,
    decisions: Mutex<VecDeque<RecordedDecision>>,
    /// The data-stream half (§8): grants waiting for a dial, and the tickets
    /// waiting for bytes. It lives on the gateway because it is the same
    /// lifetime and the same origin as the control sockets — one brain, one
    /// set of streams.
    streams: StreamHub,
}

impl<S: Socket> NodeGateway<S> {
    pub fn new(origin: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            origin: origin.into(),
            replay: ReplayCache::new(),
            nodes: RwLock::new(HashMap::new()),
            generations: AtomicU64::new(0),
            decisions: Mutex::new(VecDeque::new()),
            streams: StreamHub::new(),
        })
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The data-stream registry (§8).
    pub fn streams(&self) -> &StreamHub {
        &self.streams
    }

    /// The nonce cache, shared by every dial: a nonce replayed on a second
    /// connection is still a replay.
    pub fn replay(&self) -> &ReplayCache {
        &self.replay
    }

    /// The generation a new registration will carry.
    pub fn next_generation(&self) -> u64 {
        self.generations.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Record a connection, returning the one it replaced so the caller can close
    /// it with [`swarmbot_node_protocol::CLOSE_REPLACED`]: one node id, one live socket.
    pub fn register(&self, mut connected: Connected<S>) -> Option<Connected<S>> {
        if connected.generation == 0 {
            connected.generation = self.next_generation();
        }
        let replaced = self
            .nodes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(connected.node_id.clone(), connected.clone());
        info!(
            node = %connected.node_id,
            label = %connected.label,
            generation = connected.generation,
            replaced = replaced.is_some(),
            "a node is connected"
        );
        replaced
    }

    /// Drop a connection, but only if it is still the registered one.
    pub fn unregister(&self, node: &NodeId, generation: u64) -> Option<Connected<S>> {
        let mut nodes = self.nodes.write().unwrap_or_else(|e| e.into_inner());
        if nodes
            .get(node)
            .is_some_and(|current| current.generation != generation)
        {
            debug!(
                node = %node,
                generation,
                "an ended session did not unregister: a newer connection took over"
            );
            return None;
        }
        nodes.remove(node)
    }

    /// The peer for a node, when it is connected and its socket is still open.
    pub fn peer(&self, node: &NodeId) -> Option<Peer<S>> {
        self.nodes
            .read()
            .ok()?
            .get(node)
            .filter(|connected| !connected.peer.is_closed())
            .map(|connected| connected.peer.clone())
    }

    /// Every connected node, by id: what the roster and the tests look at.
    pub fn connected(&self) -> Vec<Connected<S>> {
        let Ok(nodes) = self.nodes.read() else {
            return Vec::new();
        };
        let mut out: Vec<Connected<S>> = nodes.values().cloned().collect();
        out.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        out
    }

    /// Remember what the owner decided about one action.
    ///
    /// Called by `POST /api/requests/{id}/respond`, which is the only way an
    /// owner answers a card, so this is a record of real decisions rather than a
    /// cache of guesses.
    pub fn note_owner_decision(&self, decision: RecordedDecision) {
        let mut decisions = self.decisions.lock().unwrap_or_else(|e| e.into_inner());
        // The same action decided twice is one entry: the newest answer is the one
        // a repeat ask should honour.
        decisions.retain(|recorded| {
            recorded.surface != decision.surface || recorded.target != decision.target
        });
        decisions.push_back(decision);
        while decisions.len() > RECENT_DECISION_CAPACITY {
            decisions.pop_front();
        }
    }

    /// The owner's decision about this exact action, if it is still recent.
    pub fn recent_decision(&self, surface: Surface, target: &str) -> Option<RecordedDecision> {
        let now = Utc::now();
        let mut decisions = self.decisions.lock().unwrap_or_else(|e| e.into_inner());
        // Drop what has aged out while we are here: the window is what makes this
        // a deduplication of one exchange rather than a standing allowance.
        decisions.retain(|recorded| now - recorded.at <= RECENT_DECISION_WINDOW);
        decisions
            .iter()
            .find(|recorded| recorded.surface == surface && recorded.target == target)
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.nodes.read().map(|nodes| nodes.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<S: Socket> std::fmt::Debug for NodeGateway<S> {
    /// Manual: a `Peer` is a channel handle with no meaningful `Debug`, and the
    /// origin is the one string a signature is bound to — worth printing, since a
    /// mismatch there is otherwise invisible.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeGateway")
            .field("origin", &self.origin)
            .field("connected", &self.len())
            .field("nonces", &self.replay.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_node_protocol::mux::PeerConfig;
    use swarmbot_node_protocol::testing::{LineSocket, duplex_pair};

    /// The socket type the registry tests drive: an in-memory pair, no ports.
    type TestSocket = LineSocket<tokio::io::DuplexStream>;
    type TestGateway = NodeGateway<TestSocket>;

    /// A registration plus the far end of its socket, which has to stay alive or
    /// the peer sees EOF and closes at once.
    fn connection(gateway: &TestGateway, node: &NodeId) -> (Connected<TestSocket>, TestSocket) {
        let (mine, theirs) = duplex_pair(64 * 1024);
        (
            Connected {
                node_id: node.clone(),
                peer: Peer::new(mine, PeerConfig::default()),
                generation: gateway.next_generation(),
                connected_at: Utc::now(),
                protocol: swarmbot_node_protocol::PROTOCOL_VERSION,
                label: "test node".to_string(),
            },
            theirs,
        )
    }

    #[tokio::test]
    async fn registering_a_node_twice_replaces_the_first_connection() {
        let gateway = TestGateway::new("http://127.0.0.1:8787");
        let node = NodeId::new();
        let (first, keep_first) = connection(&gateway, &node);
        let (second, keep_second) = connection(&gateway, &node);
        assert_ne!(first.generation, second.generation, "generations differ");

        assert!(
            gateway.register(first.clone()).is_none(),
            "nothing was there"
        );
        assert_eq!(gateway.len(), 1);
        assert_eq!(gateway.connected()[0].label, "test node");

        let replaced = gateway.register(second.clone()).expect("the first");
        assert_eq!(replaced.generation, first.generation);
        assert_eq!(gateway.len(), 1, "one node id, one live socket");

        // The ended first session must not unregister its replacement: a node
        // that redials while its old socket is still being torn down stays up.
        assert!(gateway.unregister(&node, first.generation).is_none());
        assert_eq!(gateway.len(), 1);
        assert!(gateway.unregister(&node, second.generation).is_some());
        assert!(gateway.is_empty());
        assert!(gateway.peer(&node).is_none());

        drop((keep_first, keep_second));
    }

    #[tokio::test]
    async fn a_closed_peer_is_not_handed_out() {
        let gateway = TestGateway::new("http://127.0.0.1:1");
        let node = NodeId::new();
        let (connection, keep) = connection(&gateway, &node);
        let peer = connection.peer.clone();
        gateway.register(connection);
        assert!(gateway.peer(&node).is_some());

        // An explicit close is the polite goodbye; the roster must stop handing
        // out a peer that can never answer again.
        peer.close(4000, "test").await;
        assert!(peer.is_closed());
        assert!(
            gateway.peer(&node).is_none(),
            "a closed peer is filtered out of lookups"
        );
        drop(keep);
    }

    #[test]
    fn an_owner_decision_is_reusable_briefly_and_only_for_that_action() {
        let gateway = TestGateway::new("http://127.0.0.1:1");
        let conversation = ConversationId::new();
        let record = |surface: Surface, target: &str, at: DateTime<Utc>| RecordedDecision {
            surface,
            target: target.to_string(),
            decision: Decision::AllowOnce,
            at,
            conversation: conversation.clone(),
        };
        gateway.note_owner_decision(record(Surface::Shell, "cargo test", Utc::now()));

        let found = gateway
            .recent_decision(Surface::Shell, "cargo test")
            .expect("the same action, moments ago");
        assert_eq!(found.conversation, conversation);
        // A different action, or the same one on another surface, is not the same
        // question and must be asked.
        assert!(
            gateway
                .recent_decision(Surface::Shell, "rm -rf /tmp")
                .is_none()
        );
        assert!(
            gateway
                .recent_decision(Surface::WriteFile, "cargo test")
                .is_none()
        );

        // And the window closes: an old answer is not a standing allowance.
        gateway.note_owner_decision(record(
            Surface::Shell,
            "cargo build",
            Utc::now() - RECENT_DECISION_WINDOW - chrono::Duration::seconds(1),
        ));
        assert!(
            gateway
                .recent_decision(Surface::Shell, "cargo build")
                .is_none(),
            "a decision older than the window is not reused"
        );

        // Deciding the same action again replaces the record rather than stacking.
        gateway.note_owner_decision(record(Surface::Shell, "cargo test", Utc::now()));
        gateway.note_owner_decision(record(Surface::Shell, "cargo test", Utc::now()));
        assert!(
            gateway
                .recent_decision(Surface::Shell, "cargo test")
                .is_some()
        );
    }

    #[tokio::test]
    async fn the_gateway_knows_its_origin_and_its_nonces() {
        let gateway = TestGateway::new("http://127.0.0.1:8787");
        assert_eq!(gateway.origin(), "http://127.0.0.1:8787");
        assert_eq!(gateway.replay().len(), 0);
        assert!(format!("{gateway:?}").contains("127.0.0.1:8787"));
        // The replay cache is shared by every dial, so a nonce seen on one
        // connection is burned for the next.
        assert!(
            gateway
                .replay()
                .check_and_insert("nonce", swarmbot_node_protocol::unix_ms_now())
        );
        assert!(
            !gateway
                .replay()
                .check_and_insert("nonce", swarmbot_node_protocol::unix_ms_now(),)
        );
        assert_eq!(gateway.replay().len(), 1);
    }
}
