//! The live hub: every persisted change, published once, resumable.
//!
//! Two rules make this trustworthy:
//!
//! * **writes commit before publishing** — a subscriber can always read back
//!   what it was just told about;
//! * **a cursor is a promise** — `{epoch, sequence}` lets a client that dropped
//!   resume; a gap of [`REPLAY_WINDOW`] changes or fewer is replayed from the
//!   ring, anything larger (or a new epoch) gets a fresh snapshot instead of a
//!   transcript with holes in it.
//!
//! Callers hand the hub a [`Change`], never a cursor: the sequence is assigned
//! here, when the event log has accepted the event.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, warn};

use swarmbot_contracts::bot::{Bot, BotStatus};
use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::entry::Entry;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{BotId, ConversationId, NodeId};
use swarmbot_contracts::live::{LiveCursor, LiveEvent, REPLAY_WINDOW, SNAPSHOT_ENTRIES, Snapshot};
use swarmbot_contracts::node::NodeInfo;
use swarmbot_contracts::repo::Persistence;
use swarmbot_contracts::request::RuntimeRequest;
use swarmbot_contracts::task::Task;
use swarmbot_contracts::turn::Turn;

/// How many changes the in-memory ring keeps for a resume. The store keeps its
/// own bounded log; this ring only has to cover a client that blinked.
const RING_CAPACITY: usize = REPLAY_WINDOW as usize;

/// How many changes the broadcast channel buffers per subscriber.
///
/// Sized for a boot burst, not for a steady state: provisioning a team publishes
/// a bot, a conversation and a transcript line per bot in a few milliseconds,
/// and a subscriber that is still building its snapshot would otherwise lag and
/// resync before it ever showed anything. A lag is still handled honestly (a
/// `Resync` plus a fresh snapshot); this just keeps it from being the norm.
const BROADCAST_CAPACITY: usize = 512;

/// A change worth telling subscribers about.
#[derive(Clone, Debug)]
pub enum Change {
    Entry(Entry),
    Conversation(Conversation),
    /// A group was deleted.
    ConversationDeleted(ConversationId),
    Bot(Bot),
    /// Roster-only: the bot's one status line, without the whole bot.
    BotStatus {
        bot_id: BotId,
        status: BotStatus,
    },
    Turn(Turn),
    Task(Task),
    Request(RuntimeRequest),
    Node(NodeInfo),
    NodePresence {
        node_id: NodeId,
        online: bool,
    },
}

impl Change {
    fn into_event(self, cursor: LiveCursor) -> LiveEvent {
        match self {
            Self::Entry(entry) => LiveEvent::EntryUpsert { cursor, entry },
            Self::Conversation(conversation) => LiveEvent::ConversationUpsert {
                cursor,
                conversation,
            },
            Self::ConversationDeleted(conversation_id) => LiveEvent::ConversationDeleted {
                cursor,
                conversation_id,
            },
            Self::Bot(bot) => LiveEvent::BotUpsert { cursor, bot },
            Self::BotStatus { bot_id, status } => LiveEvent::BotStatus {
                cursor,
                bot_id,
                status,
            },
            Self::Turn(turn) => LiveEvent::TurnUpsert { cursor, turn },
            Self::Task(task) => LiveEvent::TaskUpsert { cursor, task },
            Self::Request(request) => LiveEvent::RequestUpsert { cursor, request },
            Self::Node(node) => LiveEvent::NodeUpsert { cursor, node },
            Self::NodePresence { node_id, online } => LiveEvent::NodePresence {
                cursor,
                node_id,
                online,
                at: Utc::now(),
            },
        }
    }

    /// True when this change is worth keeping in the replay ring.
    fn is_replayable(&self) -> bool {
        // A presence blip is rendered from the roster row that follows it; a
        // status line is superseded by the next one.
        !matches!(self, Self::NodePresence { .. })
    }
}

#[derive(Debug)]
struct HubState {
    epoch: u64,
    sequence: u64,
    ring: VecDeque<(LiveCursor, LiveEvent)>,
}

/// Publishes changes and serves subscriptions.
pub struct LiveHub {
    store: Arc<dyn Persistence>,
    tx: broadcast::Sender<LiveEvent>,
    state: Mutex<HubState>,
}

