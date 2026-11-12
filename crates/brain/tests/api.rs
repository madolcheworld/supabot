//! The REST API, end to end, over a real socket.
//!
//! These are the assertions a client crate is written against: the statuses, the
//! envelope, the field names. Where the API table in the crate's task says
//! "409" or "201", a test here fails if it drifts.

mod common;

use common::*;

use serde_json::{Value, json};

use swarmbot_contracts::id::{BotId, ConversationId, EntryId};
use swarmbot_llm::ScriptedTurn;

// --- health and auth ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_reports_the_version_and_the_epoch() {
    let brain = TestBrain::start().await;
    let client = brain.client();

    let value = body(get(&client, &brain.url("/api/health")).await).await;
    assert_eq!(value["ok"], json!(true));
    assert_eq!(value["version"], json!(env!("CARGO_PKG_VERSION")));
    assert_eq!(
        value["epoch"],
        json!(brain.state.epoch()),
        "the epoch is the live log's"
    );
    assert!(value["uptime_secs"].as_u64().is_some(), "{value}");
    assert_eq!(value.as_object().expect("an object").len(), 4, "{value}");

    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_route_refuses_an_anonymous_request_with_the_envelope() {
    let brain = TestBrain::start().await;
    let anonymous = TestBrain::client_with("");

    // Health is behind the gate too: an unauthenticated probe should not learn
    // that a brain exists, let alone what version it runs.
    let message = expect_error(
        get(&anonymous, &brain.url("/api/health")).await,
        401,
        "unauthorized",
    )
    .await;
    assert!(message.contains("Bearer"), "{message}");

    for path in [
        "/api/bots",
        "/api/conversations",
        "/api/requests",
        "/api/tasks",
        "/api/routines",
        "/api/nodes",
        "/api/rules",
        "/api/settings",
        "/api/backends",
        "/api/backends/probe",
    ] {
        let response = get(&anonymous, &brain.url(path)).await;
        expect_error(response, 401, "unauthorized").await;
    }

    // A wrong token is refused with the same envelope, and says which header it
    // wanted rather than echoing what it got.
    let wrong = TestBrain::client_with("not-the-token");
    let message = expect_error(
        get(&wrong, &brain.url("/api/health")).await,
        401,
        "unauthorized",
    )
    .await;
    assert!(!message.contains("not-the-token"), "{message}");

    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_printed_one_time_token_authenticates() {
    // A durable brain is the one that mints a token file and prints a URL, so
    // this is the path an operator actually walks. The data dir outlives the
    // first brain, because the test boots the same one twice.
    let dir = tempfile::tempdir().expect("a temp dir");
    let brain = TestBrain::start_durable_in(dir.path()).await;
    let login_url = brain.login_url.clone().expect("a first-boot login URL");
    assert!(login_url.contains("#token="), "{login_url}");
    assert!(
        login_url.starts_with("http://127.0.0.1:"),
        "the URL is dialable: {login_url}"
    );

    let token = brain.printed_token();
    let anonymous = TestBrain::client_with("");
    let printed = TestBrain::client_with(&token);

    // The token from the URL works as a bearer …
    let value = body(get(&printed, &brain.url("/api/health")).await).await;
    assert_eq!(value["ok"], json!(true));
    // … and the token file holds exactly it.
    let on_disk = std::fs::read_to_string(dir.path().join("owner-token")).expect("read");
    assert_eq!(on_disk.trim(), token);
    assert_eq!(token, brain.token);

    // Without it, 401.
    expect_error(
        get(&anonymous, &brain.url("/api/health")).await,
        401,
        "unauthorized",
    )
    .await;

    brain.stop().await;

    // A second boot of the same data dir reuses the token and prints nothing:
    // that is what makes the URL one-time.
    let reopened = TestBrain::start_durable_in(dir.path()).await;
    assert!(reopened.login_url.is_none(), "no URL on a later boot");
    assert_eq!(
        reopened.token, token,
        "the owner token survives a restart, so a client that kept it still works"
    );
    let value = body(get(&reopened.client(), &reopened.url("/api/health")).await).await;
    assert_eq!(value["ok"], json!(true));
    reopened.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_exchange_trades_the_token_for_a_cookie() {
    let brain = TestBrain::start_durable().await;
    let anonymous = TestBrain::client_with("");

    // A wrong token is refused and sets no cookie.
    let response = post(
        &anonymous,
        &brain.url("/api/session"),
        &json!({"token": "wrong"}),
    )
    .await;
    assert!(response.headers().get("set-cookie").is_none());
    expect_error(response, 401, "unauthorized").await;

    let response = post(
        &anonymous,
        &brain.url("/api/session"),
        &json!({"token": brain.token()}),
    )
    .await;
    assert_eq!(response.status().as_u16(), 200);
    // The header is read before the body consumes the response.
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .expect("a session cookie")
        .to_str()
        .expect("ascii")
        .to_string();
    for flag in ["HttpOnly", "SameSite=Lax", "Path=/"] {
        assert!(set_cookie.contains(flag), "{set_cookie} is missing {flag}");
    }
    assert_eq!(body(response).await["ok"], json!(true));

    let pair = set_cookie.split(';').next().expect("a pair").to_string();
    assert!(pair.starts_with("swarmbot_session="), "{pair}");
    // The cookie is not the token: a leaked cookie cannot be replayed as a
    // bearer credential against the REST API by something that only reads logs.
    assert!(!pair.contains(brain.token()), "{pair}");

    // The cookie alone authorizes a request.
    let with_cookie = TestBrain::client_with("");
    let response = with_cookie
        .get(brain.url("/api/health"))
        .header("cookie", &pair)
        .send()
        .await
        .expect("the request goes out");
    assert_eq!(body(response).await["ok"], json!(true));

    // And the token still works after the exchange: the client crate keeps both,
    // because a WebSocket upgrade cannot read a cookie jar.
    let bearer = TestBrain::client_with(brain.token());
    let response = bearer
        .get(brain.url("/api/health"))
        .header("cookie", &pair)
        .send()
        .await
        .expect("the request goes out");
    assert_eq!(body(response).await["ok"], json!(true));

    // A made-up cookie does not.
    let forged = with_cookie
        .get(brain.url("/api/health"))
        .header("cookie", "swarmbot_session=forged")
        .send()
        .await
        .expect("the request goes out");
    expect_error(forged, 401, "unauthorized").await;

    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_browser_preflight_is_answered_without_a_credential() {
    let brain = TestBrain::start().await;
    let anonymous = TestBrain::client_with("");
    // A page from the owner's own machine on another port (a dev UI, say).
    let own_origin = format!("http://127.0.0.1:{}", brain.addr.port() + 1);

    let response = anonymous
        .request(reqwest::Method::OPTIONS, brain.url("/api/bots"))
        .header("origin", &own_origin)
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "authorization,content-type",
        )
        .send()
        .await
        .expect("the preflight goes out");
    assert_eq!(response.status().as_u16(), 200);
    let headers = response.headers().clone();
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some(own_origin.as_str()),
        "the origin is echoed, never `*`"
    );
    assert_eq!(
        headers
            .get("access-control-allow-credentials")
            .and_then(|v| v.to_str().ok()),
        Some("true"),
        "the session cookie has to be allowed to ride along"
    );
    assert!(
        headers
            .get("access-control-allow-headers")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("authorization")),
        "{headers:?}"
    );

    // A page from somewhere else gets no permission at all. Note the request is
    // still answered (CORS is enforced by the browser, not by us); what matters
    // is that it is not *allowed*.
    let response = anonymous
        .request(reqwest::Method::OPTIONS, brain.url("/api/bots"))
        .header("origin", "http://evil.example")
        .header("access-control-request-method", "POST")
        .send()
        .await
        .expect("the preflight goes out");
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none(),
        "{:?}",
        response.headers()
    );

    // And a real (non-preflight) cross-origin call still has to authenticate.
    let response = anonymous
        .get(brain.url("/api/health"))
        .header("origin", &own_origin)
        .send()
        .await
        .expect("the request goes out");
    expect_error(response, 401, "unauthorized").await;

    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_path_answers_401_anonymously_and_404_for_the_owner() {
    let brain = TestBrain::start().await;
    let anonymous = TestBrain::client_with("");
    let client = brain.client();

    // An anonymous probe learns nothing about which paths exist.
    expect_error(
        get(&anonymous, &brain.url("/api/nope")).await,
        401,
        "unauthorized",
    )
    .await;
    let message = expect_error(
        get(&client, &brain.url("/api/nope")).await,
        404,
        "not_found",
    )
    .await;
    assert!(message.contains("/api/nope"), "{message}");

    // A known path with the wrong method still carries the envelope.
    let response = client
        .put(brain.url("/api/bots"))
        .json(&json!({}))
        .send()
        .await
        .expect("the request goes out");
    let (status, code, _) = envelope(response).await;
    assert_eq!(status, 405);
    assert_eq!(code, "invalid_input");

    brain.stop().await;
}

