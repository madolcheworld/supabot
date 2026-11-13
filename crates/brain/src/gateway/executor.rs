//! The [`NodeExecutor`] that routes a call to this machine or across a socket.
//!
//! `compose` builds a [`swarmbot_tools::LocalExecutor`] and hands it to the swarm
//! as *the* executor, and that executor runs **any** call it is given locally — a
//! node id it does not own is logged and ignored. Without this dispatcher in
//! front of it, a bot bound to a remote node would silently execute on the
//! brain's machine, which is the one thing decision D1.1 exists to prevent.
//!
//! Routing is by id and nothing else: `nod_local` runs here, every other id has
//! to be a connected node or the call fails. A failure is the honest answer —
//! "that node is not connected" is infrastructure, not a denial.
//!
//! **Attribution is a fact, not an inference.** A forwarded call carries the
//! caller's own [`Invocation`] — every handler already has it in
//! [`swarmbot_tools::ToolContext::invocation`] — and this module puts it on the
//! wire unchanged. It used to guess instead: scan `running_turns()`, and if
//! exactly one turn belonged to a bot hosted on the node, call that the caller.
//! That is wrong the moment two turns run on one node in parallel, and it was
//! wrong out loud: the guess was abandoned for the [`unattributed`] sentinel, so
//! both calls travelled as nobody and the node's audit line and approval card
//! named nobody. The port carries the invocation now, and there is nothing left
//! here to infer.
//!
//! **Every remote call has a deadline.** A node that keeps its socket alive
//! (pings, heartbeats) while a handler hangs — `fs.read` on a dead network mount
//! is the canonical case — would otherwise hold the bot's only turn forever: the
//! mux's liveness check only notices a *silent* socket, and a hung handler is
//! not silent. [`deadline_for`] picks the bound: an `fs.*` call gets
//! [`FS_CALL_DEADLINE`], a shell command gets the caller's own `timeout_ms` plus
//! [`SHELL_DEADLINE_GRACE`], because a command may legitimately run for minutes
//! and the node enforces that timeout itself.
//!
//! On expiry the call is withdrawn (its pending entry removed, so a late answer
//! is ignored) and the node is sent `@effect/rpc/Interrupt`, and the tool call
//! fails with a [`ToolError::failed`] naming the node and the kind of call.
//! **That is not a cancellation guarantee:** the interrupt *asks* the node to
//! stop, and our own node answers it by dropping the handler future, which runs
//! its `Drop` guards and kills the process group (`docs/node-protocol.md`
//! §4.7/§5.1) — but the brain cannot kill a process on another machine, and a
//! node that ignores the frame may still be running the command when this side
//! has given up. The error says exactly that; nothing here reports a success, an
//! empty result or a finished job it cannot vouch for.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::debug;

use swarmbot_contracts::Invocation;
use swarmbot_contracts::id::{BotId, ConversationId, NodeId};
use swarmbot_contracts::repo::Persistence;
use swarmbot_contracts::tool::{ToolError, ToolResult};
use swarmbot_node_protocol::mux::{Peer, Socket};
use swarmbot_node_protocol::rpc::{self, HEADER_NODE_ID, OutputStream, ShellChunk};
use swarmbot_tools::NodeCall;
use swarmbot_tools::NodeExecutor;
use swarmbot_tools::handlers::shell::DEFAULT_SHELL_TIMEOUT_MS;

use super::NodeGateway;
use super::socket::GatewaySocket;
use super::wire::{self, Outgoing, ShellExit, ShellRun};

/// How many stream chunks are buffered before the node feels back-pressure.
///
/// Small on purpose: the driver awaits the send, so a brain that stops reading
/// pauses the node's pump instead of letting it queue an unbounded shell log.
const STREAM_BUFFER: usize = 64;

/// How much of a failed stream's partial output is kept in the error message.
const PARTIAL_OUTPUT_CHARS: usize = 4 * 1024;

/// How long a node has to answer an `fs.*` call.
///
/// A hung filesystem call is almost always a dead network mount or a stalled
/// server, and those never answer, so the bound is what turns "forever" into an
/// error a model can react to. Sixty seconds is long enough for a `fs.grep` over
/// a large tree on a slow disk and short enough that a bot's single turn
/// survives one stuck call.
pub const FS_CALL_DEADLINE: Duration = Duration::from_secs(60);

