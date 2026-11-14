//! The operational half of the API: requests, tasks, nodes, rules, settings,
//! backends and shutdown — over a real socket.

mod common;

use common::*;

use serde_json::{Value, json};

use swarmbot_contracts::id::{RuleId, TaskId};
use swarmbot_contracts::node::Surface;
use swarmbot_contracts::request::RuntimeRequest;

// --- runtime requests --------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_card_can_be_answered_and_answering_twice_is_a_conflict() {
    let brain = TestBrain::start().await;
    let client = brain.client();
    let lead = brain.bot("lead");
    let dm = lead.direct_conversation_id.clone();

    // Raised through the service, exactly as a tool call would: the API is only
    // the doorway an owner answers through.
    let mut request = RuntimeRequest::approval(
        lead.id.clone(),
        dm.clone(),
        Surface::Shell,
        "shell",
        "cargo test --workspace",
        Some("run the tests".into()),
    );
    let id = brain
        .swarm
        .approval
        .raise_async(&mut request)
        .expect("the card is raised");

    // It is pending, and it put a card in the transcript.
    let pending = array(get(&client, &brain.url("/api/requests")).await).await;
    let card = pending
        .iter()
        .find(|value| value["id"] == json!(id.to_string()))
        .unwrap_or_else(|| panic!("the card is not pending: {pending:?}"));
    assert_eq!(card["status"], json!("pending"));
    assert_eq!(card["kind"], json!("approval"));
    assert_eq!(card["surface"], json!("shell"));
    assert_eq!(card["target"], json!("cargo test --workspace"));
    assert!(card["card_entry_id"].is_string(), "{card}");
    let card_entry = card["card_entry_id"].as_str().expect("an entry id");
    assert!(
        brain
            .entries(&dm)
            .iter()
            .any(|entry| entry.id.as_str() == card_entry),
        "the card sits in the conversation"
    );

    // "Always allow" answers the card *and* writes a rule, so the same question
    // is never asked twice.
    let answered = body(
        post(
            &client,
            &brain.url(&format!("/api/requests/{id}/respond")),
            &json!({"decision": "allow_always", "answer": null}),
        )
        .await,
    )
    .await;
    assert_eq!(answered["status"], json!("answered"));
    assert_eq!(answered["decision"], json!("allow_always"));
    assert_eq!(answered["responded_by"]["kind"], json!("owner"));
    assert!(answered["resolved_at"].is_string(), "{answered}");

    let rules = array(get(&client, &brain.url("/api/rules")).await).await;
    assert_eq!(rules.len(), 1, "{rules:?}");
    assert_eq!(rules[0]["effect"], json!("allow"));
    assert_eq!(rules[0]["surface"], json!("shell"));
    assert_eq!(rules[0]["bot_scope_is_the_requesting_bot"], Value::Null);
    assert_eq!(
        rules[0]["scope"]["bot"],
        json!(lead.id.to_string()),
        "the rule is as narrow as the request: {rules:?}"
    );
    assert_eq!(
        rules[0]["command_prefix"],
        json!("cargo"),
        "the first word, not the whole command: {rules:?}"
    );

    // The card left the pending list, and the transcript's card entry followed.
    let pending = array(get(&client, &brain.url("/api/requests")).await).await;
    assert!(pending.is_empty(), "{pending:?}");

    // Answering twice is a 409, with the code that says why.
    let (status, code, message) = envelope(
        post(
            &client,
            &brain.url(&format!("/api/requests/{id}/respond")),
            &json!({"decision": "deny", "answer": null}),
        )
        .await,
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(code, "request_not_pending");
    assert!(message.contains(&id.to_string()), "{message}");

    // An unknown request is a 404.
    expect_error(
        post(
            &client,
            &brain.url("/api/requests/req_00000000000000000000000000000000/respond"),
            &json!({"decision": "allow_once", "answer": null}),
        )
        .await,
        404,
        "not_found",
    )
    .await;

    // A question is answered with JSON, not with a decision.
    let mut question = RuntimeRequest::question(
        lead.id.clone(),
        dm.clone(),
        "which port should the demo bind?",
        vec![],
    );
    let question_id = brain
        .swarm
        .approval
        .raise_async(&mut question)
        .expect("the question is raised");
    let answered = body(
        post(
            &client,
            &brain.url(&format!("/api/requests/{question_id}/respond")),
            &json!({"decision": null, "answer": {"port": 8080}}),
        )
        .await,
    )
    .await;
    assert_eq!(answered["answer"], json!({"port": 8080}));
    assert!(answered["decision"].is_null(), "{answered}");

    // Both answers woke the bot; let it finish before the test ends.
    brain.settle().await;
    brain.stop().await;
}

