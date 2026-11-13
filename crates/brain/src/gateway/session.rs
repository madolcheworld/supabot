//! One connected node: the signed upgrade, `node.hello`, presence, and the
//! methods the brain serves.
//!
//! The order here is the protocol's, and each step has its own failure:
//!
//! 1. the upgrade header is verified **before** anything is upgraded, so a
//!    refusal is an HTTP 401 a node can report (§3.1);
//! 2. the brain calls `node.hello` first, signed over *this* connection's nonce,
//!    and until the node has verified that signature every other method it is
//!    sent answers `NotAuthenticated` (§3.2);
//! 3. the hello's facts become the roster row: what the node declares, the
//!    strictest ceiling it will run under, and `online: true` — published, so the
//!    UI's Nodes panel goes green without a poll;
//! 4. when the socket ends, the row goes offline. A node that vanished is not the
//!    same as a node that was never there, and the roster has to say which.
//!
//! Revocation is the one thing that reaches into a live session from outside:
//! `DELETE /api/nodes/{id}` closes the socket with 4401, which is the code that
//! makes a node stop redialing permanently and tell its operator to re-pair.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use tracing::{debug, info, warn};

use axum::extract::ws::WebSocket;

use swarmbot_contracts::id::{BotId, ConversationId, NodeId};
use swarmbot_contracts::node::ProtocolRange;
use swarmbot_contracts::node::{
    Capabilities, NodeInfo, NodeRoot, PolicyCeiling, PolicyMap, Surface,
};
use swarmbot_contracts::request::{Decision, RuleEffect, RuntimeRequest};
use swarmbot_contracts::turn::Turn;
use swarmbot_core::{ApprovalOutcome, Change};
use swarmbot_node_protocol::PROTOCOL_VERSION;
use swarmbot_node_protocol::auth::{
    CLOSE_HELLO_FAILED, CLOSE_REPLACED, VerifiedUpgrade, sign_hello,
};
use swarmbot_node_protocol::mux::{BoxFuture, ChunkSink, Peer, PeerConfig, RpcHandler, Socket};
use swarmbot_node_protocol::rpc::{
    self, ApprovalAnswer, Heartbeat, HelloParams, NodeEvent, NodeHello, RequestApproval,
};
use swarmbot_node_protocol::wire::{ProtocolError, TypedError};

use super::executor::unattributed;
use super::socket::GatewaySocket;
use super::{Connected, NodeGateway};
use crate::state::AppState;

/// How long a node has to answer `node.hello`.
///
/// A node that dials, keeps its socket alive with pings and never answers the
/// handshake would otherwise hold its registry slot — and this session task —
/// forever: the mux's liveness check only notices a *silent* socket, and a node
/// that is pinging is not silent. Ten seconds is far more than a hello needs
/// (the node's side is one signature check and one frame) and short enough that
/// a stuck handshake is noticed while an operator is still watching.
pub const HELLO_DEADLINE: Duration = Duration::from_secs(10);

/// Why `node.hello` did not succeed.
#[derive(Debug)]
enum HelloFailure {
    /// The node did not answer within [`HELLO_DEADLINE`].
    TimedOut,
    /// The call failed for another reason.
    Failed(ProtocolError),
}

/// Send `node.hello` and wait for the answer, with a deadline.
///
/// On expiry the call is withdrawn, so the node is asked to interrupt whatever
/// it is doing and a late answer is ignored instead of settling a call nobody is
/// waiting for any more.
async fn ask_hello<S: Socket>(
    peer: &Peer<S>,
    node: &NodeId,
    params: Value,
) -> Result<Value, HelloFailure> {
    let id = peer.next_id();
    peer.call_with_id_deadline(
        id,
        rpc::NODE_HELLO,
        params,
        vec![(rpc::HEADER_NODE_ID.to_string(), node.to_string())],
        HELLO_DEADLINE,
    )
    .await
    .map_err(|error| match error {
        ProtocolError::CallTimedOut { .. } => HelloFailure::TimedOut,
        other => HelloFailure::Failed(other),
    })
}

/// Serve one verified node connection until it ends.
///
/// `verified` is the identity from the upgrade header, and it is the *only* source
/// of the node id used here: nothing read out of a frame is trusted to say who is
/// speaking (§4.10).
pub async fn serve_node(state: AppState, socket: WebSocket, verified: VerifiedUpgrade) {
    let Ok(node_id) = NodeId::parse(verified.node_id.clone()) else {
        // The id was minted by this brain at pairing. One that does not parse
        // means the key store and the roster disagree, and the honest answer is
        // the code that says "not paired".
        warn!(
            node = %verified.node_id,
            "a dial carried a node id this brain did not mint; closing as unknown"
        );
        let mut socket = GatewaySocket::new(socket);
        let _ = socket
            .close(
                swarmbot_node_protocol::CLOSE_REVOKED,
                "this node is not paired with this brain",
            )
            .await;
        return;
    };

    let stored = state.swarm.store.get_node(&node_id).ok().flatten();
    let handler = Arc::new(BrainHandler {
        state: state.clone(),
        node_id: node_id.clone(),
    });
    let peer: Peer<GatewaySocket> = Peer::new(
        GatewaySocket::new(socket),
        PeerConfig {
            // The brain does not gate on a hello it is about to send itself.
            require_hello: false,
            handler: Some(handler),
            ..PeerConfig::default()
        },
    );
    let generation = state.gateway.next_generation();
    let connected = Connected {
        node_id: node_id.clone(),
        peer: peer.clone(),
        generation,
        connected_at: Utc::now(),
        protocol: PROTOCOL_VERSION,
        label: stored
            .as_ref()
            .map(|node| node.label.clone())
            .unwrap_or_else(|| node_id.to_string()),
    };
    if let Some(replaced) = state.gateway.register(connected) {
        // One node id, one live socket: a redial while the old socket is still
        // being torn down must not leave two peers answering for one machine.
        info!(node = %node_id, "replacing this node's previous connection");
        replaced
            .peer
            .close(CLOSE_REPLACED, "another connection took over this node id")
            .await;
    }

    if hello(&state, &peer, &node_id, &verified.nonce)
        .await
        .is_err()
    {
        state.gateway.unregister(&node_id, generation);
        set_online(&state, &node_id, false).await;
        return;
    }

    // From here the node is live: it will send `node.connected`, a heartbeat
    // every 15 s, and approval asks. Nothing to do but wait for the end — or for
    // the daemon to stop, which has to close this socket: a graceful shutdown
    // waits for upgraded connections, and a node that is never told would keep the
    // listener draining forever.
    tokio::select! {
        _ = peer.closed() => {}
        _ = state.wait_for_shutdown() => {
            info!(node = %node_id, "the brain is stopping; closing the node's control socket");
            peer.close(1001, "the brain is stopping").await;
        }
    }
    state.gateway.unregister(&node_id, generation);
    set_online(&state, &node_id, false).await;
    info!(node = %node_id, "the node's control socket ended");
}