/// What a streaming `shell.exec` gets on top of the caller's own `timeout_ms`.
///
/// A shell command can legitimately run for minutes, so the caller's timeout —
/// not a brain constant — sets the scale: the wire already carries it
/// (`rpc::ShellExec::timeout_ms`, defaulting to the local executor's
/// [`DEFAULT_SHELL_TIMEOUT_MS`]). The grace covers what the node still has to do
/// *after* that timeout fires: kill the process group, drain the pipes and send
/// the `Exit` chunk. Without it, every command that ended by timing out would be
/// reported as a node failure instead of as the timed-out run it was.
pub const SHELL_DEADLINE_GRACE: Duration = Duration::from_secs(30);

/// How long the chunk collector may take to finish once the call is over.
///
/// It normally resolves at once (the driver drops the chunk sink when the call
/// settles or is withdrawn), and it is bounded anyway so that a driver wedged on
/// a socket that is not draining cannot outlive the deadline above.
const STREAM_COLLECT_GRACE: Duration = Duration::from_secs(5);

/// The deadline for one call: the caller's own budget where it has one.
pub fn deadline_for(call: &NodeCall) -> Duration {
    match call {
        NodeCall::ShellExec { timeout_ms, .. } => {
            Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_SHELL_TIMEOUT_MS))
                + SHELL_DEADLINE_GRACE
        }
        _ => FS_CALL_DEADLINE,
    }
}

/// The bot and conversation a **node's approval ask** is placed in when nothing
/// attributes it.
///
/// A forwarded call always carries its caller's [`Invocation`] now, so the
/// dispatcher never invents one; this sentinel survives for the one path that
/// still has to answer with no attribution at all — a node whose
/// `node.requestApproval` arrives naming a bot this brain cannot place (see
/// [`super::session`]). A card in the wrong conversation would be a lie about who
/// asked, so the sentinel is used instead of a guess.
pub const UNATTRIBUTED: &str = "unknown";

/// Routes a call to the local machine or to a connected node.
///
/// Generic over the socket only so its own tests can drive it over an in-memory
/// pair instead of a WebSocket; production code always means
/// [`GatewaySocket`], which is the default.
pub struct NodeDispatcher<S: Socket = GatewaySocket> {
    local: Arc<dyn NodeExecutor>,
    gateway: Arc<NodeGateway<S>>,
    store: Arc<dyn Persistence>,
}

impl<S: Socket> NodeDispatcher<S> {
    pub fn new(
        local: Arc<dyn NodeExecutor>,
        gateway: Arc<NodeGateway<S>>,
        store: Arc<dyn Persistence>,
    ) -> Arc<Self> {
        Arc::new(Self {
            local,
            gateway,
            store,
        })
    }

    /// The invocation a forwarded call travels with: the caller's own, with the
    /// target node stamped on it.
    ///
    /// The attribution is **not** worked out here any more. Every handler already
    /// holds it in [`swarmbot_tools::ToolContext::invocation`], so the port hands
    /// it over and this only records the one fact the caller could not know: which
    /// machine the call ended up being sent to. The old code scanned
    /// `running_turns()` and picked the single turn whose bot was hosted on this
    /// node — an inference that gave up (and sent the [`UNATTRIBUTED`] sentinel)
    /// as soon as two turns ran on one node, which is exactly the parallel case a
    /// multi-bot runtime exists for.
    ///
    /// `node_id` is overwritten rather than left as the caller set it: the wire's
    /// invocation has to name the node the frame actually goes to, and `None` there
    /// means "the bot's host node", which is a different claim from "this node".
    fn invocation_for(&self, node: &NodeId, invocation: &Invocation) -> Invocation {
        let mut stamped = invocation.clone();
        stamped.node_id = Some(node.clone());
        stamped
    }

    /// Why this node cannot run this call, or `None` when it can.
    fn unusable(&self, node: &NodeId, call: &NodeCall) -> Result<Option<String>, ToolError> {
        let stored = match self.store.get_node(node) {
            Ok(Some(node)) => node,
            Ok(None) => {
                return Ok(Some(format!(
                    "node {node} is not connected: no node has paired with this brain under that id"
                )));
            }
            Err(error) => {
                // The roster is unreadable. Saying "not connected" would be a
                // guess, and running the call locally would be worse.
                return Err(ToolError::failed(format!(
                    "cannot read the roster for node {node}: {error}"
                )));
            }
        };
        if stored.revoked {
            return Ok(Some(format!(
                "node {node} is not connected: its owner revoked it; re-pair it to bring it back"
            )));
        }
        if !stored.online {
            return Ok(Some(format!(
                "node {node} is not connected: is `swarmbot-node run` up on that machine?"
            )));
        }
        let capability = capability_of(call);
        if !stored.can(capability) {
            // Declared ∩ approved: a capability a node added later stays off until
            // its owner says yes, and the brain must not route work to it.
            return Ok(Some(format!(
                "node {node} cannot run this call: the `{capability}` capability is not both \
                 declared by the node and approved by its owner"
            )));
        }
        Ok(None)
    }

