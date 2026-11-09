//! `/ws/live` and the node gateway socket, from a real WebSocket client.
//!
//! The live socket is the one part of this API a REST test cannot cover: the
//! upgrade (and its `Origin` rule), the snapshot → `synchronized` handshake, the
//! resume-by-cursor promise, and — the detail a client feels most — that the
//! socket ends with a close frame instead of a TCP reset.

mod common;

use common::*;

use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::ClientRequestBuilder;

use swarmbot_contracts::live::{LiveCursor, LiveEvent};

/// The socket type `connect_async` hands back over a plain `ws://` loopback URL.
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// How long a test waits for a frame that should already be on its way.
const FRAME_BUDGET: Duration = Duration::from_secs(30);

/// Dial `/ws/live` as the owner.
async fn connect(brain: &TestBrain, query: &str) -> Socket {
    dial(&format!("{}{query}", brain.ws_url()), Some(brain.token()))
        .await
        .expect("the upgrade succeeds")
}

/// Dial an arbitrary ws:// URL, optionally with a bearer token.
async fn dial(url: &str, token: Option<&str>) -> Result<Socket, (u16, String)> {
    let uri = url
        .parse()
        .unwrap_or_else(|err| panic!("{url} is not a URI: {err}"));
    let mut request = ClientRequestBuilder::new(uri);
    if let Some(token) = token {
        request = request.with_header("authorization", format!("Bearer {token}"));
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, response)) => {
            assert_eq!(
                response.status().as_u16(),
                101,
                "a live upgrade is a switching-protocols answer"
            );
            Ok(socket)
        }
        Err(err) => Err(refusal_of(url, err)),
    }
}

/// Dial with an extra header, for the `Origin` rule.
async fn dial_with_origin(url: &str, token: &str, origin: &str) -> Result<Socket, (u16, String)> {
    let uri = url
        .parse()
        .unwrap_or_else(|err| panic!("{url} is not a URI: {err}"));
    let request = ClientRequestBuilder::new(uri)
        .with_header("authorization", format!("Bearer {token}"))
        .with_header("origin", origin.to_string());
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _)) => Ok(socket),
        Err(err) => Err(refusal_of(url, err)),
    }
}

/// A refused handshake, as `(status, reason)`.
fn refusal_of(url: &str, err: tokio_tungstenite::tungstenite::Error) -> (u16, String) {
    match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => (
            response.status().as_u16(),
            "the brain refused the upgrade".to_string(),
        ),
        other => (0, format!("{url}: {other}")),
    }
}

/// The next `LiveEvent`, skipping control frames. `None` on a clean close.
async fn next_event(socket: &mut Socket) -> Option<LiveEvent> {
    loop {
        let frame = tokio::time::timeout(FRAME_BUDGET, socket.next())
            .await
            .expect("a frame arrives inside the budget");
        match frame {
            Some(Ok(Message::Text(text))) => {
                return Some(
                    serde_json::from_str(&text)
                        .unwrap_or_else(|err| panic!("`{text}` is not a LiveEvent: {err}")),
                );
            }
            // The server's keepalive ping: tungstenite queues the pong, and the
            // next write flushes it. Neither is an event.
            Some(Ok(Message::Ping(_))) => {
                socket.flush().await.expect("the pong goes out");
            }
            Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Close(frame))) => {
                panic!("the socket closed before the test was done: {frame:?}");
            }
            Some(Ok(other)) => panic!("unexpected frame {other:?}"),
            Some(Err(err)) => panic!("the live socket failed: {err}"),
            None => return None,
        }
    }
}

/// Read until the predicate matches, and return that event. Fails loudly with
/// what *did* arrive, because "the event never came" is useless on its own.
async fn wait_for(
    socket: &mut Socket,
    what: &str,
    matches: impl Fn(&LiveEvent) -> bool,
) -> LiveEvent {
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..400 {
        let Some(event) = next_event(socket).await else {
            panic!(
                "the socket closed while waiting for {what}; saw {}",
                seen.join(", ")
            );
        };
        if matches(&event) {
            return event;
        }
        seen.push(describe(&event));
    }
    panic!("gave up waiting for {what}; saw {}", seen.join(", "));
}

