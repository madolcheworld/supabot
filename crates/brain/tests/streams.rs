//! The data-stream socket, end to end: a real brain, a real WebSocket, and a
//! dialer built from the protocol crate's own client pieces.
//!
//! The test acts as the node, the way `node_gateway.rs` does and for the same
//! reason (`crates/brain` may not depend on `swarmbot-node`): the frames are
//! produced by the same library the real daemon would use, so a mismatch in a
//! frame's spelling fails here rather than on somebody's machine.
//!
//! What these tests are about is the part that cannot be unit-tested: the
//! **order** of the three steps. A ticket is minted before the RPC, a dial
//! consumes the token once, and the ticket resolves exactly once — from a
//! `Done`, from a refusal, or from a socket that ends without one.

mod common;

use common::*;

use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};

use swarmbot_node_protocol::auth::AUTH_SCHEME;
use swarmbot_node_protocol::streams::{StreamFrame, StreamKind};
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::Message as WsMessage;
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::http::HeaderValue;
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes;
use swarmbot_node_protocol::tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use swarmbot_node_protocol::{CLOSE_STREAM_REFUSED, chunk_sha256};

/// How long a test waits for something that is already on its way.
const BUDGET: Duration = Duration::from_secs(30);

type StreamSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Dial the stream socket the way the daemon does: bearer in the header, token
/// never in the URL (a URL ends up in access logs).
async fn dial(brain: &TestBrain, stream_id: &str, token: &str) -> StreamSocket {
    let url = format!(
        "{}/node/stream/{stream_id}",
        brain.base_url.replacen("http://", "ws://", 1)
    );
    let mut request = url.into_client_request().expect("a dialable URL");
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("{AUTH_SCHEME} {token}")).expect("a valid header"),
    );
    let (socket, response) = connect_async(request)
        .await
        .expect("the stream socket dials");
    assert_eq!(response.status().as_u16(), 101, "the upgrade is accepted");
    socket
}

async fn send(socket: &mut StreamSocket, frame: &StreamFrame) {
    let text = frame.encode().expect("a frame encodes");
    socket
        .send(WsMessage::Text(Utf8Bytes::from(text)))
        .await
        .expect("the frame is sent");
}

/// The close code a socket ends with, or `None` when it ended cleanly.
async fn close_code(socket: &mut StreamSocket) -> Option<u16> {
    while let Some(Ok(message)) = socket.next().await {
        if let WsMessage::Close(frame) = message {
            return frame.map(|frame| frame.code.into());
        }
    }
    None
}

#[tokio::test]
async fn a_minted_ticket_receives_the_bytes_a_node_sends() {
    let brain = TestBrain::start().await;
    let hub = brain.state.gateway.streams();
    // Step 1 happens before the RPC on purpose: a node on a fast link can dial
    // before the brain has recorded what it is waiting for.
    let ticket = hub.open(StreamKind::Transfer).expect("a grant is minted");
    let stream_id = ticket.stream_id().to_string();
    let token = ticket.token.clone();
    assert_eq!(hub.len(), 1, "the ticket is waiting");
    assert_eq!(hub.grants(), 1, "the grant is registered");

    let content = b"the bytes the node would send";
    let mut socket = dial(&brain, &stream_id, &token).await;

    // Pump the frames on a task, then await the ticket: the ticket is what the
    // rest of the brain would be waiting on.
    let producer = tokio::spawn(async move {
        for (seq, chunk) in content.chunks(7).enumerate() {
            send(
                &mut socket,
                &StreamFrame::data(seq as u64, chunk).expect("a chunk"),
            )
            .await;
        }
        send(
            &mut socket,
            &StreamFrame::done(chunk_sha256(content), content.len() as u64),
        )
        .await;
        socket
    });

    let received = tokio::time::timeout(BUDGET, hub.receive(ticket))
        .await
        .expect("the ticket resolves within the budget")
        .expect("the transfer completes");
    assert_eq!(received.bytes, content);
    assert_eq!(received.sha256, chunk_sha256(content));

    // The socket is closed by the brain after `Done`, not left dangling.
    let mut socket = producer.await.expect("the producer task ends");
    let code = tokio::time::timeout(BUDGET, close_code(&mut socket))
        .await
        .expect("the socket closes");
    assert_eq!(code, Some(1000), "a completed stream closes cleanly");
    assert!(hub.is_empty(), "the ticket is gone once it is answered");
}

