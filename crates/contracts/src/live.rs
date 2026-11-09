//! Live replication: what a client subscribes to.
//!
//! A subscription sends a snapshot, then upserts, then a `synchronized`
//! marker. Every event carries a [`LiveCursor`] so a client that dropped can
//! resume: a gap of [`REPLAY_WINDOW`] changes or fewer is replayed, a larger
//! gap gets a fresh snapshot.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::bot::{Bot, BotStatus};
use crate::conversation::Conversation;
use crate::entry::Entry;
use crate::id::{ConversationId, NodeId};
use crate::node::NodeInfo;
use crate::request::RuntimeRequest;
use crate::task::Task;
use crate::turn::Turn;

/// How many changes the hub keeps for a resume.
pub const REPLAY_WINDOW: u64 = 256;

/// `{epoch, sequence}`: the epoch changes when the brain restarts or the log
/// is reset, which tells clients to take a snapshot instead of resuming.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LiveCursor {
    pub epoch: u64,
    pub sequence: u64,
}

impl LiveCursor {
    pub fn new(epoch: u64, sequence: u64) -> Self {
        Self { epoch, sequence }
    }

    /// Whether `self` can reach `other` by replay, or needs a snapshot.
    pub fn can_replay_to(&self, other: LiveCursor) -> bool {
        self.epoch == other.epoch
            && other.sequence >= self.sequence
            && other.sequence - self.sequence <= REPLAY_WINDOW
    }
}

impl std::fmt::Display for LiveCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.epoch, self.sequence)
    }
}

impl std::str::FromStr for LiveCursor {
    type Err = crate::error::DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (epoch, sequence) = s.split_once(':').ok_or_else(|| {
            crate::error::DomainError::invalid(format!("cursor `{s}` is not `<epoch>:<sequence>`"))
        })?;
        Ok(Self {
            epoch: epoch
                .parse()
                .map_err(|_| crate::error::DomainError::invalid("cursor epoch is not a number"))?,
            sequence: sequence.parse().map_err(|_| {
                crate::error::DomainError::invalid("cursor sequence is not a number")
            })?,
        })
    }
}

/// Everything a new subscriber needs to render the app.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub cursor: LiveCursor,
    pub bots: Vec<Bot>,
    pub conversations: Vec<Conversation>,
    /// The tail window of entries: the newest [`SNAPSHOT_ENTRIES`] per
    /// conversation the client asked about.
    pub entries: Vec<Entry>,
    pub requests: Vec<RuntimeRequest>,
    pub tasks: Vec<Task>,
    pub nodes: Vec<NodeInfo>,
    pub turns: Vec<Turn>,
}

/// How many entries a snapshot carries per conversation.
pub const SNAPSHOT_ENTRIES: u32 = 100;

/// One frame on the live socket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LiveEvent {
    Snapshot(Box<Snapshot>),
    EntryUpsert {
        cursor: LiveCursor,
        entry: Entry,
    },
    ConversationUpsert {
        cursor: LiveCursor,
        conversation: Conversation,
    },
    /// A group was deleted: clients drop it and its transcript.
    ConversationDeleted {
        cursor: LiveCursor,
        conversation_id: ConversationId,
    },
    BotUpsert {
        cursor: LiveCursor,
        bot: Bot,
    },
    /// Roster-only status changes are sent without the whole bot.
    BotStatus {
        cursor: LiveCursor,
        bot_id: crate::id::BotId,
        status: BotStatus,
    },
    TurnUpsert {
        cursor: LiveCursor,
        turn: Turn,
    },
    TaskUpsert {
        cursor: LiveCursor,
        task: Task,
    },
    RequestUpsert {
        cursor: LiveCursor,
        request: RuntimeRequest,
    },
    NodeUpsert {
        cursor: LiveCursor,
        node: NodeInfo,
    },
    /// A node went online or offline; the roster row follows as `NodeUpsert`.
    NodePresence {
        cursor: LiveCursor,
        node_id: NodeId,
        online: bool,
        at: DateTime<Utc>,
    },
    /// Everything the client asked for has been sent; live changes follow.
    Synchronized {
        cursor: LiveCursor,
    },
    /// The cursor could not be honoured (usually: too far behind).
    Resync {
        reason: String,
    },
    Failed {
        code: String,
        message: String,
    },
}