/// One line naming an event, for a failure message.
fn describe(event: &LiveEvent) -> String {
    match event {
        LiveEvent::Snapshot(_) => "snapshot".to_string(),
        LiveEvent::Synchronized { cursor } => format!("synchronized@{cursor}"),
        LiveEvent::EntryUpsert { cursor, entry } => format!(
            "entry@{cursor}({} by {} #{})",
            entry.kind.kind_str(),
            if entry.author.is_owner() {
                "the owner".to_string()
            } else {
                entry
                    .author
                    .bot_id()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "the system".to_string())
            },
            entry.seq
        ),
        LiveEvent::BotUpsert { bot, .. } => format!("bot({})", bot.name),
        LiveEvent::BotStatus { bot_id, status, .. } => {
            format!("status({bot_id}={})", status.label())
        }
        LiveEvent::ConversationUpsert { conversation, .. } => {
            format!("conversation({})", conversation.id)
        }
        LiveEvent::TurnUpsert { turn, .. } => format!("turn({})", turn.id),
        LiveEvent::TaskUpsert { task, .. } => format!("task({})", task.id),
        LiveEvent::RequestUpsert { request, .. } => format!("request({})", request.id),
        LiveEvent::NodeUpsert { node, .. } => format!("node({})", node.id),
        LiveEvent::NodePresence {
            node_id, online, ..
        } => format!("presence({node_id}={online})"),
        LiveEvent::Resync { reason } => format!("resync({reason})"),
        LiveEvent::Failed { code, message } => format!("failed({code}: {message})"),
        LiveEvent::ConversationDeleted {
            conversation_id, ..
        } => {
            format!("deleted({conversation_id})")
        }
    }
}

/// Read the snapshot and the `synchronized` marker that follows it.
async fn handshake(socket: &mut Socket) -> (LiveEvent, LiveCursor) {
    let first = next_event(socket).await.expect("a first frame");
    assert!(
        matches!(first, LiveEvent::Snapshot(_)),
        "the stream opens with a snapshot, got {}",
        describe(&first)
    );
    let synchronized = wait_for(socket, "synchronized", |event| {
        matches!(event, LiveEvent::Synchronized { .. })
    })
    .await;
    let cursor = synchronized.cursor().expect("a cursor");
    (first, cursor)
}

