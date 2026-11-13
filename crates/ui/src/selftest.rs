//! The `--selftest` driver: fabricated [`LiveEvent`]s pushed through the real
//! reducer, so the machine-checkable proof exercises the same code path a live
//! brain would — snapshot, streaming upserts at a rising revision, a bot going
//! busy, a task finishing, a card being answered — with no socket and no window
//! logic involved.
//!
//! [`events`] is also the fixture the reducer tests reuse, so "what the selftest
//! renders" and "what the tests assert on" cannot drift apart.

use gpui::{App, Entity};
use serde_json::json;
use swarmbot_contracts::{
    Author, Bot, BotId, BotStatus, CardEntry, Conversation, ConversationId, Decision, Entry,
    EntryKind, LiveCursor, LiveEvent, NodeInfo, PolicyMap, RequestId, RequestKind, RequestStatus,
    Responder, RuntimeRequest, Snapshot, Surface, Task, TaskStatus, ToolCallId, ToolCallStatus,
    Turn, TurnInitiator, TurnStatus,
};

use crate::reducer::apply;
use crate::state::{AppState, Screen};

/// Build a small, realistic scenario: a four-bot team, a group transcript with a
/// hand-off and a tool call, a two-level delegation tree, one pending approval,
/// and a live tail that streams a reply, completes the tool call, succeeds the
/// task and answers the card.
pub fn events() -> Vec<LiveEvent> {
    let lead = BotId::from_suffix("lead");
    let scout = BotId::from_suffix("scout");
    let dev = BotId::from_suffix("dev");
    let sentinel = BotId::from_suffix("sentinel");

    let dm_lead = ConversationId::from_suffix("dm-lead");
    let team = ConversationId::from_suffix("team");

    let mut bots = Vec::new();
    for (id, name, label, color) in [
        (lead.clone(), "lead", "Lead", "#599ce7"),
        (scout.clone(), "scout", "Scout", "#a78bfa"),
        (dev.clone(), "dev", "Dev", "#34d399"),
        (sentinel.clone(), "sentinel", "Sentinel", "#fbbf24"),
    ] {
        let mut bot = Bot::new(name, label, "a fabricated persona", "fake", dm_lead.clone());
        bot.id = id;
        bot.color = Some(color.to_string());
        bots.push(bot);
    }

    let mut dm = Conversation::direct(lead.clone());
    dm.id = dm_lead.clone();
    let mut group = Conversation::group(
        "Team",
        vec![lead.clone(), scout.clone(), dev.clone(), sentinel.clone()],
        lead.clone(),
    )
    .unwrap_or_else(|_| Conversation::direct(lead.clone()));
    group.id = team.clone();

    // --- transcript -------------------------------------------------------
    let mut entries: Vec<Entry> = Vec::new();
    entries.push(Entry::owner_message(
        team.clone(),
        1,
        "Please audit the login flow and fix the token-refresh bug.",
    ));
    entries.push(Entry::bot_message(
        team.clone(),
        2,
        lead.clone(),
        "On it. **Scout** will read the flow, **Dev** will fix what Scout finds.",
        None,
    ));
    entries.push(Entry::event(
        team.clone(),
        3,
        "handoff",
        "Handed off “Read the login flow” to @scout",
    ));

    let call_id = ToolCallId::from_suffix("call-1");
    let mut read_call = Entry::tool_call(
        team.clone(),
        4,
        None,
        call_id.clone(),
        "read_file",
        json!({"path": "crates/brain/src/auth.rs"}),
        Some(NodeInfo::local("here", vec![], PolicyMap::default()).id),
        None,
    );
    entries.push(read_call.clone());

    entries.push(Entry::bot_message(
        team.clone(),
        5,
        scout.clone(),
        "Found it: the session token is never refreshed after 401. `auth.rs:42` returns the stale cookie. @dev can fix it.",
        None,
    ));
    entries.push(Entry::event(
        team.clone(),
        6,
        "bot-messaged",
        "Messaged @dev",
    ));

    // The approval card, and the transcript entry that renders it.
    let mut approval = RuntimeRequest::approval(
        dev.clone(),
        team.clone(),
        Surface::Shell,
        "shell",
        "cargo test -p swarmbot-brain",
        Some("run the tests after the fix".to_string()),
    );
    approval.id = RequestId::from_suffix("approve-1");
    let card = CardEntry {
        request_id: approval.id.clone(),
        card: RequestKind::Approval,
        status: RequestStatus::Pending,
        title: approval.title(),
        body: "Dev wants to run a command.".to_string(),
        payload: json!({"command": "cargo test -p swarmbot-brain"}),
        expires_at: approval.expires_at,
    };
    entries.push(Entry::new(
        team.clone(),
        7,
        Author::System,
        EntryKind::Card(Box::new(card)),
    ));

    // --- delegation tree: lead → scout, and scout → dev --------------------
    let mut task_scout = Task::handoff(
        lead.clone(),
        scout.clone(),
        team.clone(),
        "Read the login flow",
        "Find why the token goes stale",
        Some("start at auth.rs".to_string()),
    );
    task_scout.id = swarmbot_contracts::TaskId::from_suffix("task-scout");
    task_scout.status = TaskStatus::Running;
    let mut task_dev = Task::handoff(
        scout.clone(),
        dev.clone(),
        team.clone(),
        "Fix the token refresh",
        "Refresh the session on 401 and add a test",
        None,
    );
    task_dev.id = swarmbot_contracts::TaskId::from_suffix("task-dev");
    task_dev.parent_task_id = Some(task_scout.id.clone());
    task_dev.status = TaskStatus::Queued;

    // A running turn so the header shows "Handling…" and Stop is enabled.
    let mut turn = Turn::start(
        lead.clone(),
        team.clone(),
        TurnInitiator::Owner,
        None,
        Some(task_scout.id.clone()),
    );
    turn.status = TurnStatus::Running;

    let node = NodeInfo::local("this machine", vec![], PolicyMap::default());

    let snapshot = Snapshot {
        cursor: LiveCursor::new(1, 10),
        bots,
        conversations: vec![dm, group],
        entries,
        requests: vec![approval.clone()],
        tasks: vec![task_scout.clone(), task_dev.clone()],
        nodes: vec![node],
        turns: vec![turn],
    };

    let mut events = vec![LiveEvent::Snapshot(Box::new(snapshot))];

    // Lead is now working.
    events.push(LiveEvent::BotStatus {
        cursor: LiveCursor::new(1, 11),
        bot_id: lead.clone(),
        status: BotStatus::Working {
            conversation_id: team.clone(),
            activity: "Coordinating the team".to_string(),
        },
    });

    // A streaming reply from Dev: the SAME entry id at a rising revision, so the
    // reducer must replace, not append.
    let mut stream = Entry::bot_message(team.clone(), 8, dev.clone(), "Fixing", None);
    let stream_id = stream.id.clone();
    events.push(LiveEvent::EntryUpsert {
        cursor: LiveCursor::new(1, 12),
        entry: stream.clone(),
    });
    stream.revision = 2;
    if let Some(message) = stream.as_message_mut() {
        message.text =
            "Fixing `auth.rs:42` — refreshing the session token on a 401 and adding a regression test."
                .to_string();
    }
    events.push(LiveEvent::EntryUpsert {
        cursor: LiveCursor::new(1, 13),
        entry: stream.clone(),
    });

    // The read_file tool call completes (same id, revision 2).
    let started_at = read_call.created_at;
    read_call.revision = 2;
    if let Some(call) = read_call.as_tool_call_mut() {
        call.status = ToolCallStatus::Completed;
        call.result_text = Some("fn session() { /* 120 lines */ }".to_string());
        call.duration_ms = Some(34);
        call.ended_at = Some(started_at);
    }
    events.push(LiveEvent::EntryUpsert {
        cursor: LiveCursor::new(1, 14),
        entry: read_call,
    });

    // Scout's task succeeds.
    let mut done = task_scout.clone();
    done.status = TaskStatus::Succeeded;
    done.result = Some("The token is never refreshed after a 401.".to_string());
    events.push(LiveEvent::TaskUpsert {
        cursor: LiveCursor::new(1, 15),
        task: done,
    });

    // The owner answers the approval card.
    let mut answered = approval.clone();
    answered.status = RequestStatus::Answered;
    answered.decision = Some(Decision::AllowOnce);
    answered.responded_by = Some(Responder::Owner);
    events.push(LiveEvent::RequestUpsert {
        cursor: LiveCursor::new(1, 16),
        request: answered,
    });

    events.push(LiveEvent::Synchronized {
        cursor: LiveCursor::new(1, 17),
    });

    // Keep the compiler honest about the id we captured for the streamed entry.
    debug_assert_eq!(stream.id, stream_id);
    events
}