// --- approval rules ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rules_are_listed_created_and_deleted() {
    let brain = TestBrain::start().await;
    let client = brain.client();

    assert!(
        array(get(&client, &brain.url("/api/rules")).await)
            .await
            .is_empty()
    );

    // The shape the client crate builds: a bare global rule, no matcher.
    let rule = json!({
        "id": RuleId::new().to_string(),
        "scope": "global",
        "surface": "shell",
        "tool": null,
        "command_prefix": "cargo",
        "effect": "allow",
        "created_at": "2026-01-01T00:00:00Z",
    });
    let response = post(&client, &brain.url("/api/rules"), &rule).await;
    assert_eq!(response.status().as_u16(), 201);
    let created = body(response).await;
    assert_eq!(created["effect"], json!("allow"));
    assert_eq!(created["scope"], json!("global"));
    let id = id_of(&created);

    let rules = array(get(&client, &brain.url("/api/rules")).await).await;
    assert_eq!(rules.len(), 1);

    // A bot-scoped rule has to name a bot that exists.
    expect_error(
        post(
            &client,
            &brain.url("/api/rules"),
            &json!({
                "id": RuleId::new().to_string(),
                "scope": {"bot": "bot_00000000000000000000000000000000"},
                "effect": "deny",
                "surface": null, "tool": null, "command_prefix": null,
                "created_at": "2026-01-01T00:00:00Z",
            }),
        )
        .await,
        404,
        "not_found",
    )
    .await;

    let response = delete(&client, &brain.url(&format!("/api/rules/{id}"))).await;
    assert_eq!(response.status().as_u16(), 204);
    assert!(response.content_length().is_none_or(|len| len == 0));
    assert!(
        array(get(&client, &brain.url("/api/rules")).await)
            .await
            .is_empty()
    );

    // Deleting twice is a 404: the store's delete is a silent no-op, and an owner
    // who asked to unsave a rule should hear that there was nothing to unsave.
    expect_error(
        delete(&client, &brain.url(&format!("/api/rules/{id}"))).await,
        404,
        "not_found",
    )
    .await;

    brain.stop().await;
}