impl LiveHub {
    /// Adopt the store's log position, so a restarted brain continues the
    /// sequence it left off at (and clients resync instead of seeing gaps).
    pub fn new(store: Arc<dyn Persistence>) -> Arc<Self> {
        let epoch = store.epoch().unwrap_or_else(|e| {
            warn!(error = %e, "cannot read the event log epoch; starting at 1");
            1
        });
        let cursor = store.current_cursor().unwrap_or(LiveCursor::new(epoch, 0));
        let hub = Arc::new(Self {
            store,
            tx: broadcast::channel(BROADCAST_CAPACITY).0,
            state: Mutex::new(HubState {
                epoch: cursor.epoch.max(epoch),
                sequence: cursor.sequence,
                ring: VecDeque::with_capacity(RING_CAPACITY),
            }),
        });
        debug!(
            epoch = cursor.epoch,
            sequence = cursor.sequence,
            "live hub ready"
        );
        hub
    }

    /// The store this hub writes its event log to.
    pub fn store(&self) -> Arc<dyn Persistence> {
        Arc::clone(&self.store)
    }

    pub fn cursor(&self) -> LiveCursor {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        LiveCursor::new(state.epoch, state.sequence)
    }

    /// Store the event, then publish it. Returns the cursor it was given.
    pub fn publish(&self, change: Change) -> LiveCursor {
        let replayable = change.is_replayable();
        let event = change.into_event(self.reserve());
        let cursor = event.cursor().unwrap_or_else(|| self.cursor());

        // The log is the source of truth for a resume that outlives the ring.
        if let Err(err) = self.store.append_event(&event) {
            warn!(error = %err, "cannot append to the event log; subscribers may have to resync");
        }

        if replayable {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.ring.push_back((cursor, event.clone()));
            while state.ring.len() > RING_CAPACITY {
                state.ring.pop_front();
            }
        }

        // Nobody subscribed is not an error.
        let _ = self.tx.send(event);
        cursor
    }

    /// Take the next sequence number. Done before the store write so the ring
    /// and the log agree even if the write fails.
    fn reserve(&self) -> LiveCursor {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.sequence += 1;
        LiveCursor::new(state.epoch, state.sequence)
    }

