//! An ACP agent's **own** tool calls land in the transcript, marked as the
//! agent's (D9.31).
//!
//! Why this is its own file: the assertion that matters is not "an event was
//! emitted" — `crates/llm/tests/acp.rs` covers that — but "a real turn through
//! the real runner produced a transcript row a reader can trust". So this drives
//! `Swarm::send` and then reads the store.
//!
//! The two properties pinned here:
//!
//! * one agent call becomes **one** row, even though an agent describes it with
//!   several `session/update`s (`pending` → `in_progress` → `completed`), and the
//!   row carries the arguments the agent actually sent;
//! * the row says `origin = Agent` and has **no node**: the agent ran it in its
//!   own sandbox, and no `PolicyEngine`, `AuditLog` or `NodeExecutor` of ours
//!   was anywhere in the path. A row that looked like ours would credit our
//!   policy for a command it never gated — the exact dishonesty the honesty
//!   rules are about.
//!
//! Everything is offline: the "agent" is a double that emits the same events the
//! ACP adapter emits, and the store is in memory.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use swarmbot_contracts::config::{BackendKind, BackendSpec, BrainConfig};
use swarmbot_contracts::entry::{ToolCallOrigin, ToolCallStatus};
use swarmbot_contracts::id::ConversationId;
use swarmbot_core::compose;
use swarmbot_core::swarm::Swarm;
use swarmbot_llm::{
    AgentToolCall, AgentToolState, BackendStatus, Capabilities, ChatRequest, ChatResponse,
    FinishReason, LlmBackend, LlmError, LlmEvent,
};

/// A backend that behaves like an ACP agent for one turn: it runs its own tool
/// and streams everything it would report, including the `ChatResponse` text.
struct AgentDouble {
    /// The calls this "agent" reports, in order: each one is the full sequence
    /// of notifications a real agent sends for a single call.
    scripts: Vec<Vec<AgentToolCall>>,
    /// The `cwd` each turn handed the agent, in order. This is the field
    /// `settings.agent_cwd` exists to change (D9.32), so the test reads it
    /// rather than trusting the config to have been wired.
    cwds: Mutex<Vec<Option<PathBuf>>>,
}

#[async_trait]
impl LlmBackend for AgentDouble {
    fn id(&self) -> &str {
        "agent-double"
    }

    fn kind(&self) -> BackendKind {
        BackendKind::Acp
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: true,
            // The agent owns its tool loop: the runner must not drive one.
            tool_calls: false,
            reasoning: true,
            steer: false,
            survives_restart: false,
        }
    }

    async fn chat(
        &self,
        request: ChatRequest,
        events: tokio::sync::mpsc::Sender<LlmEvent>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ChatResponse, LlmError> {
        self.cwds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.cwd.clone());
        for call in self.scripts.iter().flatten() {
            let _ = events
                .send(LlmEvent::AgentToolCall(Box::new(call.clone())))
                .await;
        }
        let mut response = ChatResponse::empty();
        response.text = "I read the file and it says hello.".to_string();
        let _ = events
            .send(LlmEvent::Finished {
                finish_reason: FinishReason::Stop,
                usage: response.usage,
                text: response.text.clone(),
                reasoning: String::new(),
                tool_calls: Vec::new(),
            })
            .await;
        Ok(response)
    }

    async fn probe(&self) -> Result<BackendStatus, LlmError> {
        Ok(BackendStatus {
            backend: self.id().to_string(),
            ok: true,
            detail: "agent double".to_string(),
            model: None,
            version: None,
        })
    }
}

struct Rig {
    swarm: Arc<Swarm>,
    dm: ConversationId,
    /// The double, so a test can read what the runner actually sent it.
    backend: Arc<AgentDouble>,
    _dir: tempfile::TempDir,
}

fn rig(scripts: Vec<Vec<AgentToolCall>>) -> Rig {
    rig_with(scripts, None)
}