/// Call `node.hello`, then write what the node said into the roster.
async fn hello(
    state: &AppState,
    peer: &Peer<GatewaySocket>,
    node: &NodeId,
    nonce: &str,
) -> Result<NodeHello, ()> {
    let params = HelloParams {
        protocol: ProtocolRange::current(),
        brain_version: env!("CARGO_PKG_VERSION").to_string(),
        // Signed over *this* connection's nonce, so a hello captured on another
        // socket cannot be replayed here (§3.2).
        brain_sig: sign_hello(
            &state.identity.keypair,
            node.as_str(),
            nonce,
            state.gateway.origin(),
        ),
    };
    let value = match serde_json::to_value(&params) {
        Ok(value) => value,
        Err(error) => {
            // Nothing but a bug in this crate can fail here, so the socket closes
            // with 1011 — the gateway's own failure — rather than one of the
            // protocol's codes, which each mean something a node acts on.
            warn!(%error, "cannot serialize the node.hello params");
            peer.close(
                crate::routes::node_gateway::CLOSE_CODE,
                "the brain could not build its hello",
            )
            .await;
            return Err(());
        }
    };
    let value = match ask_hello(peer, node, value).await {
        Ok(value) => value,
        Err(HelloFailure::TimedOut) => {
            // The socket is alive — the node is pinging — but it never answered
            // the handshake, so the slot is released instead of held forever.
            warn!(
                node = %node,
                secs = HELLO_DEADLINE.as_secs(),
                "the node did not answer node.hello in time; closing its control socket"
            );
            peer.close(
                CLOSE_HELLO_FAILED,
                "the node did not answer node.hello in time",
            )
            .await;
            return Err(());
        }
        Err(HelloFailure::Failed(error)) => {
            // The node closes the socket itself for both of these (4426 for no
            // common protocol, 4403 for a signature it could not verify), so the
            // brain's job is to report which one happened and let the close land.
            match error.typed() {
                Some(TypedError::PolicyRefused { reason, .. })
                    if reason.contains("no common protocol version") =>
                {
                    warn!(node = %node, %reason, "the node and this brain speak no common protocol version");
                }
                Some(TypedError::NotAuthenticated { reason }) => {
                    warn!(
                        node = %node,
                        %reason,
                        brain_origin = state.gateway.origin(),
                        "the node could not verify this brain's signature; if the brain moved, \
                         the node has to be re-paired"
                    );
                }
                _ => {
                    warn!(node = %node, %error, "node.hello failed");
                    peer.close(CLOSE_HELLO_FAILED, "node.hello failed").await;
                }
            }
            return Err(());
        }
    };
    let hello: NodeHello = match serde_json::from_value(value) {
        Ok(hello) => hello,
        Err(error) => {
            warn!(node = %node, %error, "the node answered node.hello with an unreadable object");
            peer.close(CLOSE_HELLO_FAILED, "node.hello was unreadable")
                .await;
            return Err(());
        }
    };
    if hello.node_id != *node {
        // The id in the payload is not the id the upgrade was verified for. The
        // verified one wins, and the mismatch is worth a line: it means a node is
        // confused about who it is.
        warn!(
            verified = %node,
            claimed = %hello.node_id,
            "node.hello claimed a different id than the upgrade was signed for; using the verified one"
        );
    }
    if let Err(failure) = apply_hello(state, node, &hello).await {
        let (code, reason) = match failure {
            // 4401 is the one code that makes a node stop redialing for good,
            // which is what a revocation has to mean.
            RosterFailure::Revoked => (
                swarmbot_node_protocol::CLOSE_REVOKED,
                "this node was revoked by its owner; re-pair it to reconnect",
            ),
            RosterFailure::Unusable => (CLOSE_HELLO_FAILED, "node.hello could not be recorded"),
        };
        peer.close(code, reason).await;
        return Err(());
    }
    info!(
        node = %node,
        hostname = %hello.hostname,
        version = %hello.version,
        protocol = hello.protocol,
        roots = hello.roots.len(),
        capabilities = ?hello.capabilities.as_slice(),
        ceiling = ?hello.policy.ceiling,
        "the node is authenticated and live"
    );
    Ok(hello)
}

/// How many times one roster write is retried after a revision conflict.
///
/// Four writers share the row — `node.hello`, `node.heartbeat`, `node.event` and
/// presence changes — and the mux spawns a task per inbound request, so they
/// genuinely overlap rather than merely being able to. A conflict therefore
/// means "somebody else got there first", and the answer is to re-read and
/// re-apply, not to fail. The bound is here so a pathological writer cannot spin
/// in this loop forever; with four writers, exhausting it means something else is
/// wrong, and every caller reports that rather than swallowing it.
const ROSTER_WRITE_ATTEMPTS: usize = 8;

/// Read–modify–write one roster row, retrying a revision conflict.
///
/// This is the only way the gateway writes a node row, and the reason it exists
/// is the lost update it replaces: every writer used to do `get_node` → mutate →
/// `update_node` by hand, and `update_node` writes the **whole row**. A heartbeat
/// that read the row before `node.hello` wrote it therefore restored the pre-hello
/// row — reverting `policy`, `roots` and `declared` on a node that was visibly
/// online. The brain then planned against a ceiling the node did not have.
///
/// `change` is [`Fn`], not `FnOnce`, on purpose: a retry calls it again on the
/// *fresh* row, which is the whole point. Every change here assigns fields from
/// data the caller already holds, so re-applying is what it meant anyway; a
/// change that depended on the row it first read would be a bug, and would now
/// silently differ between attempt one and attempt two.
///
/// Returns the row as written so the caller can publish exactly what was stored,
/// or `None` when the node has no roster row — which is not an error here,
/// because the callers differ on whether that is fatal.
///
/// Synchronous, and deliberately so: there is no `.await` between the read and
/// the write, so a single task cannot be interleaved *inside* one attempt. Two
/// tasks on different workers still can be — `get_node` and `update_node` take
/// the store's lock separately, and `ThreadedStore` even serves them on
/// different lanes — which is exactly what the revision guard and the retry are
/// for.
pub(crate) fn mutate_node(
    store: &(dyn swarmbot_contracts::repo::NodeRepo + Send + Sync),
    node: &NodeId,
    change: impl Fn(&mut NodeInfo),
) -> Result<Option<NodeInfo>, swarmbot_contracts::DomainError> {
    match mutate_node_vetoed(store, node, |_| false, change)? {
        RosterWrite::Written(row) => Ok(Some(*row)),
        // No veto can fire here, so `Vetoed` is unreachable by construction —
        // and `Absent` is the only other outcome.
        RosterWrite::Absent | RosterWrite::Vetoed => Ok(None),
    }
}

/// What one guarded roster write ended in doing.
pub(crate) enum RosterWrite {
    /// The row was written; this is what the store now holds. Boxed because
    /// `NodeInfo` is 288 bytes against two data-less variants, and clippy's
    /// `large_enum_variant` is right that every `Absent` would otherwise pay
    /// for it.
    Written(Box<NodeInfo>),
    /// There is no roster row for this node.
    Absent,
    /// The veto refused the write, and nothing was stored. The caller already
    /// knows which veto it passed, so there is no row to hand back.
    Vetoed,
}

