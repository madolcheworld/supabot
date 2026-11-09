//! Typed identifiers.
//!
//! Every entity has its own id type so a `BotId` can never be passed where a
//! `ConversationId` is expected. Ids are `<prefix>_<uuid v7 simple>`: sortable
//! by creation time, which keeps transcript paging and `ORDER BY id` honest
//! without an extra timestamp column.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// An id string that does not carry the expected prefix and shape.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid id {value:?}: expected `{prefix}_<32 hex chars>`")]
pub struct InvalidId {
    pub prefix: &'static str,
    pub value: String,
}

macro_rules! typed_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// The wire prefix, e.g. `bot` for `bot_0192…`.
            pub const PREFIX: &'static str = $prefix;

            /// A new time-sortable id.
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, Uuid::now_v7().simple()))
            }

            /// A deterministic id, for tests and fixtures.
            pub fn from_suffix(suffix: &str) -> Self {
                Self(format!("{}_{}", $prefix, suffix))
            }

            /// Parse an id, checking the prefix and the 32 hex characters.
            pub fn parse(value: impl Into<String>) -> Result<Self, InvalidId> {
                let value = value.into();
                let rest = value
                    .strip_prefix(concat!($prefix, "_"))
                    .filter(|rest| {
                        rest.len() == 32 && rest.bytes().all(|b| b.is_ascii_hexdigit())
                    })
                    .ok_or_else(|| InvalidId {
                        prefix: $prefix,
                        value: value.clone(),
                    })?;
                let _ = rest;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = InvalidId;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }
    };
}

typed_id!(
    /// A named bot (an AI teammate).
    BotId,
    "bot"
);
typed_id!(
    /// A conversation: a DM with one bot, a group of bots, or a routine run.
    ConversationId,
    "cnv"
);
typed_id!(
    /// One transcript entry (the unit of whole-entry upserts).
    EntryId,
    "ent"
);
typed_id!(
    /// One agent turn: a delivery taken from the queue to its end.
    TurnId,
    "trn"
);
typed_id!(
    /// One queued or steered delivery to a bot.
    DeliveryId,
    "dlv"
);
typed_id!(
    /// A background or handed-off task.
    TaskId,
    "tsk"
);
typed_id!(
    /// A runtime request: approval, question, form, secret or handoff.
    RequestId,
    "req"
);
typed_id!(
    /// An approval rule.
    RuleId,
    "rul"
);
typed_id!(
    /// A node (a machine that executes tools).
    NodeId,
    "nod"
);
typed_id!(
    /// A routine (a scheduled trigger owned by one bot).
    RoutineId,
    "rtn"
);
typed_id!(
    /// One routine run.
    RunId,
    "run"
);
typed_id!(
    /// One tool call made by a model inside a turn.
    ToolCallId,
    "tcl"
);
typed_id!(
    /// One side-effecting invocation on a node (policy, idempotency, audit).
    InvocationId,
    "inv"
);

/// A per-conversation monotonically increasing sequence number.
pub type Seq = u64;

impl NodeId {
    /// The id of the node that runs inside the brain process. It is fixed so
    /// that stored bot→node bindings survive restarts.
    pub const LOCAL: &'static str = "nod_local";

    /// The node that runs inside the brain process.
    pub fn local() -> Self {
        Self(Self::LOCAL.to_string())
    }

    pub fn is_local(&self) -> bool {
        self.0 == Self::LOCAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prefixed_and_sortable() {
        let a = BotId::new();
        let b = BotId::new();
        assert!(a.as_str().starts_with("bot_"));
        assert!(a.to_string().len() == 4 + 32);
        // v7 ids are time-ordered.
        assert!(a < b, "{a} should sort before {b}");
    }

    #[test]
    fn ids_round_trip_through_strings() {
        let id = ConversationId::new();
        let parsed: ConversationId = id.to_string().parse().expect("parses");
        assert_eq!(id, parsed);
        assert_eq!(id, ConversationId::parse(id.as_str()).unwrap());
    }

    #[test]
    fn wrong_prefix_and_shape_are_rejected() {
        assert!(BotId::parse("cnv_0189fbd3a4d37f0e9c0e6a3f8f1c9d2e").is_err());
        assert!(BotId::parse("bot_short").is_err());
        assert!(BotId::parse("bot_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
        let err = BotId::parse("nope").unwrap_err();
        assert_eq!(err.prefix, "bot");
    }

    #[test]
    fn serde_is_transparent() {
        let id = TaskId::new();
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{id}\""));
        let back: TaskId = serde_json::from_str(&format!("\"{id}\"")).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn local_node_is_stable() {
        assert_eq!(NodeId::local().as_str(), "nod_local");
        assert!(NodeId::local().is_local());
        assert!(!NodeId::new().is_local());
    }
}
