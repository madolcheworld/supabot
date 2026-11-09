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

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use swarmbot_contracts::Invocation;
use swarmbot_contracts::id::{BotId, ConversationId, InvocationId, NodeId};
use swarmbot_contracts::repo::Persistence;
use swarmbot_contracts::tool::{ToolError, ToolResult};
use swarmbot_node_protocol::mux::Peer;
use swarmbot_node_protocol::rpc::{self, HEADER_NODE_ID, OutputStream, ShellChunk};
use swarmbot_tools::{NodeCall, NodeExecutor};

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

/// The bot and conversation a call is attributed to when the port gives the
/// executor nothing to attribute it with. See [`NodeDispatcher::attribute`].
pub const UNATTRIBUTED: &str = "unknown";

/// Routes a call to the local machine or to a connected node.
pub struct NodeDispatcher {
    local: Arc<dyn NodeExecutor>,
    gateway: Arc<NodeGateway>,
    store: Arc<dyn Persistence>,
}

impl NodeDispatcher {
    pub fn new(
        local: Arc<dyn NodeExecutor>,
        gateway: Arc<NodeGateway>,
        store: Arc<dyn Persistence>,
    ) -> Arc<Self> {
        Arc::new(Self {
            local,
            gateway,
            store,
        })
    }

    /// The invocation a remote call travels with.
    ///
    /// The [`NodeExecutor`] port hands an executor a node id and a call and
    /// nothing else — no bot, no conversation, no turn — while a node audits every
    /// call against the invocation it is given and echoes it back in
    /// `node.requestApproval`. So the brain has to say *something*, and it says
    /// something true or nothing at all:
    ///
    /// * a tool call only happens inside a turn, and a bot runs one turn at a
    ///   time, so when **exactly one** running turn belongs to a bot hosted on
    ///   this node, that turn is the caller;
    /// * anything else — nobody running, or two bots on one node mid-turn — gets
    ///   the [`UNATTRIBUTED`] sentinel rather than a guess. A card in the wrong
    ///   conversation would be a lie about who asked, and the gateway says so
    ///   plainly when it has to answer one.
    ///
    /// Fixing this properly means widening the port (`exec` taking the
    /// invocation); that is a `swarmbot-tools` change, not one this crate may
    /// make from inside an adapter.
    fn attribute(&self, node: &NodeId) -> Invocation {
        let turns = match self.store.running_turns() {
            Ok(turns) => turns,
            Err(error) => {
                warn!(%error, "cannot read the running turns; a remote call goes unattributed");
                return unattributed();
            }
        };
        let mut candidates = Vec::new();
        for turn in turns {
            let hosted_here = self
                .store
                .get_bot(&turn.bot_id)
                .ok()
                .flatten()
                .is_some_and(|bot| &bot.host_node() == node);
            if hosted_here {
                candidates.push(turn);
            }
        }
        let [turn] = candidates.as_slice() else {
            if candidates.len() > 1 {
                debug!(
                    node = %node,
                    candidates = candidates.len(),
                    "more than one turn is running on this node; the call goes unattributed \
                     rather than being blamed on a guess"
                );
            }
            return unattributed();
        };
        Invocation {
            id: InvocationId::new(),
            bot_id: turn.bot_id.clone(),
            conversation_id: turn.conversation_id.clone(),
            turn_id: Some(turn.id.clone()),
            task_id: turn.task_id.clone(),
            tool_call_id: None,
            node_id: Some(node.clone()),
        }
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
        peer: &Peer<GatewaySocket>,
        node: &NodeId,
        outgoing: &Outgoing,
        call: &NodeCall,
    ) -> Result<ToolResult, ToolError> {
        let id = peer.next_id();
        debug!(method = outgoing.method, request = %id, %node, "a remote call goes out");
        match peer
            .call_with_id(id, outgoing.method, outgoing.params.clone(), headers(node))
            .await
        {
            Ok(value) => wire::decode_result(outgoing.method, call, value),
            Err(error) => Err(wire::translate(outgoing.method, &error)),
        }
    }