// --- tasks -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tasks_are_listed_with_their_steps_and_can_be_cancelled() {
    let brain = TestBrain::start().await;
    let client = brain.client();
    let lead = brain.bot("lead");
    let dm = lead.direct_conversation_id.clone();

    // Take the runner away first, so the task stays in a cancellable state
    // instead of being finished by the bot before the test can look at it. The
    // delivery parks; that is the real "bot has no runner yet" path.
    let handle = brain
        .swarm
        .runners
        .unregister(&lead.id)
        .expect("the lead has a runner");
    handle.shutdown();

    let task = brain
        .swarm
        .tasks
        .start(
            &lead,
            dm.clone(),
            "audit the diff",
            "look for risky commands",
        )
        .expect("the task starts");
    let id = task.id.to_string();

    // Listed among all tasks …
    let tasks = array(get(&client, &brain.url("/api/tasks")).await).await;
    let row = tasks
        .iter()
        .find(|value| value["id"] == json!(id))
        .unwrap_or_else(|| panic!("the task is not listed: {tasks:?}"));
    assert_eq!(row["title"], json!("audit the diff"));
    assert_eq!(row["status"], json!("running"));
    assert_eq!(row["owner"], json!(lead.id.to_string()));
    assert_eq!(row["origin"], json!(dm.to_string()));

    // … and by its bot.
    let mine = array(get(&client, &brain.url(&format!("/api/tasks?bot={}", lead.id))).await).await;
    assert!(
        mine.iter().any(|value| value["id"] == json!(id)),
        "{mine:?}"
    );
    // A handle works too.
    let by_handle = array(get(&client, &brain.url("/api/tasks?bot=lead")).await).await;
    assert!(by_handle.iter().any(|value| value["id"] == json!(id)));
    // Another bot's filter does not show it.
    let others = array(get(&client, &brain.url("/api/tasks?bot=scout")).await).await;
    assert!(
        !others.iter().any(|value| value["id"] == json!(id)),
        "{others:?}"
    );

    // The detail carries the steps a task pane renders.
    let detail = body(get(&client, &brain.url(&format!("/api/tasks/{id}"))).await).await;
    assert_eq!(detail["task"]["id"], json!(id));
    let steps = detail["steps"].as_array().expect("steps");
    assert!(
        !steps.is_empty(),
        "the task recorded its own start: {detail}"
    );
    assert_eq!(steps[0]["task_id"], json!(id));
    assert_eq!(steps[0]["seq"], json!(1));
    assert!(steps[0]["kind"].is_string(), "{steps:?}");
    assert_eq!(detail.as_object().expect("an object").len(), 2);

    // Cancel, as the owner: the bot is told and the conversation gets a line.
    let cancelled =
        body(post_empty(&client, &brain.url(&format!("/api/tasks/{id}/cancel"))).await).await;
    assert_eq!(cancelled["id"], json!(id));
    assert_eq!(cancelled["status"], json!("cancelled"));
    assert!(cancelled["ended_at"].is_string(), "{cancelled}");
    assert!(
        brain.entries(&dm).iter().any(|entry| entry
            .kind
            .text()
            .is_some_and(|text| text.contains("owner cancelled"))),
        "the origin conversation is told"
    );

    // Cancelling again answers with the same terminal task, not an error.
    let again =
        body(post_empty(&client, &brain.url(&format!("/api/tasks/{id}/cancel"))).await).await;
    assert_eq!(again["status"], json!("cancelled"));

    // An unknown task is a 404 on every verb.
    let missing = TaskId::new().to_string();
    expect_error(
        get(&client, &brain.url(&format!("/api/tasks/{missing}"))).await,
        404,
        "not_found",
    )
    .await;
    expect_error(
        post_empty(&client, &brain.url(&format!("/api/tasks/{missing}/cancel"))).await,
        404,
        "not_found",
    )
    .await;

    brain.stop().await;
}