/// [`mutate_node`], with a veto that is evaluated on the **freshly read** row
/// inside the retry loop.
///
/// The veto has to live inside the loop, not before it. A check that runs once
/// before the write has the same window the revision guard exists to close:
/// read (not revoked) → somebody revokes → write. Checking after the write is
/// worse still — the row has already been changed by then, so a refused hello
/// would leave the node marked `online` with freshly re-approved capabilities.
/// Inside the loop the veto and the write are guarded by the same generation:
/// if a revoke lands in between, the write conflicts, the loop re-reads, and the
/// veto then sees `revoked` and refuses without having stored anything.
pub(crate) fn mutate_node_vetoed(
    store: &(dyn swarmbot_contracts::repo::NodeRepo + Send + Sync),
    node: &NodeId,
    veto: impl Fn(&NodeInfo) -> bool,
    change: impl Fn(&mut NodeInfo),
) -> Result<RosterWrite, swarmbot_contracts::DomainError> {
    for _ in 0..ROSTER_WRITE_ATTEMPTS {
        let Some(mut row) = store.get_node(node)? else {
            return Ok(RosterWrite::Absent);
        };
        if veto(&row) {
            return Ok(RosterWrite::Vetoed);
        }
        change(&mut row);
        row.bump_revision();
        match store.update_node(&row) {
            Ok(()) => return Ok(RosterWrite::Written(Box::new(row))),
            // Somebody wrote between the read and the write. Re-read and apply
            // our own change on top of theirs: both writers keep their fields,
            // which is what the row should have said all along. Re-reading also
            // re-runs the veto, which is the point.
            Err(swarmbot_contracts::DomainError::Conflict { .. }) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(swarmbot_contracts::DomainError::conflict(format!(
        "node {node} could not be written after {ROSTER_WRITE_ATTEMPTS} attempts; \
         the roster row is being rewritten faster than this writer can re-read it"
    )))
}

/// Why a hello could not be *recorded*, and the close code each one deserves.
///
/// Named apart from [`HelloFailure`], which is about the `node.hello` RPC call
/// itself: a node that never answered is a different fact from a node that
/// answered and whose row this brain then refused to write.
///
/// These are separate because they mean different things to the node on the
/// other end: a revoked node must stop redialing for good (4401), while a
/// roster bug should let it try again (4403). Collapsing them would leave a
/// revoked machine knocking forever, or a confused one giving up.
#[derive(Debug)]
enum RosterFailure {
    /// The node authenticated but has no roster row, or the row could not be
    /// read or written. A wiring or storage problem, not a decision.
    Unusable,
    /// The owner revoked this node while its handshake was still in flight.
    Revoked,
}

/// Write the hello's facts into the roster row and publish them.
async fn apply_hello(
    state: &AppState,
    node: &NodeId,
    hello: &NodeHello,
) -> Result<(), RosterFailure> {
    // The veto runs inside the guarded write, not before it: a revoke that
    // lands while this hello is in flight has to be seen by the same read that
    // the write is based on, or the row gets marked online first and refused
    // afterwards — which leaves a revoked node sitting in the roster as
    // `online: true` with freshly re-approved capabilities.
    let written = match mutate_node_vetoed(
        state.swarm.store.as_ref(),
        node,
        |row| row.revoked,
        |stored| {
            stored.hostname = hello.hostname.clone();
            stored.platform = hello.platform.clone();
            stored.arch = hello.arch.clone();
            stored.version = hello.version.clone();
            stored.protocol = hello.protocol;
            stored.roots = hello.roots.clone();
            stored.declared = hello.capabilities.clone();
            // Redeeming a single-use pairing code *is* the owner's approval of
            // the machine, so the first hello's declarations are approved. A
            // capability a node starts declaring later is not: the effective set
            // stays `declared ∩ approved`, and it takes an owner to widen it.
            if stored.approved.as_slice().is_empty() {
                stored.approved = stored.declared.clone();
            }
            // The hello carries one aggregate ceiling — the strictest of the
            // node's per-surface ceilings. Storing it uniformly is the
            // conservative reading: the brain can then never plan an auto-run
            // the node would refuse. The node still enforces per surface, and
            // the effective rule stays `min(brain, node)`.
            stored.policy = PolicyMap::uniform(hello.policy.ceiling);
            stored.online = true;
            stored.last_seen_at = Some(Utc::now());
            // `revoked` is deliberately *not* touched here. Revocation is the
            // owner's decision and only re-pairing — which writes a fresh row —
            // may clear it. This line used to reset it, so a hello already in
            // flight when the owner pressed revoke un-revoked the node.
        },
    ) {
        Ok(RosterWrite::Written(row)) => *row,
        Ok(RosterWrite::Absent) => {
            warn!(%node, "a node authenticated but has no roster row; it cannot be served");
            return Err(RosterFailure::Unusable);
        }
        Ok(RosterWrite::Vetoed) => {
            // The owner revoked this node while its upgrade was in flight: the
            // upgrade check passed when the row was still clean, and the
            // revocation landed afterwards. Serving it anyway would be a bypass,
            // so the socket closes with 4401 — the one code that makes a node
            // stop redialing for good.
            warn!(
                %node,
                "node.hello arrived for a node its owner revoked during the handshake; \
                 nothing was written and the socket closes with 4401"
            );
            return Err(RosterFailure::Revoked);
        }
        Err(error) => {
            // Failing here closes the socket with `CLOSE_HELLO_FAILED`, which is
            // the honest outcome: a node whose ceiling the brain could not record
            // must not be served, because the brain would plan against a policy
            // it never read. The node redials and the hello is retried.
            warn!(%node, %error, "cannot record the node's hello");
            return Err(RosterFailure::Unusable);
        }
    };
    state.swarm.hub.publish(Change::NodePresence {
        node_id: node.clone(),
        online: true,
    });
    state.swarm.hub.publish(Change::Node(written));
    Ok(())
}

/// Mark a node online or offline in the roster, and say so on the live bus.
async fn set_online(state: &AppState, node: &NodeId, online: bool) {
    // The pre-read is only here to skip the common no-op: publishing a presence
    // change for a node whose presence did not change would make the UI's node
    // panel flicker on every reconnect attempt. Racing this check is harmless —
    // the closure below writes the state we were asked for regardless.
    let Ok(Some(current)) = state.swarm.store.get_node(node) else {
        return;
    };
    if current.online == online {
        return;
    }
    let written = match mutate_node(state.swarm.store.as_ref(), node, |stored| {
        stored.online = online;
        stored.last_seen_at = Some(Utc::now());
    }) {
        Ok(Some(row)) => row,
        Ok(None) => return,
        Err(error) => {
            warn!(%node, %error, online, "cannot record a node's presence");
            return;
        }
    };
    state.swarm.hub.publish(Change::NodePresence {
        node_id: node.clone(),
        online,
    });
    state.swarm.hub.publish(Change::Node(written));
}

/// Close a node's live socket because its pairing was revoked.
///
/// 4401 is the only code that makes a node stop redialing for good, which is the
/// point: a revoked machine knocking every 30 s forever is a log full of noise and
/// an operator who cannot tell it from a bug.
pub async fn revoke(gateway: &NodeGateway, node: &NodeId) -> bool {
    let Some(peer) = gateway.peer(node) else {
        return false;
    };
    info!(%node, "revoking a connected node");
    peer.close(
        swarmbot_node_protocol::CLOSE_REVOKED,
        "this node was revoked by its owner; re-pair it to reconnect",
    )
    .await;
    true
}

/// The methods a node calls on the brain (§5.2).
pub struct BrainHandler {
    state: AppState,
    /// The verified id of the connection this handler serves. A payload's own
    /// claim about who it is from is never used.
    node_id: NodeId,
}

impl RpcHandler for BrainHandler {
    fn handle<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        tx: Option<ChunkSink>,
    ) -> BoxFuture<'a, Result<Value, TypedError>> {
        Box::pin(self.route(method, params, tx))
    }
}