    async fn unary(
        &self,
        peer: &Peer<S>,
        node: &NodeId,
        outgoing: &Outgoing,
        call: &NodeCall,
    ) -> Result<ToolResult, ToolError> {
        let id = peer.next_id();
        let deadline = deadline_for(call);
        debug!(
            method = outgoing.method,
            request = %id,
            %node,
            deadline_ms = deadline.as_millis(),
            "a remote call goes out"
        );
        match peer
            .call_with_id_deadline(
                id,
                outgoing.method,
                outgoing.params.clone(),
                headers(node),
                deadline,
            )
            .await
        {
            Ok(value) => wire::decode_result(outgoing.method, call, value),
            Err(error) => Err(wire::translate(node, outgoing.method, &error)),
        }
    }

    /// A streaming call: chunks are collected on their own task, because the
    /// driver awaits the buffer and would stall the connection otherwise.
    ///
    /// The deadline covers the whole stream. On expiry the call is withdrawn,
    /// which drops the chunk sink, so the collector finishes instead of waiting
    /// for a stream that will never end — and whatever output *did* arrive is
    /// kept in the error, because a command that printed half its diagnosis
    /// before the node went quiet still said something worth reading.
    async fn stream(
        &self,
        peer: &Peer<S>,
        node: &NodeId,
        outgoing: &Outgoing,
        call: &NodeCall,
    ) -> Result<ToolResult, ToolError> {
        let (tx, rx) = mpsc::channel::<Value>(STREAM_BUFFER);
        let mut collector = tokio::spawn(collect_stream(rx));
        let id = peer.next_id();
        let deadline = deadline_for(call);
        debug!(
            method = outgoing.method,
            request = %id,
            %node,
            deadline_ms = deadline.as_millis(),
            "a remote stream goes out"
        );
        let outcome = peer
            .call_stream_with_id_deadline(
                id,
                outgoing.method,
                outgoing.params.clone(),
                headers(node),
                tx,
                deadline,
            )
            .await;
        let run = match collect_within(&mut collector).await {
            Ok(run) => Some(run),
            // The call is over but the driver has not let go of the sink yet (a
            // socket that is not draining). The deadline is the promise this
            // module makes, so the cleanup does not get to outlast it.
            Err(CollectFailure::Wedged) => None,
            Err(CollectFailure::Join(error)) => {
                return Err(ToolError::failed(format!(
                    "the stream collector failed: {error}"
                )));
            }
        };
        match outcome {
            Ok(()) => match run {
                Some(run) => wire::shell_result(&run),
                // A settled stream releases the sink, so this is unreachable in
                // practice — and still a typed failure rather than a fake
                // success if it ever is not.
                None => Err(ToolError::failed(
                    "the node ended the shell stream but its output could not be collected in time",
                )),
            },
            Err(error) => {
                let translated = wire::translate(node, outgoing.method, &error);
                // Output that arrived before the failure is kept in the message.
                let partial = run.as_ref().map(partial_output).unwrap_or_default();
                if partial.is_empty() {
                    return Err(translated);
                }
                Err(ToolError::failed(format!(
                    "{translated}\n--- partial output before the failure ---\n{partial}"
                )))
            }
        }
    }
}

#[async_trait]
impl<S: Socket> NodeExecutor for NodeDispatcher<S> {
    /// Route one call. `invocation` is the caller's own and is forwarded as it
    /// stands (with the node stamped on it) — the node audits against it and echoes
    /// it in `node.requestApproval`, so it is never re-derived from "who looks like
    /// they are running here".
    async fn exec(
        &self,
        node: &NodeId,
        invocation: &Invocation,
        call: NodeCall,
    ) -> Result<ToolResult, ToolError> {
        if node.is_local() {
            return self.local.exec(node, invocation, call).await;
        }
        // Refuse here rather than on the socket when the roster already knows the
        // answer: an unpaired, revoked or offline node, or one whose owner never
        // approved the capability this call needs. `docs/node-protocol.md` puts
        // this on the brain — a tool the node cannot run is not offered to the
        // model, and a call that arrives anyway does not deserve a 20 s wait for a
        // liveness timeout to explain it.
        if let Some(reason) = self.unusable(node, &call)? {
            return Err(ToolError::failed(reason));
        }
        let Some(peer) = self.gateway.peer(node) else {
            return Err(ToolError::failed(format!(
                "node {node} is not connected: its control socket is gone"
            )));
        };
        let outgoing = wire::encode(self.invocation_for(node, invocation), &call)?;
        if outgoing.streaming {
            self.stream(&peer, node, &outgoing, &call).await
        } else {
            self.unary(&peer, node, &outgoing, &call).await
        }
    }
}