impl LiveEvent {
    pub fn cursor(&self) -> Option<LiveCursor> {
        match self {
            Self::Snapshot(s) => Some(s.cursor),
            Self::EntryUpsert { cursor, .. }
            | Self::ConversationUpsert { cursor, .. }
            | Self::ConversationDeleted { cursor, .. }
            | Self::BotUpsert { cursor, .. }
            | Self::BotStatus { cursor, .. }
            | Self::TurnUpsert { cursor, .. }
            | Self::TaskUpsert { cursor, .. }
            | Self::RequestUpsert { cursor, .. }
            | Self::NodeUpsert { cursor, .. }
            | Self::NodePresence { cursor, .. }
            | Self::Synchronized { cursor } => Some(*cursor),
            Self::Resync { .. } | Self::Failed { .. } => None,
        }
    }

    /// The conversation an event belongs to, when it has one (used to filter a
    /// per-conversation subscription).
    pub fn conversation_id(&self) -> Option<&ConversationId> {
        match self {
            Self::EntryUpsert { entry, .. } => Some(&entry.conversation_id),
            Self::TurnUpsert { turn, .. } => Some(&turn.conversation_id),
            Self::RequestUpsert { request, .. } => Some(&request.conversation_id),
            Self::TaskUpsert { task, .. } => Some(&task.origin),
            Self::ConversationUpsert { conversation, .. } => Some(&conversation.id),
            Self::ConversationDeleted {
                conversation_id, ..
            } => Some(conversation_id),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_through_the_query_string_form() {
        let cursor = LiveCursor::new(3, 42);
        assert_eq!(cursor.to_string(), "3:42");
        assert_eq!("3:42".parse::<LiveCursor>().unwrap(), cursor);
        assert!("3".parse::<LiveCursor>().is_err());
        assert!("a:b".parse::<LiveCursor>().is_err());
    }

    #[test]
    fn replay_is_bounded_by_the_window_and_the_epoch() {
        let from = LiveCursor::new(1, 100);
        assert!(from.can_replay_to(LiveCursor::new(1, 100 + REPLAY_WINDOW)));
        assert!(!from.can_replay_to(LiveCursor::new(1, 100 + REPLAY_WINDOW + 1)));
        assert!(
            !from.can_replay_to(LiveCursor::new(2, 101)),
            "a new epoch resyncs"
        );
        assert!(
            !from.can_replay_to(LiveCursor::new(1, 99)),
            "behind is not a gap"
        );
    }

    #[test]
    fn events_report_their_cursor_and_conversation() {
        let entry = Entry::owner_message(ConversationId::new(), 1, "hi");
        let conversation = entry.conversation_id.clone();
        let event = LiveEvent::EntryUpsert {
            cursor: LiveCursor::new(1, 1),
            entry,
        };
        assert_eq!(event.cursor(), Some(LiveCursor::new(1, 1)));
        assert_eq!(event.conversation_id(), Some(&conversation));
        let failed = LiveEvent::Failed {
            code: "internal".into(),
            message: "boom".into(),
        };
        assert_eq!(failed.cursor(), None);
        assert_eq!(failed.conversation_id(), None);
    }

    #[test]
    fn events_are_tagged_for_the_wire() {
        let event = LiveEvent::Synchronized {
            cursor: LiveCursor::new(1, 7),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "synchronized");
        assert_eq!(json["cursor"]["sequence"], 7);
        assert_eq!(serde_json::from_value::<LiveEvent>(json).unwrap(), event);
    }
}
