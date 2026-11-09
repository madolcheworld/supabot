//! The two halves of the API, agreeing.
//!
//! `swarmbot-client` is what the CLI and the GPUI app actually use, and it was
//! written against the same route table as this crate — by somebody else. So this
//! file drives the *real* axum router through the *real* [`BrainClient`], with no
//! stub on either side: every assertion here is a place where a mismatch would
//! otherwise show up as a broken CLI.
//!
//! The client is blocking (`ureq`, no tokio), so each step runs on a blocking
//! thread and the orchestration between steps runs on the async one. [`step`]
//! takes the value the call needs by move, because a blocking closure cannot
//! borrow from the test that outlives it.

mod common;

use common::*;

use serde_json::json;

use swarmbot_client::{BrainClient, ClientError, NewBotRequest, NewRoutineRequest, SessionAction};
use swarmbot_contracts::id::{BotId, ConversationId, RequestId, TaskId};
use swarmbot_contracts::live::LiveEvent;
use swarmbot_contracts::node::Surface;
use swarmbot_contracts::request::{Decision, RuntimeRequest};
use swarmbot_contracts::routine::{RoutineSchedule, RoutineStatus};
use swarmbot_contracts::task::TaskStatus;
use swarmbot_contracts::{BackendKind, NodeId, Toolset};