impl<S: Socket> std::fmt::Debug for NodeDispatcher<S> {
    /// Manual: the local executor and the store are trait objects.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeDispatcher")
            .field("gateway", &self.gateway)
            .finish_non_exhaustive()
    }
}

/// The header every forwarded request carries (§4.10).
///
/// It says which node the brain believes it is talking to. The node stamps nothing
/// under `x-swarmbot-*` itself, so this stamp is unambiguous — and a frame that
/// arrives on the wrong socket shows up in a log instead of running somewhere
/// nobody meant.
fn headers(node: &NodeId) -> Vec<(String, String)> {
    vec![(HEADER_NODE_ID.to_string(), node.to_string())]
}

/// The capability a call needs from the node it is aimed at.
pub fn capability_of(call: &NodeCall) -> &'static str {
    match call {
        NodeCall::ShellExec { .. } => "shell",
        _ => "fs",
    }
}

/// The invocation placeholder, for a call that cannot be attributed.
pub fn unattributed() -> Invocation {
    Invocation::new(
        BotId::from_suffix(UNATTRIBUTED),
        ConversationId::from_suffix(UNATTRIBUTED),
    )
}

/// Why a stream collector did not produce a [`ShellRun`].
enum CollectFailure {
    /// The collector task itself failed.
    Join(tokio::task::JoinError),
    /// The chunk sink was still held after the call ended: a driver wedged on a
    /// socket that is not draining.
    Wedged,
}

/// Wait for a stream collector, but not forever.
///
/// The collector ends when the driver drops the chunk sink, which happens as
/// soon as the call settles or is withdrawn — so this normally returns at once.
/// The bound exists for the one case that has no other bound: a driver stuck
/// writing to a socket that is not draining. Abandoning the collector is honest
/// (its output is reported as missing) where waiting past the deadline would
/// break the promise this module makes.
async fn collect_within(collector: &mut JoinHandle<ShellRun>) -> Result<ShellRun, CollectFailure> {
    match tokio::time::timeout(STREAM_COLLECT_GRACE, &mut *collector).await {
        Ok(joined) => joined.map_err(CollectFailure::Join),
        Err(_) => {
            collector.abort();
            Err(CollectFailure::Wedged)
        }
    }
}

/// Collect a stream into a [`ShellRun`].
///
/// Chunks are appended in arrival order. They carry a shared `seq`, and one TCP
/// socket delivers them in order, so arrival order *is* sequence order; the field
/// is there for a future transport that could reorder, and a gap would show up as
/// interleaved output rather than as a silent loss.
async fn collect_stream(mut rx: mpsc::Receiver<Value>) -> ShellRun {
    let mut run = ShellRun::default();
    while let Some(value) = rx.recv().await {
        match serde_json::from_value::<ShellChunk>(value) {
            Ok(ShellChunk::Started { pid, pgid }) => {
                run.pid = Some(pid);
                run.pgid = Some(pgid);
            }
            Ok(ShellChunk::Output { stream, data, .. }) => {
                run.push(stream == OutputStream::Stderr, &data);
            }
            Ok(ShellChunk::Exit {
                code,
                signal,
                timed_out,
                duration_ms,
                output_truncated,
                spill_path,
            }) => {
                run.exit = Some(ShellExit {
                    code,
                    signal,
                    timed_out,
                    duration_ms,
                    output_truncated,
                    spill_path,
                });
            }
            Err(error) => {
                // A chunk this build cannot decode is skipped and counted, never
                // silently dropped: the result says how many lines went missing.
                run.skipped += 1;
                debug!(%error, "a shell chunk could not be decoded and was skipped");
            }
        }
    }
    run
}

/// The output a failed stream managed to deliver, clipped for an error message.
fn partial_output(run: &ShellRun) -> String {
    let mut text = String::new();
    if !run.stdout.is_empty() {
        text.push_str("--- stdout ---\n");
        text.push_str(&run.stdout);
    }
    if !run.stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("--- stderr ---\n");
        text.push_str(&run.stderr);
    }
    text.chars().take(PARTIAL_OUTPUT_CHARS).collect()
}