/// Apply every fabricated event to `state` through the real reducer and return
/// how many were applied (reported by `--selftest`).
pub fn drive(cx: &mut App, state: &Entity<AppState>) -> usize {
    let events = events();
    let count = events.len();
    state.update(cx, |state, _cx| {
        for event in events {
            apply(state, event);
        }
    });
    count
}

/// One window configuration exercised by `--selftest`.
#[derive(Clone, Copy, Debug)]
pub struct ScreenConfig {
    pub screen: Screen,
    pub graph: bool,
    pub palette: bool,
    pub provider_menu: bool,
    pub plus_menu: bool,
    pub create_bot: bool,
    pub create_group: bool,
}

impl ScreenConfig {
    const fn base(screen: Screen) -> Self {
        Self {
            screen,
            graph: false,
            palette: false,
            provider_menu: false,
            plus_menu: false,
            create_bot: false,
            create_group: false,
        }
    }
}

/// The screen configurations `--selftest` opens one window per. `open_window`
/// forces a synchronous draw before it returns, so opening a window per config
/// proves each screen's render path runs (and does not panic) on a real
/// compositor — coverage a headless test cannot give here, because GPUI's
/// `TestAppContext` is behind the `test-support` feature, which pulls in `x11`
/// and would not link on this machine.
pub fn screen_configs() -> Vec<ScreenConfig> {
    let mut graph = ScreenConfig::base(Screen::Conversation);
    graph.graph = true;

    let mut palette = ScreenConfig::base(Screen::Conversation);
    palette.palette = true;

    let mut provider_menu = ScreenConfig::base(Screen::Conversation);
    provider_menu.provider_menu = true;

    let mut plus_menu = ScreenConfig::base(Screen::Conversation);
    plus_menu.plus_menu = true;

    let mut create_bot = ScreenConfig::base(Screen::Conversation);
    create_bot.create_bot = true;

    let mut create_group = ScreenConfig::base(Screen::Conversation);
    create_group.create_group = true;

    vec![
        ScreenConfig::base(Screen::Conversation),
        graph,
        ScreenConfig::base(Screen::Nodes),
        ScreenConfig::base(Screen::Routines),
        ScreenConfig::base(Screen::Usage),
        ScreenConfig::base(Screen::Settings),
        palette,
        // The two-column provider menu over a DM: both columns, the search
        // box and the footer render on the live compositor.
        provider_menu,
        plus_menu,
        create_bot,
        create_group,
    ]
}