/// One call to the brain, on a blocking thread.
async fn step<F, T, A>(client: &BrainClient, arg: A, work: F) -> T
where
    F: FnOnce(BrainClient, A) -> T + Send + 'static,
    A: Send + 'static,
    T: Send + 'static,
{
    let client = client.clone();
    blocking(move || work(client, arg)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_blocking_client_drives_the_whole_api() {
    let brain = TestBrain::start_durable().await;
    let client = BrainClient::new(&brain.base_url).expect("the base URL is valid");
    let token = brain.printed_token();
    let lead = brain.bot("lead");
    let scout = brain.bot_id("scout");
    let dm = lead.direct_conversation_id.clone();

    // --- login and health ---------------------------------------------------
    // `login` posts the one-time token *without* a credential, then keeps using
    // the token as a bearer, because a WebSocket upgrade cannot read a cookie jar.
    step(&client, token.clone(), |c, token| c.login(&token))
        .await
        .expect("login");
    let health = step(&client, (), |c, ()| c.health()).await.expect("health");
    assert!(health.ok);
    assert_eq!(health.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(health.epoch, brain.state.epoch());

    // A client that never logged in is refused, and says so as `Unauthorized`.
    let anonymous = BrainClient::new(&brain.base_url).expect("a client");
    let err = step(&anonymous, (), |c, ()| c.health())
        .await
        .expect_err("refused");
    assert!(matches!(err, ClientError::Unauthorized), "{err:?}");

    // --- bots ---------------------------------------------------------------
    let bots = step(&client, (), |c, ()| c.bots())
        .await
        .expect("the roster");
    assert_eq!(bots.len(), 4);
    assert!(bots.iter().any(|bot| bot.name == "lead"));

    // By handle and by id, the same bot.
    let by_handle = step(&client, "lead", |c, name| c.bot(name))
        .await
        .expect("by handle");
    let by_id = step(&client, by_handle.id.to_string(), |c, id| c.bot(&id))
        .await
        .expect("by id");
    assert_eq!(by_handle.id, by_id.id);
    assert_eq!(by_handle.direct_conversation_id, dm);

    let created = step(&client, (), |c, ()| {
        c.create_bot(
            &NewBotRequest::new("scribe", "Scribe", "writes the release notes", "fake")
                .color("#abcdef")
                .toolsets(vec![Toolset::Bot]),
        )
    })
    .await
    .expect("create");
    assert_eq!(created.name, "scribe");
    assert_eq!(created.label, "Scribe");
    assert_eq!(created.color.as_deref(), Some("#abcdef"));
    assert_eq!(created.toolsets, vec![Toolset::Bot]);

    // A duplicate handle is a conflict, and the client can tell.
    let err = step(&client, (), |c, ()| {
        c.create_bot(&NewBotRequest::new("scribe", "Second", "", "fake"))
    })
    .await
    .expect_err("refused");
    assert!(err.is_conflict(), "{err:?}");
    assert_eq!(err.code(), Some("conflict"));

    let renamed = step(&client, "scribe", |c, name| {
        c.update_bot(name, &json!({"label": "The Scribe"}))
    })
    .await
    .expect("patch");
    assert_eq!(renamed.label, "The Scribe");
    assert_eq!(renamed.instructions, "writes the release notes", "merged");

    let folded = step(&client, "scribe", |c, name| {
        c.bot_session(name, SessionAction::New)
    })
    .await
    .expect("a new session");
    assert_eq!(folded, 0);
    let folded = step(&client, "scribe", |c, name| {
        c.bot_session(name, SessionAction::Compact)
    })
    .await
    .expect("a compacted session");
    assert_eq!(folded, 0, "nothing to fold yet");

    step(&client, "scribe", |c, name| c.delete_bot(name))
        .await
        .expect("deleted");
    let err = step(&client, "scribe", |c, name| c.bot(name))
        .await
        .expect_err("gone");
    assert!(err.is_not_found(), "{err:?}");

    // --- conversations ------------------------------------------------------
    let members = (lead.id.clone(), scout.clone());
    let group = step(&client, members, |c, (lead, scout)| {
        c.create_group("Pair", &[lead, scout], None)
    })
    .await
    .expect("a group");
    assert_eq!(group.title.as_deref(), Some("Pair"));
    assert_eq!(group.kind.participants().len(), 2);
    assert!(group.kind.is_group());

    let group_id = group.id.clone();
    let renamed = step(&client, group_id.clone(), |c, id| {
        c.rename_conversation(&id, "Pair, renamed")
    })
    .await
    .expect("renamed");
    assert_eq!(renamed.title.as_deref(), Some("Pair, renamed"));

    let listed = step(&client, (), |c, ()| c.conversations())
        .await
        .expect("the list");
    assert!(listed.iter().any(|c| c.id == group_id));
    assert!(listed.iter().any(|c| c.id == dm));

    let fetched = step(&client, group_id.clone(), |c, id| c.conversation(&id))
        .await
        .expect("by id");
    assert_eq!(fetched.id, group_id);

    // A DM cannot be deleted; the client sees a conflict, not a transport error.
    let err = step(&client, dm.clone(), |c, id| c.delete_conversation(&id))
        .await
        .expect_err("refused");
    assert!(err.is_conflict(), "{err:?}");

    // --- messages -----------------------------------------------------------
    brain.script_text("the client and the brain agree");
    let sent = step(&client, dm.clone(), |c, id| {
        c.send_message(&id, "do we agree?", &[], None)
    })
    .await
    .expect("sent");
    assert_eq!(sent.entry.kind.text(), Some("do we agree?"));
    assert_eq!(sent.targets, vec![lead.id.clone()]);
    assert!(sent.queued.is_empty());
    assert_eq!(
        sent.reason, "addressed",
        "the routing reason is display text"
    );
    assert!(sent.entry.seq >= 1);

    let mentioned = step(&client, group_id.clone(), |c, id| {
        c.send_message(&id, "@scout anything there?", &[], None)
    })
    .await
    .expect("sent");
    assert_eq!(mentioned.targets, vec![scout]);
    assert_eq!(mentioned.reason, "mention");

    brain.settle().await;

    // Paging, in both directions the client offers.
    let tail = step(&client, dm.clone(), |c, id| c.tail(&id, 50))
        .await
        .expect("the tail");
    assert!(tail.iter().any(|e| e.kind.text() == Some("do we agree?")));
    assert!(
        tail.iter()
            .any(|e| e.kind.text() == Some("the client and the brain agree")),
        "the bot's reply is in the transcript: {tail:?}"
    );
    let seqs: Vec<u64> = tail.iter().map(|e| e.seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "ascending");

    let first = tail.first().expect("an entry").seq;
    let page = step(&client, (dm.clone(), first), |c, (id, after)| {
        c.entries(&id, after, 100)
    })
    .await
    .expect("a page");
    assert!(
        page.iter().all(|e| e.seq > first),
        "`after_seq` is exclusive: {page:?}"
    );
    assert_eq!(page.len(), tail.len() - 1);

    let stopped = step(&client, dm.clone(), |c, id| c.interrupt(&id))
        .await
        .expect("interrupt");
    assert!(!stopped, "nothing was running");

    // --- runtime requests and rules ----------------------------------------
    let mut request = RuntimeRequest::approval(
        lead.id.clone(),
        dm.clone(),
        Surface::Shell,
        "shell",
        "cargo build",
        Some("build it".into()),
    );
    let request_id: RequestId = brain
        .swarm
        .approval
        .raise_async(&mut request)
        .expect("the card is raised");

    let pending = step(&client, (), |c, ()| c.pending_requests())
        .await
        .expect("the cards");
    assert!(pending.iter().any(|r| r.id == request_id));

    let answered = step(&client, request_id.clone(), |c, id| {
        c.respond_request(&id, Some(Decision::AllowAlways), None)
    })
    .await
    .expect("answered");
    assert_eq!(answered.decision, Some(Decision::AllowAlways));
    assert!(answered.resolved_at.is_some());

    // "Always allow" wrote a rule the client can now read.
    let rules = step(&client, (), |c, ()| c.rules())
        .await
        .expect("the rules");
    assert_eq!(rules.len(), 1, "{rules:?}");
    assert_eq!(rules[0].command_prefix.as_deref(), Some("cargo"));
    assert_eq!(rules[0].surface, Some(Surface::Shell));

    // Answering twice keeps its status *and* its code.
    let err = step(&client, request_id, |c, id| {
        c.respond_request(&id, Some(Decision::Deny), None)
    })
    .await
    .expect_err("refused");
    assert!(err.is_conflict(), "{err:?}");
    assert_eq!(err.code(), Some("request_not_pending"), "{err:?}");

    // A question is answered with JSON.
    let mut question = RuntimeRequest::question(lead.id.clone(), dm.clone(), "which port?", vec![]);
    let question_id = brain
        .swarm
        .approval
        .raise_async(&mut question)
        .expect("the question is raised");
    let answered = step(&client, question_id, |c, id| {
        c.respond_request(&id, None, Some(json!({"port": 8080})))
    })
    .await
    .expect("answered");
    assert_eq!(answered.answer, Some(json!({"port": 8080})));

    // Deleting the rule the "always allow" wrote.
    let rule_id = rules[0].id.clone();
    step(&client, rule_id, |c, id| c.delete_rule(&id))
        .await
        .expect("deleted");
    let rules = step(&client, (), |c, ()| c.rules())
        .await
        .expect("the rules");
    assert!(rules.is_empty(), "{rules:?}");

    // --- tasks --------------------------------------------------------------
    // The runner is taken away first, so the task stays cancellable instead of
    // being finished by the bot before the client can look at it. The delivery
    // parks, which is the real "this bot has no runner yet" path.
    let handle = brain
        .swarm
        .runners
        .unregister(&lead.id)
        .expect("the lead has a runner");
    handle.shutdown();
    let task = brain
        .swarm
        .tasks
        .start(&lead, dm.clone(), "write the notes", "for the release")
        .expect("the task starts");
    let task_id: TaskId = task.id.clone();

    let tasks = step(&client, (), |c, ()| c.tasks(None))
        .await
        .expect("all tasks");
    assert!(tasks.iter().any(|t| t.id == task_id));
    let owner: BotId = lead.id.clone();
    let mine = step(&client, owner, |c, bot| c.tasks(Some(&bot)))
        .await
        .expect("one bot's tasks");
    assert!(mine.iter().any(|t| t.id == task_id));

    let detail = step(&client, task_id.clone(), |c, id| c.task(&id))
        .await
        .expect("the detail");
    assert_eq!(detail.task.title, "write the notes");
    assert!(!detail.steps.is_empty(), "the task recorded its own start");

    let cancelled = step(&client, task_id.clone(), |c, id| c.cancel_task(&id))
        .await
        .expect("cancelled");
    assert_eq!(cancelled.status, TaskStatus::Cancelled);

    let missing = TaskId::new();
    let err = step(&client, missing, |c, id| c.task(&id))
        .await
        .expect_err("refused");
    assert!(err.is_not_found(), "{err:?}");

    // Put the runner back and let the parked work drain, so the rest of the test
    // sees a swarm that can still answer.
    brain.swarm.ensure_runners().expect("runners come back");
    brain.settle().await;

    // --- nodes --------------------------------------------------------------
    let nodes = step(&client, (), |c, ()| c.nodes())
        .await
        .expect("the roster");
    let local = nodes
        .iter()
        .find(|node| node.id.is_local())
        .expect("the local node");
    assert!(local.online);
    assert!(local.can("shell"));

    let code = step(&client, (), |c, ()| c.create_pairing_code())
        .await
        .expect("a pairing code");
    assert_eq!(code.code.len(), 9, "{}", code.code);
    assert_eq!(code.code.chars().nth(4), Some('-'), "{}", code.code);
    assert!(code.node_id.is_none());
    assert!(
        code.expires_at.contains('T') && code.expires_at.ends_with('Z'),
        "an rfc3339 expiry as text: {}",
        code.expires_at
    );

    // The local node is this process, so revoking it is refused — as a conflict.
    let err = step(&client, NodeId::local(), |c, id| c.revoke_node(&id))
        .await
        .expect_err("refused");
    assert!(err.is_conflict(), "{err:?}");

    // --- routines -----------------------------------------------------------
    let req = NewRoutineRequest::new(
        "lead",
        "Nightly build",
        "Run compilation check",
        RoutineSchedule::Interval { every_secs: 3600 },
    );
    let routine = step(&client, req, |c, r| c.create_routine(&r))
        .await
        .expect("routine created");
    assert_eq!(routine.name, "Nightly build");

    let list = step(&client, (), |c, ()| c.routines(None))
        .await
        .expect("routines listed");
    assert_eq!(list.len(), 1);

    let paused = step(&client, routine.id.clone(), |c, id| c.pause_routine(&id))
        .await
        .expect("paused");
    assert_eq!(paused.status, RoutineStatus::Paused);

    let resumed = step(&client, routine.id.clone(), |c, id| c.resume_routine(&id))
        .await
        .expect("resumed");
    assert_eq!(resumed.status, RoutineStatus::Active);

    step(&client, routine.id.clone(), |c, id| c.delete_routine(&id))
        .await
        .expect("deleted");

    // --- settings and backends ---------------------------------------------
    let settings = step(&client, (), |c, ()| c.settings())
        .await
        .expect("settings");
    assert_eq!(settings.max_hops, 8);
    assert!(settings.bot_messaging);

    let patched = step(&client, (), |c, ()| {
        c.update_settings(&json!({"max_hops": 3}))
    })
    .await
    .expect("patched");
    assert_eq!(patched.max_hops, 3);
    assert_eq!(
        patched.max_tool_rounds, settings.max_tool_rounds,
        "a merge changes one key"
    );
    assert_eq!(brain.swarm.settings.get().max_hops, 3);

    let err = step(&client, (), |c, ()| {
        c.update_settings(&json!({"max_hops": 0}))
    })
    .await
    .expect_err("refused");
    assert_eq!(err.code(), Some("invalid_input"), "{err:?}");

    let backends = step(&client, (), |c, ()| c.backends())
        .await
        .expect("backends");
    let fake = backends
        .iter()
        .find(|backend| backend.id == "fake")
        .expect("the fake backend is listed");
    assert_eq!(fake.kind, BackendKind::Fake);
    assert!(fake.api_key_env.is_none());

    let probes = step(&client, (), |c, ()| c.probe_backends())
        .await
        .expect("probes");
    let probe = probes
        .iter()
        .find(|probe| probe.backend == "fake")
        .expect("the fake backend is probed");
    assert!(probe.ok, "{:?}", probe.detail);

    // --- the live socket ----------------------------------------------------
    let (handshake_events, cursor) = step(&client, (), |c, ()| {
        let mut live = c.live(None, None).expect("the socket opens");
        let mut events = Vec::new();
        loop {
            match live.next_event().expect("a frame") {
                Some(LiveEvent::Synchronized { .. }) => break,
                Some(event) => events.push(event),
                None => panic!("the socket closed during the handshake"),
            }
        }
        let cursor = live.cursor().expect("a cursor");
        live.close();
        (events, cursor)
    })
    .await;
    assert!(
        matches!(handshake_events.first(), Some(LiveEvent::Snapshot(_))),
        "the stream opens with a snapshot: {handshake_events:?}"
    );
    assert_eq!(cursor.epoch, brain.state.epoch());

    // Something happens while the client is away, and resuming picks it up
    // without repeating what was already delivered.
    let sent = step(&client, dm.clone(), |c, id| {
        c.send_message(&id, "while you were out", &[], None)
    })
    .await
    .expect("sent");
    let unseen_id = sent.entry.id.clone();
    brain.settle().await;

    let replayed = step(&client, cursor, |c, cursor| {
        let mut live = c.live(Some(cursor), None).expect("the socket resumes");
        let mut events = Vec::new();
        loop {
            match live.next_event().expect("a frame") {
                Some(LiveEvent::Synchronized { .. }) => break,
                Some(event) => events.push(event),
                None => panic!("the socket closed during the replay"),
            }
        }
        live.close();
        events
    })
    .await;
    assert!(
        replayed.iter().any(|event| match event {
            LiveEvent::EntryUpsert { entry, .. } => entry.id == unseen_id,
            _ => false,
        }),
        "the gap is filled: {replayed:?}"
    );
    assert!(
        !replayed
            .iter()
            .any(|event| matches!(event, LiveEvent::Resync { .. })),
        "a small gap replays rather than resyncing: {replayed:?}"
    );

    // A conversation-filtered subscription, through the client's own parameter.
    let other: ConversationId = group_id.clone();
    let filtered = step(&client, other.clone(), |c, conversation| {
        let mut live = c
            .live(None, Some(&conversation))
            .expect("a filtered socket");
        let mut events = Vec::new();
        loop {
            match live.next_event().expect("a frame") {
                Some(LiveEvent::Synchronized { .. }) => break,
                Some(event) => events.push(event),
                None => panic!("the socket closed during the handshake"),
            }
        }
        live.close();
        events
    })
    .await;
    assert!(
        matches!(&filtered[..], [LiveEvent::Snapshot(snapshot), ..]
            if snapshot.entries.iter().all(|entry| entry.conversation_id == other)),
        "the snapshot is narrowed to the conversation: {filtered:?}"
    );

    // --- deleting the group, then shutting down -----------------------------
    step(&client, group_id.clone(), |c, id| {
        c.delete_conversation(&id)
    })
    .await
    .expect("the group goes");
    let err = step(&client, group_id, |c, id| c.conversation(&id))
        .await
        .expect_err("gone");
    assert!(err.is_not_found(), "{err:?}");

    step(&client, (), |c, ()| c.shutdown())
        .await
        .expect("acknowledged");

    // The listener stops accepting, so the client's next call fails in transport
    // rather than hanging: that is the difference between "it stopped" and "it
    // died", and it is what a supervising CLI has to report.
    let mut stopped = false;
    for _ in 0..200 {
        match step(&client, (), |c, ()| c.health()).await {
            Err(ClientError::Transport { .. }) => {
                stopped = true;
                break;
            }
            Err(other) => panic!("a shutdown should end in a transport failure, got {other:?}"),
            Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
        }
    }
    assert!(stopped, "the brain kept answering after a shutdown");

    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_client_reports_an_unknown_bot_as_not_found() {
    let brain = TestBrain::start().await;
    let client = BrainClient::new(&brain.base_url)
        .expect("a client")
        .with_token(brain.token());

    // The client builds every URL from a typed id or a handle, so this is what a
    // deleted bot looks like from the other side of the wire.
    let err = step(&client, "nobody", |c, name| c.bot(name))
        .await
        .expect_err("refused");
    assert!(err.is_not_found(), "{err:?}");
    assert_eq!(err.code(), Some("not_found"));

    brain.stop().await;
}