// --- nodes -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_local_node_is_listed_and_a_pairing_code_is_minted() {
    let brain = TestBrain::start().await;
    let client = brain.client();

    let nodes = array(get(&client, &brain.url("/api/nodes")).await).await;
    let local = nodes
        .iter()
        .find(|node| node["id"] == json!("nod_local"))
        .unwrap_or_else(|| panic!("the local node is missing: {nodes:?}"));
    assert_eq!(local["kind"], json!("local"));
    assert_eq!(local["online"], json!(true));
    assert_eq!(local["revoked"], json!(false));
    let roots = local["roots"].as_array().expect("roots");
    assert_eq!(roots.len(), 1, "the workspaces root: {roots:?}");
    assert_eq!(roots[0]["id"], json!("work"));
    assert_eq!(roots[0]["mode"], json!("read_write"));

    // A pairing code: `ABCD-EFGH`, an expiry in the future, no node yet.
    let response = post_empty(&client, &brain.url("/api/nodes/pairing-codes")).await;
    assert_eq!(response.status().as_u16(), 200);
    let value = body(response).await;
    let code = value["code"].as_str().expect("a code").to_string();
    assert_eq!(code.len(), 9, "{code}");
    let (head, tail) = code.split_once('-').expect("ABCD-EFGH");
    assert_eq!(head.len(), 4, "{code}");
    assert_eq!(tail.len(), 4, "{code}");
    assert!(
        code.chars()
            .filter(|c| *c != '-')
            .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase()),
        "{code}"
    );
    assert!(value["node_id"].is_null(), "{value}");
    let expires_at = value["expires_at"].as_str().expect("an rfc3339 expiry");
    let expiry: chrono::DateTime<chrono::Utc> = chrono::DateTime::parse_from_rfc3339(expires_at)
        .expect("rfc3339")
        .into();
    assert!(
        expiry > chrono::Utc::now(),
        "the code has not expired before it was handed out: {expires_at}"
    );
    assert!(
        expiry - chrono::Utc::now() <= chrono::Duration::minutes(10),
        "and it lives ten minutes at most: {expires_at}"
    );

    // Two codes are not the same code.
    let second = body(post_empty(&client, &brain.url("/api/nodes/pairing-codes")).await).await;
    assert_ne!(second["code"], json!(code));

    // The plain code is never what the store holds.
    let stored = brain
        .swarm
        .store
        .expired_pairings(chrono::Utc::now() + chrono::Duration::hours(1))
        .expect("the pairing rows read");
    assert_eq!(stored.len(), 2, "{stored:?}");
    assert!(
        stored
            .iter()
            .all(|record| !record.code_hash.contains(&code))
    );
    assert!(stored.iter().all(|record| record.code_hash.len() == 64));

    // The local node cannot be revoked: it is this process.
    let message = expect_error(
        delete(&client, &brain.url("/api/nodes/nod_local")).await,
        409,
        "conflict",
    )
    .await;
    assert!(message.contains("local node"), "{message}");
    // An unknown node is a 404.
    expect_error(
        delete(
            &client,
            &brain.url("/api/nodes/nod_00000000000000000000000000000000"),
        )
        .await,
        404,
        "not_found",
    )
    .await;

    // The gateway is real now (see tests/node_gateway.rs for the whole flow);
    // what belongs here is that a *failed* pairing does not burn the code. A node
    // whose key was mangled in transit must be able to try again with the same
    // code, or its operator has to go and mint another one for no reason.
    let response = post(
        &TestBrain::client_with(""),
        &brain.url("/node/pair"),
        &json!({"code": code, "publicKey": "not a key", "hostname": "h", "platform": "linux"}),
    )
    .await;
    let message = expect_error(response, 400, "invalid_input").await;
    assert!(message.contains("publicKey"), "{message}");

    // The same code is still live, so a code that was never minted is the only
    // thing that answers "not valid here".
    let response = post(
        &TestBrain::client_with(""),
        &brain.url("/node/pair"),
        &json!({"code": "QQQQ-QQQQ", "publicKey": "not a key", "hostname": "h", "platform": "linux"}),
    )
    .await;
    let message = expect_error(response, 409, "conflict").await;
    assert!(message.contains("not valid here"), "{message}");

    brain.stop().await;
}