/// The method a `NodeCall` becomes, for logs and tests.
pub fn method_of(call: &NodeCall) -> &'static str {
    match call {
        NodeCall::FsRead { .. } => rpc::FS_READ,
        NodeCall::FsWrite { .. } => rpc::FS_WRITE,
        NodeCall::FsEdit { .. } => rpc::FS_EDIT,
        NodeCall::FsStat { .. } => rpc::FS_STAT,
        NodeCall::FsList { .. } => rpc::FS_LIST,
        NodeCall::FsGlob { .. } => rpc::FS_GLOB,
        NodeCall::FsGrep { .. } => rpc::FS_GREP,
        NodeCall::FsMkdir { .. } => rpc::FS_MKDIR,
        NodeCall::FsMove { .. } => rpc::FS_MOVE,
        NodeCall::FsRemove { .. } => rpc::FS_REMOVE,
        NodeCall::ShellExec { .. } => rpc::SHELL_EXEC,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use swarmbot_contracts::id::InvocationId;
    use swarmbot_contracts::node::{NodeInfo, NodeKind, PolicyCeiling, PolicyMap};
    use swarmbot_contracts::repo::{BotRepo as _, NodeRepo as _, TurnRepo as _};
    use swarmbot_contracts::turn::{Turn, TurnInitiator};
    use swarmbot_node_protocol::mux::PeerConfig;
    use swarmbot_node_protocol::testing::{LineSocket, duplex_pair};
    use swarmbot_node_protocol::wire::{Frame, Id, parse as parse_frame};
    use swarmbot_store::MemoryStore;

    use crate::gateway::Connected;

    /// The socket type these tests drive: an in-memory pair, no ports.
    type TestSocket = LineSocket<tokio::io::DuplexStream>;

    /// A peer config whose timers cannot fire inside a test: the liveness
    /// timeout would otherwise close the very socket the test leaves silent, and
    /// a ping would put frames between the ones the test asserts on.
    fn quiet_config() -> PeerConfig {
        PeerConfig {
            ping_interval: Duration::from_secs(3600),
            liveness_timeout: Duration::from_secs(3600),
            ..PeerConfig::default()
        }
    }

    fn dispatcher_with(
        turns: Vec<Turn>,
        bots: Vec<swarmbot_contracts::bot::Bot>,
    ) -> NodeDispatcher {
        let store = Arc::new(MemoryStore::default());
        for bot in &bots {
            store.insert_bot(bot).expect("a bot is stored");
        }
        for turn in &turns {
            store.insert_turn(turn).expect("a turn is stored");
        }
        NodeDispatcher {
            local: Arc::new(RecordingExecutor),
            gateway: NodeGateway::new("http://127.0.0.1:1"),
            store,
        }
    }

    /// A dispatcher with one *connected* node, plus the far end of its socket so
    /// the test decides what the node answers — including nothing at all.
    fn dispatcher_with_connected_node(
        node: &NodeId,
    ) -> (Arc<NodeDispatcher<TestSocket>>, TestSocket) {
        let store = Arc::new(MemoryStore::default());
        let mut info = NodeInfo::local(
            "test node",
            Vec::new(),
            PolicyMap::uniform(PolicyCeiling::Always),
        );
        info.id = node.clone();
        info.kind = NodeKind::Remote;
        info.online = true;
        store.insert_node(&info).expect("the node is stored");
        let (mine, theirs) = duplex_pair(64 * 1024);
        let gateway = NodeGateway::<TestSocket>::new("http://127.0.0.1:1");
        let _ = gateway.register(Connected {
            node_id: node.clone(),
            peer: Peer::new(mine, quiet_config()),
            generation: gateway.next_generation(),
            connected_at: Utc::now(),
            protocol: swarmbot_node_protocol::PROTOCOL_VERSION,
            label: "test node".into(),
        });
        let dispatcher = NodeDispatcher::new(Arc::new(RecordingExecutor), gateway, store);
        (dispatcher, theirs)
    }

    /// An executor that never runs anything: these tests are about attribution.
    struct RecordingExecutor;

    #[async_trait]
    impl NodeExecutor for RecordingExecutor {
        async fn exec(
            &self,
            _node: &NodeId,
            _invocation: &Invocation,
            _call: NodeCall,
        ) -> Result<ToolResult, ToolError> {
            Err(ToolError::failed("not used by these tests"))
        }
    }

    /// A caller's invocation, for the tests that only care that *something* was
    /// attributed (the two-turn test builds its own, by hand).
    fn test_invocation() -> Invocation {
        Invocation::new(BotId::new(), ConversationId::new())
    }

    fn bot_on(node: &NodeId, name: &str) -> swarmbot_contracts::bot::Bot {
        let mut bot =
            swarmbot_contracts::bot::Bot::new(name, name, "", "fake", ConversationId::new());
        bot.node = Some(node.clone());
        bot
    }

    fn running(bot: &BotId, conversation: &ConversationId) -> Turn {
        Turn::start(
            bot.clone(),
            conversation.clone(),
            TurnInitiator::Owner,
            None,
            None,
        )
    }

    /// The defect the widened port removes: two turns on one node at the same
    /// time. `attribute()` gave up as soon as it saw more than one candidate and
    /// sent [`unattributed`] for both calls, so the node's audit line and any
    /// approval card named nobody — for the exact concurrency a multi-bot runtime
    /// exists to have. Now each call carries the invocation of the turn that made
    /// it, and nothing on this side has to guess.
    #[tokio::test]
    async fn two_turns_on_one_node_each_carry_their_own_invocation_to_the_node() {
        let node = NodeId::new();
        let (dispatcher, mut node_side) = dispatcher_with_connected_node(&node);

        // Both bots are hosted here and both are mid-turn: the ambiguous case the
        // old inference refused to answer.
        let dev = bot_on(&node, "dev");
        let scout = bot_on(&node, "scout");
        for bot in [&dev, &scout] {
            dispatcher.store.insert_bot(bot).expect("a bot is stored");
        }
        let dev_turn = running(&dev.id, &dev.direct_conversation_id);
        let scout_turn = running(&scout.id, &scout.direct_conversation_id);
        for turn in [&dev_turn, &scout_turn] {
            dispatcher
                .store
                .insert_turn(turn)
                .expect("a turn is stored");
        }
        assert_eq!(
            dispatcher
                .store
                .running_turns()
                .expect("the turns read")
                .len(),
            2,
            "the premise of this test: two turns are running on one node"
        );

        let caller = |bot: &swarmbot_contracts::bot::Bot, turn: &Turn| Invocation {
            id: InvocationId::new(),
            bot_id: bot.id.clone(),
            conversation_id: bot.direct_conversation_id.clone(),
            turn_id: Some(turn.id.clone()),
            task_id: None,
            tool_call_id: None,
            node_id: None,
        };
        let dev_inv = caller(&dev, &dev_turn);
        let scout_inv = caller(&scout, &scout_turn);

        // Two remote calls in flight together, exactly as two bots' turns would
        // be. Neither answers, so the frames are read while the calls wait.
        let first = tokio::spawn({
            let dispatcher = Arc::clone(&dispatcher);
            let node = node.clone();
            let invocation = dev_inv.clone();
            async move {
                dispatcher
                    .exec(
                        &node,
                        &invocation,
                        NodeCall::FsStat {
                            path: "dev.txt".into(),
                        },
                    )
                    .await
            }
        });
        let second = tokio::spawn({
            let dispatcher = Arc::clone(&dispatcher);
            let node = node.clone();
            let invocation = scout_inv.clone();
            async move {
                dispatcher
                    .exec(
                        &node,
                        &invocation,
                        NodeCall::FsStat {
                            path: "scout.txt".into(),
                        },
                    )
                    .await
            }
        });

        let mut seen = Vec::new();
        for _ in 0..2 {
            let text = node_side
                .recv_text()
                .await
                .expect("a frame")
                .expect("a request frame");
            match parse_frame(&text).expect("parses") {
                Frame::Request { params, .. } => seen.push(params),
                other => panic!("expected a Request frame, got {other:?}"),
            }
        }
        first.abort();
        second.abort();

        // Each frame is attributed to *its* caller, by bot, conversation and turn —
        // matched on the call's path so this does not depend on frame order.
        let attribution = |path: &str| {
            let params = seen
                .iter()
                .find(|params| params["path"] == path)
                .unwrap_or_else(|| panic!("no frame for {path}: {seen:?}"));
            &params["inv"]
        };
        let dev_seen = attribution("dev.txt");
        assert_eq!(dev_seen["bot_id"], Value::from(dev.id.to_string()));
        assert_eq!(
            dev_seen["conversation_id"],
            Value::from(dev.direct_conversation_id.to_string())
        );
        assert_eq!(dev_seen["turn_id"], Value::from(dev_turn.id.to_string()));
        assert_eq!(dev_seen["id"], Value::from(dev_inv.id.to_string()));

        let scout_seen = attribution("scout.txt");
        assert_eq!(scout_seen["bot_id"], Value::from(scout.id.to_string()));
        assert_eq!(
            scout_seen["conversation_id"],
            Value::from(scout.direct_conversation_id.to_string())
        );
        assert_eq!(
            scout_seen["turn_id"],
            Value::from(scout_turn.id.to_string())
        );

        // And neither is the sentinel: the old code would have sent this for both.
        for params in [dev_seen, scout_seen] {
            assert_ne!(
                params["bot_id"],
                Value::from(format!("bot_{UNATTRIBUTED}")),
                "a call that reached the node named nobody: {params}"
            );
        }
    }

    #[test]
    fn the_sentinel_is_recognisable_and_stable() {
        // Kept for the one caller that still has no attribution to offer: a node's
        // own `node.requestApproval` naming a bot this brain cannot place.
        let invocation = unattributed();
        assert_eq!(invocation.bot_id.as_str(), "bot_unknown");
        assert_eq!(invocation.conversation_id.as_str(), "cnv_unknown");
        // Two sentinels differ in id, so a node's audit log can still tell two
        // calls apart even when neither could be attributed.
        assert_ne!(unattributed().id, invocation.id);
    }

    #[tokio::test]
    async fn a_call_to_an_unconnected_node_fails_rather_than_running_locally() {
        let node = NodeId::new();
        let dispatcher = dispatcher_with(vec![], vec![]);
        let error = dispatcher
            .exec(
                &node,
                &test_invocation(),
                NodeCall::ShellExec {
                    command: "rm -rf /".into(),
                    cwd: None,
                    timeout_ms: None,
                    env: None,
                },
            )
            .await
            .expect_err("no such node is connected");
        match error {
            ToolError::Failed { message } => {
                assert!(message.contains("not connected"), "{message}");
                assert!(message.contains(&node.to_string()), "{message}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_filesystem_call_gets_the_short_deadline() {
        for call in [
            NodeCall::FsRead {
                path: "a.txt".into(),
                offset: None,
                limit: None,
                encoding: None,
            },
            NodeCall::FsStat {
                path: "a.txt".into(),
            },
            NodeCall::FsGrep {
                pattern: "todo".into(),
                path: None,
                glob: None,
                ignore_case: None,
                literal: None,
                context_lines: None,
                output_mode: None,
                limit: None,
            },
        ] {
            assert_eq!(deadline_for(&call), FS_CALL_DEADLINE);
        }
        // The bound is stated here so that changing it is a deliberate change:
        // it has to be long enough for a big grep on a slow disk and short
        // enough that a bot's single turn survives a stuck call.
        assert!(FS_CALL_DEADLINE >= Duration::from_secs(30));
        assert!(FS_CALL_DEADLINE <= Duration::from_secs(120));
    }

    #[test]
    fn a_shell_call_is_bounded_by_its_own_timeout_not_by_the_fs_deadline() {
        // The caller's `timeout_ms` is what a shell command is allowed to spend,
        // so the deadline has to follow it — plus a grace for the node to kill
        // the group, drain the pipes and send the `Exit` chunk.
        let with_timeout = deadline_for(&NodeCall::ShellExec {
            command: "cargo test".into(),
            cwd: None,
            timeout_ms: Some(5_000),
            env: None,
        });
        assert_eq!(with_timeout, Duration::from_secs(5) + SHELL_DEADLINE_GRACE);
        // Without one, the local executor's default is the budget — not the fs
        // bound, which would cut a legitimate two-minute build in half.
        let default = deadline_for(&NodeCall::ShellExec {
            command: "true".into(),
            cwd: None,
            timeout_ms: None,
            env: None,
        });
        assert_eq!(
            default,
            Duration::from_millis(DEFAULT_SHELL_TIMEOUT_MS) + SHELL_DEADLINE_GRACE
        );
        assert!(
            default > FS_CALL_DEADLINE,
            "a shell command must not be cut at the filesystem bound"
        );
    }

    /// The defect this pins: a node that keeps its socket alive while a handler
    /// hangs left the bot's only turn waiting forever, because nothing bounded a
    /// call. Now the call fails with a typed error naming the node, and the
    /// turn gets its model back.
    #[tokio::test(start_paused = true)]
    async fn a_hanging_node_fails_the_call_with_a_typed_error_naming_the_node() {
        let node = NodeId::new();
        let (dispatcher, mut node_side) = dispatcher_with_connected_node(&node);
        let call = tokio::spawn({
            let dispatcher = Arc::clone(&dispatcher);
            let node = node.clone();
            async move {
                dispatcher
                    .exec(
                        &node,
                        &test_invocation(),
                        NodeCall::FsRead {
                            path: "notes.txt".into(),
                            offset: None,
                            limit: None,
                            encoding: None,
                        },
                    )
                    .await
            }
        });

        // The request reaches the node; the node deliberately never answers.
        let request = node_side
            .recv_text()
            .await
            .expect("a frame")
            .expect("the request frame");
        assert!(matches!(
            parse_frame(&request).expect("parses"),
            Frame::Request { .. }
        ));

        // Before the deadline the call is still running: this is a deadline, not
        // an instant failure.
        tokio::time::advance(FS_CALL_DEADLINE - Duration::from_secs(1)).await;
        assert!(!call.is_finished(), "the call failed before its deadline");

        // After it, the tool call fails instead of holding the bot's turn.
        tokio::time::advance(Duration::from_secs(2)).await;
        let error = call
            .await
            .expect("the exec task finishes")
            .expect_err("a node that never answers cannot produce a result");
        match error {
            ToolError::Failed { message } => {
                assert!(message.contains(&node.to_string()), "{message}");
                assert!(message.contains(rpc::FS_READ), "{message}");
                assert!(message.contains("did not answer"), "{message}");
            }
            // A timeout is infrastructure breaking, never a policy denial the
            // model could retry into a different answer.
            other => panic!("a timeout is a failure, not a denial: {other:?}"),
        }

        // And the node was asked to stop, not merely abandoned.
        let interrupt = node_side
            .recv_text()
            .await
            .expect("a frame")
            .expect("the interrupt frame");
        match parse_frame(&interrupt).expect("parses") {
            Frame::Interrupt { request_id } => assert_eq!(request_id, Id::Number(0)),
            other => panic!("expected an Interrupt frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_local_call_never_touches_the_gateway() {
        let dispatcher = dispatcher_with(vec![], vec![]);
        // The recording executor proves the local branch was taken: the gateway is
        // empty, so a remote route would have failed with "not connected".
        let error = dispatcher
            .exec(
                &NodeId::local(),
                &test_invocation(),
                NodeCall::FsStat { path: ".".into() },
            )
            .await
            .expect_err("the recording executor always fails");
        assert!(
            error.to_string().contains("not used by these tests"),
            "{error}"
        );
    }

    #[test]
    fn every_call_kind_has_a_method() {
        for call in [
            NodeCall::FsRead {
                path: "a".into(),
                offset: None,
                limit: None,
                encoding: None,
            },
            NodeCall::FsStat { path: "a".into() },
            NodeCall::ShellExec {
                command: "true".into(),
                cwd: None,
                timeout_ms: None,
                env: None,
            },
        ] {
            assert!(!method_of(&call).is_empty());
        }
        assert_eq!(
            method_of(&NodeCall::ShellExec {
                command: "true".into(),
                cwd: None,
                timeout_ms: None,
                env: None,
            }),
            rpc::SHELL_EXEC
        );
    }

    #[tokio::test]
    async fn a_stream_is_collected_in_arrival_order() {
        let (tx, rx) = mpsc::channel(8);
        let collector = tokio::spawn(collect_stream(rx));
        for value in [
            serde_json::to_value(ShellChunk::Started { pid: 7, pgid: 7 }).expect("a chunk"),
            serde_json::to_value(ShellChunk::Output {
                stream: OutputStream::Stdout,
                data: "one\n".into(),
                seq: 1,
            })
            .expect("a chunk"),
            serde_json::to_value(ShellChunk::Output {
                stream: OutputStream::Stderr,
                data: "oops".into(),
                seq: 2,
            })
            .expect("a chunk"),
            serde_json::to_value(ShellChunk::Output {
                stream: OutputStream::Stdout,
                data: "two\n".into(),
                seq: 3,
            })
            .expect("a chunk"),
            serde_json::to_value(ShellChunk::Exit {
                code: Some(0),
                signal: None,
                timed_out: false,
                duration_ms: 3,
                output_truncated: None,
                spill_path: None,
            })
            .expect("a chunk"),
            // An undecodable chunk is counted, not dropped silently.
            serde_json::json!({"_tag": "Mystery"}),
        ] {
            tx.send(value).await.expect("the collector is reading");
        }
        drop(tx);
        let run = collector.await.expect("the collector finishes");
        assert_eq!(run.stdout, "one\ntwo\n");
        assert_eq!(run.stderr, "oops");
        assert_eq!(run.pid, Some(7));
        assert_eq!(run.skipped, 1);
        assert_eq!(run.exit.as_ref().expect("an exit").code, Some(0));
        let result = wire::shell_result(&run).expect("renders");
        assert!(
            result
                .text_only()
                .contains("1 chunk(s) could not be decoded")
                || result.text_only().contains("could not be decoded"),
            "{}",
            result.text_only()
        );
    }
}