impl BrainHandler {
    async fn route(
        &self,
        method: &str,
        params: Value,
        tx: Option<ChunkSink>,
    ) -> Result<Value, TypedError> {
        // The brain serves no streaming method; a chunk sink arriving here means
        // the peer thinks it is talking to a node.
        drop(tx);
        match method {
            rpc::NODE_EVENT => self.event(params).await,
            rpc::NODE_HEARTBEAT => self.heartbeat(params).await,
            rpc::NODE_REQUEST_APPROVAL => self.approval(params).await,
            other => Err(refusal(other)),
        }
    }

    /// `node.event`: facts, fire-and-forget. Unknown types are ignored rather
    /// than rejected (§5.2), because a node newer than this brain must not break
    /// the connection by telling it something new.
    async fn event(&self, params: Value) -> Result<Value, TypedError> {
        let event: NodeEvent = rpc::decode_params(rpc::NODE_EVENT, params)?;
        match event.event_type.as_str() {
            rpc::EVENT_NODE_CONNECTED => {
                let label = event.data.get("label").and_then(Value::as_str);
                let version = event.data.get("version").and_then(Value::as_str);
                let roots: Option<Vec<NodeRoot>> = event
                    .data
                    .get("roots")
                    .and_then(|value| serde_json::from_value(value.clone()).ok());
                let capabilities: Option<Capabilities> = event
                    .data
                    .get("capabilities")
                    .and_then(|value| serde_json::from_value(value.clone()).ok());
                self.update_node(|node| {
                    // The owner's `--label` is the name a human gave this machine;
                    // it beats the hostname the pairing request carried.
                    if let Some(label) = label.filter(|label| !label.trim().is_empty()) {
                        node.label = label.trim().to_string();
                    }
                    if let Some(version) = version {
                        node.version = version.to_string();
                    }
                    if let Some(roots) = &roots {
                        node.roots = roots.clone();
                    }
                    if let Some(capabilities) = &capabilities {
                        node.declared = capabilities.clone();
                        if node.approved.as_slice().is_empty() {
                            node.approved = node.declared.clone();
                        }
                    }
                    node.online = true;
                    node.last_seen_at = Some(Utc::now());
                })
                .await;
                info!(
                    node = %self.node_id,
                    label = label.unwrap_or("<hostname>"),
                    "a node announced itself"
                );
            }
            rpc::EVENT_ROOTS_CHANGED => {
                let roots: Option<Vec<NodeRoot>> = event
                    .data
                    .get("roots")
                    .and_then(|value| serde_json::from_value(value.clone()).ok());
                if let Some(roots) = roots {
                    let count = roots.len();
                    self.update_node(move |node| {
                        node.roots = roots.clone();
                        node.last_seen_at = Some(Utc::now());
                    })
                    .await;
                    info!(node = %self.node_id, roots = count, "a node's roots changed");
                }
            }
            other => {
                debug!(node = %self.node_id, event = other, "ignoring a node event this build does not know")
            }
        }
        Ok(Value::Null)
    }

    /// `node.heartbeat`: liveness, every 15 s.
    async fn heartbeat(&self, params: Value) -> Result<Value, TypedError> {
        let beat: Heartbeat = rpc::decode_params(rpc::NODE_HEARTBEAT, params)?;
        // The node's own clock is the honest source for "last seen": a brain whose
        // clock is off must not make a node look stale or immortal.
        let seen = DateTime::from_timestamp_millis(beat.at).unwrap_or_else(Utc::now);
        self.update_node(move |node| {
            node.online = true;
            node.last_seen_at = Some(seen);
        })
        .await;
        debug!(
            node = %self.node_id,
            active = beat.active_invocations,
            load = ?beat.load,
            "heartbeat"
        );
        Ok(Value::Null)
    }

    /// `node.requestApproval`: the node's own ceiling said "ask" (§5.2, D1.4).
    async fn approval(&self, params: Value) -> Result<Value, TypedError> {
        let request: RequestApproval = rpc::decode_params(rpc::NODE_REQUEST_APPROVAL, params)?;
        let answer = answer_approval(&self.state, &self.node_id, request).await;
        serde_json::to_value(&answer).map_err(|error| {
            TypedError::policy_refused(
                rpc::NODE_REQUEST_APPROVAL,
                format!("cannot serialize the approval answer: {error}"),
                false,
            )
        })
    }

    /// Apply a change to this node's roster row and publish it.
    async fn update_node(&self, change: impl Fn(&mut NodeInfo) + Send) {
        // `Fn`, not `FnOnce`: `mutate_node` re-applies the change on a fresh row
        // after a revision conflict, so a closure that consumed its captures
        // could not be retried. The call sites clone what they assign rather
        // than move it, which is a few bytes on a roster row.
        match mutate_node(self.state.swarm.store.as_ref(), &self.node_id, change) {
            // `publish` returns a cursor; this arm must not make that the
            // match's value, or the two `warn!` arms below fail to typecheck.
            Ok(Some(node)) => {
                self.state.swarm.hub.publish(Change::Node(node));
            }
            Ok(None) => {
                warn!(node = %self.node_id, "a node sent an update but has no roster row");
            }
            Err(error) => {
                warn!(node = %self.node_id, %error, "cannot record a node update");
            }
        }
    }
}

/// A method this half of the protocol does not serve.
///
/// `fs.*` and `shell.exec` belong to the node and `node.*` to the brain; being
/// asked for the other side's method is a wiring bug, and saying so beats
/// answering `null` and letting the caller guess.
fn refusal(method: &str) -> TypedError {
    if let Some(refusal) = rpc::unimplemented(method) {
        return refusal;
    }
    TypedError::policy_refused(
        method,
        format!("`{method}` is not served by the brain"),
        false,
    )
}