fn rig_with(scripts: Vec<Vec<AgentToolCall>>, agent_cwd: Option<PathBuf>) -> Rig {
    let dir = tempfile::tempdir().expect("a temp dir");
    // The config names only the scripted fake — a config that named the double
    // would be a config naming a backend the brain cannot construct, and this
    // test is about the runner, not about config loading. The double is
    // registered on the registry and every bot is pointed at it *before*
    // `boot`, so the runners start on it.
    // The config declares an ACP backend (`validate` demands a `command` for
    // one), and the double is registered as an override for that id below, so
    // the command is never spawned: what runs is `AgentDouble`.
    let mut acp = BackendSpec::fake("agent-double");
    acp.kind = BackendKind::Acp;
    acp.model = None;
    acp.options.insert(
        "command".to_string(),
        serde_json::json!("/nonexistent/swarmbot-test/agent"),
    );
    let settings = swarmbot_contracts::config::Settings {
        agent_cwd: agent_cwd
            .as_ref()
            .map(|path| path.to_string_lossy().to_string()),
        ..Default::default()
    };
    let config = BrainConfig {
        default_backend: "agent-double".to_string(),
        backends: vec![BackendSpec::fake("fake"), acp],
        team: Some(BrainConfig::demo_team("agent-double")),
        settings,
        ..BrainConfig::default()
    };

    let swarm = compose::memory(config, dir.path(), vec![], &BTreeMap::new()).expect("a swarm");
    let backend = Arc::new(AgentDouble {
        scripts,
        cwds: Mutex::new(Vec::new()),
    });
    swarm
        .backends
        .register("agent-double", Arc::clone(&backend) as Arc<dyn LlmBackend>);
    swarm.boot().expect("boots");
    swarm.start_expiry_ticker();

    let lead = swarm.bots.by_name("lead").expect("a lead").id;
    let dm = swarm
        .bots
        .get(&lead)
        .expect("the lead")
        .direct_conversation_id;
    Rig {
        swarm,
        dm,
        backend,
        _dir: dir,
    }
}

impl Rig {
    /// Send one message to the lead's DM and wait for the team to go quiet. The
    /// runner is the real one; only the model and the store are doubles.
    async fn one_turn(&self, text: &str) {
        self.swarm.send(&self.dm, text).expect("sent");
        let idle = self.swarm.wait_for_idle(Duration::from_secs(20)).await;
        assert!(idle, "the bot finished its turn");
    }

    fn tool_rows(&self) -> Vec<swarmbot_contracts::entry::ToolCallEntry> {
        self.swarm
            .transcript
            .tail(&self.dm, 100)
            .expect("the transcript reads")
            .into_iter()
            .filter_map(|entry| entry.as_tool_call().cloned())
            .collect()
    }
}