    /// A streaming call: chunks are collected on their own task, because the
    /// driver awaits the buffer and would stall the connection otherwise.
    async fn stream(
        &self,
        peer: &Peer<GatewaySocket>,
        node: &NodeId,
        outgoing: &Outgoing,
    ) -> Result<ToolResult, ToolError> {
        let (tx, rx) = mpsc::channel::<Value>(STREAM_BUFFER);
        let collector = tokio::spawn(collect_stream(rx));
        let id = peer.next_id();
        debug!(method = outgoing.method, request = %id, %node, "a remote stream goes out");
        let outcome = peer
            .call_stream_with_id(
                id,
                outgoing.method,
                outgoing.params.clone(),
                headers(node),
                tx,
            )
            .await;
        let run = collector
            .await
            .map_err(|error| ToolError::failed(format!("the stream collector failed: {error}")))?;
        match outcome {
            Ok(()) => wire::shell_result(&run),
            Err(error) => {
                let translated = wire::translate(outgoing.method, &error);
                // Output that arrived before the failure is kept in the message:
                // a command that printed half its diagnosis and then lost the
                // socket still said something worth reading.
                let partial = partial_output(&run);
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
impl NodeExecutor for NodeDispatcher {
    async fn exec(&self, node: &NodeId, call: NodeCall) -> Result<ToolResult, ToolError> {
        if node.is_local() {
            return self.local.exec(node, call).await;
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
        let invocation = self.attribute(node);
        let outgoing = wire::encode(invocation, &call)?;
        if outgoing.streaming {
            self.stream(&peer, node, &outgoing).await
        } else {
            self.unary(&peer, node, &outgoing, &call).await
        }
    }
}

impl std::fmt::Debug for NodeDispatcher {
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

/// Is this invocation the placeholder rather than a real bot?
pub fn is_unattributed(invocation: &Invocation) -> bool {
    invocation.bot_id.as_str() == BotId::from_suffix(UNATTRIBUTED).as_str()
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
    use swarmbot_contracts::repo::{BotRepo as _, TurnRepo as _};
    use swarmbot_contracts::turn::{Turn, TurnInitiator};
    use swarmbot_store::MemoryStore;

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

    /// An executor that never runs anything: these tests are about attribution.
    struct RecordingExecutor;

    #[async_trait]
    impl NodeExecutor for RecordingExecutor {
        async fn exec(&self, _node: &NodeId, _call: NodeCall) -> Result<ToolResult, ToolError> {
            Err(ToolError::failed("not used by these tests"))
        }
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

    #[test]
    fn a_single_running_turn_on_the_node_attributes_the_call() {
        let node = NodeId::new();
        let bot = bot_on(&node, "dev");
        let turn = running(&bot.id, &bot.direct_conversation_id);
        let dispatcher = dispatcher_with(vec![turn.clone()], vec![bot.clone()]);

        let invocation = dispatcher.attribute(&node);
        assert_eq!(invocation.bot_id, bot.id);
        assert_eq!(invocation.conversation_id, bot.direct_conversation_id);
        assert_eq!(invocation.turn_id, Some(turn.id));
        assert_eq!(invocation.node_id, Some(node.clone()));
        assert!(!is_unattributed(&invocation));
    }

    #[test]
    fn an_ambiguous_or_empty_node_is_never_guessed_at() {
        let node = NodeId::new();
        let one = bot_on(&node, "dev");
        let two = bot_on(&node, "scout");
        let elsewhere = bot_on(&NodeId::new(), "lead");

        // Nobody running.
        let dispatcher = dispatcher_with(vec![], vec![one.clone()]);
        assert!(is_unattributed(&dispatcher.attribute(&node)));

        // Two turns on one node: blaming either would be a guess.
        let dispatcher = dispatcher_with(
            vec![
                running(&one.id, &one.direct_conversation_id),
                running(&two.id, &two.direct_conversation_id),
            ],
            vec![one.clone(), two],
        );
        assert!(is_unattributed(&dispatcher.attribute(&node)));

        // A turn running on a *different* node's bot does not count.
        let dispatcher = dispatcher_with(
            vec![running(&elsewhere.id, &elsewhere.direct_conversation_id)],
            vec![elsewhere],
        );
        assert!(is_unattributed(&dispatcher.attribute(&node)));
    }

    #[test]
    fn the_sentinel_is_recognisable_and_stable() {
        let invocation = unattributed();
        assert!(is_unattributed(&invocation));
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

    #[tokio::test]
    async fn a_local_call_never_touches_the_gateway() {
        let dispatcher = dispatcher_with(vec![], vec![]);
        // The recording executor proves the local branch was taken: the gateway is
        // empty, so a remote route would have failed with "not connected".
        let error = dispatcher
            .exec(&NodeId::local(), NodeCall::FsStat { path: ".".into() })
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