    /// A live subscription. The returned receiver gets the backlog (replay or
    /// snapshot), a [`LiveEvent::Synchronized`] marker, then live events.
    ///
    /// The forwarder task ends when the receiver is dropped. A lagged receiver
    /// is told the truth (`Resync`) and given a fresh snapshot rather than a
    /// silently incomplete stream.
    pub fn subscribe(
        self: &Arc<Self>,
        cursor: Option<LiveCursor>,
        conversation: Option<ConversationId>,
    ) -> mpsc::UnboundedReceiver<LiveEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let hub = Arc::clone(self);
        let mut live = self.tx.subscribe();
        tokio::spawn(async move {
            let backlog = match cursor {
                Some(from) if from.can_replay_to(hub.cursor()) => {
                    hub.replay(from, conversation.as_ref())
                }
                Some(_) => vec![LiveEvent::Resync {
                    reason: "the cursor is too far behind or from another epoch".into(),
                }]
                .into_iter()
                .chain(hub.snapshot_events(conversation.as_ref()))
                .collect(),
                None => hub.snapshot_events(conversation.as_ref()),
            };
            for event in backlog {
                if tx.send(event).is_err() {
                    return;
                }
            }
            if tx
                .send(LiveEvent::Synchronized {
                    cursor: hub.cursor(),
                })
                .is_err()
            {
                return;
            }
            loop {
                match live.recv().await {
                    Ok(event) => {
                        if let Some(wanted) = &conversation
                            && event.conversation_id().is_some_and(|id| id != wanted)
                        {
                            continue;
                        }
                        if tx.send(event).is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(skipped, "a live subscriber lagged; resyncing it");
                        let _ = tx.send(LiveEvent::Resync {
                            reason: format!("{skipped} changes were dropped"),
                        });
                        for event in hub.snapshot_events(conversation.as_ref()) {
                            if tx.send(event).is_err() {
                                return;
                            }
                        }
                        let _ = tx.send(LiveEvent::Synchronized {
                            cursor: hub.cursor(),
                        });
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        rx
    }

    /// The changes a cursor missed, from the ring.
    fn replay(&self, from: LiveCursor, conversation: Option<&ConversationId>) -> Vec<LiveEvent> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .ring
            .iter()
            .filter(|(cursor, _)| *cursor > from)
            .filter(|(_, event)| match (conversation, event.conversation_id()) {
                (Some(wanted), Some(id)) => id == wanted,
                // Events without a conversation (roster, nodes) always pass.
                _ => true,
            })
            .map(|(_, event)| event.clone())
            .collect()
    }

    fn snapshot_events(&self, conversation: Option<&ConversationId>) -> Vec<LiveEvent> {
        match self.snapshot(conversation) {
            Ok(snapshot) => vec![LiveEvent::Snapshot(Box::new(snapshot))],
            Err(err) => vec![LiveEvent::Failed {
                code: err.code().to_string(),
                message: err.to_string(),
            }],
        }
    }

    /// Everything a new subscriber needs. `conversation` narrows the entries to
    /// one transcript; the rest is always the whole roster.
    pub fn snapshot(&self, conversation: Option<&ConversationId>) -> Result<Snapshot, DomainError> {
        let cursor = self.cursor();
        let bots = self.store.list_bots()?;
        let conversations = self.store.list_conversations()?;
        let requests = self.store.pending_requests()?;
        let nodes = self.store.list_nodes()?;

        let wanted: Vec<ConversationId> = match conversation {
            Some(id) => vec![id.clone()],
            None => conversations.iter().map(|c| c.id.clone()).collect(),
        };
        let mut entries = Vec::new();
        for id in &wanted {
            entries.extend(self.store.entry_tail(id, SNAPSHOT_ENTRIES)?);
        }
        entries.sort_by(|a, b| (&a.conversation_id, a.seq).cmp(&(&b.conversation_id, b.seq)));

        let mut turns = Vec::new();
        for id in &wanted {
            turns.extend(self.store.turns_in(id, 20)?);
        }

        let mut tasks = Vec::new();
        for bot in &bots {
            tasks.extend(self.store.tasks_of(&bot.id)?);
        }

        Ok(Snapshot {
            cursor,
            bots,
            conversations,
            entries,
            requests,
            tasks,
            nodes,
            turns,
        })
    }

    /// Drop all but the newest events in the store's log. Called on a timer by
    /// the brain; a bounded log is what keeps a long-lived data dir small.
    pub fn truncate_log(&self, keep: u64) -> Result<u64, DomainError> {
        self.store.truncate_events(keep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::entry::{Author, EntryKind, NoticeEntry, NoticeLevel};
    use swarmbot_contracts::node::{PolicyCeiling, PolicyMap, Surface};
    use swarmbot_contracts::request::RequestKind;

    use crate::testing;

    fn hub() -> Arc<LiveHub> {
        LiveHub::new(testing::memory_store())
    }

    fn notice(conversation: ConversationId, text: &str) -> Change {
        Change::Entry(Entry::new(
            conversation,
            1,
            Author::Owner,
            EntryKind::Notice(Box::new(NoticeEntry {
                text: text.into(),
                level: NoticeLevel::Info,
            })),
        ))
    }

    /// Drain a subscription up to (and including) the synchronized marker.
    async fn drain_initial(rx: &mut mpsc::UnboundedReceiver<LiveEvent>) -> Vec<LiveEvent> {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            let last = matches!(event, LiveEvent::Synchronized { .. });
            events.push(event);
            if last {
                break;
            }
        }
        events
    }

    #[tokio::test]
    async fn a_new_subscriber_gets_a_snapshot_then_synchronized() {
        let hub = hub();
        hub.publish(notice(ConversationId::new(), "first"));
        let mut rx = hub.subscribe(None, None);
        let events = drain_initial(&mut rx).await;
        assert!(matches!(events[0], LiveEvent::Snapshot(_)));
        assert!(matches!(
            events.last(),
            Some(LiveEvent::Synchronized { .. })
        ));
    }

    #[tokio::test]
    async fn a_recent_cursor_replays_only_what_it_missed() {
        let hub = hub();
        let conversation = ConversationId::new();
        let first = hub.publish(notice(conversation.clone(), "one"));
        hub.publish(notice(conversation.clone(), "two"));
        hub.publish(notice(conversation.clone(), "three"));

        let mut rx = hub.subscribe(Some(first), None);
        let mut texts = Vec::new();
        while let Some(event) = rx.recv().await {
            match event {
                LiveEvent::EntryUpsert { entry, .. } => {
                    texts.push(entry.kind.text().unwrap_or_default().to_string())
                }
                LiveEvent::Synchronized { .. } => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(texts, vec!["two", "three"]);
    }

    #[tokio::test]
    async fn a_stale_cursor_resyncs_instead_of_lying() {
        let hub = hub();
        let stale = LiveCursor::new(hub.cursor().epoch, 0);
        let conversation = ConversationId::new();
        for i in 0..(REPLAY_WINDOW + 10) {
            hub.publish(notice(conversation.clone(), &format!("change {i}")));
        }
        let mut rx = hub.subscribe(Some(stale), None);
        match rx.recv().await {
            Some(LiveEvent::Resync { reason }) => {
                assert!(reason.contains("too far behind"), "{reason}")
            }
            other => panic!("expected a resync, got {other:?}"),
        }
        assert!(matches!(rx.recv().await, Some(LiveEvent::Snapshot(_))));
    }

    #[tokio::test]
    async fn another_epoch_resyncs() {
        let hub = hub();
        let from_another_life = LiveCursor::new(hub.cursor().epoch + 1, 1);
        let mut rx = hub.subscribe(Some(from_another_life), None);
        assert!(matches!(rx.recv().await, Some(LiveEvent::Resync { .. })));
    }

    #[tokio::test]
    async fn live_events_reach_an_open_subscription() {
        let hub = hub();
        let mut rx = hub.subscribe(None, None);
        drain_initial(&mut rx).await;

        let conversation = ConversationId::new();
        hub.publish(notice(conversation.clone(), "live"));
        match rx.recv().await {
            Some(LiveEvent::EntryUpsert { entry, cursor }) => {
                assert_eq!(entry.conversation_id, conversation);
                assert_eq!(entry.kind.text(), Some("live"));
                assert_eq!(cursor.sequence, 1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_conversation_subscription_filters_other_transcripts() {
        let hub = hub();
        let mine = ConversationId::new();
        let mut rx = hub.subscribe(None, Some(mine.clone()));
        drain_initial(&mut rx).await;

        hub.publish(notice(ConversationId::new(), "elsewhere"));
        hub.publish(notice(mine.clone(), "mine"));

        match rx.recv().await {
            Some(LiveEvent::EntryUpsert { entry, .. }) => {
                assert_eq!(entry.conversation_id, mine);
                assert_eq!(entry.kind.text(), Some("mine"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_snapshot_carries_the_roster_and_the_tail() {
        let hub = hub();
        let store = hub.store();
        let conversation = testing::seed_bot(&store, "lead").direct_conversation_id;
        for i in 0..5 {
            let entry = Entry::owner_message(conversation.clone(), i + 1, format!("line {i}"));
            store.upsert_entry(&entry).unwrap();
            hub.publish(Change::Entry(entry));
        }
        let snapshot = hub.snapshot(None).unwrap();
        assert_eq!(snapshot.bots.len(), 1);
        assert_eq!(snapshot.conversations.len(), 1);
        assert_eq!(snapshot.entries.len(), 5);
        assert_eq!(snapshot.entries[0].seq, 1, "ascending by seq");
    }

    #[test]
    fn presence_changes_stay_out_of_the_ring() {
        let hub = hub();
        hub.publish(Change::NodePresence {
            node_id: NodeId::new(),
            online: false,
        });
        hub.publish(Change::Node(NodeInfo::local(
            "here",
            vec![],
            PolicyMap::uniform(PolicyCeiling::Ask),
        )));
        let state = hub.state.lock().unwrap();
        assert_eq!(state.ring.len(), 1, "only the roster row is replayable");
    }

    #[test]
    fn requests_and_status_publish_as_their_own_events() {
        let hub = hub();
        let cursor = hub.publish(Change::BotStatus {
            bot_id: BotId::new(),
            status: BotStatus::Idle,
        });
        assert_eq!(cursor.sequence, 1);
        let request =
            RuntimeRequest::question(BotId::new(), ConversationId::new(), "which?", vec![]);
        assert_eq!(request.kind, RequestKind::Question);
        assert_eq!(hub.publish(Change::Request(request)).sequence, 2);
        let _ = Surface::Shell;
    }
}
