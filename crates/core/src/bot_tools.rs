//! Bot-facing tools: how a model cooperates with its team.
//!
//! This is the half of the tool catalogue that turns a pile of bots into a
//! *team*: talking to each other (`message_bot`, `post_to_group`, `react`),
//! delegating work (`handoff_task` and the `*_task` tools), asking the human
//! (`ask_user`, `request_approval`) and seeing the roster (`list_bots`,
//! `list_nodes`). The node tools (files, shell) live in `swarmbot_tools`; the
//! brain adds these to the same registry with [`register_bot_tools`].
//!
//! Three rules shape every handler:
//!
//! * **never block on a human** — `ask_user` and `request_approval` raise a
//!   card and return `{status: pending, id}` at once; the answer wakes the bot
//!   later as a delivery. A tool call that waited for a human would hold the
//!   turn (and the model's whole context) hostage for minutes;
//! * **the services decide, the tools translate** — the hop limit, "group work
//!   stays in its group", the task lifecycle and the approval rules are all
//!   enforced by [`Messaging`], [`BackgroundTasks`] and [`ApprovalService`].
//!   A handler here only resolves the caller, calls the service, and turns the
//!   outcome into one or two lines a model can act on;
//! * **results carry the ids the next call needs** — a task id or a request id
//!   is part of the result text, because `check_task` and the answer delivery
//!   refer to it.
//!
//! Errors are text too: a [`DomainError`] from a service becomes a
//! [`ToolError::Failed`] whose message says what the model can do instead
//! ("there is no bot called `nobody`; the team is @lead, @scout").

use std::sync::Arc;

use chrono::Utc;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::delivery::Urgency;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{RequestId, TaskId, TurnId};
use swarmbot_contracts::node::{NodeKind, RootMode};
use swarmbot_contracts::repo::Persistence;
use swarmbot_contracts::request::{
    APPROVAL_EXPIRY_SECS, FieldKind, FormField, RequestKind, RequestStatus, RuntimeRequest,
};
use swarmbot_contracts::task::{StepKind, Task, TaskStatus};
use swarmbot_contracts::tool::{Content, ToolError, ToolResult, ToolSpec, Toolset};
use swarmbot_tools::{ToolContext, ToolHandler, ToolRegistry};

use crate::approval::ApprovalService;
use crate::bus::LiveHub;
use crate::messaging::{Messaging, SendContext};
use crate::runners::RunnerMap;
use crate::session::MEMORY_INDEX_LIMIT_BYTES;
use crate::tasks::BackgroundTasks;
use crate::transcript::Transcript;

/// How many tasks `list_tasks` shows before it starts eliding the oldest.
const MAX_LISTED_TASKS: usize = 30;
/// How many of a task's steps `check_task` shows: enough to see what the task
/// is doing, few enough to stay well under `TOOL_RESULT_LIMIT_BYTES`.
const MAX_TASK_STEPS: usize = 20;
/// How large one remembered fact may be. A single line is the shape; this only
/// stops a model from pasting a whole document into a "fact" and crowding out
/// everything it already knows.
const MAX_MEMORY_FACT_BYTES: usize = 512;

/// Everything the bot-facing tools share. Built once by the brain (the same
/// services the runners use, so a tool call and the orchestration agree on
/// every rule); each handler holds a clone, which is cheap — all `Arc`s.
#[derive(Clone)]
pub struct BotToolEnv {
    pub store: Arc<dyn Persistence>,
    pub hub: Arc<LiveHub>,
    pub transcript: Arc<Transcript>,
    pub messaging: Arc<Messaging>,
    pub tasks: Arc<BackgroundTasks>,
    pub approval: Arc<ApprovalService>,
    pub runners: Arc<RunnerMap>,
}

/// The catalogue, as a table rather than thirteen near-identical structs:
/// every tool shares one environment and one resolution step
/// ([`BotToolEnv::send_context`]), and keeping the specs in a single `match`
/// is what makes the catalogue reviewable in one screen.
#[derive(Clone, Copy)]
enum BotToolKind {
    MessageBot,
    PostToGroup,
    HandoffTask,
    React,
    ListBots,
    ListNodes,
    AskUser,
    RequestApproval,
    StartTask,
    ListTasks,
    CheckTask,
    UpdateTask,
    CancelTask,
    Remember,
}

impl BotToolKind {
    /// All of them, in catalogue order.
    const ALL: [Self; 14] = [
        Self::MessageBot,
        Self::PostToGroup,
        Self::HandoffTask,
        Self::React,
        Self::ListBots,
        Self::ListNodes,
        Self::AskUser,
        Self::RequestApproval,
        Self::StartTask,
        Self::ListTasks,
        Self::CheckTask,
        Self::UpdateTask,
        Self::CancelTask,
        Self::Remember,
    ];

    /// The definition a model sees. Every schema is a plain object schema —
    /// named properties, an explicit `required` — because the registry
    /// validates arguments against it before the handler runs.
    fn spec(self) -> ToolSpec {
        match self {
            Self::MessageBot => ToolSpec::new(
                "message_bot",
                "Send a message to another bot on the team. Use `urgent` only for asks that \
                 cannot wait — it may be steered into the turn the recipient is running; \
                 `background` (the default) is read on their next turn. From a group turn, a \
                 message to a fellow member is posted in the group, where the owner can see it.",
                json!({
                    "type": "object",
                    "properties": {
                        "to": {"type": "string", "description": "recipient handle, e.g. `scout` or `@scout`"},
                        "text": {"type": "string", "description": "the message; say what you need and why"},
                        "urgency": {"type": "string", "enum": ["urgent", "background"], "description": "default `background`; `urgent` only when it cannot wait"}
                    },
                    "required": ["to", "text"]
                }),
                Toolset::Bot,
            ),
            Self::PostToGroup => ToolSpec::new(
                "post_to_group",
                "Speak in a group you belong to, named by its title or id. Teammates you \
                 @mention in the text are woken; everybody else reads it on their next turn.",
                json!({
                    "type": "object",
                    "properties": {
                        "group": {"type": "string", "description": "title or id of a group you are a member of"},
                        "text": {"type": "string", "description": "the message; @mention members to wake them"}
                    },
                    "required": ["group", "text"]
                }),
                Toolset::Bot,
            ),
            Self::HandoffTask => ToolSpec::new(
                "handoff_task",
                "Give a teammate a piece of work as a task they own, with everything they need \
                 in `task` and `context` — they do not see this conversation. When they finish, \
                 their result comes back here; follow progress with check_task.",
                json!({
                    "type": "object",
                    "properties": {
                        "to": {"type": "string", "description": "the teammate that should do the work"},
                        "title": {"type": "string", "description": "short name of the task"},
                        "task": {"type": "string", "description": "what to do, self-contained"},
                        "context": {"type": "string", "description": "optional extras: paths, constraints, what done means"}
                    },
                    "required": ["to", "title", "task"]
                }),
                Toolset::Bot,
            ),
            Self::React => ToolSpec::new(
                "react",
                "Acknowledge the last message with an emoji reaction. It is recorded in the \
                 transcript and wakes nobody — use it when \"seen\" is all you have to say.",
                json!({
                    "type": "object",
                    "properties": {
                        "emoji": {"type": "string", "description": "one emoji, e.g. 👍 or ✅"}
                    },
                    "required": ["emoji"]
                }),
                Toolset::Bot,
            ),
            Self::ListBots => ToolSpec::new(
                "list_bots",
                "List the team: every bot with its one-line purpose and its current status \
                 (idle, working, queued, disabled). Read-only.",
                json!({"type": "object", "properties": {}, "required": []}),
                Toolset::Bot,
            )
            .read_only(),
            Self::ListNodes => ToolSpec::new(
                "list_nodes",
                "List the machines (nodes) that can run tools: label, local or remote, online, \
                 capabilities and directory roots. Read-only; an offline node is reported as \
                 offline, so do not plan work that needs it.",
                json!({"type": "object", "properties": {}, "required": []}),
                Toolset::Bot,
            )
            .read_only(),
            Self::AskUser => ToolSpec::new(
                "ask_user",
                "Ask the owner a question and return immediately — this never blocks. The \
                 answer arrives later as a message that wakes you, so carry on with other \
                 work or end your turn after asking; offer `options` when a choice is enough.",
                json!({
                    "type": "object",
                    "properties": {
                        "question": {"type": "string", "description": "the question, in words the owner understands without your context"},
                        "options": {"type": "array", "items": {"type": "string"}, "description": "optional choices; the owner may still answer freely"}
                    },
                    "required": ["question"]
                }),
                Toolset::Bot,
            ),
            Self::RequestApproval => ToolSpec::new(
                "request_approval",
                "Ask the owner to approve an action you want to take, and return immediately. \
                 Nothing runs by itself: the decision arrives later as a message, and you may \
                 only proceed with the action once that message says it was allowed.",
                json!({
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "description": "what you want to do, e.g. `delete build/ and rebuild`"},
                        "reason": {"type": "string", "description": "why it is needed and why it is safe"}
                    },
                    "required": ["action"]
                }),
                Toolset::Bot,
            ),
            Self::StartTask => ToolSpec::new(
                "start_task",
                "Start a piece of your own background work and keep chatting while it runs. \
                 Returns the task id at once; the result comes back to this conversation when \
                 the task finishes.",
                json!({
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "short name of the work"},
                        "brief": {"type": "string", "description": "what to do and what done means; the task only sees this"}
                    },
                    "required": ["title", "brief"]
                }),
                Toolset::Task,
            ),
            Self::ListTasks => ToolSpec::new(
                "list_tasks",
                "List the tasks you own with their status and title. Read-only.",
                json!({"type": "object", "properties": {}, "required": []}),
                Toolset::Task,
            )
            .read_only(),
            Self::CheckTask => ToolSpec::new(
                "check_task",
                "Check one task: its status, its result so far and its last steps. Read-only.",
                json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "description": "a task id like `tsk_…`, from start_task, handoff_task or list_tasks"}
                    },
                    "required": ["task_id"]
                }),
                Toolset::Task,
            )
            .read_only(),
            Self::UpdateTask => ToolSpec::new(
                "update_task",
                "Add information or a correction to a task that has not finished. It is steered \
                 into the running turn when possible, otherwise recorded so the next turn \
                 reads it first.",
                json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "description": "a task id like `tsk_…`"},
                        "text": {"type": "string", "description": "what to add or change"}
                    },
                    "required": ["task_id", "text"]
                }),
                Toolset::Task,
            ),
            Self::CancelTask => ToolSpec::new(
                "cancel_task",
                "Cancel a task you own. The turn running it is stopped and no result is \
                 delivered; use it when the work is no longer needed.",
                json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "description": "a task id like `tsk_…`"}
                    },
                    "required": ["task_id"]
                }),
                Toolset::Task,
            ),
            Self::Remember => ToolSpec::new(
                "remember",
                "Write one durable fact about your own work into your long-term memory \
                 (`MEMORY.md` in your workspace), which is shown to you at the start of every \
                 turn. Use it for things you will need again: a convention, a path, a decision \
                 and its reason. It takes effect from your NEXT turn — the memory you were given \
                 this turn does not change under you. Do not use it as a scratchpad; keep one \
                 fact per call, and it must fit on one line.",
                json!({
                    "type": "object",
                    "properties": {
                        "fact": {
                            "type": "string",
                            "description": "one durable fact, one line, no newlines"
                        }
                    },
                    "required": ["fact"]
                }),
                Toolset::Bot,
            ),
        }
    }
}