/// Answer a node's approval ask.
///
/// Anything that is not an explicit allow is a deny: an unreachable owner, a
/// cancelled turn, a card that could not be placed. The node treats every one of
/// those as a refusal, so guessing "probably fine" here would run a command
/// nobody approved.
///
/// `by` names *who* decided — the field's contract — with a short parenthetical
/// when the brain itself had to decide, because that is the only place the node's
/// operator will see a reason. The full explanation goes to this brain's log.
async fn answer_approval(
    state: &AppState,
    node: &NodeId,
    request: RequestApproval,
) -> ApprovalAnswer {
    let deny = |by: &str| ApprovalAnswer {
        decision: Decision::Deny,
        by: format!("brain ({by})"),
    };
    let allow = |by: &str| ApprovalAnswer {
        decision: Decision::AllowOnce,
        by: by.to_string(),
    };

    let Some(surface) = surface_of(&request.action) else {
        // A node newer than this brain asked about a surface this build has no
        // ceiling for. Guessing one would be a permission decision made up on the
        // spot, so it is refused and logged.
        warn!(
            node = %node,
            action = %request.action,
            target = %request.target,
            "a node asked about a policy surface this brain does not know; denying"
        );
        // The surface is named in the answer: it is the one thing the node's
        // operator can act on, and it is not a secret.
        return deny(&format!("unknown policy surface `{}`", request.action));
    };

    // Where a card would live, and whose it is. `None` means this brain cannot
    // attribute the call to a bot, and a card with no conversation has nowhere to
    // be shown.
    let placement = place(state, node, &request);
    let (bot_id, conversation_id) = placement.clone().unwrap_or_else(|| {
        let invocation = unattributed();
        (invocation.bot_id, invocation.conversation_id)
    });
    let card = RuntimeRequest::approval(
        bot_id,
        conversation_id,
        surface,
        request.action.clone(),
        request.target.clone(),
        Some(request.reason.clone()),
    );

    // Rules speak first, exactly as they do for a brain-side ask: an "always
    // allow" the owner already gave answers this at once, with no second card.
    match state.swarm.approval.evaluate(&card).effect {
        RuleEffect::Allow => {
            debug!(node = %node, target = %request.target, "a rule allows what the node asked about");
            return allow("rule");
        }
        RuleEffect::Deny => {
            info!(node = %node, target = %request.target, "a saved rule denies what the node asked about");
            return ApprovalAnswer {
                decision: Decision::Deny,
                by: "rule".to_string(),
            };
        }
        RuleEffect::Ask => {}
    }

    let Some((bot_id, conversation_id)) = placement else {
        // No bot, so no conversation, so nowhere to put a card. This is the only
        // path that reaches `honour_recent_decision`; see its docs for why that is
        // not a bypass.
        if let Some(answer) = honour_recent_decision(state, node, surface, &request).await {
            return answer;
        }
        warn!(
            node = %node,
            action = %request.action,
            target = %request.target,
            "a node asked for an approval this brain cannot attribute to a bot, and the owner \
             has not decided this exact action recently; denying"
        );
        return deny("the call could not be attributed to a bot, so no card could be shown");
    };

    // A real bot and a real conversation: ask the owner, and wait. `ask` writes
    // the card next to the work, applies the rules, and wakes the bot with the
    // answer.
    let card = RuntimeRequest::approval(
        bot_id,
        conversation_id,
        surface,
        request.action.clone(),
        request.target.clone(),
        Some(request.reason.clone()),
    );
    info!(
        node = %node,
        action = %request.action,
        target = %request.target,
        "a node is asking the owner for approval"
    );
    match state.swarm.approval.ask(card).await {
        // `allow_once`, never `allow_always`: a standing permission belongs in
        // this brain's rule list, where the owner can see and unsave it. Telling
        // the node "always" would hand a machine a permission its owner only gave
        // here.
        Ok(ApprovalOutcome::Allowed { .. }) => allow("owner"),
        Ok(ApprovalOutcome::Denied { reason }) => {
            info!(node = %node, target = %request.target, %reason, "the owner denied a node's ask");
            ApprovalAnswer {
                decision: Decision::Deny,
                by: "owner".to_string(),
            }
        }
        Ok(ApprovalOutcome::Cancelled) => deny("the turn was stopped while the card waited"),
        Err(error) => {
            warn!(node = %node, %error, "cannot raise a card for a node's approval ask");
            deny("the card could not be raised")
        }
    }
}

/// Which bot and conversation a node's ask belongs to.
fn place(
    state: &AppState,
    node: &NodeId,
    request: &RequestApproval,
) -> Option<(BotId, ConversationId)> {
    // The invocation the call travelled with, when the dispatcher could attribute
    // it — which is the common case, and the only answer that is not an inference.
    if !is_unattributed_bot(&request.bot_id)
        && let Ok(bot) = state.swarm.bots.get(&request.bot_id)
    {
        let conversation = state
            .swarm
            .orchestrator
            .active_turn(&bot.id)
            .unwrap_or_else(|| bot.direct_conversation_id.clone());
        return Some((bot.id, conversation));
    }
    // Otherwise: a tool call happens inside a turn, and a bot runs one turn at a
    // time, so exactly one running turn on this node identifies the caller. Two
    // or more is ambiguous, and an approval card in the wrong conversation is a
    // lie about who asked — so ambiguity is reported, not resolved by guessing.
    let turns: Vec<Turn> = state
        .swarm
        .store
        .running_turns()
        .unwrap_or_default()
        .into_iter()
        .filter(|turn| {
            state
                .swarm
                .store
                .get_bot(&turn.bot_id)
                .ok()
                .flatten()
                .is_some_and(|bot| &bot.host_node() == node)
        })
        .collect();
    match turns.as_slice() {
        [turn] => Some((turn.bot_id.clone(), turn.conversation_id.clone())),
        many => {
            if many.len() > 1 {
                debug!(
                    node = %node,
                    candidates = many.len(),
                    "more than one turn is running on this node; the approval cannot be attributed"
                );
            }
            None
        }
    }
}

fn is_unattributed_bot(bot: &BotId) -> bool {
    bot.as_str() == unattributed().bot_id.as_str()
}

