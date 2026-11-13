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
    apply_hello(state, node, &hello).await?;
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

/// Write the hello's facts into the roster row and publish them.
async fn apply_hello(state: &AppState, node: &NodeId, hello: &NodeHello) -> Result<(), ()> {
    let mut stored = match state.swarm.store.get_node(node) {
        Ok(Some(node)) => node,
        Ok(None) => {
            warn!(%node, "a node authenticated but has no roster row; it cannot be served");
            return Err(());
        }
        Err(error) => {
            warn!(%node, %error, "cannot read the roster row for an authenticated node");
            return Err(());
        }
    };
    stored.hostname = hello.hostname.clone();
    stored.platform = hello.platform.clone();
    stored.arch = hello.arch.clone();
    stored.version = hello.version.clone();
    stored.protocol = hello.protocol;
    stored.roots = hello.roots.clone();
    stored.declared = hello.capabilities.clone();
    // Redeeming a single-use pairing code *is* the owner's approval of the
    // machine, so the first hello's declarations are approved. A capability a
    // node starts declaring later is not: the effective set stays
    // `declared ∩ approved`, and it takes an owner to widen it.
    if stored.approved.as_slice().is_empty() {
        stored.approved = stored.declared.clone();
    }
    // The hello carries one aggregate ceiling — the strictest of the node's
    // per-surface ceilings. Storing it uniformly is the conservative reading: the
    // brain can then never plan an auto-run the node would refuse. The node still
    // enforces per surface, and the effective rule stays `min(brain, node)`.
    stored.policy = PolicyMap::uniform(hello.policy.ceiling);
    stored.online = true;
    stored.last_seen_at = Some(Utc::now());
    stored.revoked = false;

    if let Err(error) = state.swarm.store.update_node(&stored) {
        warn!(%node, %error, "cannot record the node's hello");
        return Err(());
    }
    state.swarm.hub.publish(Change::NodePresence {
        node_id: node.clone(),
        online: true,
    });
    state.swarm.hub.publish(Change::Node(stored));
    Ok(())
}

/// Mark a node online or offline in the roster, and say so on the live bus.
async fn set_online(state: &AppState, node: &NodeId, online: bool) {
    let Ok(Some(mut stored)) = state.swarm.store.get_node(node) else {
        return;
    };
    if stored.online == online {
        return;
    }
    stored.online = online;
    stored.last_seen_at = Some(Utc::now());
    if let Err(error) = state.swarm.store.update_node(&stored) {
        warn!(%node, %error, online, "cannot record a node's presence");
        return;
    }
    state.swarm.hub.publish(Change::NodePresence {
        node_id: node.clone(),
        online,
    });
    state.swarm.hub.publish(Change::Node(stored));
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
                    if let Some(roots) = roots {
                        node.roots = roots;
                    }
                    if let Some(capabilities) = capabilities {
                        node.declared = capabilities;
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
                        node.roots = roots;
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
    async fn update_node(&self, change: impl FnOnce(&mut NodeInfo)) {
        let Ok(Some(mut node)) = self.state.swarm.store.get_node(&self.node_id) else {
            warn!(node = %self.node_id, "a node sent an update but has no roster row");
            return;
        };
        change(&mut node);
        if let Err(error) = self.state.swarm.store.update_node(&node) {
            warn!(node = %self.node_id, %error, "cannot record a node update");
            return;
        }
        self.state.swarm.hub.publish(Change::Node(node));
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
    use swarmbot_contracts::id::RequestId;
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
}