/// One registered bot-facing tool: a kind plus the shared environment.
struct BotTool {
    tool: BotToolKind,
    env: BotToolEnv,
}

#[async_trait::async_trait]
impl ToolHandler for BotTool {
    fn spec(&self) -> ToolSpec {
        self.tool.spec()
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        // Every arm is synchronous underneath: the services are plain SQLite
        // calls, and the async signature is only what the registry dispatches
        // through. Nothing here may await a human — see the module docs.
        match self.tool {
            BotToolKind::MessageBot => self.env.message_bot(ctx, args),
            BotToolKind::PostToGroup => self.env.post_to_group(ctx, args),
            BotToolKind::HandoffTask => self.env.handoff_task(ctx, args),
            BotToolKind::React => self.env.react(ctx, args),
            BotToolKind::ListBots => self.env.list_bots(ctx, args),
            BotToolKind::ListNodes => self.env.list_nodes(ctx, args),
            BotToolKind::AskUser => self.env.ask_user(ctx, args),
            BotToolKind::RequestApproval => self.env.request_approval(ctx, args),
            BotToolKind::StartTask => self.env.start_task(ctx, args),
            BotToolKind::ListTasks => self.env.list_tasks(ctx, args),
            BotToolKind::CheckTask => self.env.check_task(ctx, args),
            BotToolKind::UpdateTask => self.env.update_task(ctx, args),
            BotToolKind::CancelTask => self.env.cancel_task(ctx, args),
            BotToolKind::Remember => self.env.remember(ctx, args),
        }
    }
}

/// Register every bot-facing tool. Called once by the brain when it builds the
/// registry; each handler holds a clone of `env`, which is cheap (all `Arc`s).
pub fn register_bot_tools(registry: &mut ToolRegistry, env: BotToolEnv) {
    for tool in BotToolKind::ALL {
        registry.register(Arc::new(BotTool {
            tool,
            env: env.clone(),
        }));
    }
}

// --- argument shapes ---------------------------------------------------------
//
// The registry schema-validates before dispatch, but a handler may also be
// called directly (tests, the local executor), so parsing fails loudly with
// the missing field named rather than defaulting or panicking. Unknown keys
// are ignored on purpose: the schema is the contract, not the struct.

#[derive(Deserialize)]
struct MessageBotArgs {
    to: String,
    text: String,
    #[serde(default)]
    urgency: Urgency,
}

#[derive(Deserialize)]
struct PostToGroupArgs {
    group: String,
    text: String,
}

#[derive(Deserialize)]
struct HandoffTaskArgs {
    to: String,
    title: String,
    task: String,
    #[serde(default)]
    context: Option<String>,
}

#[derive(Deserialize)]
struct ReactArgs {
    emoji: String,
}