// --- settings ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_merge_one_key_at_a_time() {
    let brain = TestBrain::start().await;
    let client = brain.client();

    let defaults = body(get(&client, &brain.url("/api/settings")).await).await;
    assert_eq!(defaults["bot_messaging"], json!(true));
    assert_eq!(defaults["max_hops"], json!(8));
    assert_eq!(defaults["max_tool_rounds"], json!(32));
    assert_eq!(defaults["compact_at_fraction"], json!(0.6));
    assert_eq!(defaults["safety"]["default"], json!("ask"));

    let patched = body(
        patch(
            &client,
            &brain.url("/api/settings"),
            &json!({"max_hops": 3}),
        )
        .await,
    )
    .await;
    assert_eq!(patched["max_hops"], json!(3));
    // Everything else is untouched, including the nested policy map.
    for key in [
        "bot_messaging",
        "max_tool_rounds",
        "compact_at_fraction",
        "approval_expiry_secs",
        "safety",
        "auto_approve_read_only",
        "transcript_mirror",
    ] {
        assert_eq!(patched[key], defaults[key], "{key} changed");
    }
    // The change is what the swarm now runs with.
    assert_eq!(brain.swarm.settings.get().max_hops, 3);

    // Several keys at once.
    let patched = body(
        patch(
            &client,
            &brain.url("/api/settings"),
            &json!({"bot_messaging": false, "max_tool_rounds": 4}),
        )
        .await,
    )
    .await;
    assert_eq!(patched["bot_messaging"], json!(false));
    assert_eq!(patched["max_tool_rounds"], json!(4));
    assert_eq!(patched["max_hops"], json!(3), "the earlier change stayed");

    // Invalid values are refused, and nothing is written.
    for (patch, wanted) in [
        (json!({"max_hops": 0}), "max_hops"),
        (json!({"max_tool_rounds": 0}), "max_tool_rounds"),
        (json!({"compact_at_fraction": 2.0}), "compact_at_fraction"),
        (json!({"approval_expiry_secs": 0}), "approval_expiry_secs"),
    ] {
        let message = expect_error(
            patch_req(&client, &brain.url("/api/settings"), &patch).await,
            400,
            "invalid_input",
        )
        .await;
        assert!(
            message.contains(wanted),
            "{message} should mention {wanted}"
        );
    }
    assert_eq!(
        brain.swarm.settings.get().max_hops,
        3,
        "nothing was written"
    );

    // An unknown key names the ones that exist.
    let message = expect_error(
        patch_req(
            &client,
            &brain.url("/api/settings"),
            &json!({"max_hoops": 3}),
        )
        .await,
        400,
        "invalid_input",
    )
    .await;
    assert!(message.contains("unknown setting"), "{message}");
    assert!(message.contains("max_hops"), "{message}");

    // A body that is not an object is a 400 too.
    expect_error(
        patch_req(&client, &brain.url("/api/settings"), &json!([])).await,
        400,
        "invalid_input",
    )
    .await;

    // A token budget travels as a number or as `null`, and `null` is the only
    // spelling of "no cap": a `0` is a 400, never a synonym for unlimited.
    assert_eq!(
        defaults["max_tokens_per_turn"],
        json!(null),
        "no cap by default"
    );
    assert_eq!(defaults["max_tokens_per_day"], json!(null));
    let patched = body(
        patch(
            &client,
            &brain.url("/api/settings"),
            &json!({"max_tokens_per_day": 250_000}),
        )
        .await,
    )
    .await;
    assert_eq!(patched["max_tokens_per_day"], json!(250_000));
    assert_eq!(
        patched["max_tokens_per_turn"],
        json!(null),
        "setting one cap leaves the other off"
    );
    assert_eq!(
        brain.swarm.settings.get().max_tokens_per_day,
        Some(250_000),
        "the runner reads the cap back through the same handle"
    );
    let cleared = body(
        patch(
            &client,
            &brain.url("/api/settings"),
            &json!({"max_tokens_per_day": null}),
        )
        .await,
    )
    .await;
    assert_eq!(cleared["max_tokens_per_day"], json!(null), "null clears it");
    for key in ["max_tokens_per_turn", "max_tokens_per_day"] {
        let message = expect_error(
            patch_req(&client, &brain.url("/api/settings"), &json!({ key: 0 })).await,
            400,
            "invalid_input",
        )
        .await;
        assert!(
            message.contains(key) && message.contains("null"),
            "{message} should say {key} is at least 1 or null"
        );
    }

    brain.stop().await;
}

/// PATCH is not in the helper set above because only settings use it twice.
async fn patch_req(client: &reqwest::Client, url: &str, body: &Value) -> reqwest::Response {
    patch(client, url, body).await
}

// --- backends ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backends_are_listed_redacted_and_probed() {
    let brain = TestBrain::start().await;
    let client = brain.client();

    let response = get(&client, &brain.url("/api/backends")).await;
    assert_eq!(response.status().as_u16(), 200);
    let rendered = body(response).await;
    let text = rendered.to_string();

    // The one invariant that matters: no key, and no field that could hold one.
    assert!(!text.contains("api_key\""), "{text}");
    assert!(!text.contains("sk-"), "{text}");
    assert!(!text.contains("extra_headers"), "{text}");
    let backends = rendered.as_array().expect("an array");
    assert!(!backends.is_empty());
    let fake = backends
        .iter()
        .find(|backend| backend["id"] == json!("fake"))
        .unwrap_or_else(|| panic!("the fake backend is always listed: {rendered}"));
    assert_eq!(fake["kind"], json!("fake"));
    assert!(fake["api_key_env"].is_null(), "{fake}");
    // The fields the client crate's `BackendView` decodes are all present.
    for key in ["id", "kind", "base_url", "model", "api_key_env"] {
        assert!(fake.get(key).is_some(), "{key} is missing from {fake}");
    }

    // Probing is honest and does not spend a model call.
    let before = brain.fake().request_count();
    let probes = array(get(&client, &brain.url("/api/backends/probe")).await).await;
    let fake = probes
        .iter()
        .find(|probe| probe["backend"] == json!("fake"))
        .unwrap_or_else(|| panic!("the fake backend is probed: {probes:?}"));
    assert_eq!(fake["ok"], json!(true), "{fake}");
    assert!(fake["detail"].is_string(), "{fake}");
    for key in ["backend", "ok", "detail", "model", "version"] {
        assert!(fake.get(key).is_some(), "{key} is missing from {fake}");
    }
    assert_eq!(
        brain.fake().request_count(),
        before,
        "a probe must not call the model"
    );

    brain.stop().await;
}

