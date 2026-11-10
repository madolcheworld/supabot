//! The reducer: [`LiveEvent`] in, [`AppState`] out, with no I/O in between.
//!
//! This is where the app's correctness lives, and it is a *pure* function of
//! the state and the event — no clock reads that matter, no sockets, no GPUI.
//! That is what makes it testable headlessly (`#[test]`, fabricated
//! events) and what makes a resync safe: a `Resync` or a dropped socket is just
//! "apply the next `Snapshot`", because every collection here is upserted by id.
//!
//! Two mapping rules from [`../../../docs/ui.md`] §3 are load-bearing and are
//! enforced here, not in the view:
//!
//! * **an entry upsert is not a new line** — [`AppState::upsert_entry`] keys by
//!   `EntryId` and replaces, so a streamed reply at a rising `revision` updates
//!   one bubble instead of appending eleven;
//! * **a card's state comes from `RuntimeRequest.status`** — the reducer stores
//!   the request verbatim; the view reads `status`, so an expiry looks like an
//!   expiry rather than a clicked button.

use std::collections::HashMap;
use std::time::Instant;

use serde_json::Value;
use swarmbot_client::ClientError;
use swarmbot_contracts::{
    ConversationId, Decision, FormField, LiveEvent, RequestId, RuntimeRequest,
};

use crate::proto::{Command, CommandResult, ConnEvent, WorkerMsg};
use crate::state::{AppState, ConnectionState};

/// Apply one frame from the live socket.
pub fn apply(state: &mut AppState, event: LiveEvent) {
    match event {
        LiveEvent::Snapshot(snapshot) => {
            let had_cursor = state.cursor.is_some();
            state.bots = snapshot.bots;
            state.conversations = snapshot.conversations;
            state.nodes = snapshot.nodes;
            state.tasks = snapshot.tasks;
            state.requests = snapshot.requests;
            state.turns = snapshot.turns;

            // Replace the transcript wholesale, grouped per conversation.
            state.entries.clear();
            for entry in snapshot.entries {
                state.upsert_entry(entry);
            }
            // Unread counts are a client-side notion the snapshot does not
            // carry; drop counts for conversations that vanished, keep the rest.
            state
                .unread
                .retain(|id, _| state.conversations.iter().any(|c| &c.id == id));

            state.cursor = Some(snapshot.cursor);
            if had_cursor {
                // A snapshot after we already had a cursor is a resync.
                state.resynced_at = Some(Instant::now());
            }
            state.conn = ConnectionState::Connected;

            // If nothing is open (first load, or the open conversation was
            // deleted), open the first conversation so the main pane is useful.
            let still_open = state
                .open_conversation
                .as_ref()
                .is_some_and(|id| state.conversations.iter().any(|c| &c.id == id));
            if !still_open {
                if let Some(first) = state.conversations.first() {
                    let id: ConversationId = first.id.clone();
                    state.open_conversation = Some(id.clone());
                    state.unread.insert(id, 0);
                } else {
                    state.open_conversation = None;
                }
            }
        }
        LiveEvent::EntryUpsert { cursor, entry } => {
            let conv = entry.conversation_id.clone();
            let from_owner = entry.author.is_owner();
            let is_open = state.open_conversation.as_ref() == Some(&conv);
            state.upsert_entry(entry);
            if !is_open && !from_owner {
                *state.unread.entry(conv).or_insert(0) += 1;
            }
            state.cursor = Some(cursor);
        }
        LiveEvent::ConversationUpsert {
            cursor,
            conversation,
        } => {
            state.upsert_conversation(conversation);
            state.cursor = Some(cursor);
        }
        LiveEvent::ConversationDeleted {
            cursor,
            conversation_id,
        } => {
            state.remove_conversation(&conversation_id);
            state.cursor = Some(cursor);
        }
        LiveEvent::BotUpsert { cursor, bot } => {
            state.upsert_bot(bot);
            state.cursor = Some(cursor);
        }
        LiveEvent::BotStatus {
            cursor,
            bot_id,
            status,
        } => {
            state.set_bot_status(&bot_id, status);
            state.cursor = Some(cursor);
        }
        LiveEvent::TurnUpsert { cursor, turn } => {
            state.upsert_turn(turn);
            state.cursor = Some(cursor);
        }
        LiveEvent::TaskUpsert { cursor, task } => {
            state.upsert_task(task);
            state.cursor = Some(cursor);
        }
        LiveEvent::RequestUpsert { cursor, request } => {
            state.upsert_request(request);
            state.cursor = Some(cursor);
        }
        LiveEvent::NodeUpsert { cursor, node } => {
            state.upsert_node(node);
            state.cursor = Some(cursor);
        }
        LiveEvent::NodePresence {
            cursor,
            node_id,
            online,
            ..
        } => {
            state.set_node_presence(&node_id, online);
            state.cursor = Some(cursor);
        }
        LiveEvent::Synchronized { cursor } => {
            state.cursor = Some(cursor);
            state.conn = ConnectionState::Connected;
        }
        LiveEvent::Resync { reason } => {
            // The socket stays open; the brain sends a fresh `Snapshot` next.
            // Say so honestly and let the snapshot replace the world.
            state.toast = Some(format!("resyncing: {reason}"));
        }
        LiveEvent::Failed { code, message } => {
            state.conn = ConnectionState::Offline {
                reason: format!("{code}: {message}"),
            };
        }
    }
}