/// The announcements a real agent sends for one `read` call, captured from
/// `opencode acp` on 2026-11-14 — including the terminal update that repeats
/// `locations` but **not** `rawInput`.
fn read_call() -> Vec<AgentToolCall> {
    vec![
        AgentToolCall {
            id: "chatcmpl-tool-aeff".into(),
            tool: "read".into(),
            args: None,
            locations: Vec::new(),
            state: AgentToolState::Pending,
            result_text: None,
        },
        AgentToolCall {
            id: "chatcmpl-tool-aeff".into(),
            tool: "read".into(),
            args: Some(serde_json::json!({"path": "README.md", "limit": 5})),
            locations: vec!["/repo/README.md".into()],
            state: AgentToolState::Running,
            result_text: None,
        },
        AgentToolCall {
            id: "chatcmpl-tool-aeff".into(),
            tool: "read".into(),
            args: None,
            locations: vec!["/repo/README.md".into()],
            state: AgentToolState::Completed,
            result_text: Some("Read file README.md, lines 1-5\n1: # SwarmBot".into()),
        },
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_records_the_agents_own_tool_call_as_the_agents() {
    let rig = rig(vec![read_call()]);
    rig.one_turn("read the README and tell me the first line")
        .await;

    let rows = rig.tool_rows();
    assert_eq!(
        rows.len(),
        1,
        "three notifications describing one call are one row; transcript: {:#?}",
        rig.swarm.transcript.tail(&rig.dm, 50).expect("tail")
    );
    let row = &rows[0];
    assert_eq!(row.tool, "read");
    assert_eq!(
        row.origin,
        ToolCallOrigin::Agent,
        "recorded as the agent's call, not ours"
    );
    assert_eq!(
        row.node_id, None,
        "no SwarmBot node ran it, so naming one would be a lie"
    );
    assert_eq!(
        row.turn_id.as_ref(),
        Some(&rig.turn_id()),
        "the row belongs to the turn that produced it"
    );
    assert_eq!(row.status, ToolCallStatus::Completed);
    assert_eq!(
        row.args,
        serde_json::json!({"path": "README.md", "limit": 5}),
        "the agent's real arguments survive the update that omits them"
    );
    assert!(
        row.result_text
            .as_deref()
            .unwrap_or_default()
            .contains("# SwarmBot")
    );
}

impl Rig {
    /// The one turn the DM has, whatever its id: the test asserts the row hangs
    /// off *a* turn rather than inventing an id.
    fn turn_id(&self) -> swarmbot_contracts::id::TurnId {
        let turns = self.swarm.store.turns_in(&self.dm, 10).expect("turns");
        turns.last().expect("the turn ran").id.clone()
    }
}

/// A tool call the agent never finished stays open — and the store must say so
/// rather than showing a call that looks done.
#[tokio::test(flavor = "multi_thread")]
async fn an_unfinished_agent_call_is_not_recorded_as_completed() {
    let rig = rig(vec![vec![AgentToolCall {
        id: "tool-hung".into(),
        tool: "bash".into(),
        args: Some(serde_json::json!({"command": "sleep 999"})),
        locations: Vec::new(),
        state: AgentToolState::Running,
        result_text: None,
    }]]);
    rig.one_turn("run something slow").await;

    let rows = rig.tool_rows();
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(
        rows[0].status,
        ToolCallStatus::Running,
        "the agent never reported an end, so the row must not claim one"
    );
    assert_eq!(rows[0].ended_at, None);
    assert_eq!(rows[0].duration_ms, None, "no duration was reported");
}

/// Two calls are two rows, in the order the agent made them.
#[tokio::test(flavor = "multi_thread")]
async fn two_agent_calls_are_two_rows_in_order() {
    let rig = rig(vec![
        read_call(),
        vec![AgentToolCall {
            id: "tool-grep".into(),
            tool: "grep".into(),
            args: Some(serde_json::json!({"pattern": "swarmbot"})),
            locations: Vec::new(),
            state: AgentToolState::Completed,
            result_text: Some("3 matches".into()),
        }],
    ]);
    rig.one_turn("look something up").await;

    let tools: Vec<String> = rig.tool_rows().iter().map(|row| row.tool.clone()).collect();
    assert_eq!(
        tools,
        vec!["read".to_string(), "grep".to_string()],
        "the transcript keeps the order the agent worked in"
    );
}

/// `settings.agent_cwd` is what decides where an agent that owns its tool loop
/// starts, and therefore which files a relative path reaches (D9.32).
///
/// Two facts, both about the *runner* rather than the config: the value reaches
/// the backend as `ChatRequest::cwd`, and the absence of it keeps the bot's
/// private workspace — a bot that silently started somewhere else because a
/// field was missing would be the worst kind of surprise.
#[tokio::test(flavor = "multi_thread")]
async fn the_agent_starts_where_the_settings_say() {
    // A directory that is emphatically not a bot workspace: the checkout.
    let checkout = tempfile::tempdir().expect("a temp dir");
    let rig = rig_with(Vec::new(), Some(checkout.path().to_path_buf()));
    rig.one_turn("where are you working?").await;

    let seen = rig
        .backend
        .cwds
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(seen.len(), 1, "one turn, one session");
    assert_eq!(
        seen[0].as_deref(),
        Some(checkout.path()),
        "the agent got the configured directory, not the private workspace"
    );
}

/// Without `agent_cwd`, the session starts in the bot's own workspace — the
/// default this field must not change by accident.
#[tokio::test(flavor = "multi_thread")]
async fn without_agent_cwd_the_private_workspace_is_used() {
    let rig = rig_with(Vec::new(), None);
    rig.one_turn("where are you working?").await;
    let seen = rig
        .backend
        .cwds
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(seen.len(), 1);
    let cwd = seen[0].as_ref().expect("a local bot always gets a cwd");
    assert!(
        cwd.components()
            .any(|part| part.as_os_str() == "workspaces"),
        "the private workspace lives under <data>/workspaces: {}",
        cwd.display()
    );
    assert!(
        cwd.ends_with("lead"),
        "and it is the bot's own: {}",
        cwd.display()
    );
}