/// The owner's own answer to this exact action, moments ago.
///
/// Reached **only** when the call could not be attributed to a bot, so there is no
/// conversation to put a card in and the alternatives are "deny" or this. Policy is
/// enforced twice by design (D1.4) — the brain asks, then the node asks again
/// because it cannot know the brain already did — so the common shape here is the
/// owner having answered this very command seconds ago.
///
/// It is not a standing allowance: the window is
/// [`RECENT_DECISION_WINDOW`](crate::gateway::RECENT_DECISION_WINDOW), the match is
/// on the surface *and* the exact target string both sides derive from
/// `NodeCall::describe`, the record is written only by
/// `POST /api/requests/{id}/respond` (the only way an owner answers a card), and
/// reusing it writes a line into the transcript of the conversation where the
/// decision was made. An owner who wants a standing answer has "always allow",
/// which becomes a rule and is handled above.
async fn honour_recent_decision(
    state: &AppState,
    node: &NodeId,
    surface: Surface,
    request: &RequestApproval,
) -> Option<ApprovalAnswer> {
    let recorded = state.gateway.recent_decision(surface, &request.target)?;
    let age = Utc::now()
        .signed_duration_since(recorded.at)
        .num_seconds()
        .max(0);
    // Written into the conversation the original decision was made in, so the
    // transcript shows that the node asked and what answered it.
    if let Err(error) = state.swarm.transcript.event(
        &recorded.conversation,
        "node-approval-reused",
        format!(
            "The node asked again for `{}`; the owner's {} from {age}s ago was honoured",
            request.target,
            match recorded.decision {
                Decision::AllowAlways => "\"always allow\"",
                Decision::AllowOnce => "\"allow\"",
                Decision::Deny => "\"deny\"",
            }
        ),
    ) {
        debug!(%error, "cannot record the reused approval in the transcript");
    }
    info!(
        node = %node,
        invocation = %request.invocation_id,
        target = %request.target,
        age_secs = age,
        decided_in = %recorded.conversation,
        "a node's approval ask was answered from the owner's decision moments ago"
    );
    Some(match recorded.decision {
        Decision::Deny => ApprovalAnswer {
            decision: Decision::Deny,
            by: "owner (denied the same action moments ago)".to_string(),
        },
        // `allow_once`, for the reason given above: the node is not told "always".
        _ => ApprovalAnswer {
            decision: Decision::AllowOnce,
            by: "owner (decided the same action moments ago)".to_string(),
        },
    })
}

/// The policy surface a node's `action` names.
pub fn surface_of(action: &str) -> Option<Surface> {
    [
        Surface::Shell,
        Surface::ReadFile,
        Surface::WriteFile,
        Surface::EditFile,
        Surface::Computer,
    ]
    .into_iter()
    .find(|surface| surface.as_str() == action)
}