/// Apply one message from a worker thread (connection lifecycle, a live event,
/// or the result of a command).
pub fn apply_msg(state: &mut AppState, msg: WorkerMsg) {
    match msg {
        WorkerMsg::Conn(event) => apply_conn(state, event),
        WorkerMsg::Live(event) => apply(state, event),
        WorkerMsg::Result(result) => apply_result(state, result),
    }
}

/// Fold a connection-lifecycle event into the header's connection state.
pub fn apply_conn(state: &mut AppState, event: ConnEvent) {
    match event {
        ConnEvent::Connecting => state.conn = ConnectionState::Connecting,
        ConnEvent::Established => {
            // The socket is open; the snapshot/`synchronized` that follows is
            // what actually promotes us to `Connected`.
            if !state.conn.is_online() {
                state.conn = ConnectionState::Connecting;
            }
        }
        ConnEvent::Reconnecting { attempt } => {
            state.conn = ConnectionState::Reconnecting { attempt }
        }
        ConnEvent::Offline { reason } => state.conn = ConnectionState::Offline { reason },
        ConnEvent::NeedsLogin { reason } => {
            state.needs_login = true;
            state.login_error = Some(reason.clone());
            state.conn = ConnectionState::Offline { reason };
        }
    }
}

/// Fold a command's typed result into the state. Errors become a toast (and,
/// for a 401, the login prompt) — never a panic.
pub fn apply_result(state: &mut AppState, result: CommandResult) {
    match result {
        CommandResult::Login(Ok(())) => {
            state.needs_login = false;
            state.login_error = None;
            state.toast = Some("logged in".to_string());
        }
        CommandResult::Login(Err(err)) => fail(state, "login failed", &err),
        CommandResult::Health(Ok(health)) => state.health = Some(health),
        CommandResult::Health(Err(err)) => fail(state, "health check failed", &err),
        CommandResult::Send(Ok(sent)) => {
            // The committed owner entry, applied now for snappiness; the live
            // socket will confirm it (an upsert of the same id is a no-op).
            let conv = sent.entry.conversation_id.clone();
            state.upsert_entry(sent.entry.clone());
            state.unread.insert(conv, 0);
            state.toast = Some(if sent.queued.is_empty() {
                if sent.reason.is_empty() {
                    "sent".to_string()
                } else {
                    format!("sent · {}", sent.reason)
                }
            } else {
                format!("queued ({} bot(s) busy)", sent.queued.len())
            });
        }
        CommandResult::Send(Err(err)) => fail(state, "send failed", &err),
        CommandResult::Interrupt(Ok(true)) => state.toast = Some("stopped".to_string()),
        CommandResult::Interrupt(Ok(false)) => {
            state.toast = Some("nothing was running".to_string())
        }
        CommandResult::Interrupt(Err(err)) => fail(state, "stop failed", &err),
        CommandResult::Respond(Ok(request)) => {
            state.upsert_request(request);
            state.toast = Some("answered".to_string());
        }
        CommandResult::Respond(Err(err)) => fail(state, "answer failed", &err),
        CommandResult::CancelTask(Ok(task)) => state.upsert_task(task),
        CommandResult::CancelTask(Err(err)) => fail(state, "cancel failed", &err),
        CommandResult::TaskDetail(Ok(detail)) => {
            let id = detail.task.id.clone();
            state.upsert_task(detail.task);
            state.steps.insert(id, detail.steps);
        }
        CommandResult::TaskDetail(Err(err)) => fail(state, "could not load the task", &err),
        CommandResult::CreateGroup(Ok(conversation)) => {
            let id = conversation.id.clone();
            state.upsert_conversation(conversation);
            state.open(id);
            state.toast = Some("group created".to_string());
        }
        CommandResult::CreateGroup(Err(err)) => fail(state, "could not create the group", &err),
        CommandResult::BotSession(Ok(folded)) => {
            state.toast = Some(format!("folded {folded} message(s)"))
        }
        CommandResult::BotSession(Err(err)) => fail(state, "session action failed", &err),
        CommandResult::Pairing(Ok(pairing)) => state.pairing = Some(pairing),
        CommandResult::Pairing(Err(err)) => fail(state, "could not mint a pairing code", &err),
        CommandResult::RevokeNode(Ok(())) => state.toast = Some("node revoked".to_string()),
        CommandResult::RevokeNode(Err(err)) => fail(state, "revoke failed", &err),
        CommandResult::Settings(Ok(settings)) => {
            state.settings_draft = Some(settings.clone());
            state.settings = Some(settings);
        }
        CommandResult::Settings(Err(err)) => fail(state, "could not load settings", &err),
        CommandResult::UpdateSettings(Ok(settings)) => {
            state.settings_draft = Some(settings.clone());
            state.settings = Some(settings);
            state.toast = Some("settings saved".to_string());
        }
        CommandResult::UpdateSettings(Err(err)) => fail(state, "could not save settings", &err),
        CommandResult::Backends(Ok(backends)) => state.backends = backends,
        CommandResult::Backends(Err(err)) => fail(state, "could not load backends", &err),
        CommandResult::Probes(Ok(probes)) => state.probes = Some(probes),
        // The fresh bot state arrives as a live BotUpsert; a refusal is the
        // only thing this arm has to say.
        CommandResult::BotUpdated(Ok(())) => {}
        CommandResult::BotUpdated(Err(err)) => fail(state, "could not update the bot", &err),
        CommandResult::Models(backend, Ok(models)) => {
            state.models.insert(backend, Ok(models));
        }
        CommandResult::Models(backend, Err(err)) => {
            // Cached as a failure, not as an empty list: "the fetch broke"
            // and "this backend advertises nothing" are different truths,
            // and the menu renders them differently (with a Retry).
            state.models.insert(backend, Err(err.to_string()));
        }
        CommandResult::Probes(Err(err)) => fail(state, "probe failed", &err),
        CommandResult::Nodes(Ok(nodes)) => state.nodes = nodes,
        CommandResult::Nodes(Err(err)) => fail(state, "could not load nodes", &err),
        CommandResult::Routines(Ok(routines)) => state.routines = routines,
        CommandResult::Routines(Err(err)) => fail(state, "could not load routines", &err),
        CommandResult::RoutineCreated(Ok(routine)) => {
            state.upsert_routine(routine);
            state.routine_creator_open = false;
            state.toast = Some("routine created".to_string());
        }
        CommandResult::RoutineCreated(Err(err)) => fail(state, "could not create routine", &err),
        CommandResult::RoutineUpdated(Ok(routine)) => {
            state.upsert_routine(routine);
            state.toast = Some("routine updated".to_string());
        }
        CommandResult::RoutineUpdated(Err(err)) => fail(state, "could not update routine", &err),
        CommandResult::RoutineDeleted(id, Ok(())) => {
            state.remove_routine(&id);
            state.toast = Some("routine deleted".to_string());
        }
        CommandResult::RoutineDeleted(_, Err(err)) => fail(state, "could not delete routine", &err),
        CommandResult::RoutineRuns(id, Ok(runs)) => {
            state.routine_runs.insert(id, runs);
        }
        CommandResult::RoutineRuns(_, Err(err)) => fail(state, "could not load routine runs", &err),
        CommandResult::Shutdown(Ok(())) => {}
        CommandResult::Shutdown(Err(err)) => fail(state, "shutdown failed", &err),
    }
}