// --- bots --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bots_create_list_get_patch_and_delete() {
    let brain = TestBrain::start().await;
    let client = brain.client();

    // Create: 201 and the stored bot.
    let response = post(
        &client,
        &brain.url("/api/bots"),
        &json!({
            "name": "@Reviewer Two",
            "label": "Reviewer",
            "instructions": "read the diff",
            "backend": "fake",
            "color": "#123456",
        }),
    )
    .await;
    assert_eq!(response.status().as_u16(), 201);
    let created = body(response).await;
    assert_eq!(
        created["name"],
        json!("reviewer-two"),
        "the handle is normalized"
    );
    assert_eq!(created["label"], json!("Reviewer"));
    assert_eq!(created["backend"], json!("fake"));
    assert_eq!(created["color"], json!("#123456"));
    let id = id_of(&created);
    assert!(id.starts_with("bot_"), "{id}");
    let dm = created["direct_conversation_id"].as_str().expect("a DM");

    // The bot has a runner at once: without one it could be messaged but never
    // answer, which is indistinguishable from a hung brain.
    let bot_id: BotId = id.parse().expect("a bot id");
    assert!(brain.swarm.runners.get(&bot_id).is_some(), "no runner");
    let bot_ws = brain.swarm.workspaces_root().join("reviewer-two");
    assert!(
        bot_ws.join("MEMORY.md").is_file(),
        "bot workspace initialized"
    );
    assert!(bot_ws.join(".spool").is_dir(), "bot spool initialized");

    // List: the demo team plus this one.
    let bots = array(get(&client, &brain.url("/api/bots")).await).await;
    assert_eq!(bots.len(), 5, "{bots:?}");
    assert!(bots.iter().any(|bot| bot["id"] == created["id"]));

    // Get by id, by handle and by label.
    for key in [&id, "reviewer-two", "@reviewer-two", "Reviewer"] {
        let response = get(&client, &brain.url(&format!("/api/bots/{key}"))).await;
        assert_eq!(response.status().as_u16(), 200, "{key}");
        assert_eq!(id_of(&body(response).await), id, "{key}");
    }

    // Patch: only what is named changes.
    let response = patch(
        &client,
        &brain.url(&format!("/api/bots/{id}")),
        &json!({"label": "Senior Reviewer", "model": "fake-2"}),
    )
    .await;
    let patched = body(response).await;
    assert_eq!(patched["label"], json!("Senior Reviewer"));
    assert_eq!(patched["model"], json!("fake-2"));
    assert_eq!(
        patched["instructions"], created["instructions"],
        "untouched"
    );
    assert_eq!(patched["color"], created["color"], "untouched");

    // A handle is an address: renaming is refused, not silently done.
    let message = expect_error(
        patch(
            &client,
            &brain.url(&format!("/api/bots/{id}")),
            &json!({"name": "someone-else"}),
        )
        .await,
        409,
        "conflict",
    )
    .await;
    assert!(message.contains("handle"), "{message}");

    // Enabling and disabling go through the same PATCH.
    let disabled = body(
        patch(
            &client,
            &brain.url(&format!("/api/bots/{id}")),
            &json!({"enabled": false}),
        )
        .await,
    )
    .await;
    assert_eq!(disabled["status"]["state"], json!("disabled"));
    let enabled = body(
        patch(
            &client,
            &brain.url(&format!("/api/bots/{id}")),
            &json!({"enabled": true}),
        )
        .await,
    )
    .await;
    assert_eq!(enabled["status"]["state"], json!("idle"));
    assert!(
        brain.swarm.runners.get(&bot_id).is_some(),
        "re-enabling brings the runner back"
    );

    // Delete: 204 with no body, and the bot is gone.
    let response = delete(&client, &brain.url(&format!("/api/bots/{id}"))).await;
    assert_eq!(response.status().as_u16(), 204);
    assert!(
        response.content_length().is_none_or(|len| len == 0),
        "a 204 carries no body"
    );
    expect_error(
        get(&client, &brain.url(&format!("/api/bots/{id}"))).await,
        404,
        "not_found",
    )
    .await;
    assert!(
        brain.swarm.runners.get(&bot_id).is_none(),
        "the runner is gone"
    );
    // The DM stays: deleting a transcript would be a lie about what happened.
    assert!(
        brain
            .swarm
            .conversations
            .get(&dm.parse::<ConversationId>().expect("a conversation id"))
            .is_ok()
    );

    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bots_refuse_a_duplicate_handle_and_an_unknown_backend() {
    let brain = TestBrain::start().await;
    let client = brain.client();

    let message = expect_error(
        post(
            &client,
            &brain.url("/api/bots"),
            &json!({"name": "lead", "label": "Second Lead", "instructions": "", "backend": "fake"}),
        )
        .await,
        409,
        "conflict",
    )
    .await;
    assert!(message.contains("@lead"), "{message}");

    // An unknown id is a 404 whatever shape it has.
    for key in [
        "bot_00000000000000000000000000000000",
        "nobody",
        "bot_not-even-an-id",
    ] {
        expect_error(
            get(&client, &brain.url(&format!("/api/bots/{key}"))).await,
            404,
            "not_found",
        )
        .await;
    }

    // A backend that cannot run a bot is a 400, not a 502: nothing is down, the
    // request names something that was never configured.
    let message = expect_error(
        post(
            &client,
            &brain.url("/api/bots"),
            &json!({"name": "ghost", "backend": "inferhub"}),
        )
        .await,
        400,
        "invalid_input",
    )
    .await;
    assert!(message.contains("inferhub"), "{message}");
    assert!(
        message.contains("fake"),
        "the known backends are named: {message}"
    );

    // A node that never paired is a 404.
    expect_error(
        post(
            &client,
            &brain.url("/api/bots"),
            &json!({"name": "ghost", "backend": "fake", "node": "nod_00000000000000000000000000000000"}),
        )
        .await,
        404,
        "not_found",
    )
    .await;

    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bot_session_can_be_reset_or_folded() {
    let brain = TestBrain::start().await;
    let client = brain.client();

    // `new` folds nothing and always answers.
    let value = body(
        post(
            &client,
            &brain.url("/api/bots/lead/session"),
            &json!({"action": "new"}),
        )
        .await,
    )
    .await;
    assert_eq!(value["folded"], json!(0));

    // Give the bot some history, then compact it.
    brain.script_text("noted");
    let dm = brain.bot("lead").direct_conversation_id;
    let _ = body(
        post(
            &client,
            &brain.url(&format!("/api/conversations/{dm}/messages")),
            &json!({"text": "remember this", "addressed": []}),
        )
        .await,
    )
    .await;
    brain.settle().await;

    let value = body(
        post(
            &client,
            &brain.url("/api/bots/lead/session"),
            &json!({"action": "compact"}),
        )
        .await,
    )
    .await;
    assert!(
        value["folded"].as_u64().is_some(),
        "compact reports a count: {value}"
    );

    // An unknown action is a 400 at the extractor.
    expect_error(
        post(
            &client,
            &brain.url("/api/bots/lead/session"),
            &json!({"action": "wipe"}),
        )
        .await,
        400,
        "invalid_input",
    )
    .await;

    brain.stop().await;
}

// --- conversations -----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn groups_are_created_renamed_paged_and_deleted() {
    let brain = TestBrain::start().await;
    let client = brain.client();
    let lead = brain.bot_id("lead");
    let scout = brain.bot_id("scout");

    // Create a group of two, letting the lead default to the first member.
    let response = post(
        &client,
        &brain.url("/api/conversations"),
        &json!({"title": "Pair", "members": [lead.to_string(), scout.to_string()]}),
    )
    .await;
    assert_eq!(response.status().as_u16(), 201);
    let group = body(response).await;
    assert_eq!(group["title"], json!("Pair"));
    assert_eq!(group["kind"]["kind"], json!("group"));
    assert_eq!(group["kind"]["lead"], json!(lead.to_string()));
    assert_eq!(
        group["kind"]["members"].as_array().expect("members").len(),
        2
    );
    let group_id = id_of(&group);

    // Handles are accepted in place of ids, so a caller that just listed the
    // roster does not need a second lookup.
    let by_handle = body(
        post(
            &client,
            &brain.url("/api/conversations"),
            &json!({"title": "By handle", "members": ["lead", "dev"]}),
        )
        .await,
    )
    .await;
    assert_eq!(
        by_handle["kind"]["members"]
            .as_array()
            .expect("members")
            .len(),
        2
    );

    // A title is optional: the group is named after its members.
    let untitled = body(
        post(
            &client,
            &brain.url("/api/conversations"),
            &json!({"members": ["lead", "sentinel"]}),
        )
        .await,
    )
    .await;
    assert_eq!(untitled["title"], json!("@lead, @sentinel"));

    // Rename.
    let renamed = body(
        patch(
            &client,
            &brain.url(&format!("/api/conversations/{group_id}")),
            &json!({"title": "Pair, renamed"}),
        )
        .await,
    )
    .await;
    assert_eq!(renamed["title"], json!("Pair, renamed"));
    expect_error(
        patch(
            &client,
            &brain.url(&format!("/api/conversations/{group_id}")),
            &json!({"title": "   "}),
        )
        .await,
        400,
        "invalid_input",
    )
    .await;

    // A group of one is refused by the domain, and the API passes the 400 on.
    expect_error(
        post(
            &client,
            &brain.url("/api/conversations"),
            &json!({"title": "Solo", "members": [lead.to_string()]}),
        )
        .await,
        400,
        "invalid_input",
    )
    .await;
    // So is a member that does not exist.
    expect_error(
        post(
            &client,
            &brain.url("/api/conversations"),
            &json!({"title": "Ghost", "members": ["lead", "nobody"]}),
        )
        .await,
        404,
        "not_found",
    )
    .await;

    // List.
    let conversations = array(get(&client, &brain.url("/api/conversations")).await).await;
    assert!(conversations.iter().any(|c| c["id"] == group["id"]));
    // Four DMs, the demo team's group, and the three created here.
    assert!(
        conversations.len() >= 7,
        "{} conversations",
        conversations.len()
    );

    // Get by id; a malformed id is a 404.
    assert_eq!(
        id_of(
            &body(
                get(
                    &client,
                    &brain.url(&format!("/api/conversations/{group_id}"))
                )
                .await
            )
            .await
        ),
        group_id
    );
    expect_error(
        get(&client, &brain.url("/api/conversations/cnv_nope")).await,
        404,
        "not_found",
    )
    .await;

    // Delete a group: 204, and it is gone.
    let response = delete(
        &client,
        &brain.url(&format!("/api/conversations/{group_id}")),
    )
    .await;
    assert_eq!(response.status().as_u16(), 204);
    assert!(response.content_length().is_none_or(|len| len == 0));
    expect_error(
        get(
            &client,
            &brain.url(&format!("/api/conversations/{group_id}")),
        )
        .await,
        404,
        "not_found",
    )
    .await;

    // A DM cannot be deleted: that is a 409, not a 404 and not a success.
    let dm = brain.bot("lead").direct_conversation_id;
    let message = expect_error(
        delete(&client, &brain.url(&format!("/api/conversations/{dm}"))).await,
        409,
        "conflict",
    )
    .await;
    assert!(message.contains("DM"), "{message}");
    // A DM cannot be renamed either.
    expect_error(
        patch(
            &client,
            &brain.url(&format!("/api/conversations/{dm}")),
            &json!({"title": "mine"}),
        )
        .await,
        409,
        "conflict",
    )
    .await;

    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn groups_update_title_members_and_lead_and_validate_inputs() {
    let brain = TestBrain::start().await;
    let client = brain.client();
    let lead = brain.bot_id("lead");
    let scout = brain.bot_id("scout");
    let dev = brain.bot_id("dev");
    let sentinel = brain.bot_id("sentinel");

    let created = body(
        post(
            &client,
            &brain.url("/api/conversations"),
            &json!({"title": "Squad", "members": ["lead", "scout"]}),
        )
        .await,
    )
    .await;
    let group_id = id_of(&created);
    assert_eq!(created["title"], json!("Squad"));
    assert_eq!(created["kind"]["lead"], json!(lead.to_string()));

    // Rename-only payload stays compatible.
    let renamed = body(
        patch(
            &client,
            &brain.url(&format!("/api/conversations/{group_id}")),
            &json!({"title": "Squad Two"}),
        )
        .await,
    )
    .await;
    assert_eq!(renamed["title"], json!("Squad Two"));
    assert_eq!(renamed["kind"]["lead"], json!(lead.to_string()));
    assert_eq!(
        renamed["kind"]["members"],
        json!([lead.to_string(), scout.to_string()])
    );

    // Update title, members (mixed ids and handles), and lead together.
    let updated = body(
        patch(
            &client,
            &brain.url(&format!("/api/conversations/{group_id}")),
            &json!({
                "title": "Build Squad",
                "members": [scout.to_string(), dev.to_string(), "sentinel"],
                "lead": "dev"
            }),
        )
        .await,
    )
    .await;
    assert_eq!(updated["title"], json!("Build Squad"));
    assert_eq!(updated["kind"]["lead"], json!(dev.to_string()));
    assert_eq!(
        updated["kind"]["members"],
        json!([scout.to_string(), dev.to_string(), sentinel.to_string()])
    );

    // Update lead alone among the new members.
    let relinked = body(
        patch(
            &client,
            &brain.url(&format!("/api/conversations/{group_id}")),
            &json!({"lead": sentinel.to_string()}),
        )
        .await,
    )
    .await;
    assert_eq!(relinked["kind"]["lead"], json!(sentinel.to_string()));

    // Invalid inputs and missing bots map cleanly to 400 / 404 / 409.
    expect_error(
        patch(
            &client,
            &brain.url(&format!("/api/conversations/{group_id}")),
            &json!({"members": ["dev"]}),
        )
        .await,
        400,
        "invalid_input",
    )
    .await;
    expect_error(
        patch(
            &client,
            &brain.url(&format!("/api/conversations/{group_id}")),
            &json!({"members": ["dev", "dev"]}),
        )
        .await,
        400,
        "invalid_input",
    )
    .await;
    expect_error(
        patch(
            &client,
            &brain.url(&format!("/api/conversations/{group_id}")),
            &json!({"lead": "lead"}),
        )
        .await,
        400,
        "invalid_input",
    )
    .await;
    expect_error(
        patch(
            &client,
            &brain.url(&format!("/api/conversations/{group_id}")),
            &json!({"members": ["dev", "nobody"]}),
        )
        .await,
        404,
        "not_found",
    )
    .await;
    expect_error(
        patch(
            &client,
            &brain.url(&format!("/api/conversations/{group_id}")),
            &json!({"lead": "nobody"}),
        )
        .await,
        404,
        "not_found",
    )
    .await;

    let dm = brain.bot("lead").direct_conversation_id;
    expect_error(
        patch(
            &client,
            &brain.url(&format!("/api/conversations/{dm}")),
            &json!({"members": ["lead", "scout"], "lead": "lead"}),
        )
        .await,
        409,
        "conflict",
    )
    .await;

    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transcripts_page_forward_and_show_their_tail() {
    let brain = TestBrain::start().await;
    let client = brain.client();
    let dm = brain.bot("lead").direct_conversation_id;
    let url = format!("/api/conversations/{dm}");

    // Four owner messages. Each wakes the lead, whose reply is whatever the
    // scripted fake falls back to, so the transcript grows by more than four —
    // which is exactly why paging is asserted against the tail rather than
    // against a guessed count.
    for index in 0..4 {
        let response = post(
            &client,
            &brain.url(&format!("{url}/messages")),
            &json!({"text": format!("message {index}"), "addressed": []}),
        )
        .await;
        assert_eq!(response.status().as_u16(), 200);
    }
    brain.settle().await;

    let tail = array(get(&client, &brain.url(&format!("{url}/tail?limit=1000"))).await).await;
    assert!(tail.len() >= 4, "{} entries", tail.len());
    let seqs: Vec<u64> = tail
        .iter()
        .map(|entry| entry["seq"].as_u64().expect("a seq"))
        .collect();
    assert_eq!(
        seqs.clone(),
        {
            let mut sorted = seqs.clone();
            sorted.sort_unstable();
            sorted
        },
        "the tail is ascending"
    );
    assert!(
        tail.iter()
            .any(|entry| entry["kind"]["text"] == json!("message 3")),
        "{tail:?}"
    );

    // Page forward two at a time and reassemble: the union has to be the tail.
    let mut paged: Vec<Value> = Vec::new();
    let mut after_seq = 0u64;
    for _ in 0..(tail.len() + 2) {
        let page = array(
            get(
                &client,
                &brain.url(&format!("{url}/entries?after_seq={after_seq}&limit=2")),
            )
            .await,
        )
        .await;
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 2, "{page:?}");
        after_seq = page.last().expect("a last entry")["seq"]
            .as_u64()
            .expect("a seq");
        paged.extend(page);
    }
    assert_eq!(
        paged.len(),
        tail.len(),
        "paging loses or duplicates entries"
    );
    assert_eq!(paged, tail, "the pages reassemble into the tail");

    // `after_seq` past the end is an empty page, not an error.
    let empty = array(
        get(
            &client,
            &brain.url(&format!("{url}/entries?after_seq={}&limit=10", u64::MAX)),
        )
        .await,
    )
    .await;
    assert!(empty.is_empty(), "{empty:?}");

    // A limit of 0 means "the default page", not "nothing".
    let default_page = array(get(&client, &brain.url(&format!("{url}/tail?limit=0"))).await).await;
    assert!(!default_page.is_empty());

    // A malformed query is a 400 with the envelope.
    expect_error(
        get(&client, &brain.url(&format!("{url}/entries?limit=nope"))).await,
        400,
        "invalid_input",
    )
    .await;

    brain.stop().await;
}

// --- messages ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_direct_message_creates_an_entry_and_a_delivery() {
    let brain = TestBrain::start().await;
    let client = brain.client();
    let lead = brain.bot("lead");
    let dm = lead.direct_conversation_id.clone();

    // Take the runner out of the map so the delivery has to wait where it can be
    // observed. It is put back afterwards, which is also the path a bot created
    // before its runner started takes.
    let handle = brain
        .swarm
        .runners
        .unregister(&lead.id)
        .expect("the lead has a runner");

    let response = post(
        &client,
        &brain.url(&format!("/api/conversations/{dm}/messages")),
        &json!({"text": "hello there", "addressed": []}),
    )
    .await;
    assert_eq!(response.status().as_u16(), 200);
    let value = body(response).await;

    // The entry is the committed owner message, with a real id and sequence.
    let entry = &value["entry"];
    assert_eq!(entry["conversation_id"], json!(dm.to_string()));
    assert_eq!(entry["kind"]["kind"], json!("message"));
    assert_eq!(entry["kind"]["text"], json!("hello there"));
    assert_eq!(entry["author"]["kind"], json!("owner"));
    assert!(entry["seq"].as_u64().expect("a seq") >= 1, "{entry}");
    assert!(entry["id"].as_str().expect("an id").starts_with("ent_"));
    // A DM does not route: the one bot in it is the target.
    assert_eq!(value["targets"], json!([lead.id.to_string()]));
    assert_eq!(value["reason"], json!("addressed"));
    assert_eq!(value["queued"], json!([]));

    // And a delivery exists for that bot, pointing at the entry.
    let queued = brain
        .swarm
        .store
        .queued_deliveries(&lead.id)
        .expect("the queue reads");
    assert_eq!(queued.len(), 1, "{queued:?}");
    assert_eq!(queued[0].conversation_id, dm);
    let entry_id: EntryId = entry["id"]
        .as_str()
        .expect("an id")
        .parse()
        .expect("an entry id");
    assert_eq!(queued[0].source_entry_id, Some(entry_id));

    // It is readable through the transcript endpoint too.
    let tail = array(
        get(
            &client,
            &brain.url(&format!("/api/conversations/{dm}/tail?limit=10")),
        )
        .await,
    )
    .await;
    assert!(tail.iter().any(|e| e["id"] == entry["id"]), "{tail:?}");

    // Put the runner back: the parked delivery is handed over and the bot answers.
    brain.script_text("hello yourself");
    handle.shutdown();
    brain.swarm.ensure_runners().expect("runners come back");
    brain.settle().await;
    let tail = brain.entries(&dm);
    assert!(
        tail.iter().any(|e| e.kind.text() == Some("hello yourself")),
        "the parked delivery woke the bot: {tail:?}"
    );

    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mention_in_a_group_routes_to_the_mentioned_bot() {
    let brain = TestBrain::start().await;
    let client = brain.client();
    let group = brain.conversation_titled("Team");
    let scout = brain.bot_id("scout");

    brain.script_text("on it");
    let value = body(
        post(
            &client,
            &brain.url(&format!("/api/conversations/{group}/messages")),
            &json!({"text": "@scout what is in the workspace?", "addressed": []}),
        )
        .await,
    )
    .await;
    assert_eq!(value["targets"], json!([scout.to_string()]));
    assert_eq!(value["reason"], json!("mention"));
    assert_eq!(value["queued"], json!([]));
    brain.settle().await;

    // Nobody mentioned: the lead answers, and the transcript says why.
    brain.script_text("I will take this");
    let value = body(
        post(
            &client,
            &brain.url(&format!("/api/conversations/{group}/messages")),
            &json!({"text": "who is on point?", "addressed": []}),
        )
        .await,
    )
    .await;
    assert_eq!(value["targets"], json!([brain.bot_id("lead").to_string()]));
    assert_eq!(value["reason"], json!("lead"));

    // An explicit addressee beats the text.
    let value = body(
        post(
            &client,
            &brain.url(&format!("/api/conversations/{group}/messages")),
            &json!({"text": "over here", "addressed": [brain.bot_id("dev").to_string()]}),
        )
        .await,
    )
    .await;
    assert_eq!(value["targets"], json!([brain.bot_id("dev").to_string()]));
    assert_eq!(value["reason"], json!("addressed"));

    // `@everyone` wakes the whole group.
    let value = body(
        post(
            &client,
            &brain.url(&format!("/api/conversations/{group}/messages")),
            &json!({"text": "@everyone status?", "addressed": []}),
        )
        .await,
    )
    .await;
    assert_eq!(value["reason"], json!("everyone"));
    assert_eq!(
        value["targets"].as_array().expect("targets").len(),
        4,
        "{value}"
    );
    brain.settle().await;

    // An empty message is a 400 and writes nothing.
    let before = brain.entries(&group).len();
    for text in ["", "   ", "\n\t"] {
        expect_error(
            post(
                &client,
                &brain.url(&format!("/api/conversations/{group}/messages")),
                &json!({"text": text, "addressed": []}),
            )
            .await,
            400,
            "invalid_input",
        )
        .await;
    }
    assert_eq!(brain.entries(&group).len(), before, "nothing was written");

    // A message to a conversation that does not exist is a 404.
    expect_error(
        post(
            &client,
            &brain.url("/api/conversations/cnv_00000000000000000000000000000000/messages"),
            &json!({"text": "anybody home", "addressed": []}),
        )
        .await,
        404,
        "not_found",
    )
    .await;

    // Interrupting an idle conversation is honest about it.
    let value = body(
        post_empty(
            &client,
            &brain.url(&format!("/api/conversations/{group}/interrupt")),
        )
        .await,
    )
    .await;
    assert_eq!(value["stopped"], json!(false));

    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_full_turn_with_a_scripted_backend() {
    let brain = TestBrain::start().await;
    let client = brain.client();
    let lead = brain.bot("lead");
    let dm = lead.direct_conversation_id.clone();

    // The whole path: REST in, a runner, the scripted model, a committed reply.
    brain.script(ScriptedTurn::text(
        "The workspace holds one crate and two tests.",
    ));
    let sent = body(
        post(
            &client,
            &brain.url(&format!("/api/conversations/{dm}/messages")),
            &json!({"text": "what is in the workspace?", "addressed": []}),
        )
        .await,
    )
    .await;
    let owner_seq = sent["entry"]["seq"].as_u64().expect("a seq");

    brain.settle().await;

    // The answer is in that conversation, after the owner's message, and the
    // roster says the bot is idle again.
    let entries = brain.entries(&dm);
    let reply = entries
        .iter()
        .find(|entry| entry.seq > owner_seq && entry.author.bot_id() == Some(&lead.id))
        .unwrap_or_else(|| panic!("no reply from @lead in {entries:?}"));
    assert_eq!(
        reply.kind.text(),
        Some("The workspace holds one crate and two tests.")
    );
    assert!(reply.kind.text().is_some());
    let roster = array(get(&client, &brain.url("/api/bots")).await).await;
    let lead_row = roster
        .iter()
        .find(|bot| bot["id"] == json!(lead.id.to_string()))
        .expect("the lead is listed");
    assert_eq!(lead_row["status"]["state"], json!("idle"));

    // And the reply is readable through the API, not just through the store.
    let tail = array(
        get(
            &client,
            &brain.url(&format!("/api/conversations/{dm}/tail?limit=100")),
        )
        .await,
    )
    .await;
    assert!(
        tail.iter()
            .any(|entry| entry["id"] == json!(reply.id.to_string())),
        "{tail:?}"
    );
    // The model saw the owner's text.
    let requests = brain.fake().requests();
    assert!(
        requests.iter().any(|request| request
            .messages
            .iter()
            .any(|message| message.content.contains("what is in the workspace?"))),
        "the turn carried the owner's message"
    );

    brain.stop().await;
}