/// The ceiling a node's aggregate policy implies, for the roster row.
pub fn uniform_policy(ceiling: PolicyCeiling) -> PolicyMap {
    PolicyMap::uniform(ceiling)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use swarmbot_contracts::DomainError;
    use swarmbot_contracts::id::RequestId;
    use swarmbot_contracts::repo::NodeRepo as _;
    use swarmbot_node_protocol::testing::duplex_pair;
    use swarmbot_node_protocol::wire::{Frame, parse as parse_frame};

    /// The defect this pins: a node that dials, keeps its socket alive with
    /// pings and never answers `node.hello` held its registry slot (and this
    /// session task) forever, because the liveness timeout only notices a
    /// *silent* socket. The ask is bounded here, and the test's own outer bound
    /// turns a missing deadline into a failure instead of a hang.
    #[tokio::test(start_paused = true)]
    async fn a_node_that_never_answers_hello_does_not_hold_its_slot() {
        let (sock, mut node_side) = duplex_pair(64 * 1024);
        let peer = Peer::new(
            sock,
            PeerConfig {
                // Timers that cannot fire inside the test: the point is that the
                // *hello deadline*, not the liveness timeout, ends this.
                ping_interval: Duration::from_secs(3600),
                liveness_timeout: Duration::from_secs(3600),
                ..PeerConfig::default()
            },
        );
        let node = NodeId::new();
        let ask = tokio::spawn({
            let peer = peer.clone();
            let node = node.clone();
            async move {
                ask_hello(
                    &peer,
                    &node,
                    json!({ "protocol": { "min": 1, "max": 1 }, "brainVersion": "test" }),
                )
                .await
            }
        });

        // The hello goes out and this node answers nothing, ever.
        let request = node_side
            .recv_text()
            .await
            .expect("a frame")
            .expect("the hello request");
        match parse_frame(&request).expect("parses") {
            Frame::Request { method, .. } => assert_eq!(method, rpc::NODE_HELLO),
            other => panic!("expected the hello request, got {other:?}"),
        }

        let outcome = tokio::time::timeout(HELLO_DEADLINE * 2, ask)
            .await
            .expect("node.hello must not wait forever")
            .expect("the ask task finishes");
        match outcome {
            Err(HelloFailure::TimedOut) => {}
            other => panic!("expected the hello deadline to fire, got {other:?}"),
        }

        // The withdrawal asks the node to stop, so a wedged node is interrupted
        // rather than left running something nobody waits for.
        let interrupt = node_side
            .recv_text()
            .await
            .expect("a frame")
            .expect("the interrupt frame");
        assert!(matches!(
            parse_frame(&interrupt).expect("parses"),
            Frame::Interrupt { .. }
        ));
    }

    #[test]
    fn surfaces_are_the_names_the_node_sends() {
        assert_eq!(surface_of("shell"), Some(Surface::Shell));
        assert_eq!(surface_of("read_file"), Some(Surface::ReadFile));
        assert_eq!(surface_of("write_file"), Some(Surface::WriteFile));
        assert_eq!(surface_of("edit_file"), Some(Surface::EditFile));
        assert_eq!(surface_of("computer"), Some(Surface::Computer));
        // A node newer than this brain must get a typed refusal, not a guess.
        assert_eq!(surface_of("desktop"), None);
        assert_eq!(surface_of("Shell"), None, "the wire is snake_case");
    }

    #[test]
    fn a_method_the_other_half_serves_is_refused_by_name() {
        for method in [rpc::FS_READ, rpc::SHELL_EXEC, rpc::NODE_HELLO] {
            let refusal = refusal(method);
            match refusal {
                TypedError::PolicyRefused { reason, retry, .. } => {
                    assert!(reason.contains(method), "{reason}");
                    assert!(!retry, "a wiring bug will not fix itself on retry");
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
        // A method in the protocol's unimplemented table keeps that refusal.
        let refused = refusal("browser.navigate");
        assert!(matches!(refused, TypedError::PolicyRefused { .. }));
    }

    #[test]
    fn the_unattributed_sentinel_is_recognised() {
        let invocation = unattributed();
        assert!(is_unattributed_bot(&invocation.bot_id));
        assert!(!is_unattributed_bot(&BotId::new()));
        assert!(!is_unattributed_bot(&BotId::from_suffix(
            "0189fbd3a4d37f0e9c0e6a3f8f1c9d2e"
        )));
    }

    #[test]
    fn a_node_ceiling_is_stored_uniformly() {
        let policy = uniform_policy(PolicyCeiling::Ask);
        assert_eq!(policy.get(Surface::Shell), PolicyCeiling::Ask);
        assert_eq!(policy.get(Surface::ReadFile), PolicyCeiling::Ask);
        // Uniform at the strictest ceiling: the brain can never plan an auto-run
        // the node would refuse, which is the conservative direction.
        let never = uniform_policy(PolicyCeiling::Never);
        assert!(never.get(Surface::WriteFile).refuses());
    }

    #[test]
    fn an_approval_answer_is_the_documented_shape() {
        let answer = ApprovalAnswer {
            decision: Decision::AllowOnce,
            by: "owner".to_string(),
        };
        let value = serde_json::to_value(&answer).expect("serializes");
        assert_eq!(value["decision"], serde_json::json!("allow_once"));
        assert_eq!(value["by"], serde_json::json!("owner"));
        // Anything else is a deny on the node side, so `by` carries the reason.
        let denied = ApprovalAnswer {
            decision: Decision::Deny,
            by: "nobody answered in time".to_string(),
        };
        let value = serde_json::to_value(&denied).expect("serializes");
        assert_eq!(value["decision"], serde_json::json!("deny"));
    }

    #[test]
    fn a_request_carries_the_facts_the_owner_needs() {
        let request = RequestApproval {
            invocation_id: swarmbot_contracts::id::InvocationId::new(),
            bot_id: BotId::new(),
            action: "shell".to_string(),
            target: "cargo test".to_string(),
            reason: "this node's `shell` ceiling is `ask`".to_string(),
        };
        let value = serde_json::to_value(&request).expect("serializes");
        assert_eq!(value["action"], serde_json::json!("shell"));
        assert_eq!(value["target"], serde_json::json!("cargo test"));
        assert!(value["invocationId"].is_string());
        let back: RequestApproval = serde_json::from_value(value).expect("round-trips");
        assert_eq!(back.target, request.target);
        let _: RequestId = RequestId::new();
    }

    #[test]
    fn a_hello_is_decoded_from_the_wire_shape() {
        let value = serde_json::json!({
            "nodeId": "nod_0189fbd3a4d37f0e9c0e6a3f8f1c9d2e",
            "version": "0.1.0",
            "hostname": "workstation",
            "platform": "linux",
            "arch": "x86_64",
            "protocol": 1,
            "roots": [{"id": "work", "path": "/tmp/work", "mode": "read_write"}],
            "capabilities": ["fs@1", "shell@1"],
            "policy": {"ceiling": "ask", "remoteManaged": false},
            "brainVerified": true,
        });
        let hello: NodeHello = serde_json::from_value(value).expect("decodes");
        assert_eq!(hello.hostname, "workstation");
        assert!(hello.brain_verified);
        assert_eq!(hello.policy.ceiling, PolicyCeiling::Ask);
        assert!(!hello.policy.remote_managed);
        assert_eq!(hello.roots.len(), 1);
        assert!(hello.capabilities.has("shell"));
        // `tunnels` is omitted by this build and defaults to empty.
        assert!(hello.tunnels.is_empty());
    }

    #[test]
    fn a_heartbeat_carries_unix_milliseconds() {
        let value = serde_json::json!({
            "at": 1_700_000_000_000i64,
            "activeInvocations": 2u32,
            "load": null,
            "freeMemBytes": null,
        });
        let beat: Heartbeat = serde_json::from_value(value).expect("decodes");
        assert_eq!(beat.active_invocations, 2);
        assert!(beat.load.is_none(), "null off Linux");
        assert!(beat.free_mem_bytes.is_none());
        // The node's clock is the source of truth for "last seen", so the millis
        // have to survive the round trip exactly.
        let seen = DateTime::from_timestamp_millis(beat.at).expect("a timestamp");
        assert_eq!(seen.timestamp_millis(), 1_700_000_000_000);
        assert_eq!(
            seen,
            Utc.timestamp_millis_opt(beat.at).single().expect("one")
        );
    }

    #[test]
    fn a_node_event_carries_its_type_and_payload() {
        let value = serde_json::json!({
            "type": rpc::EVENT_NODE_CONNECTED,
            "data": {"label": "workstation", "version": "0.1.0", "capabilities": ["fs@1"], "roots": []},
        });
        let event: NodeEvent = serde_json::from_value(value).expect("decodes");
        assert_eq!(event.event_type, rpc::EVENT_NODE_CONNECTED);
        assert_eq!(event.data["label"], serde_json::json!("workstation"));
        let roots: Vec<NodeRoot> =
            serde_json::from_value(event.data["roots"].clone()).expect("roots decode");
        assert!(roots.is_empty());
    }

    /// A [`NodeRepo`] that fails its first `failures` writes with a conflict,
    /// so the retry in [`mutate_node`] can be tested without manufacturing a
    /// real race: two tasks on two worker threads interleaving between
    /// `get_node` and `update_node` is exactly what happens under load, and a
    /// test that waits for the scheduler to produce it is a test that passes or
    /// fails by luck.
    struct ConflictingStore {
        inner: swarmbot_store::MemoryStore,
        failures: AtomicUsize,
        writes: AtomicUsize,
    }

    impl ConflictingStore {
        fn new(failures: usize) -> Self {
            Self {
                inner: swarmbot_store::MemoryStore::default(),
                failures: AtomicUsize::new(failures),
                writes: AtomicUsize::new(0),
            }
        }
    }

    impl swarmbot_contracts::repo::NodeRepo for ConflictingStore {
        fn update_node(&self, node: &NodeInfo) -> Result<(), DomainError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            if self.failures.fetch_sub(1, Ordering::SeqCst) > 0 {
                // The same error a genuinely superseded row produces, so the
                // retry path cannot tell this apart from the real thing.
                return Err(DomainError::conflict(
                    "injected: another writer reached the row first",
                ));
            }
            self.inner.update_node(node)
        }

        fn insert_node(&self, node: &NodeInfo) -> Result<(), DomainError> {
            self.inner.insert_node(node)
        }
        fn get_node(&self, id: &NodeId) -> Result<Option<NodeInfo>, DomainError> {
            self.inner.get_node(id)
        }
        fn list_nodes(&self) -> Result<Vec<NodeInfo>, DomainError> {
            self.inner.list_nodes()
        }
        fn insert_pairing(
            &self,
            record: &swarmbot_contracts::node::PairingRecord,
        ) -> Result<(), DomainError> {
            self.inner.insert_pairing(record)
        }
        fn get_pairing(
            &self,
            code_hash: &str,
        ) -> Result<Option<swarmbot_contracts::node::PairingRecord>, DomainError> {
            self.inner.get_pairing(code_hash)
        }
        fn update_pairing(
            &self,
            record: &swarmbot_contracts::node::PairingRecord,
        ) -> Result<(), DomainError> {
            self.inner.update_pairing(record)
        }
        fn expired_pairings(
            &self,
            now: DateTime<Utc>,
        ) -> Result<Vec<swarmbot_contracts::node::PairingRecord>, DomainError> {
            self.inner.expired_pairings(now)
        }
        fn put_node_key(&self, node: &NodeId, key: &str) -> Result<(), DomainError> {
            self.inner.put_node_key(node, key)
        }
        fn node_key(&self, node: &NodeId) -> Result<Option<String>, DomainError> {
            self.inner.node_key(node)
        }
        fn list_node_keys(&self) -> Result<Vec<(NodeId, String)>, DomainError> {
            self.inner.list_node_keys()
        }
    }

    /// A conflicting write is retried, and the change still lands.
    ///
    /// Without the retry this is where a heartbeat would silently drop
    /// `last_seen_at` — or, in the hello's case, where the node's ceiling would
    /// never be recorded and the brain would plan against the default.
    #[test]
    fn a_conflicting_roster_write_is_retried_and_lands() {
        let store = ConflictingStore::new(2);
        let node = NodeInfo::local("box", vec![], PolicyMap::default());
        store.insert_node(&node).expect("the row exists");

        let written = mutate_node(&store, &node.id, |row| {
            row.policy = PolicyMap::uniform(PolicyCeiling::Never);
        })
        .expect("the write eventually succeeds")
        .expect("the row exists");

        assert_eq!(
            written.policy,
            PolicyMap::uniform(PolicyCeiling::Never),
            "the change landed: {written:?}"
        );
        assert_eq!(
            store.writes.load(Ordering::SeqCst),
            3,
            "two injected conflicts, then the accepted write"
        );
        assert_eq!(
            store
                .get_node(&node.id)
                .expect("reads")
                .expect("row")
                .revision,
            1,
            "the row advanced by exactly one generation, not one per attempt"
        );
    }

    /// Retrying has a bound: a row that keeps changing underneath is reported,
    /// not spun on forever.
    #[test]
    fn a_roster_write_that_never_wins_is_reported_not_spun_on() {
        let store = ConflictingStore::new(ROSTER_WRITE_ATTEMPTS + 5);
        let node = NodeInfo::local("box", vec![], PolicyMap::default());
        store.insert_node(&node).expect("the row exists");

        let error = mutate_node(&store, &node.id, |row| {
            row.online = false;
        })
        .expect_err("a writer that never wins must fail");
        assert!(
            matches!(error, DomainError::Conflict { .. }),
            "the failure is a typed conflict: {error}"
        );
        assert_eq!(
            store.writes.load(Ordering::SeqCst),
            ROSTER_WRITE_ATTEMPTS,
            "it stopped at the bound instead of retrying forever"
        );
    }

    /// A node with no roster row is `Ok(None)`, not an error: `apply_hello`
    /// treats it as fatal and closes the socket, while a heartbeat only warns,
    /// and that difference belongs to the callers.
    #[test]
    fn mutating_a_node_that_is_not_in_the_roster_is_none_not_an_error() {
        let store = ConflictingStore::new(0);
        let missing = NodeId::new();
        let outcome = mutate_node(&store, &missing, |row| row.online = false)
            .expect("a missing row is not a store failure");
        assert!(outcome.is_none(), "{outcome:?}");
        assert_eq!(
            store.writes.load(Ordering::SeqCst),
            0,
            "nothing was written"
        );
    }

    /// A hello that lands after the owner revoked the node must not un-revoke it.
    ///
    /// The window is real, not theoretical: the upgrade is verified while the row
    /// is still clean, the owner presses revoke, and the hello that was already
    /// in flight is recorded afterwards. `apply_hello` used to assign
    /// `revoked = false` as part of "the node is live again", so the roster ended
    /// up saying a machine the owner had just cut off was trusted — and
    /// `NodeDispatcher` routes on that row, so the brain would send it tool calls
    /// again. Revocation is the owner's decision; only re-pairing, which writes
    /// a fresh row, may clear it.
    #[tokio::test]
    async fn a_hello_for_a_revoked_node_does_not_unrevoke_it() {
        use std::collections::BTreeMap;
        use swarmbot_contracts::config::BrainConfig;
        use swarmbot_node_protocol::rpc::HelloPolicy;

        let dir = tempfile::tempdir().expect("a temp dir");
        let swarm = swarmbot_core::compose::memory(
            BrainConfig {
                team: Some(BrainConfig::demo_team("fake")),
                ..BrainConfig::default()
            },
            dir.path(),
            vec![],
            &BTreeMap::new(),
        )
        .expect("a swarm");
        swarm.boot().expect("boots");
        let state = AppState::ephemeral(
            Arc::clone(&swarm),
            Arc::new(crate::config::Resolved {
                config: BrainConfig::default(),
                data_dir: dir.path().to_path_buf(),
                paths: crate::config::Paths::new(dir.path()),
                config_path: None,
                team_path: None,
                roots: Vec::new(),
                expiry_ticker: false,
                offline: true,
                fresh: true,
            }),
            crate::gateway::NodeGateway::new("http://127.0.0.1:0"),
        )
        .expect("a state");

        // A paired node, then its owner revokes it — through the same guarded
        // writer the route uses, so the row is exactly what a revoke leaves.
        let node = NodeId::new();
        let mut row = NodeInfo::local("it-node", vec![], PolicyMap::default());
        row.id = node.clone();
        row.kind = swarmbot_contracts::node::NodeKind::Remote;
        state
            .swarm
            .store
            .insert_node(&row)
            .expect("the node is paired");
        let mut revoked = state
            .swarm
            .store
            .get_node(&node)
            .expect("reads")
            .expect("the row exists");
        revoked.revoked = true;
        revoked.online = false;
        // What the revoke route also does: a revoked node is approved for
        // nothing, so the fixture is the row a real revocation leaves behind.
        revoked.approved = Capabilities::new(Vec::<String>::new());
        revoked.bump_revision();
        state
            .swarm
            .store
            .update_node(&revoked)
            .expect("the revoke is written");

        let hello = NodeHello {
            node_id: node.clone(),
            version: "0.1.0".into(),
            hostname: "it-node".into(),
            platform: "linux".into(),
            arch: "x86_64".into(),
            protocol: PROTOCOL_VERSION,
            roots: vec![],
            capabilities: Capabilities::new(["fs@1", "shell@1"]),
            tunnels: Vec::new(),
            policy: HelloPolicy {
                ceiling: PolicyCeiling::Always,
                remote_managed: false,
            },
            brain_verified: true,
        };

        let failure = apply_hello(&state, &node, &hello)
            .await
            .expect_err("a revoked node's hello is refused, not recorded");
        assert!(
            matches!(failure, RosterFailure::Revoked),
            "the refusal says it was revoked, so the socket closes with 4401 and              the node stops redialing: {failure:?}"
        );

        let after = state
            .swarm
            .store
            .get_node(&node)
            .expect("reads")
            .expect("the row exists");
        assert!(after.revoked, "still revoked: {after:?}");
        assert!(
            !after.online,
            "the refused hello did not mark a revoked node online: {after:?}"
        );
        assert_eq!(
            after.approved.as_slice(),
            &[] as &[String],
            "a revoked node is approved for nothing, and the hello did not re-approve it: {after:?}"
        );
        // The strongest form of the claim: the refused hello wrote nothing at
        // all. A check placed *after* the write would pass every assertion above
        // while still leaving the row marked online with the hello's ceiling.
        assert_eq!(
            after.revision, revoked.revision,
            "the refused hello did not write the row: {after:?}"
        );
        assert_eq!(
            after.policy,
            PolicyMap::default(),
            "the hello's ceiling was not recorded for a node that will never be served: {after:?}"
        );
    }
}