/// Record a command failure as a toast; a 401 also raises the login prompt.
fn fail(state: &mut AppState, what: &str, err: &ClientError) {
    if matches!(err, ClientError::Unauthorized) {
        state.needs_login = true;
        state.login_error = Some(err.to_string());
    }
    state.toast = Some(format!("{what}: {err}"));
}

/// Build the [`Command`] a card button produces. Split out so a test can assert
/// the exact wire shape each button yields without a window or a socket.
pub fn respond_command(
    id: RequestId,
    decision: Option<Decision>,
    answer: Option<Value>,
) -> Command {
    Command::Respond {
        id,
        decision,
        answer,
    }
}

/// Assemble a question/form answer JSON from the field values the owner typed.
///
/// Rules that keep the answer honest to the [`FormField`] shape:
/// * a `secret` field's value is sent as-is to the brain (which never echoes it
///   into the transcript) but is *masked in the view*, not here;
/// * empty optional fields are omitted rather than sent as `""`;
/// * a `boolean` field becomes a real JSON bool; everything else a string.
pub fn build_answer(fields: &[FormField], values: &HashMap<String, String>) -> Value {
    use swarmbot_contracts::FieldKind;
    let mut map = serde_json::Map::new();
    for field in fields {
        let raw = values.get(&field.name).map(String::as_str).unwrap_or("");
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            // Send an empty value only when the field is required, so the brain
            // can reject it; otherwise omit it entirely.
            if field.required {
                map.insert(field.name.clone(), Value::String(String::new()));
            }
            continue;
        }
        let value = match field.kind {
            FieldKind::Boolean => Value::Bool(matches!(trimmed, "true" | "yes" | "y" | "on" | "1")),
            _ => Value::String(trimmed.to_string()),
        };
        map.insert(field.name.clone(), value);
    }
    Value::Object(map)
}