#[derive(Deserialize)]
struct AskUserArgs {
    question: String,
    #[serde(default)]
    options: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RequestApprovalArgs {
    action: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
struct StartTaskArgs {
    title: String,
    brief: String,
}

#[derive(Deserialize)]
struct RememberArgs {
    fact: String,
}

#[derive(Deserialize)]
struct TaskIdArgs {
    task_id: String,
}

#[derive(Deserialize)]
struct UpdateTaskArgs {
    task_id: String,
    text: String,
}

// --- the tools ----------------------------------------------------------------

impl BotToolEnv {
    /// Resolve the calling bot, its conversation and the hop count of the
    /// chain that woke it. Every sending tool starts here; a missing bot or
    /// conversation is a `Failed` error that tells the model the world moved
    /// on without it (the owner deleted the bot or the group mid-turn).
    fn send_context(&self, ctx: &ToolContext) -> Result<SendContext, ToolError> {
        let bot = self.calling_bot(ctx)?;
        let conversation = self
            .store
            .get_conversation(&ctx.invocation.conversation_id)
            .map_err(tool_error)?
            .ok_or_else(|| {
                ToolError::failed(format!(
                    "the conversation ({}) this call belongs to no longer exists, so there is \
                     nowhere to send from",
                    ctx.invocation.conversation_id
                ))
            })?;
        let hops = hops_of(self.store.as_ref(), ctx.invocation.turn_id.as_ref());
        Ok(SendContext {
            from: bot,
            conversation,
            hops,
            source_entry: None,
        })
    }

    /// The bot behind the call. Read fresh from the store every time: a tool
    /// call must never act on a bot that was deleted since the turn started.
    fn calling_bot(&self, ctx: &ToolContext) -> Result<Bot, ToolError> {
        self.store
            .get_bot(&ctx.invocation.bot_id)
            .map_err(tool_error)?
            .ok_or_else(|| {
                ToolError::failed(format!(
                    "the bot behind this call ({}) no longer exists — the owner deleted it \
                     mid-turn, so it cannot act",
                    ctx.invocation.bot_id
                ))
            })
    }

    fn message_bot(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let args: MessageBotArgs = parse_args("message_bot", args)?;
        let send = self.send_context(ctx)?;
        let sent = self
            .messaging
            .message_bot(&send, &args.to, &args.text, args.urgency)
            .map_err(tool_error)?;
        if sent.stopped {
            if let (Some(turn_id), Ok(recipient)) = (
                &ctx.invocation.turn_id,
                self.messaging.resolve_bot(&args.to),
            ) {
                self.messaging.record_turn_target(turn_id, &recipient.id);
            }
            // Report, don't fail: the chain ending is a fact the model has to
            // act on (answer here instead), not a broken call.
            return Ok(ToolResult::text(format!(
                "Stopped: this chain of bot-to-bot messages has reached the hop limit, so \
                 nothing was sent to `{}`. The chain ends with you — answer in this \
                 conversation instead.",
                args.to
            )));
        }
        // Safe to resolve now: the send already resolved the same name.
        let recipient = self.messaging.resolve_bot(&args.to).map_err(tool_error)?;
        if let Some(turn_id) = &ctx.invocation.turn_id {
            self.messaging.record_turn_target(turn_id, &recipient.id);
        }
        let place = if sent.conversation == send.conversation.id {
            "in this group — a message to a fellow member stays where the owner can see it"
        } else {
            "in their DM"
        };
        let busy = self.runners.active_turn(&recipient.id).is_some();
        let reach = match (sent.woke, args.urgency == Urgency::Urgent, busy) {
            (false, _, _) => {
                " They were NOT woken (a disabled bot stays asleep); the message is kept for \
                 the owner to see."
            }
            (true, _, true) => {
                " They are mid-turn right now; your message waits for that turn to end."
            }
            (true, true, false) => " They were woken; your message is flagged urgent.",
            (true, false, false) => " They were woken and will read it on their next turn.",
        };
        Ok(ToolResult::text(format!(
            "Messaged @{} {place}.{reach}",
            recipient.name
        )))
    }

    fn post_to_group(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let args: PostToGroupArgs = parse_args("post_to_group", args)?;
        let send = self.send_context(ctx)?;
        let sent = self
            .messaging
            .post_to_group_recorded(
                &send,
                &args.group,
                &args.text,
                ctx.invocation.turn_id.as_ref(),
            )
            .map_err(tool_error)?;
        if sent.stopped {
            return Ok(ToolResult::text(
                "Stopped: this chain has reached the hop limit, so your post was not sent. \
                 Answer in this conversation instead.",
            ));
        }
        let title = self
            .store
            .get_conversation(&sent.conversation)
            .map_err(tool_error)?
            .and_then(|conversation| conversation.title)
            .unwrap_or_else(|| sent.conversation.to_string());
        let wake_note = if sent.woke {
            "Teammates you @mentioned were woken and will read it on their next turn."
        } else {
            "No teammates were woken (none @mentioned or active)."
        };
        Ok(ToolResult::text(format!(
            "Posted in \"{title}\". {wake_note}"
        )))
    }

    fn handoff_task(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let args: HandoffTaskArgs = parse_args("handoff_task", args)?;
        let send = self.send_context(ctx)?;
        let sent = self
            .messaging
            .handoff_task(
                &send,
                &args.to,
                &args.title,
                &args.task,
                args.context.as_deref(),
            )
            .map_err(tool_error)?;
        if sent.stopped {
            return Ok(ToolResult::text(format!(
                "Stopped: this chain has reached the hop limit, so the handoff to `{}` was \
                 not made. Do the work yourself (start_task) or answer here.",
                args.to
            )));
        }
        // A successful handoff always carries its task; the `else` branch is
        // a contract violation, reported honestly rather than unwrapped.
        let Some(task) = sent.task else {
            return Ok(ToolResult::error(
                "the handoff produced no task; tell the owner, nothing was sent",
            ));
        };
        let recipient = self.messaging.resolve_bot(&args.to).map_err(tool_error)?;
        if let Some(turn_id) = &ctx.invocation.turn_id {
            self.messaging.record_turn_target(turn_id, &recipient.id);
        }
        Ok(ToolResult::text(format!(
            "Handed \"{}\" to @{} as task {}. It is theirs to run; when they finish, their \
             result comes back to this conversation. Follow progress with check_task.",
            one_line(&task.title, 80),
            recipient.name,
            task.id
        )))
    }

    fn react(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let args: ReactArgs = parse_args("react", args)?;
        let send = self.send_context(ctx)?;
        self.messaging
            .react(&send, &args.emoji)
            .map_err(tool_error)?;
        Ok(ToolResult::text(format!(
            "Your {} reaction was recorded in the transcript. Nobody was woken — if you have \
             something to say, use message_bot.",
            one_line(&args.emoji, 16)
        )))
    }

    fn list_bots(&self, ctx: &ToolContext, _args: Value) -> Result<ToolResult, ToolError> {
        let mut result = self.messaging.list_bots().map_err(tool_error)?;
        // Mark the caller in the list: a model that cannot tell itself from
        // its teammates messages itself (and gets refused). A vanished caller
        // only loses the annotation — a pure read should not fail over it.
        if let Ok(Some(me)) = self.store.get_bot(&ctx.invocation.bot_id)
            && let Some(Content::Text { text }) = result.content.first_mut()
        {
            *text = text.replacen(
                &format!("- @{} (", me.name),
                &format!("- @{} (you) (", me.name),
                1,
            );
        }
        Ok(result)
    }

    fn list_nodes(&self, _ctx: &ToolContext, _args: Value) -> Result<ToolResult, ToolError> {
        let nodes = self.store.list_nodes().map_err(tool_error)?;
        if nodes.is_empty() {
            return Ok(ToolResult::text(
                "No nodes are paired. Your tools run on the brain's built-in local node.",
            ));
        }
        let mut lines = Vec::new();
        for node in &nodes {
            let kind = match node.kind {
                NodeKind::Local => "local",
                NodeKind::Remote => "remote",
            };
            // Honest about a node that cannot work: a model that plans a
            // build on an offline machine wastes a whole turn finding out.
            let presence = if node.revoked {
                "REVOKED — it may not reconnect"
            } else if node.online {
                "online"
            } else {
                "OFFLINE — it cannot run anything until it reconnects"
            };
            let caps = node.capabilities();
            let caps = if caps.as_slice().is_empty() {
                "none approved".to_string()
            } else {
                caps.as_slice().join(", ")
            };
            let roots = if node.roots.is_empty() {
                "no roots".to_string()
            } else {
                node.roots
                    .iter()
                    .map(|root| {
                        let mode = match root.mode {
                            RootMode::ReadOnly => "read-only",
                            RootMode::ReadWrite => "read-write",
                        };
                        format!("{}: {} ({mode})", root.id, one_line(&root.path, 80))
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            lines.push(format!(
                "- {} ({}) [{kind} node, {presence}] — capabilities: {caps}; roots: {roots}",
                node.label, node.id
            ));
        }
        Ok(ToolResult::text(lines.join("\n")))
    }

    fn ask_user(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let args: AskUserArgs = parse_args("ask_user", args)?;
        let send = self.send_context(ctx)?;
        // One field: a choice when the model offered options, free text
        // otherwise. The card renders the question itself as its title.
        let fields = match args.options {
            Some(options) if !options.is_empty() => vec![FormField {
                name: "answer".into(),
                label: "Answer".into(),
                kind: FieldKind::Choice,
                choices: options,
                required: true,
                default: None,
            }],
            _ => vec![FormField::text("answer", "Answer").required()],
        };
        let mut request = async_request(&send, ctx, RequestKind::Question);
        request.tool = Some("ask_user".into());
        request.prompt = Some(args.question.clone());
        request.fields = fields;
        let id = self
            .approval
            .raise_async(&mut request)
            .map_err(tool_error)?;
        Ok(ToolResult::text(pending_text(
            &id,
            "the owner is being asked your question. This call did not wait: the answer \
             arrives later as a message that wakes you. Carry on with other work, or end \
             your turn; do not ask twice.",
        )))
    }

    fn request_approval(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let args: RequestApprovalArgs = parse_args("request_approval", args)?;
        let send = self.send_context(ctx)?;
        let mut request = async_request(&send, ctx, RequestKind::Approval);
        request.tool = Some("request_approval".into());
        request.target = Some(args.action.clone());
        request.reason = args.reason.clone();
        let id = self
            .approval
            .raise_async(&mut request)
            .map_err(tool_error)?;
        Ok(ToolResult::text(pending_text(
            &id,
            &format!(
                "the owner is being asked to approve `{}`. This call did not wait and did \
                 NOT run anything: the decision arrives later as a message that wakes you. \
                 Only proceed with the action once that message says it was allowed.",
                one_line(&args.action, 120)
            ),
        )))
    }

    fn start_task(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let args: StartTaskArgs = parse_args("start_task", args)?;
        let send = self.send_context(ctx)?;
        let task = self
            .tasks
            .start(
                &send.from,
                send.conversation.id.clone(),
                &args.title,
                &args.brief,
            )
            .map_err(tool_error)?;
        // `start` returns the queued row, but the pump inside it may already
        // have handed the task over — read back what is true now. A read-back
        // failure must not fail the call: the task exists either way.
        let status = self
            .tasks
            .get(&task.id)
            .ok()
            .flatten()
            .map_or(task.status, |current| current.status);
        Ok(ToolResult::text(format!(
            "Started \"{}\" as your background task {} ({}). It runs while you stay free \
             here; when it finishes, its result comes back to this conversation.",
            one_line(&task.title, 80),
            task.id,
            status.label()
        )))
    }

    fn list_tasks(&self, ctx: &ToolContext, _args: Value) -> Result<ToolResult, ToolError> {
        let bot = self.calling_bot(ctx)?;
        let tasks = self.tasks.list(&bot.id).map_err(tool_error)?;
        if tasks.is_empty() {
            return Ok(ToolResult::text(
                "You have no tasks. Use start_task for your own background work, or \
                 handoff_task to give work to a teammate.",
            ));
        }
        // Newest last, oldest elided first: a hoarder bot must not blow the
        // result budget, and the recent tasks are the actionable ones.
        let shown = tasks.len().min(MAX_LISTED_TASKS);
        let mut lines: Vec<String> = tasks
            .iter()
            .skip(tasks.len() - shown)
            .map(|task| {
                format!(
                    "- {} [{}] \"{}\"",
                    task.id,
                    task.status.label(),
                    one_line(&task.title, 80)
                )
            })
            .collect();
        if shown < tasks.len() {
            lines.push(format!("…and {} earlier task(s).", tasks.len() - shown));
        }
        Ok(ToolResult::text(lines.join("\n")))
    }

    fn check_task(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let args: TaskIdArgs = parse_args("check_task", args)?;
        let task_id = parse_task_id("check_task", &args.task_id)?;
        let bot = self.calling_bot(ctx)?;
        // Any task on the team may be read (a requester follows its handoff,
        // and the delegation graph is not a secret); only update and cancel
        // are guarded by ownership.
        let Some(task) = self.tasks.get(&task_id).map_err(tool_error)? else {
            return Err(ToolError::failed(format!(
                "there is no task {task_id}; use list_tasks to see the ones you have"
            )));
        };
        let mut lines = vec![check_head(&task, &bot, self)?];
        lines.push(match (&task.result, task.status) {
            (Some(result), _) => format!("Result: {}", one_line(result, 600)),
            (None, status) if status.is_terminal() => {
                format!("It ended ({}) without a written result.", status.label())
            }
            (None, _) => "No result yet.".to_string(),
        });
        if let Some(context) = &task.context {
            lines.push(format!(
                "Context from the sender: {}",
                one_line(context, 300)
            ));
        }
        let mut steps = self.tasks.steps(&task_id).map_err(tool_error)?;
        let total = steps.len();
        if total > MAX_TASK_STEPS {
            steps = steps.split_off(total - MAX_TASK_STEPS);
        }
        if steps.is_empty() {
            lines.push("No steps recorded yet.".to_string());
        } else {
            lines.push(format!("Last {} of {total} step(s):", steps.len()));
            for step in &steps {
                lines.push(format!(
                    "{}. [{}] {}",
                    step.seq,
                    step_label(step.kind),
                    one_line(&step.text, 200)
                ));
            }
        }
        Ok(ToolResult::text(lines.join("\n")))
    }

    fn update_task(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let args: UpdateTaskArgs = parse_args("update_task", args)?;
        let task_id = parse_task_id("update_task", &args.task_id)?;
        let bot = self.calling_bot(ctx)?;
        let Some(task) = self.tasks.get(&task_id).map_err(tool_error)? else {
            return Err(ToolError::failed(format!(
                "there is no task {task_id}; use list_tasks to see the ones you have"
            )));
        };
        // The bot that owns the task may steer it, and the bot that handed it
        // over may add to its own handoff. Everybody else messages the owner
        // instead — a task is not a public bulletin board.
        if task.owner != bot.id && task.requester.as_ref() != Some(&bot.id) {
            return Err(ToolError::failed(format!(
                "that task belongs to {} and is not yours to change; send them a message \
                 with what you want added",
                task_owner_name(self, &task)?
            )));
        }
        let steered = self
            .tasks
            .update(&task_id, &args.text)
            .map_err(tool_error)?;
        let title = one_line(&task.title, 80);
        let text = if steered {
            format!(
                "Added to \"{title}\" ({task_id}): it was steered straight into the turn working on the task."
            )
        } else if task.status == TaskStatus::Running {
            format!(
                "Recorded on \"{title}\" ({task_id}), but the running turn could not take it \
                 right now; the note is on the task record."
            )
        } else {
            format!(
                "Recorded on \"{title}\" ({task_id}). The task is {} — it reads your note \
                 when it next runs.",
                task.status.label()
            )
        };
        Ok(ToolResult::text(text))
    }

    fn cancel_task(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let args: TaskIdArgs = parse_args("cancel_task", args)?;
        let task_id = parse_task_id("cancel_task", &args.task_id)?;
        let bot = self.calling_bot(ctx)?;
        let Some(task) = self.tasks.get(&task_id).map_err(tool_error)? else {
            return Err(ToolError::failed(format!(
                "there is no task {task_id}; use list_tasks to see the ones you have"
            )));
        };
        // Only the bot that owns a task may cancel it: cancelling interrupts
        // the turn of the owner bot, and a stranger pulling that lever would
        // stop work it does not understand. A requester who wants a handoff
        // stopped messages its owner.
        if task.owner != bot.id {
            return Err(ToolError::failed(format!(
                "that task belongs to {}; only the bot that owns a task can cancel it — \
                 send them a message instead",
                task_owner_name(self, &task)?
            )));
        }
        // `by_owner = false`: no event line and no result delivery, because
        // the bot is not told about a cancellation it just made itself — a
        // notification would only wake it with news it already has.
        let cancelled = self.tasks.cancel(&task_id, false).map_err(tool_error)?;
        let title = one_line(&cancelled.title, 80);
        let text = if cancelled.status == TaskStatus::Cancelled {
            format!("Cancelled \"{title}\" ({task_id}). No result will be delivered.")
        } else {
            format!(
                "\"{title}\" ({task_id}) was already {} — nothing to cancel.",
                cancelled.status.label()
            )
        };
        Ok(ToolResult::text(text))
    }

    /// Append one durable fact to the bot's own `MEMORY.md`.
    ///
    /// The file is the bot's long-term memory: `runners::read_bot_memory` reads
    /// it at the start of every turn and `instructions::memory_section` injects
    /// it under "What you remember", capped at
    /// [`crate::session::MEMORY_INDEX_LIMIT_BYTES`]. Before this tool nothing
    /// ever wrote it, so "memory" was a file that could only ever be empty.
    ///
    /// Three properties are deliberate:
    ///
    /// * **one line per fact, deduplicated** — a memory is a set of things that
    ///   are true, not an append-only log, so writing the same fact twice does
    ///   not make it twice as true;
    /// * **the file stays inside the prompt budget** — writing past the cap
    ///   would be silently truncated out of the prompt, which is the kind of
    ///   quiet loss this repository refuses. When the budget is full the oldest
    ///   line is dropped and the result says so;
    /// * **the change is visible next turn, not this one** — the prompt for the
    ///   turn in flight was already rendered, and the result says that plainly
    ///   rather than implying the bot can read back what it just wrote.
    ///
    /// The write is a read-modify-write, which is safe because a bot has
    /// exactly one runner and therefore one turn at a time (D3.1). A bot with
    /// `shell` could still race this from a command it runs itself; that is the
    /// same class of race as any other file in its workspace, and the
    /// deduplication makes the common case idempotent.
    fn remember(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let args: RememberArgs = parse_args("remember", args)?;
        let fact = args.fact.trim();
        if fact.is_empty() {
            return Err(ToolError::invalid(
                "remember: `fact` is empty; write the thing you want to keep",
            ));
        }
        let fact = fact.replace(['\n', '\r'], " ");
        let fact = fact.trim().to_string();
        if fact.len() > MAX_MEMORY_FACT_BYTES {
            return Err(ToolError::invalid(format!(
                "remember: the fact is {} bytes; keep one fact under {MAX_MEMORY_FACT_BYTES} bytes \
                 (split it if it is really several facts)",
                fact.len()
            )));
        }

        let path = ctx.workspace.join("MEMORY.md");
        // The workspace is the bot's own, but a bot with `shell` could have
        // replaced `MEMORY.md` with a symlink to something outside it. Writing
        // through that would let a model append to a file the path policy would
        // have refused, so the write stops here instead.
        if let Ok(metadata) = std::fs::symlink_metadata(&path)
            && metadata.file_type().is_symlink()
        {
            return Err(ToolError::failed(
                "remember: MEMORY.md in your workspace is a symlink; refusing to write through it",
            ));
        }
        let existing = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => {
                return Err(ToolError::failed(format!(
                    "remember: cannot read {}: {err}",
                    path.display()
                )));
            }
        };

        let mut lines: Vec<String> = existing
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();

        // Compare on the text, not the whole line: "- x" and "x" are the same
        // fact, and a bot may write either shape.
        let bare = |line: &str| -> String {
            line.trim_start_matches(['-', '*', ' '])
                .trim()
                .to_lowercase()
        };
        if lines.iter().any(|line| bare(line) == bare(&fact)) {
            return Ok(ToolResult::text(format!(
                "Already remembered: {fact}\n(It is in your memory from your next turn on.)"
            )));
        }

        lines.push(format!("- {fact}"));
        // Drop the oldest lines until the whole file fits the budget the prompt
        // renderer applies. The newest fact is last, so it is never the one
        // dropped.
        let mut dropped = 0usize;
        while lines.len() > 1 && lines.join("\n").len() > MEMORY_INDEX_LIMIT_BYTES {
            lines.remove(0);
            dropped += 1;
        }
        let body = format!("{}\n", lines.join("\n"));
        if body.len() > MEMORY_INDEX_LIMIT_BYTES {
            // One fact larger than the whole budget: refuse rather than write a
            // file the prompt would silently cut.
            return Err(ToolError::invalid(format!(
                "remember: this fact alone is larger than the {MEMORY_INDEX_LIMIT_BYTES}-byte \
                 memory budget; shorten it"
            )));
        }
        std::fs::write(&path, body).map_err(|err| {
            ToolError::failed(format!("remember: cannot write {}: {err}", path.display()))
        })?;

        let mut text = format!(
            "Remembered: {fact}\nIt is in your memory from your next turn on — the prompt for \
             this turn was already built."
        );
        if dropped > 0 {
            text.push_str(&format!(
                "\nDropped the {dropped} oldest line(s) to stay inside the \
                 {MEMORY_INDEX_LIMIT_BYTES}-byte memory budget."
            ));
        }
        Ok(ToolResult::text(text))
    }
}

// --- shared plumbing -----------------------------------------------------------

/// How many bot-to-bot hops the chain behind this turn has already made.
///
/// This number is what stops two bots from waking each other forever: every
/// bot-to-bot send inherits the chain of the delivery that woke the sender,
/// adds one hop, and [`Messaging`] refuses to extend a chain that reached
/// `max_hops`. A turn without a delivery — the owner typed it, a routine
/// fired, a restart resumed it — starts a fresh chain, so it counts zero. A
/// turn whose delivery row is gone also counts zero: a bookkeeping gap must
/// not punish the bot with a refusal it cannot do anything about, and the
/// hop-limit event line still fires one hop later if the chain really is
/// long.
fn hops_of(store: &dyn Persistence, turn_id: Option<&TurnId>) -> u8 {
    let Some(turn_id) = turn_id else {
        return 0;
    };
    let Ok(Some(turn)) = store.get_turn(turn_id) else {
        return 0;
    };
    let Some(delivery_id) = &turn.delivery_id else {
        return 0;
    };
    match store.get_delivery(delivery_id) {
        Ok(Some(delivery)) => delivery.causal.hops,
        _ => 0,
    }
}

/// The one way a service failure reaches a model. `DomainError`'s `Display`
/// already reads as plain language ("invalid input: there is no bot called
/// `nobody`; the team is @lead, @scout"), so it is carried over verbatim as
/// a tool failure — the `ERROR: ` prefix is added by the result layer.
fn tool_error(err: DomainError) -> ToolError {
    ToolError::Failed {
        message: err.to_string(),
    }
}

/// Parse a tool call's arguments. A model that calls a tool with no arguments
/// at all may send `null` instead of `{}`; both mean "empty object", so the
/// error names the missing field rather than the JSON shape.
fn parse_args<T: DeserializeOwned>(tool: &str, args: Value) -> Result<T, ToolError> {
    let args = if args.is_null() {
        Value::Object(Map::new())
    } else {
        args
    };
    serde_json::from_value(args).map_err(|err| ToolError::InvalidArgs {
        message: format!("{tool}: {err}"),
    })
}

/// Parse and validate a task id argument, with a message that says what the
/// model should do instead of guessing ids.
fn parse_task_id(tool: &str, raw: &str) -> Result<TaskId, ToolError> {
    TaskId::parse(raw.trim()).map_err(|_| {
        ToolError::invalid(format!(
            "{tool}: `{raw}` is not a task id; task ids look like `tsk_…` (use list_tasks to \
             see yours)"
        ))
    })
}

/// An async request card: the shape `ask_user` and `request_approval` share.
///
/// Built here rather than through `RuntimeRequest::approval`, because that
/// constructor always sets a `Surface` and these asks have none — a bot
/// asking about *its own plan* names no tool surface for rules to match. The
/// expiry mirrors the constructors': an unattended card must never wait
/// forever.
fn async_request(send: &SendContext, ctx: &ToolContext, kind: RequestKind) -> RuntimeRequest {
    let created_at = Utc::now();
    RuntimeRequest {
        id: RequestId::new(),
        bot_id: send.from.id.clone(),
        conversation_id: send.conversation.id.clone(),
        turn_id: ctx.invocation.turn_id.clone(),
        task_id: ctx.invocation.task_id.clone(),
        kind,
        surface: None,
        tool: None,
        target: None,
        reason: None,
        prompt: None,
        fields: Vec::new(),
        status: RequestStatus::Pending,
        decision: None,
        answer: None,
        responded_by: None,
        card_entry_id: None,
        created_at,
        expires_at: Some(created_at + chrono::Duration::seconds(APPROVAL_EXPIRY_SECS as i64)),
        resolved_at: None,
    }
}

/// The result both async asks return: the machine-readable status line a
/// model (and the UI) can key on, plus an explicit statement that nothing is
/// being waited for. A model that thinks it should poll burns turns; this
/// tells it not to.
fn pending_text(id: &RequestId, next: &str) -> String {
    format!("{{\"status\":\"pending\",\"id\":\"{id}\"}} — {next}")
}

/// The first line of `check_task`: what the task is, whose it is, and who
/// asked for it — relative to the caller, so "yours" means something.
fn check_head(task: &Task, bot: &Bot, env: &BotToolEnv) -> Result<String, ToolError> {
    let mut head = format!(
        "\"{}\" ({}) — {}",
        one_line(&task.title, 80),
        task.id,
        task.status.label()
    );
    if task.owner != bot.id {
        head.push_str(&format!(", owned by {}", task_owner_name(env, task)?));
    }
    if let Some(requester) = &task.requester
        && *requester != bot.id
        && let Ok(Some(asker)) = env.store.get_bot(requester)
    {
        head.push_str(&format!(", asked by @{}", asker.name));
    }
    head.push('.');
    Ok(head)
}

/// Who owns this task, by name — for the refusals that have to say who the
/// model should talk to instead.
fn task_owner_name(env: &BotToolEnv, task: &Task) -> Result<String, ToolError> {
    Ok(env
        .store
        .get_bot(&task.owner)
        .map_err(tool_error)?
        .map_or_else(
            || "another bot".to_string(),
            |owner| format!("@{}", owner.name),
        ))
}

fn step_label(kind: StepKind) -> &'static str {
    match kind {
        StepKind::Message => "message",
        StepKind::Reasoning => "reasoning",
        StepKind::Tool => "tool",
        StepKind::Update => "update",
        StepKind::Result => "result",
        StepKind::Notice => "notice",
    }
}

/// One line, at most `max_chars` of it: list and step lines must stay well
/// under `TOOL_RESULT_LIMIT_BYTES` even when a store is full of long titles.
fn one_line(text: &str, max_chars: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= max_chars {
        line.to_string()
    } else {
        let cut: String = line.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    use tokio_util::sync::CancellationToken;

    use swarmbot_contracts::conversation::Conversation;
    use swarmbot_contracts::delivery::{CausalChain, Delivery, DeliveryKind};
    use swarmbot_contracts::id::{ConversationId, DeliveryId, NodeId};
    use swarmbot_contracts::request::{Decision, Responder};
    use swarmbot_contracts::task::TaskKind;
    use swarmbot_contracts::tool::Invocation;
    use swarmbot_contracts::turn::{Turn, TurnInitiator};
    use swarmbot_tools::{ApprovalAsk, ApprovalGate, NodeCall, NodeExecutor};

    use crate::settings::SettingsHandle;
    use crate::testing;

    use super::*;

    // --- test doubles ---------------------------------------------------------
    //
    // A `ToolContext` demands the two tools-crate ports. Neither is what these
    // tests exercise, so both are as thin as possible: approvals always allow,
    // nodes never exist.

    struct AllowAll;

    #[async_trait::async_trait]
    impl ApprovalGate for AllowAll {
        async fn request(
            &self,
            _ctx: &ToolContext,
            _ask: ApprovalAsk,
        ) -> Result<bool, DomainError> {
            Ok(true)
        }
    }

    struct NoNodes;

    #[async_trait::async_trait]
    impl NodeExecutor for NoNodes {
        async fn exec(
            &self,
            node: &NodeId,
            _invocation: &Invocation,
            _call: NodeCall,
        ) -> Result<ToolResult, ToolError> {
            Err(ToolError::failed(format!(
                "node {node} cannot run anything in this test"
            )))
        }
    }

    // --- rig ------------------------------------------------------------------

    struct Rig {
        store: Arc<dyn Persistence>,
        runners: Arc<RunnerMap>,
        env: BotToolEnv,
        lead: Bot,
        scout: Bot,
        group: Conversation,
    }

    impl Rig {
        fn new() -> Self {
            let fixture = testing::Fixture::memory();
            let lead = fixture.bot("lead");
            let scout = fixture.bot("scout");
            let group = fixture.group("Team", &["lead", "scout"]);
            let settings = SettingsHandle::new(fixture.store.clone());
            let runners = RunnerMap::new();
            // Tasks before messaging: a handoff is submitted to the task pump.
            let tasks = BackgroundTasks::new(
                fixture.store.clone(),
                fixture.hub.clone(),
                fixture.transcript.clone(),
                settings.clone(),
                runners.clone(),
            );
            let messaging = Messaging::new(
                fixture.store.clone(),
                fixture.transcript.clone(),
                settings.clone(),
                runners.clone(),
                tasks.clone(),
            );
            let approval = ApprovalService::new(
                fixture.store.clone(),
                fixture.hub.clone(),
                fixture.transcript.clone(),
                settings,
                runners.clone(),
            );
            let env = BotToolEnv {
                store: fixture.store.clone(),
                hub: fixture.hub.clone(),
                transcript: fixture.transcript.clone(),
                messaging,
                tasks,
                approval,
                runners: runners.clone(),
            };
            Self {
                store: fixture.store,
                runners,
                env,
                lead,
                scout,
                group,
            }
        }

        fn ctx(&self, bot: &Bot, conversation: &ConversationId) -> ToolContext {
            self.ctx_with_turn(bot, conversation, None)
        }

        fn ctx_with_turn(
            &self,
            bot: &Bot,
            conversation: &ConversationId,
            turn_id: Option<TurnId>,
        ) -> ToolContext {
            let mut invocation = Invocation::new(bot.id.clone(), conversation.clone());
            invocation.turn_id = turn_id;
            // Built field by field, exactly the way `runners.rs` builds it for
            // a real turn.
            ToolContext {
                invocation,
                workspace: PathBuf::from("/tmp/swarmbot-bot-tools-test"),
                node_id: NodeId::local(),
                approval: Arc::new(AllowAll),
                nodes: Arc::new(NoNodes),
                cancel: CancellationToken::new(),
                spool_dir: PathBuf::from("/tmp/swarmbot-bot-tools-test/.spool"),
            }
        }

        /// A turn whose delivery already made `hops` hops — the hop-limit
        /// situation without running a real chain.
        fn turn_with_hops(&self, bot: &Bot, conversation: &ConversationId, hops: u8) -> TurnId {
            let delivery = Delivery::bot_message(
                conversation.clone(),
                bot.id.clone(),
                bot.id.clone(),
                "an earlier hop",
                CausalChain {
                    root: DeliveryId::new(),
                    hops,
                },
                Urgency::Background,
            );
            self.store.insert_delivery(&delivery).unwrap();
            let turn = Turn::start(
                bot.id.clone(),
                conversation.clone(),
                TurnInitiator::Owner,
                Some(delivery.id.clone()),
                None,
            );
            self.store.insert_turn(&turn).unwrap();
            turn.id
        }

        /// Call one tool the way the registry would, flattening `Err` into the
        /// `ERROR:` result a model actually sees.
        async fn call(&self, tool: BotToolKind, ctx: &ToolContext, args: Value) -> ToolResult {
            let handler = BotTool {
                tool,
                env: self.env.clone(),
            };
            match handler.call(ctx, args).await {
                Ok(result) => result,
                Err(err) => err.to_result(),
            }
        }
    }

    // --- remember --------------------------------------------------------------

    /// A context whose workspace is a real temp directory, because `remember`
    /// writes `MEMORY.md` and a test must not touch `/tmp` by name.
    fn remember_ctx(rig: &Rig, workspace: &std::path::Path) -> ToolContext {
        let mut ctx = rig.ctx(&rig.lead, &rig.lead.direct_conversation_id);
        ctx.workspace = workspace.to_path_buf();
        ctx.spool_dir = workspace.join(".spool");
        ctx
    }

    #[tokio::test]
    async fn remember_appends_one_line_and_is_idempotent() {
        let rig = Rig::new();
        let dir = tempfile::tempdir().expect("temp dir");
        let ctx = remember_ctx(&rig, dir.path());

        let first = rig
            .call(
                BotToolKind::Remember,
                &ctx,
                json!({"fact": "the build is `cargo build -p swarmbot-core`"}),
            )
            .await;
        assert!(!first.is_error, "{}", first.text_only());
        assert!(
            first.text_only().contains("next turn"),
            "{}",
            first.text_only()
        );

        let memory = std::fs::read_to_string(dir.path().join("MEMORY.md")).expect("MEMORY.md");
        assert_eq!(
            memory.trim(),
            "- the build is `cargo build -p swarmbot-core`"
        );

        // The same fact twice does not become two lines: memory is a set of
        // things that are true, not a log.
        let again = rig
            .call(
                BotToolKind::Remember,
                &ctx,
                json!({"fact": "the build is `cargo build -p swarmbot-core`"}),
            )
            .await;
        assert!(!again.is_error);
        assert!(
            again.text_only().contains("Already remembered"),
            "{}",
            again.text_only()
        );
        let memory = std::fs::read_to_string(dir.path().join("MEMORY.md")).expect("MEMORY.md");
        assert_eq!(memory.lines().count(), 1, "{memory:?}");
    }

    #[tokio::test]
    async fn remember_keeps_one_fact_per_line() {
        let rig = Rig::new();
        let dir = tempfile::tempdir().expect("temp dir");
        let ctx = remember_ctx(&rig, dir.path());
        let result = rig
            .call(
                BotToolKind::Remember,
                &ctx,
                json!({"fact": "first thing\nsecond thing"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.text_only());
        let memory = std::fs::read_to_string(dir.path().join("MEMORY.md")).expect("MEMORY.md");
        assert_eq!(
            memory.lines().count(),
            1,
            "a newline would let one fact pose as two: {memory:?}"
        );
        assert!(memory.contains("first thing second thing"), "{memory:?}");
    }

    #[tokio::test]
    async fn remember_refuses_an_empty_fact_and_an_oversized_one() {
        let rig = Rig::new();
        let dir = tempfile::tempdir().expect("temp dir");
        let ctx = remember_ctx(&rig, dir.path());

        let empty = rig
            .call(BotToolKind::Remember, &ctx, json!({"fact": "   "}))
            .await;
        assert!(empty.is_error, "an empty fact is not a fact");

        let huge = "x".repeat(MAX_MEMORY_FACT_BYTES + 1);
        let oversized = rig
            .call(BotToolKind::Remember, &ctx, json!({"fact": huge}))
            .await;
        assert!(oversized.is_error, "{}", oversized.text_only());
        assert!(
            !dir.path().join("MEMORY.md").exists(),
            "a refused fact must not leave a file behind"
        );
    }

    #[tokio::test]
    async fn remember_drops_the_oldest_lines_to_stay_inside_the_prompt_budget() {
        // The renderer caps memory at MEMORY_INDEX_LIMIT_BYTES. Writing past
        // that would be silently truncated from the prompt — the quiet loss
        // this repository refuses — so the tool trims instead and says so.
        let rig = Rig::new();
        let dir = tempfile::tempdir().expect("temp dir");
        let ctx = remember_ctx(&rig, dir.path());

        let line = "y".repeat(400);
        let mut last = None;
        for i in 0..20 {
            last = Some(
                rig.call(
                    BotToolKind::Remember,
                    &ctx,
                    json!({"fact": format!("fact {i} {line}")}),
                )
                .await,
            );
        }
        let last = last.expect("a result");
        assert!(!last.is_error, "{}", last.text_only());
        assert!(
            last.text_only().contains("Dropped the"),
            "the result must say what it dropped: {}",
            last.text_only()
        );

        let memory = std::fs::read_to_string(dir.path().join("MEMORY.md")).expect("MEMORY.md");
        assert!(
            memory.len() <= MEMORY_INDEX_LIMIT_BYTES,
            "memory stays inside the prompt budget: {} bytes",
            memory.len()
        );
        assert!(
            memory.contains("fact 19"),
            "the newest fact survives the trim: {memory:?}"
        );
        assert!(
            !memory.contains("fact 0 "),
            "the oldest facts are the ones dropped"
        );
    }

    #[tokio::test]
    async fn remember_refuses_to_write_through_a_symlinked_memory_file() {
        // A bot with `shell` could replace MEMORY.md with a symlink to a file
        // the path policy would have refused. `remember` must not become the
        // way around that.
        let rig = Rig::new();
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("outside.txt");
        std::fs::write(&target, b"do not touch").expect("target");
        std::os::unix::fs::symlink(&target, dir.path().join("MEMORY.md")).expect("symlink");
        let ctx = remember_ctx(&rig, dir.path());

        let result = rig
            .call(BotToolKind::Remember, &ctx, json!({"fact": "leak"}))
            .await;
        assert!(result.is_error, "{}", result.text_only());
        assert_eq!(
            std::fs::read_to_string(&target).expect("target"),
            "do not touch",
            "the file behind the symlink is untouched"
        );
    }

    // --- message_bot -----------------------------------------------------------

    #[tokio::test]
    async fn message_bot_to_a_teammate_creates_a_delivery_and_names_them() {
        let rig = Rig::new();
        let ctx = rig.ctx(&rig.lead, &rig.lead.direct_conversation_id);
        let result = rig
            .call(
                BotToolKind::MessageBot,
                &ctx,
                json!({"to": "scout", "text": "look at src/", "urgency": "background"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.text_only());
        let text = result.text_only();
        assert!(text.contains("@scout"), "{text}");
        assert!(text.contains("woken"), "{text}");

        let deliveries = rig.store.queued_deliveries(&rig.scout.id).unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].kind, DeliveryKind::BotMessage);
        assert!(deliveries[0].text.contains("look at src/"));
        assert_eq!(deliveries[0].from_bot, Some(rig.lead.id.clone()));
        assert_eq!(
            deliveries[0].conversation_id,
            rig.scout.direct_conversation_id
        );
    }

    #[tokio::test]
    async fn message_bot_to_a_stranger_errors_with_the_team_list() {
        let rig = Rig::new();
        let ctx = rig.ctx(&rig.lead, &rig.lead.direct_conversation_id);
        let result = rig
            .call(
                BotToolKind::MessageBot,
                &ctx,
                json!({"to": "nobody", "text": "hi"}),
            )
            .await;
        assert!(result.is_error);
        let text = result.text_only();
        assert!(text.starts_with("ERROR: "), "{text}");
        assert!(text.contains("no bot called `nobody`"), "{text}");
        assert!(text.contains("@lead") && text.contains("@scout"), "{text}");
        assert!(
            rig.store
                .queued_deliveries(&rig.scout.id)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn message_bot_to_itself_is_refused() {
        let rig = Rig::new();
        let ctx = rig.ctx(&rig.lead, &rig.lead.direct_conversation_id);
        let result = rig
            .call(
                BotToolKind::MessageBot,
                &ctx,
                json!({"to": "lead", "text": "hi me"}),
            )
            .await;
        assert!(result.is_error);
        assert!(result.text_only().contains("that is you"), "{result:?}");
    }

    #[tokio::test]
    async fn message_bot_from_a_group_turn_posts_in_the_group() {
        let rig = Rig::new();
        let ctx = rig.ctx(&rig.lead, &rig.group.id);
        let result = rig
            .call(
                BotToolKind::MessageBot,
                &ctx,
                json!({"to": "scout", "text": "check the diff"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.text_only());
        assert!(result.text_only().contains("group"), "{result:?}");

        let entries = rig.store.entry_tail(&rig.group.id, 10).unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.kind.text() == Some("@scout check the diff")),
            "the post is in the group: {entries:?}"
        );
        // No DM side channel: the owner follows the whole exchange.
        assert!(
            rig.store
                .entry_tail(&rig.scout.direct_conversation_id, 10)
                .unwrap()
                .is_empty()
        );
        let deliveries = rig.store.queued_deliveries(&rig.scout.id).unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].conversation_id, rig.group.id);
    }

    #[tokio::test]
    async fn the_hop_limit_stops_a_message_and_writes_no_delivery() {
        let rig = Rig::new();
        let turn = rig.turn_with_hops(&rig.lead, &rig.lead.direct_conversation_id, 8);
        // The helper itself: the delivery's hop count, and zero without one.
        assert_eq!(hops_of(rig.store.as_ref(), Some(&turn)), 8);
        assert_eq!(hops_of(rig.store.as_ref(), None), 0);

        let ctx = rig.ctx_with_turn(&rig.lead, &rig.lead.direct_conversation_id, Some(turn));
        let result = rig
            .call(
                BotToolKind::MessageBot,
                &ctx,
                json!({"to": "scout", "text": "again"}),
            )
            .await;
        // Stopping is a fact to report, not a failed call.
        assert!(!result.is_error, "{}", result.text_only());
        assert!(result.text_only().contains("hop limit"), "{result:?}");
        assert!(
            rig.store
                .queued_deliveries(&rig.scout.id)
                .unwrap()
                .is_empty()
        );
        // The stop is recorded where the owner can see it.
        let entries = rig
            .store
            .entry_tail(&rig.lead.direct_conversation_id, 10)
            .unwrap();
        assert!(
            entries.iter().any(|e| e
                .kind
                .text()
                .unwrap_or_default()
                .contains("too many hand-offs")),
            "{entries:?}"
        );
    }

    // --- post_to_group, handoff_task, react -------------------------------------

    #[tokio::test]
    async fn post_to_group_posts_and_wakes_the_mentioned() {
        let rig = Rig::new();
        let ctx = rig.ctx(&rig.lead, &rig.lead.direct_conversation_id);
        let result = rig
            .call(
                BotToolKind::PostToGroup,
                &ctx,
                json!({"group": "Team", "text": "@scout status?"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.text_only());
        assert!(result.text_only().contains("Team"), "{result:?}");
        let entries = rig.store.entry_tail(&rig.group.id, 10).unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.kind.text() == Some("@scout status?"))
        );
        assert_eq!(rig.runners.parked_count(), 1, "the mention woke scout");
    }

    #[tokio::test]
    async fn handoff_task_creates_a_handoff_task_and_returns_its_id() {
        let rig = Rig::new();
        let ctx = rig.ctx(&rig.lead, &rig.group.id);
        let result = rig
            .call(
                BotToolKind::HandoffTask,
                &ctx,
                json!({
                    "to": "scout",
                    "title": "review the diff",
                    "task": "look for risky changes",
                    "context": "the diff is in /tmp/x.patch"
                }),
            )
            .await;
        assert!(!result.is_error, "{}", result.text_only());

        let tasks = rig.store.tasks_of(&rig.scout.id).unwrap();
        assert_eq!(tasks.len(), 1);
        let task = &tasks[0];
        assert_eq!(
            task.kind,
            TaskKind::Handoff {
                from: rig.lead.id.clone()
            }
        );
        assert_eq!(task.owner, rig.scout.id);
        assert_eq!(task.requester, Some(rig.lead.id.clone()));
        assert_eq!(task.origin, rig.group.id, "the result comes back here");
        assert_eq!(task.context.as_deref(), Some("the diff is in /tmp/x.patch"));

        let text = result.text_only();
        assert!(text.contains(task.id.as_str()), "{text}");
        assert!(text.contains("@scout"), "{text}");
    }

    #[tokio::test]
    async fn react_records_an_event_and_wakes_nobody() {
        let rig = Rig::new();
        let ctx = rig.ctx(&rig.lead, &rig.lead.direct_conversation_id);
        // There has to be somebody else's message to react to.
        rig.env
            .transcript
            .owner_message(&rig.lead.direct_conversation_id, "thanks, seen")
            .unwrap();
        let result = rig
            .call(BotToolKind::React, &ctx, json!({"emoji": "👍"}))
            .await;
        assert!(!result.is_error, "{}", result.text_only());
        assert!(
            result.text_only().contains("Nobody was woken"),
            "{result:?}"
        );

        let entries = rig
            .store
            .entry_tail(&rig.lead.direct_conversation_id, 10)
            .unwrap();
        let last = entries.last().unwrap();
        assert_eq!(last.kind.kind_str(), "event");
        assert!(last.kind.text().unwrap().contains("reacted 👍"));
        assert_eq!(rig.runners.parked_count(), 0);
    }

    // --- ask_user, request_approval ----------------------------------------------

    #[tokio::test]
    async fn ask_user_is_pending_at_once_and_the_answer_parks_a_delivery() {
        let rig = Rig::new();
        let ctx = rig.ctx(&rig.lead, &rig.lead.direct_conversation_id);
        let result = rig
            .call(
                BotToolKind::AskUser,
                &ctx,
                json!({"question": "which environment?", "options": ["staging", "prod"]}),
            )
            .await;
        assert!(!result.is_error, "{}", result.text_only());
        let text = result.text_only();
        assert!(text.contains("\"status\":\"pending\""), "{text}");

        let pending = rig.env.approval.pending().unwrap();
        assert_eq!(pending.len(), 1);
        let request = &pending[0];
        assert_eq!(request.kind, RequestKind::Question);
        assert_eq!(request.status, RequestStatus::Pending);
        assert_eq!(request.bot_id, rig.lead.id);
        assert!(text.contains(request.id.as_str()), "{text}");
        assert_eq!(request.fields.len(), 1);
        assert_eq!(request.fields[0].kind, FieldKind::Choice);
        assert_eq!(
            request.fields[0].choices,
            vec!["staging".to_string(), "prod".to_string()]
        );

        // The card exists in the conversation, so the owner can see the ask.
        let card_id = request.card_entry_id.clone().expect("a card entry");
        let entry = rig
            .store
            .get_entry(&card_id)
            .unwrap()
            .expect("the card is stored");
        assert_eq!(entry.kind.kind_str(), "card");

        // Answering parks an answer delivery for the bot (no runner exists
        // yet in this test, so it waits in the map — which is the point).
        let id = request.id.clone();
        rig.env
            .approval
            .respond(&id, None, Some(json!("staging")), Responder::Owner)
            .unwrap();
        assert_eq!(rig.runners.parked_count(), 1);
        let stored = rig.store.get_request(&id).unwrap().unwrap();
        assert_eq!(stored.status, RequestStatus::Answered);
        assert_eq!(stored.answer, Some(json!("staging")));
    }

    #[tokio::test]
    async fn ask_user_without_options_asks_for_free_text() {
        let rig = Rig::new();
        let ctx = rig.ctx(&rig.lead, &rig.lead.direct_conversation_id);
        let result = rig
            .call(
                BotToolKind::AskUser,
                &ctx,
                json!({"question": "what did you mean by 'later'?"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.text_only());
        let request = &rig.env.approval.pending().unwrap()[0];
        assert_eq!(request.fields.len(), 1);
        assert_eq!(request.fields[0].kind, FieldKind::Text);
        assert!(request.fields[0].choices.is_empty());
    }

    #[tokio::test]
    async fn request_approval_is_pending_at_once_and_runs_nothing() {
        let rig = Rig::new();
        let ctx = rig.ctx(&rig.lead, &rig.lead.direct_conversation_id);
        let result = rig
            .call(
                BotToolKind::RequestApproval,
                &ctx,
                json!({"action": "delete build/ and rebuild", "reason": "the cache is corrupt"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.text_only());
        let text = result.text_only();
        assert!(text.contains("\"status\":\"pending\""), "{text}");
        assert!(text.contains("did NOT run anything"), "{text}");

        let pending = rig.env.approval.pending().unwrap();
        assert_eq!(pending.len(), 1);
        let request = &pending[0];
        assert_eq!(request.kind, RequestKind::Approval);
        assert_eq!(
            request.surface, None,
            "no tool surface: the bot asked, not a tool"
        );
        assert_eq!(request.target.as_deref(), Some("delete build/ and rebuild"));
        assert_eq!(request.reason.as_deref(), Some("the cache is corrupt"));
        assert!(text.contains(request.id.as_str()), "{text}");
        assert!(
            request.card_entry_id.is_some(),
            "a card the owner can act on"
        );

        let id = request.id.clone();
        rig.env
            .approval
            .respond(&id, Some(Decision::AllowOnce), None, Responder::Owner)
            .unwrap();
        assert_eq!(rig.runners.parked_count(), 1, "the decision wakes the bot");
    }

    // --- tasks --------------------------------------------------------------------

    #[tokio::test]
    async fn a_task_lifecycle_through_the_tools() {
        let rig = Rig::new();
        let ctx = rig.ctx(&rig.lead, &rig.lead.direct_conversation_id);

        // start_task returns the id, and the pump hands the task over.
        let result = rig
            .call(
                BotToolKind::StartTask,
                &ctx,
                json!({"title": "audit the diff", "brief": "look for holes"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.text_only());
        let tasks = rig.store.tasks_of(&rig.lead.id).unwrap();
        assert_eq!(tasks.len(), 1);
        let task = &tasks[0];
        assert_eq!(task.status, TaskStatus::Running, "the pump handed it over");
        let text = result.text_only();
        assert!(text.contains(task.id.as_str()), "{text}");
        assert!(text.contains("running"), "{text}");
        assert_eq!(rig.runners.parked_count(), 1);

        // list_tasks shows it.
        let listed = rig.call(BotToolKind::ListTasks, &ctx, json!({})).await;
        let text = listed.text_only();
        assert!(text.contains(task.id.as_str()), "{text}");
        assert!(text.contains("[running]"), "{text}");
        assert!(text.contains("audit the diff"), "{text}");

        // check_task shows the status and the steps recorded so far.
        let checked = rig
            .call(
                BotToolKind::CheckTask,
                &ctx,
                json!({"task_id": task.id.to_string()}),
            )
            .await;
        assert!(!checked.is_error, "{}", checked.text_only());
        let text = checked.text_only();
        assert!(text.contains("running"), "{text}");
        assert!(text.contains("queued for the bot"), "{text}");

        // update_task records the note; with no live runner it cannot steer.
        let updated = rig
            .call(
                BotToolKind::UpdateTask,
                &ctx,
                json!({"task_id": task.id.to_string(), "text": "also check the tests"}),
            )
            .await;
        assert!(!updated.is_error, "{}", updated.text_only());
        let steps = rig.store.task_steps(&task.id).unwrap();
        assert!(steps.iter().any(|s| s.text == "also check the tests"));

        // cancel_task cancels it.
        let cancelled = rig
            .call(
                BotToolKind::CancelTask,
                &ctx,
                json!({"task_id": task.id.to_string()}),
            )
            .await;
        assert!(!cancelled.is_error, "{}", cancelled.text_only());
        assert!(cancelled.text_only().contains("Cancelled"), "{cancelled:?}");
        let stored = rig.store.get_task(&task.id).unwrap().unwrap();
        assert_eq!(stored.status, TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn check_task_on_an_unknown_id_errors_helpfully() {
        let rig = Rig::new();
        let ctx = rig.ctx(&rig.lead, &rig.lead.direct_conversation_id);

        // A well-formed id that does not exist.
        let missing = TaskId::from_suffix(&"0".repeat(32)).to_string();
        let result = rig
            .call(BotToolKind::CheckTask, &ctx, json!({"task_id": missing}))
            .await;
        assert!(result.is_error);
        assert!(
            result.text_only().contains("there is no task"),
            "{}",
            result.text_only()
        );

        // Something that is not an id at all.
        let result = rig
            .call(BotToolKind::CheckTask, &ctx, json!({"task_id": "audit-1"}))
            .await;
        assert!(result.is_error);
        assert!(
            result.text_only().contains("not a task id"),
            "{}",
            result.text_only()
        );
    }

    #[tokio::test]
    async fn another_bots_task_cannot_be_cancelled_but_a_handoff_can_be_updated() {
        let rig = Rig::new();
        let group_ctx = rig.ctx(&rig.lead, &rig.group.id);
        rig.call(
            BotToolKind::HandoffTask,
            &group_ctx,
            json!({"to": "scout", "title": "review", "task": "check it"}),
        )
        .await;
        let task = rig.store.tasks_of(&rig.scout.id).unwrap().remove(0);
        let ctx = rig.ctx(&rig.lead, &rig.lead.direct_conversation_id);

        // Cancelling somebody else's task is refused, naming who to ask.
        let cancelled = rig
            .call(
                BotToolKind::CancelTask,
                &ctx,
                json!({"task_id": task.id.to_string()}),
            )
            .await;
        assert!(cancelled.is_error);
        assert!(cancelled.text_only().contains("@scout"), "{cancelled:?}");

        // The requester may still add context to their own handoff.
        let updated = rig
            .call(
                BotToolKind::UpdateTask,
                &ctx,
                json!({"task_id": task.id.to_string(), "text": "focus on auth"}),
            )
            .await;
        assert!(!updated.is_error, "{}", updated.text_only());
        let steps = rig.store.task_steps(&task.id).unwrap();
        assert!(steps.iter().any(|s| s.text == "focus on auth"));
    }

    // --- roster reads ----------------------------------------------------------------

    #[tokio::test]
    async fn list_bots_and_list_nodes_name_what_exists() {
        let rig = Rig::new();
        rig.store.insert_node(&testing::local_node()).unwrap();
        let mut offline = testing::local_node();
        offline.id = testing::remote_node_id();
        offline.label = "office mac".into();
        offline.kind = NodeKind::Remote;
        offline.online = false;
        rig.store.insert_node(&offline).unwrap();

        let ctx = rig.ctx(&rig.lead, &rig.lead.direct_conversation_id);
        let bots = rig.call(BotToolKind::ListBots, &ctx, json!({})).await;
        let text = bots.text_only();
        assert!(text.contains("@lead (you)"), "{text}");
        assert!(text.contains("@scout"), "{text}");
        assert!(text.contains("idle"), "{text}");

        let nodes = rig.call(BotToolKind::ListNodes, &ctx, json!({})).await;
        let text = nodes.text_only();
        assert!(text.contains("this machine"), "{text}");
        assert!(text.contains("local node"), "{text}");
        assert!(text.contains("online"), "{text}");
        assert!(text.contains("fs@1"), "{text}");
        assert!(text.contains("swarmbot-fixture"), "{text}");
        assert!(text.contains("read-write"), "{text}");
        assert!(text.contains("office mac"), "{text}");
        assert!(text.contains("remote node"), "{text}");
        assert!(text.contains("OFFLINE"), "an offline node says so: {text}");
    }

    // --- robustness and registration ---------------------------------------------------

    #[tokio::test]
    async fn handlers_do_not_panic_on_missing_or_extra_fields() {
        let rig = Rig::new();
        let ctx = rig.ctx(&rig.lead, &rig.lead.direct_conversation_id);

        // Missing everything: a clean error naming the first missing field.
        let result = rig.call(BotToolKind::MessageBot, &ctx, json!({})).await;
        assert!(result.is_error);
        assert!(
            result.text_only().contains("missing field"),
            "{}",
            result.text_only()
        );

        // An unknown extra key is the registry validator's business; the
        // handler ignores it and does its job.
        let result = rig
            .call(
                BotToolKind::MessageBot,
                &ctx,
                json!({"to": "scout", "text": "hi", "priority": "high"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.text_only());
        assert_eq!(rig.store.queued_deliveries(&rig.scout.id).unwrap().len(), 1);

        // A model that sends no arguments at all may send `null`: a no-args
        // tool still runs, and an args tool fails cleanly.
        let result = rig.call(BotToolKind::ListBots, &ctx, Value::Null).await;
        assert!(!result.is_error, "{}", result.text_only());
        let result = rig.call(BotToolKind::MessageBot, &ctx, Value::Null).await;
        assert!(result.is_error);
        assert!(
            result.text_only().contains("missing field `to`"),
            "{}",
            result.text_only()
        );
    }

    #[test]
    fn register_bot_tools_fills_the_registry_with_the_catalogue() {
        let rig = Rig::new();
        let mut registry = ToolRegistry::new();
        register_bot_tools(&mut registry, rig.env.clone());
        let specs = registry.specs();
        let by_name: HashMap<&str, &ToolSpec> = specs
            .iter()
            .map(|spec| (spec.name.as_str(), spec))
            .collect();
        for name in [
            "message_bot",
            "post_to_group",
            "handoff_task",
            "react",
            "list_bots",
            "list_nodes",
            "ask_user",
            "request_approval",
            "start_task",
            "list_tasks",
            "check_task",
            "update_task",
            "cancel_task",
            "remember",
        ] {
            assert!(
                by_name.contains_key(name),
                "{name} is not registered: {by_name:?}"
            );
        }
        // Bot tools act on the team, never on a machine: none needs a node.
        assert!(specs.iter().all(|spec| !spec.needs_node), "{specs:?}");
        let reads: HashSet<&str> = specs
            .iter()
            .filter(|spec| spec.read_only)
            .map(|spec| spec.name.as_str())
            .collect();
        assert_eq!(
            reads,
            HashSet::from(["list_bots", "list_nodes", "list_tasks", "check_task"])
        );
        // Every schema is a real object schema with an explicit `required`.
        for spec in &specs {
            assert_eq!(spec.parameters["type"], json!("object"), "{}", spec.name);
            assert!(spec.parameters["properties"].is_object(), "{}", spec.name);
            assert!(spec.parameters["required"].is_array(), "{}", spec.name);
            assert!(!spec.description.is_empty(), "{}", spec.name);
        }
        // The toolsets split where the gating expects: task tools are hidden
        // from a task's own session, bot tools are not.
        for spec in &specs {
            let expected = match spec.name.as_str() {
                "start_task" | "list_tasks" | "check_task" | "update_task" | "cancel_task" => {
                    Toolset::Task
                }
                _ => Toolset::Bot,
            };
            assert_eq!(spec.toolset, expected, "{}", spec.name);
        }
    }
}