#[tokio::test]
async fn the_token_is_checked_and_the_kind_comes_from_the_grant() {
    let brain = TestBrain::start().await;
    let hub = brain.state.gateway.streams();
    let ticket = hub.open(StreamKind::Transfer).expect("a grant is minted");
    let stream_id = ticket.stream_id().to_string();

    // A wrong token is refused with its own close code — deliberately not 4401,
    // so a node does not mistake a mistyped token for a revocation and stop
    // redialing its control socket forever.
    let mut socket = dial(&brain, &stream_id, "AAAAAAAAAAAAAAAAAAAAAA").await;
    let code = tokio::time::timeout(BUDGET, close_code(&mut socket))
        .await
        .expect("the refused socket closes");
    assert_eq!(
        code,
        Some(CLOSE_STREAM_REFUSED),
        "a wrong token is its own refusal, not a revocation"
    );
    assert_ne!(CLOSE_STREAM_REFUSED, swarmbot_node_protocol::CLOSE_REVOKED);

    // The legitimate dial still works: a guessing attack must not be able to
    // burn the token.
    let mut socket = dial(&brain, &stream_id, &ticket.token).await;
    send(&mut socket, &StreamFrame::error("policy_refused", "enough")).await;
    let error = tokio::time::timeout(BUDGET, hub.receive(ticket))
        .await
        .expect("the ticket resolves")
        .expect_err("the node refused");
    assert!(error.contains("policy_refused"), "{error}");

    // An unknown id is refused the same way, and says which case it was.
    let mut socket = dial(&brain, "000000000000000000000000", "x").await;
    let code = tokio::time::timeout(BUDGET, close_code(&mut socket))
        .await
        .expect("the refused socket closes");
    assert_eq!(code, Some(CLOSE_STREAM_REFUSED));
}

#[tokio::test]
async fn a_stream_that_ends_without_done_is_a_failure_not_a_short_file() {
    let brain = TestBrain::start().await;
    let hub = brain.state.gateway.streams();
    let ticket = hub.open(StreamKind::Transfer).expect("a grant is minted");
    let stream_id = ticket.stream_id().to_string();
    let token = ticket.token.clone();

    let mut socket = dial(&brain, &stream_id, &token).await;
    send(
        &mut socket,
        &StreamFrame::data(0, b"the first half of a file").expect("a chunk"),
    )
    .await;
    // The node disappears: no `Done`, no error frame, just a close.
    socket.close(None).await.expect("the socket closes");

    let error = tokio::time::timeout(BUDGET, hub.receive(ticket))
        .await
        .expect("the ticket resolves rather than hanging")
        .expect_err("a truncated stream is not a file");
    assert!(
        error.contains("without finishing"),
        "the reason says the stream was cut short: {error}"
    );
}

#[tokio::test]
async fn a_digest_that_does_not_match_what_arrived_is_refused() {
    let brain = TestBrain::start().await;
    let hub = brain.state.gateway.streams();
    let ticket = hub.open(StreamKind::Transfer).expect("a grant is minted");
    let stream_id = ticket.stream_id().to_string();
    let token = ticket.token.clone();

    let mut socket = dial(&brain, &stream_id, &token).await;
    let content = b"what actually arrived";
    send(
        &mut socket,
        &StreamFrame::data(0, content).expect("a chunk"),
    )
    .await;
    // A `Done` that names different bytes: a node that lies, or a link that
    // dropped a frame. Either way it is detected here.
    send(
        &mut socket,
        &StreamFrame::done(chunk_sha256(b"what the node claims"), content.len() as u64),
    )
    .await;

    let error = tokio::time::timeout(BUDGET, hub.receive(ticket))
        .await
        .expect("the ticket resolves")
        .expect_err("a mismatched digest is a failure");
    assert!(error.contains("digest does not match"), "{error}");
    let _ = socket.close(None).await;
}