// --- the tests ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_socket_opens_with_a_snapshot_and_then_synchronizes() {
    let brain = TestBrain::start().await;
    let mut socket = connect(&brain, "").await;

    let (snapshot, cursor) = handshake(&mut socket).await;
    let LiveEvent::Snapshot(snapshot) = snapshot else {
        panic!("the first frame is a snapshot");
    };
    assert_eq!(snapshot.bots.len(), 4, "the demo team is in it");
    assert!(snapshot.bots.iter().any(|bot| bot.name == "lead"));
    assert!(
        snapshot
            .conversations
            .iter()
            .any(|conversation| conversation.title.as_deref() == Some("Team")),
        "the group is in it"
    );
    assert!(
        snapshot.nodes.iter().any(|node| node.id.is_local()),
        "the local node is in it"
    );
    assert_eq!(cursor.epoch, brain.state.epoch());

    // Every frame is JSON text, and every event carries a cursor a client can
    // resume from.
    let _ = socket.close(None).await;
    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_posted_over_rest_arrives_as_an_entry_upsert() {
    let brain = TestBrain::start().await;
    let client = brain.client();
    let dm = brain.bot("lead").direct_conversation_id;
    let mut socket = connect(&brain, "").await;
    let (_, cursor) = handshake(&mut socket).await;

    brain.script_text("read you loud");
    let sent = body(
        post(
            &client,
            &brain.url(&format!("/api/conversations/{dm}/messages")),
            &json!({"text": "can you hear me", "addressed": []}),
        )
        .await,
    )
    .await;
    let entry_id = sent["entry"]["id"].as_str().expect("an id").to_string();

    // The owner's own entry comes back over the socket, after the cursor the
    // handshake reported.
    let event = wait_for(&mut socket, "the owner's entry", |event| match event {
        LiveEvent::EntryUpsert { entry, .. } => entry.id.as_str() == entry_id,
        _ => false,
    })
    .await;
    let LiveEvent::EntryUpsert { cursor: at, entry } = event else {
        panic!("an entry upsert");
    };
    assert!(
        at > cursor,
        "the event is newer than the handshake: {at} > {cursor}"
    );
    assert_eq!(entry.kind.text(), Some("can you hear me"));
    assert_eq!(entry.conversation_id, dm);

    // Let the turn finish, then look for the finished reply on the same socket.
    // A streaming turn upserts the same entry several times; the last one carries
    // the whole text, and nothing is lost while the test is elsewhere: the hub's
    // channel to this subscriber is unbounded.
    brain.settle().await;
    let lead = brain.bot_id("lead");
    let reply = wait_for(
        &mut socket,
        "the bot's finished reply",
        |event| match event {
            LiveEvent::EntryUpsert { entry, .. } => {
                entry.author.bot_id() == Some(&lead) && entry.kind.text() == Some("read you loud")
            }
            _ => false,
        },
    )
    .await;
    let LiveEvent::EntryUpsert { entry, .. } = reply else {
        panic!("an entry upsert");
    };
    assert_eq!(entry.conversation_id, dm);

    let _ = socket.close(None).await;
    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resuming_from_a_cursor_repeats_nothing() {
    let brain = TestBrain::start().await;
    let client = brain.client();
    let dm = brain.bot("lead").direct_conversation_id;

    // See one entry, note its cursor, and go away.
    let mut socket = connect(&brain, "").await;
    handshake(&mut socket).await;
    let sent = body(
        post(
            &client,
            &brain.url(&format!("/api/conversations/{dm}/messages")),
            &json!({"text": "first", "addressed": []}),
        )
        .await,
    )
    .await;
    let first_id = sent["entry"]["id"].as_str().expect("an id").to_string();
    let seen = wait_for(&mut socket, "the first entry", |event| match event {
        LiveEvent::EntryUpsert { entry, .. } => entry.id.as_str() == first_id,
        _ => false,
    })
    .await;
    let resume_from = seen.cursor().expect("a cursor");
    let _ = socket.close(None).await;
    drop(socket);

    // Something happens while nobody is watching.
    let sent = body(
        post(
            &client,
            &brain.url(&format!("/api/conversations/{dm}/messages")),
            &json!({"text": "second", "addressed": []}),
        )
        .await,
    )
    .await;
    let second_id = sent["entry"]["id"].as_str().expect("an id").to_string();

    // Reconnect with the cursor. The percent-encoded form is what a real client
    // sends (`3:42` → `3%3A42`), and it has to parse back to the same cursor.
    let encoded = resume_from.to_string().replace(':', "%3A");
    let mut socket = connect(&brain, &format!("?cursor={encoded}")).await;

    // No resync: the gap is small, so the hub replays instead of re-snapshotting.
    let mut replayed = Vec::new();
    let synchronized = loop {
        let event = next_event(&mut socket).await.expect("a frame");
        if let LiveEvent::Resync { reason } = &event {
            panic!("a small gap must replay, not resync: {reason}");
        }
        if matches!(event, LiveEvent::Synchronized { .. }) {
            break event;
        }
        replayed.push(event);
    };
    assert!(
        !replayed
            .iter()
            .any(|event| matches!(event, LiveEvent::Snapshot(_))),
        "a replay does not start with a snapshot: {}",
        replayed.iter().map(describe).collect::<Vec<_>>().join(", ")
    );
    // The entry seen before the drop is not sent again …
    assert!(
        !replayed.iter().any(|event| match event {
            LiveEvent::EntryUpsert { entry, .. } => entry.id.as_str() == first_id,
            _ => false,
        }),
        "the cursor is a promise: {first_id} was already delivered"
    );
    // … but the one that happened meanwhile is.
    assert!(
        replayed.iter().any(|event| match event {
            LiveEvent::EntryUpsert { entry, .. } => entry.id.as_str() == second_id,
            _ => false,
        }),
        "the gap is filled: {}",
        replayed.iter().map(describe).collect::<Vec<_>>().join(", ")
    );
    // Every replayed event is newer than the cursor we resumed from.
    for event in &replayed {
        if let Some(cursor) = event.cursor() {
            assert!(cursor > resume_from, "{cursor} is not after {resume_from}");
        }
    }
    assert!(synchronized.cursor().expect("a cursor") >= resume_from);

    let _ = socket.close(None).await;
    brain.settle().await;
    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cursor_from_another_epoch_resyncs_instead_of_lying() {
    let brain = TestBrain::start().await;
    // An epoch that cannot be honoured: the honest answer is a `Resync` and a
    // fresh snapshot, never a transcript with a hole in it.
    let mut socket = connect(&brain, "?cursor=9999:1").await;
    let first = next_event(&mut socket).await.expect("a frame");
    assert!(
        matches!(first, LiveEvent::Resync { .. }),
        "expected a resync, got {}",
        describe(&first)
    );
    let second = next_event(&mut socket).await.expect("a frame");
    assert!(
        matches!(second, LiveEvent::Snapshot(_)),
        "a resync is followed by a snapshot, got {}",
        describe(&second)
    );
    let _ = socket.close(None).await;
    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conversation_filter_narrows_the_stream() {
    let brain = TestBrain::start().await;
    let client = brain.client();
    let dm = brain.bot("lead").direct_conversation_id;
    let group = brain.conversation_titled("Team");

    let mut socket = connect(&brain, &format!("?conversation={dm}")).await;
    let (snapshot, _) = handshake(&mut socket).await;
    let LiveEvent::Snapshot(snapshot) = snapshot else {
        panic!("the handshake returned a snapshot");
    };
    assert!(
        snapshot
            .entries
            .iter()
            .all(|entry| entry.conversation_id == dm),
        "the snapshot carries only the filtered transcript"
    );
    // The roster is never filtered: a client renders names next to entries.
    assert_eq!(snapshot.bots.len(), 4);

    // A message somewhere else must not arrive here.
    let elsewhere = body(
        post(
            &client,
            &brain.url(&format!("/api/conversations/{group}/messages")),
            &json!({"text": "@scout over here", "addressed": []}),
        )
        .await,
    )
    .await;
    let elsewhere_id = elsewhere["entry"]["id"]
        .as_str()
        .expect("an id")
        .to_string();

    // One in the filtered conversation does.
    let here = body(
        post(
            &client,
            &brain.url(&format!("/api/conversations/{dm}/messages")),
            &json!({"text": "and over here", "addressed": []}),
        )
        .await,
    )
    .await;
    let here_id = here["entry"]["id"].as_str().expect("an id").to_string();

    let event = wait_for(&mut socket, "the filtered conversation's entry", |event| {
        matches!(event, LiveEvent::EntryUpsert { entry, .. } if entry.id.as_str() == here_id)
    })
    .await;
    assert!(matches!(event, LiveEvent::EntryUpsert { .. }));

    // Nothing from the group ever showed up on the way. Drain until the socket
    // goes quiet: the group's turn takes a moment, and "no frame for half a
    // second" is the honest end of a live stream in a test.
    let mut saw_elsewhere = false;
    loop {
        let frame = tokio::time::timeout(Duration::from_millis(750), socket.next()).await;
        let Ok(Some(Ok(Message::Text(text)))) = frame else {
            break;
        };
        if let Ok(LiveEvent::EntryUpsert { entry, .. }) = serde_json::from_str::<LiveEvent>(&text)
            && entry.id.as_str() == elsewhere_id
        {
            saw_elsewhere = true;
        }
    }
    assert!(
        !saw_elsewhere,
        "the filter let another conversation through"
    );

    let _ = socket.close(None).await;
    brain.settle().await;
    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upgrade_needs_a_credential_and_an_allowed_origin() {
    let brain = TestBrain::start().await;
    let url = brain.ws_url();

    // No token at all: the handshake is refused with the documented 401, before
    // any upgrade happens.
    let (status, reason) = dial(&url, None).await.expect_err("refused without a token");
    assert_eq!(status, 401, "{reason}");

    // A wrong token is refused the same way.
    let (status, _) = dial(&url, Some("not-the-token"))
        .await
        .expect_err("refused");
    assert_eq!(status, 401);

    // A browser page from somewhere else is refused even with a valid token:
    // cookies ride along on any origin's handshake, so the origin is the only
    // thing standing between a malicious page and the owner's transcript.
    for origin in [
        "http://evil.example",
        "https://127.0.0.1.evil.example",
        "null",
    ] {
        match dial_with_origin(&url, brain.token(), origin).await {
            Ok(mut socket) => {
                let _ = socket.close(None).await;
                panic!("{origin} was allowed to upgrade with a valid token");
            }
            Err((status, reason)) => assert_eq!(status, 401, "{origin}: {reason}"),
        }
    }

    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_loopback_origin_may_upgrade_and_a_session_id_may_authenticate() {
    let brain = TestBrain::start().await;

    // The owner's own machine, on any port.
    let origin = format!("http://127.0.0.1:{}", brain.addr.port() + 1);
    let mut socket = dial_with_origin(&brain.ws_url(), brain.token(), &origin)
        .await
        .expect("a loopback origin is allowed");
    handshake(&mut socket).await;
    let _ = socket.close(None).await;

    // A session id works as a bearer, which is how a client that logged in over
    // REST can upgrade without keeping the owner token around.
    let response = post(
        &TestBrain::client_with(""),
        &brain.url("/api/session"),
        &json!({"token": brain.token()}),
    )
    .await;
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .expect("a cookie")
        .to_str()
        .expect("ascii")
        .to_string();
    let session = set_cookie
        .split(';')
        .next()
        .expect("a pair")
        .split_once('=')
        .expect("name=value")
        .1
        .to_string();
    let mut socket = dial(&brain.ws_url(), Some(&session))
        .await
        .expect("the session upgrades");
    handshake(&mut socket).await;
    let _ = socket.close(None).await;

    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_cursor_or_conversation_is_refused_before_the_upgrade() {
    let brain = TestBrain::start().await;

    // A real WebSocket handshake with a cursor that is not `<epoch>:<sequence>`
    // is refused *as HTTP*, before any upgrade: a client gets a status it can
    // report, not a socket that dies on the first read.
    let (status, reason) = dial(
        &format!("{}?cursor=nope", brain.ws_url()),
        Some(brain.token()),
    )
    .await
    .expect_err("a bad cursor is refused");
    assert_eq!(status, 400, "{reason}");

    let (status, reason) = dial(&format!("{}?cursor=1", brain.ws_url()), Some(brain.token()))
        .await
        .expect_err("a cursor with no sequence is refused");
    assert_eq!(status, 400, "{reason}");

    // A conversation id that is not one is a 404: nothing by that name exists.
    let (status, reason) = dial(
        &format!("{}?conversation=cnv_nope", brain.ws_url()),
        Some(brain.token()),
    )
    .await
    .expect_err("a bad conversation is refused");
    assert_eq!(status, 404, "{reason}");

    // And a plain HTTP GET on the same path is told it is not an upgrade, in the
    // envelope rather than as a bare string.
    let response = brain
        .client()
        .get(format!("{}/ws/live", brain.base_url))
        .send()
        .await
        .expect("the request goes out");
    let (status, code, message) = envelope(response).await;
    assert_eq!(status, 400);
    assert_eq!(code, "invalid_input");
    assert!(message.contains("upgrade"), "{message}");

    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_socket_ends_with_a_close_frame_not_a_reset() {
    let brain = TestBrain::start().await;
    let mut socket = connect(&brain, "").await;
    handshake(&mut socket).await;

    // Ask the daemon to stop: the live socket has to say goodbye properly, or the
    // client sees `ResetWithoutClosingHandshake` where the truth is "it stopped".
    let client = brain.client();
    assert_eq!(
        body(post_empty(&client, &brain.url("/api/shutdown")).await).await["ok"],
        json!(true)
    );

    // Drain whatever was already in flight, then require the goodbye.
    let deadline = tokio::time::Instant::now() + FRAME_BUDGET;
    let close = loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the socket never closed after a shutdown"
        );
        let frame = tokio::time::timeout(FRAME_BUDGET, socket.next())
            .await
            .expect("a frame arrives")
            .expect("the socket is still open")
            .expect("and did not fail");
        match frame {
            Message::Close(close) => break close.expect("a code and a reason"),
            other => assert!(
                matches!(
                    other,
                    Message::Text(_) | Message::Ping(_) | Message::Pong(_)
                ),
                "unexpected frame while draining: {other:?}"
            ),
        }
    };
    assert_eq!(u16::from(close.code), 1001, "going away: {close:?}");
    assert!(
        close.reason.to_string().contains("stopping"),
        "the reason says why: {close:?}"
    );
    brain.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_node_gateway_refuses_an_unsigned_upgrade_without_upgrading() {
    let brain = TestBrain::start().await;
    // A node has no owner credential — its credential is a signature over a fresh
    // nonce — so the owner token must not open this socket, and the absence of a
    // node signature must be an HTTP answer rather than a socket that dies.
    let url = format!("ws://{}/node", brain.addr);
    let (status, reason) = dial(&url, Some(brain.token()))
        .await
        .expect_err("the owner token is not a node credential");
    assert_eq!(status, 401, "{reason}");

    let (status, reason) = dial(&url, None).await.expect_err("refused");
    assert_eq!(status, 401, "{reason}");

    // The full pairing and hello flow, including the close codes, lives in
    // `tests/node_gateway.rs`: it needs a node identity, which this file has no
    // business minting.
    brain.stop().await;
}