// --- shutdown ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shutdown_request_answers_and_then_the_listener_stops() {
    let brain = TestBrain::start().await;
    let client = brain.client();
    let addr = brain.addr;

    // It answers first …
    let value = body(post_empty(&client, &brain.url("/api/shutdown")).await).await;
    assert_eq!(value["ok"], json!(true));
    // … and the request that asked for it completed rather than being cut off.
    assert_eq!(
        body(get(&client, &brain.url("/api/health")).await).await["ok"],
        json!(true)
    );

    // Then the socket stops accepting. The grace period is 100 ms, so this polls
    // rather than sleeping on a guess.
    let mut refused = false;
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_err() {
            refused = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        refused,
        "{addr} is still accepting connections after a shutdown"
    );

    // `stop` now only has to shut the swarm down: the listener already finished.
    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unauthenticated_shutdown_is_refused_and_changes_nothing() {
    let brain = TestBrain::start().await;
    let anonymous = TestBrain::client_with("");
    expect_error(
        post_empty(&anonymous, &brain.url("/api/shutdown")).await,
        401,
        "unauthorized",
    )
    .await;
    assert!(!brain.state.shutdown_requested(), "the brain is still up");
    assert_eq!(
        body(get(&brain.client(), &brain.url("/api/health")).await).await["ok"],
        json!(true)
    );
    brain.stop().await;
}

// --- routines ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn routines_are_created_listed_paused_resumed_and_deleted() {
    let brain = TestBrain::start().await;
    let client = brain.client();

    // Create routine
    let create_payload = json!({
        "bot": "lead",
        "name": "Hourly Sync",
        "prompt": "Summarize team activity",
        "schedule": {
            "kind": "interval",
            "every_secs": 3600
        }
    });
    let res = post(&client, &brain.url("/api/routines"), &create_payload).await;
    assert_eq!(res.status(), 201);
    let routine = body(res).await;
    assert_eq!(routine["name"], "Hourly Sync");
    assert_eq!(routine["status"], "active");
    let routine_id = routine["id"].as_str().expect("id string");

    // List routines
    let list = array(get(&client, &brain.url("/api/routines")).await).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], routine_id);

    // List routines filtered by bot
    let lead_routines = array(get(&client, &brain.url("/api/routines?bot=lead")).await).await;
    assert_eq!(lead_routines.len(), 1);
    let scout_routines = array(get(&client, &brain.url("/api/routines?bot=scout")).await).await;
    assert_eq!(scout_routines.len(), 0);

    // Get routine
    let fetched =
        body(get(&client, &brain.url(&format!("/api/routines/{routine_id}"))).await).await;
    assert_eq!(fetched["id"], routine_id);
    assert_eq!(fetched["name"], "Hourly Sync");

    // Pause routine
    let paused = body(
        post_empty(
            &client,
            &brain.url(&format!("/api/routines/{routine_id}/pause")),
        )
        .await,
    )
    .await;
    assert_eq!(paused["status"], "paused");

    // Resume routine
    let resumed = body(
        post_empty(
            &client,
            &brain.url(&format!("/api/routines/{routine_id}/resume")),
        )
        .await,
    )
    .await;
    assert_eq!(resumed["status"], "active");

    // List runs (empty)
    let runs = array(
        get(
            &client,
            &brain.url(&format!("/api/routines/{routine_id}/runs")),
        )
        .await,
    )
    .await;
    assert_eq!(runs.len(), 0);

    // Delete routine
    let del_res = delete(&client, &brain.url(&format!("/api/routines/{routine_id}"))).await;
    assert_eq!(del_res.status(), 204);

    // Get after delete returns 404
    expect_error(
        get(&client, &brain.url(&format!("/api/routines/{routine_id}"))).await,
        404,
        "not_found",
    )
    .await;

    brain.stop().await;
}