/// The request a card is bound to, looked up by id (a small helper the views and
/// tests share so neither re-implements the search).
pub fn find_request<'a>(state: &'a AppState, id: &RequestId) -> Option<&'a RuntimeRequest> {
    state.request(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selftest;
    use swarmbot_contracts::{BotId, Conversation, Entry, LiveCursor, Snapshot};

    #[test]
    fn model_lists_are_cached_per_backend_and_a_failure_caches_empty() {
        let mut state = AppState::new("http://brain.test");
        apply_msg(
            &mut state,
            WorkerMsg::Result(CommandResult::Models(
                "codex".into(),
                Ok(swarmbot_client::BackendModels {
                    models: vec![swarmbot_client::ModelOption {
                        id: "m1".into(),
                        label: None,
                    }],
                    authoritative: true,
                }),
            )),
        );
        assert_eq!(
            state
                .models
                .get("codex")
                .and_then(|listed| listed.as_ref().ok())
                .map(|list| list.models.len()),
            Some(1)
        );
        // A failed fetch is cached as a failure — the menu then shows the
        // reason plus a Retry, never a fabricated "advertises nothing".
        apply_msg(
            &mut state,
            WorkerMsg::Result(CommandResult::Models(
                "opencode".into(),
                Err(swarmbot_client::ClientError::Transport {
                    url: "http://brain.test".into(),
                    reason: "boom".into(),
                }),
            )),
        );
        match state.models.get("opencode").expect("cached") {
            Err(message) => assert!(message.contains("boom"), "{message}"),
            Ok(models) => panic!("a failure must not cache as a list: {models:?}"),
        }
    }

    fn drive(events: Vec<LiveEvent>) -> AppState {
        let mut state = AppState::new("http://brain.test");
        for event in events {
            apply(&mut state, event);
        }
        state
    }

    fn snapshot_with(entries: Vec<Entry>, conversations: Vec<Conversation>) -> LiveEvent {
        LiveEvent::Snapshot(Box::new(Snapshot {
            cursor: LiveCursor::new(1, 1),
            bots: Vec::new(),
            conversations,
            entries,
            requests: Vec::new(),
            tasks: Vec::new(),
            nodes: Vec::new(),
            turns: Vec::new(),
        }))
    }

    #[test]
    fn the_whole_selftest_scenario_reduces_consistently() {
        let state = drive(selftest::events());
        assert_eq!(state.bots.len(), 4);
        assert_eq!(state.cursor, Some(LiveCursor::new(1, 17)));
        assert!(matches!(state.conn, ConnectionState::Connected));
        assert!(
            state.open_conversation.is_some(),
            "a conversation is opened"
        );

        let team = state
            .conversations
            .iter()
            .find(|c| c.title.as_deref() == Some("Team"))
            .expect("the group exists");
        let entries = state.entries(&team.id);
        // Dev's streamed reply is ONE entry at its latest revision, never two.
        let dev_final: Vec<_> = entries
            .iter()
            .filter(|e| {
                e.as_message()
                    .is_some_and(|m| m.text.contains("refreshing the session token"))
            })
            .collect();
        assert_eq!(dev_final.len(), 1, "the stream collapsed to one bubble");
        assert!(
            !entries
                .iter()
                .any(|e| e.as_message().is_some_and(|m| m.text == "Fixing")),
            "the intermediate revision is gone"
        );
        // The tool call completed in place (one entry, terminal status).
        let tools: Vec<_> = entries
            .iter()
            .filter(|e| matches!(e.kind, swarmbot_contracts::EntryKind::ToolCall(_)))
            .collect();
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0].as_tool_call().expect("tool").status,
            swarmbot_contracts::ToolCallStatus::Completed
        );
        // The answered approval is stored with its terminal status.
        let answered = state
            .requests
            .iter()
            .find(|r| r.id.as_str().ends_with("approve-1"))
            .expect("the request");
        assert_eq!(answered.status, swarmbot_contracts::RequestStatus::Answered);
    }

    #[test]
    fn an_entry_upsert_replaces_by_id_and_a_stale_revision_is_ignored() {
        let conversation = Conversation::direct(BotId::new());
        let mut entry = Entry::bot_message(conversation.id.clone(), 1, BotId::new(), "first", None);
        let id = entry.id.clone();
        let mut state = drive(vec![snapshot_with(
            vec![entry.clone()],
            vec![conversation.clone()],
        )]);
        assert_eq!(state.entries(&conversation.id).len(), 1);

        // Revision 2 replaces revision 1 under the same id.
        entry.revision = 2;
        if let Some(m) = entry.as_message_mut() {
            m.text = "second".into();
        }
        apply(
            &mut state,
            LiveEvent::EntryUpsert {
                cursor: LiveCursor::new(1, 2),
                entry: entry.clone(),
            },
        );
        let entries = state.entries(&conversation.id);
        assert_eq!(entries.len(), 1, "still one entry");
        assert_eq!(entries[0].as_message().expect("msg").text, "second");

        // A late, lower revision must not clobber the newer text.
        let mut stale = entry.clone();
        stale.revision = 1;
        if let Some(m) = stale.as_message_mut() {
            m.text = "stale".into();
        }
        apply(
            &mut state,
            LiveEvent::EntryUpsert {
                cursor: LiveCursor::new(1, 3),
                entry: stale,
            },
        );
        assert_eq!(
            state.entries(&conversation.id)[0]
                .as_message()
                .expect("msg")
                .text,
            "second",
            "a stale revision is ignored"
        );
        let _ = id;
    }

    #[test]
    fn dismissed_entry_is_removed_and_cannot_be_re_upserted_or_restored_by_snapshot() {
        let conversation = Conversation::direct(BotId::new());
        let entry = Entry::bot_message(conversation.id.clone(), 1, BotId::new(), "err", None);
        let id = entry.id.clone();
        let mut state = drive(vec![
            LiveEvent::ConversationUpsert {
                cursor: LiveCursor::new(1, 1),
                conversation: conversation.clone(),
            },
            LiveEvent::EntryUpsert {
                cursor: LiveCursor::new(1, 2),
                entry: entry.clone(),
            },
        ]);
        assert_eq!(state.entries(&conversation.id).len(), 1);

        // Dismiss the entry
        state.dismiss_entry(&id);
        assert!(state.is_dismissed(&id));
        assert_eq!(state.entries(&conversation.id).len(), 0);

        // Re-upserting the same entry must do nothing
        apply(
            &mut state,
            LiveEvent::EntryUpsert {
                cursor: LiveCursor::new(1, 3),
                entry: entry.clone(),
            },
        );
        assert_eq!(state.entries(&conversation.id).len(), 0);

        // Snapshot containing the dismissed entry must filter it out
        apply(
            &mut state,
            snapshot_with(vec![entry], vec![conversation.clone()]),
        );
        assert_eq!(state.entries(&conversation.id).len(), 0);
    }

    #[test]
    fn unread_counts_background_conversations_and_resets_on_open() {
        let a = Conversation::direct(BotId::new());
        let b = Conversation::direct(BotId::new());
        let bot = BotId::new();
        let mut state = drive(vec![snapshot_with(Vec::new(), vec![a.clone(), b.clone()])]);
        state.open(a.id.clone());

        apply(
            &mut state,
            LiveEvent::EntryUpsert {
                cursor: LiveCursor::new(1, 2),
                entry: Entry::bot_message(b.id.clone(), 1, bot.clone(), "hey", None),
            },
        );
        assert_eq!(state.unread(&b.id), 1, "a background message is unread");
        assert_eq!(state.unread(&a.id), 0);

        state.open(b.id.clone());
        assert_eq!(state.unread(&b.id), 0, "opening clears the badge");
    }

    #[test]
    fn deleting_a_conversation_drops_its_entries_and_selection() {
        let a = Conversation::direct(BotId::new());
        let entry = Entry::owner_message(a.id.clone(), 1, "hi");
        let mut state = drive(vec![snapshot_with(vec![entry], vec![a.clone()])]);
        state.open(a.id.clone());
        assert!(!state.entries(&a.id).is_empty());

        apply(
            &mut state,
            LiveEvent::ConversationDeleted {
                cursor: LiveCursor::new(1, 2),
                conversation_id: a.id.clone(),
            },
        );
        assert!(state.entries(&a.id).is_empty());
        assert_ne!(state.open_conversation.as_ref(), Some(&a.id));
    }

    #[test]
    fn a_snapshot_after_a_cursor_marks_a_resync() {
        let mut state = drive(vec![snapshot_with(Vec::new(), Vec::new())]);
        assert!(
            state.resynced_at.is_none(),
            "the first snapshot is not a resync"
        );
        // A second snapshot (a gap too big to replay) is a resync.
        apply(&mut state, snapshot_with(Vec::new(), Vec::new()));
        assert!(state.resynced_at.is_some());
    }

    #[test]
    fn connection_events_map_to_honest_labels() {
        let mut state = AppState::new("http://brain.test");
        apply_conn(&mut state, ConnEvent::Reconnecting { attempt: 3 });
        assert_eq!(state.conn.label(), "reconnecting (attempt 3)");
        apply_conn(
            &mut state,
            ConnEvent::NeedsLogin {
                reason: "token please".into(),
            },
        );
        assert!(state.needs_login);
        assert!(state.conn.label().starts_with("offline:"));
        apply_conn(&mut state, ConnEvent::Connecting);
        assert_eq!(state.conn.label(), "connecting…");
    }

    #[test]
    fn a_card_answer_result_updates_the_request() {
        let mut request = swarmbot_contracts::RuntimeRequest::approval(
            BotId::new(),
            ConversationId::new(),
            swarmbot_contracts::Surface::Shell,
            "shell",
            "cargo test",
            None,
        );
        request.respond(
            chrono::Utc::now(),
            Some(Decision::AllowOnce),
            None,
            swarmbot_contracts::Responder::Owner,
        );
        let mut state = AppState::new("http://brain.test");
        apply_result(&mut state, CommandResult::Respond(Ok(request.clone())));
        let stored = state.request(&request.id).expect("stored");
        assert_eq!(stored.status, swarmbot_contracts::RequestStatus::Answered);
        assert_eq!(stored.decision, Some(Decision::AllowOnce));
    }

    #[test]
    fn build_answer_omits_empty_optional_fields_and_types_booleans() {
        use std::collections::HashMap;
        use swarmbot_contracts::{FieldKind, FormField};
        let fields = vec![
            FormField::text("port", "Port"),
            FormField {
                name: "confirm".into(),
                label: "Confirm".into(),
                kind: FieldKind::Boolean,
                choices: Vec::new(),
                required: false,
                default: None,
            },
            FormField::text("nick", "Nick"),
        ];
        let mut values = HashMap::new();
        values.insert("port".to_string(), "8080".to_string());
        values.insert("confirm".to_string(), "yes".to_string());
        values.insert("nick".to_string(), "   ".to_string());
        let answer = build_answer(&fields, &values);
        assert_eq!(answer["port"], "8080");
        assert_eq!(answer["confirm"], true, "a boolean becomes a real bool");
        assert!(
            answer.get("nick").is_none(),
            "an empty optional field is omitted"
        );
    }

    #[test]
    fn respond_command_carries_the_exact_wire_shape() {
        let id = RequestId::from_suffix("r1");
        match respond_command(id.clone(), Some(Decision::Deny), None) {
            Command::Respond {
                id: got,
                decision,
                answer,
            } => {
                assert_eq!(got, id);
                assert_eq!(decision, Some(Decision::Deny));
                assert!(answer.is_none());
            }
            other => panic!("expected a Respond command, got {other:?}"),
        }
    }

    #[test]
    fn routine_results_update_state_consistently() {
        use swarmbot_contracts::{Routine, RoutineSchedule};
        let mut state = AppState::new("http://brain.test");
        let r = Routine::new(
            BotId::new(),
            "Daily check",
            "check all systems",
            RoutineSchedule::Interval { every_secs: 60 },
            chrono::Utc::now(),
        )
        .expect("valid routine");
        let id = r.id.clone();

        // Created
        apply_result(&mut state, CommandResult::RoutineCreated(Ok(r.clone())));
        assert_eq!(state.routines.len(), 1);
        assert_eq!(state.routines[0].id, id);

        // Updated
        let mut updated = r.clone();
        updated.pause();
        apply_result(&mut state, CommandResult::RoutineUpdated(Ok(updated)));
        assert_eq!(state.routines.len(), 1);
        assert_eq!(
            state.routines[0].status,
            swarmbot_contracts::RoutineStatus::Paused
        );

        // Deleted
        apply_result(&mut state, CommandResult::RoutineDeleted(id, Ok(())));
        assert_eq!(state.routines.len(), 0);
    }
}
