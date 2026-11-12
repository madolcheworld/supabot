//! SwarmBot core: the orchestration.
//!
//! What lives here, and the rule each module exists to keep:
//!
//! * [`runners`] — one runner per bot, one mind per bot, queue instead of
//!   interrupt, and a turn that belongs to exactly one conversation;
//! * [`orchestrator`] — where the owner's messages enter, and how they route;
//! * [`router`] — who a group message wakes (mentions, `@everyone`, reply-to,
//!   lead), and the `NO_REPLY` rule;
//! * [`messaging`] — bot-to-bot messages, group posts and handoffs, with the
//!   hop limit and "group work stays in its group";
//! * [`tasks`] — background and handed-off work, and where its result goes;
//! * [`approval`] — rules first, a human second, never a blocked tool call;
//! * [`transcript`] — sequence allocation, commit-before-publish, JSONL mirror;
//! * [`bus`] — one published change per write, resumable by cursor;
//! * [`session`] — the bot's single memory, its read cursors, and compaction;
//! * [`instructions`] — the prompt, rendered from the store and from the tools
//!   the bot actually has;
//! * [`backends`], [`settings`], [`bots`], [`conversations`] — the seams the
//!   brain and the API hang off;
//! * [`swarm`] — the composition root;
//! * [`testing`] — fixtures that run the real orchestration on a memory store.
//!
//! Nothing here opens a socket, reads a config file or knows about HTTP: the
//! brain crate mounts this on axum, and the CLI runs it in-process.

pub mod approval;
pub mod backends;
pub mod bot_tools;
pub mod bots;
pub mod bus;
pub mod compose;
pub mod conversations;
pub mod instructions;
pub mod messaging;
pub mod orchestrator;
pub mod router;
pub mod routines;
pub mod runners;
pub mod session;
pub mod settings;
pub mod swarm;
pub mod tasks;
pub mod testing;
pub mod transcript;

pub use approval::{ApprovalBridge, ApprovalService, Outcome as ApprovalOutcome, Settled};
pub use backends::BackendSet;
pub use bots::{BotService, NewBot};
pub use bus::{Change, LiveHub};
pub use compose::{durable as compose_durable, memory as compose_memory};
pub use conversations::ConversationService;
pub use instructions::{InstructionRenderer, RenderOptions};
pub use messaging::{Messaging, SendContext};
pub use orchestrator::Orchestrator;
pub use router::{GroupRouter, NO_REPLY, RouteDecision, RouteReason};
pub use routines::RoutineScheduler;
pub use runners::{
    BotRunner, NoHooks, RunnerCmd, RunnerEnv, RunnerHandle, RunnerHooks, RunnerMap, SharedState,
    TurnOutcome,
};
pub use session::Session;
pub use settings::SettingsHandle;
pub use swarm::{McpProviderSlot, McpTurnProvider, McpTurnRegistration, McpTurnRequest, Swarm};
pub use tasks::BackgroundTasks;
pub use transcript::Transcript;
