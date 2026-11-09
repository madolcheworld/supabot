//! SwarmBot contracts: the domain every crate agrees on.
//!
//! This crate has no I/O, no async runtime and no database. It is the spine of
//! the dependency graph: `llm`, `tools`, `store`, `node-protocol` and `core`
//! all depend on it and nothing else in common.
//!
//! Conventions used throughout:
//!
//! * **ids** are typed newtypes (`BotId`, `EntryId`, …), time-sortable;
//! * **entries** are whole-entity upserts guarded by a `revision`;
//! * **enums** serialize with an internal tag (`{"kind": "…"}`) so a client
//!   built against an older schema can still decode what it knows;
//! * **errors** are values ([`DomainError`]), never panics.
//!
//! The design inherits the parts of OpenBot's decision record that make a
//! multi-bot team behave: one mind per bot, one turn per conversation,
//! deliveries that queue instead of interrupting, and a hop limit on
//! bot-to-bot chains.

pub mod bot;
pub mod config;
pub mod conversation;
pub mod delivery;
pub mod entry;
pub mod error;
pub mod id;
pub mod live;
pub mod node;
pub mod repo;
pub mod request;
pub mod routine;
pub mod task;
pub mod tool;
pub mod turn;

/// Node wire protocol version spoken by this build. The brain also accepts
/// `PROTOCOL_VERSION - 1` from an older node.
pub const PROTOCOL_VERSION: u32 = 1;

/// The pairing-code hash prefix, so a code from another product never matches.
pub const PAIRING_HASH_PREFIX: &str = "swarmbot-pairing-v1";

/// String the node signs on upgrade: `<prefix>|<nodeId>|<ts>|<nonce>|<origin>`.
pub const NODE_SIGN_PREFIX: &str = "swarmbot-node-v1";

/// String the brain signs in `node.hello`: `<prefix>|<nodeId>|<nonce>|<origin>`.
pub const BRAIN_SIGN_PREFIX: &str = "swarmbot-brain-v1";

pub use bot::{Bot, BotStatus, normalize_name};
pub use config::{BackendKind, BackendSpec, BotSpec, BrainConfig, GroupSpec, Settings, TeamSpec};
pub use conversation::{Conversation, ConversationKind, MAX_GROUP_MEMBERS, MIN_GROUP_MEMBERS};
pub use delivery::{CausalChain, Delivery, DeliveryKind, DeliveryState, HOP_LIMIT, Urgency};
pub use entry::{
    Attachment, Author, CardEntry, Entry, EntryKind, EventEntry, MessageEntry, NoticeEntry,
    NoticeLevel, PostState, ReasoningEntry, ToolCallEntry, ToolCallStatus,
};
pub use error::{DomainError, not_found};
pub use id::{
    BotId, ConversationId, DeliveryId, EntryId, InvalidId, InvocationId, NodeId, RequestId,
    RoutineId, RuleId, RunId, Seq, TaskId, ToolCallId, TurnId,
};
pub use live::{LiveCursor, LiveEvent, REPLAY_WINDOW, SNAPSHOT_ENTRIES, Snapshot};
pub use node::{
    Capabilities, NodeInfo, NodeKind, NodeRoot, PairRequest, PairResponse, PairingRecord,
    PolicyCeiling, PolicyMap, ProtocolRange, RootMode, Surface,
};
pub use repo::{
    BotRepo, BotSessionRecord, ConversationRepo, DeliveryRepo, EntryRepo, EventLog, NodeRepo,
    Persistence, RecoveryReport, RequestRepo, RoutineRepo, RuleRepo, SessionRepo, SettingsStore,
    TaskRepo, ToolCallRecord, ToolCallRepo, TurnRepo, recover,
};
pub use request::{
    APPROVAL_EXPIRY_SECS, ApprovalRule, Decision, FieldKind, FormField, RequestKind, RequestStatus,
    Responder, RuleEffect, RuleScope, RuntimeRequest,
};
pub use routine::{Routine, RoutineRun, RoutineSchedule, RoutineStatus, RunStatus};
pub use task::{
    MAX_RUNNING_TASKS_PER_BOT, MAX_TASK_RESTARTS, StepKind, Task, TaskKind, TaskStatus, TaskStep,
};
pub use tool::{
    Content, Invocation, SHELL_OUTPUT_LIMIT_BYTES, TOOL_RESULT_LIMIT_BYTES, ToolCallRequest,
    ToolError, ToolResult, ToolSpec, Toolset,
};
pub use turn::{Turn, TurnInitiator, TurnStatus, Usage};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_whole_domain_round_trips_through_json() {
        let bot_id = BotId::new();
        let conversation = Conversation::direct(bot_id.clone());
        let bot = Bot::new("lead", "Lead", "you lead", "fake", conversation.id.clone());
        let entry = Entry::bot_message(conversation.id.clone(), 1, bot_id.clone(), "hi", None);
        let turn = Turn::start(
            bot_id.clone(),
            conversation.id.clone(),
            TurnInitiator::Owner,
            None,
            None,
        );
        let task = Task::own(bot_id.clone(), conversation.id.clone(), "audit", "check it");
        let request = RuntimeRequest::approval(
            bot_id.clone(),
            conversation.id.clone(),
            Surface::Shell,
            "shell",
            "cargo test",
            None,
        );
        let node = NodeInfo::local("here", vec![], PolicyMap::default());

        for value in [
            serde_json::to_value(&conversation).unwrap(),
            serde_json::to_value(&bot).unwrap(),
            serde_json::to_value(&entry).unwrap(),
            serde_json::to_value(&turn).unwrap(),
            serde_json::to_value(&task).unwrap(),
            serde_json::to_value(&request).unwrap(),
            serde_json::to_value(&node).unwrap(),
            serde_json::to_value(Settings::default()).unwrap(),
            serde_json::to_value(BrainConfig::default()).unwrap(),
        ] {
            let text = value.to_string();
            let back: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(back, value);
        }

        assert_eq!(
            serde_json::from_value::<Conversation>(serde_json::to_value(&conversation).unwrap())
                .unwrap(),
            conversation
        );
        assert_eq!(
            serde_json::from_value::<Bot>(serde_json::to_value(&bot).unwrap()).unwrap(),
            bot
        );
        assert_eq!(
            serde_json::from_value::<Entry>(serde_json::to_value(&entry).unwrap()).unwrap(),
            entry
        );
    }

    #[test]
    fn protocol_constants_are_stable() {
        assert_eq!(PROTOCOL_VERSION, 1);
        assert_eq!(PAIRING_HASH_PREFIX, "swarmbot-pairing-v1");
        assert!(!NODE_SIGN_PREFIX.is_empty());
        assert!(!BRAIN_SIGN_PREFIX.is_empty());
        assert_eq!(HOP_LIMIT, 8);
    }
}
