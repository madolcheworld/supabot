//! `GET /node/stream/:id` — the data-stream socket (§8).
//!
//! The node dials this with the one-time bearer the brain put inside the RPC
//! that asked for the stream. Three rules shape the handler:
//!
//! * **the token is checked, and the kind comes from the grant.** The dialer
//!   cannot ask for a different framing, and a wrong token gets a socket that
//!   closes immediately with [`CLOSE_STREAM_REFUSED`] — a distinct code from
//!   `4401` on purpose, so a node does not mistake a mistyped token for having
//!   been revoked and stop redialing its control socket.
//! * **the upgrade happens before the check.** Validating first would burn the
//!   single-use token when the *upgrade* fails (a proxy hiccup, a restart), and
//!   the node could never retry — it has no way to ask for a second token for
//!   the same RPC. Accepting the socket costs nothing: an unauthenticated dial
//!   is closed on the next line.
//! * **a truncated stream is an error, not a short payload.** If the socket
//!   ends before `Done`, the ticket resolves with a failure naming how much
//!   arrived. The alternative is handing the caller a file that looks complete.

use axum::extract::ws::WebSocket;
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use tracing::{debug, info, warn};

use swarmbot_node_protocol::auth::{AUTH_SCHEME, CLOSE_STREAM_REFUSED};
use swarmbot_node_protocol::mux::Socket as _;
use swarmbot_node_protocol::streams::{StreamFrame, StreamKind};

use crate::gateway::socket::GatewaySocket;
use crate::gateway::streams::{Assembly, Received};
use crate::state::AppState;

/// `GET /node/stream/:id` — upgrade, then serve one data stream.
pub async fn stream(
    State(state): State<AppState>,
    Path(stream_id): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    // The bearer is read from the header rather than the query string: a URL
    // ends up in every access log, and this token is a credential.
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix(AUTH_SCHEME))
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    upgrade.on_upgrade(move |socket| serve(state, socket, stream_id, token))
}

/// One data stream, from dial to the last frame.
async fn serve(state: AppState, socket: WebSocket, stream_id: String, token: String) {
    let mut socket = GatewaySocket::new(socket);
    let hub = state.gateway.streams();
    let kind: StreamKind = match hub.take_dial(&stream_id, &token) {
        Ok(kind) => kind,
        Err(error) => {
            // Refused, and the reason is in the close frame rather than only in
            // the log: the node operator's log is the other half of this.
            warn!(stream = %stream_id, reason = %error, "a data-stream dial was refused");
            let _ = socket.close(CLOSE_STREAM_REFUSED, &error.to_string()).await;
            return;
        }
    };
    info!(stream = %stream_id, kind = kind.as_str(), "a data stream is open");

    let mut frames: u64 = 0;
    loop {
        let text = match socket.recv_text().await {
            Ok(Some(text)) => text,
            Ok(None) => {
                // The peer went away without a `Done`. Whatever arrived is not a
                // file, and saying so is the whole point of the ticket.
                hub.complete(
                    &stream_id,
                    Err(format!(
                        "the node closed the stream after {frames} frame(s) without finishing it"
                    )),
                );
                debug!(stream = %stream_id, frames, "a data stream ended early");
                return;
            }
            Err(error) => {
                hub.complete(
                    &stream_id,
                    Err(format!(
                        "the data stream broke after {frames} frame(s): {error}"
                    )),
                );
                return;
            }
        };
        frames += 1;
        let frame = match StreamFrame::decode(&text) {
            Ok(frame) => frame,
            Err(error) => {
                // A frame the framing does not define is a protocol failure, not
                // something to skip: skipping one chunk yields a shorter file
                // with a correct-looking digest only if the sender also lied
                // about the size, which is exactly what `Done` checks.
                let reason = format!("undecodable stream frame: {error}");
                let _ = socket
                    .send_text(&error_frame("stream_broken", &reason))
                    .await;
                hub.complete(&stream_id, Err(reason));
                let _ = socket.close(4400, "the stream protocol was violated").await;
                return;
            }
        };

        match hub.assemble(&stream_id, frame) {
            Assembly::Continue => {}
            Assembly::Ack(total) => {
                if let Err(error) = socket.send_text(&ack_frame(total)).await {
                    hub.complete(
                        &stream_id,
                        Err(format!("the acknowledgement could not be sent: {error}")),
                    );
                    return;
                }
            }
            Assembly::Finished(Ok(received)) => {
                hub.complete(&stream_id, Ok(received));
                let _ = socket.close(1000, "the stream is complete").await;
                info!(stream = %stream_id, frames, "a data stream finished");
                return;
            }
            Assembly::Finished(Err(reason)) => {
                // The sender said why; it already knows, so nothing is sent back.
                hub.complete(&stream_id, Err(reason));
                let _ = socket.close(1000, "the sender refused the stream").await;
                return;
            }
            Assembly::Failed(reason) => {
                let _ = socket
                    .send_text(&error_frame("stream_broken", &reason))
                    .await;
                hub.complete(&stream_id, Err(reason));
                let _ = socket.close(4400, "the stream broke").await;
                return;
            }
        }
    }
}

/// The `Error` frame a refusing endpoint sends, as one text frame.
fn error_frame(code: &str, reason: &str) -> String {
    // Encoding a frame of known shape cannot fail; if it somehow did, an empty
    // string would be an empty frame, which the peer refuses as malformed —
    // still a failure, never a silent success.
    StreamFrame::error(code, reason)
        .encode()
        .unwrap_or_default()
}

fn ack_frame(total: u64) -> String {
    StreamFrame::ack(total).encode().unwrap_or_default()
}

/// Assemble a `Received` for a test or a caller that already has the bytes.
///
/// Exists so `crates/brain`'s tests can build the value a stream would produce
/// without a socket; the digest is computed here rather than passed in, so a
/// test cannot accidentally assert against a digest it made up.
pub fn received_from(bytes: Vec<u8>) -> Received {
    let sha256 = swarmbot_node_protocol::chunk_sha256(&bytes);
    Received { bytes, sha256 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_refusal_codes_are_the_documented_ones() {
        // The node's stream dialer branches on these; a change here is a wire
        // change and must fail in a test with a name, not at a machine.
        assert_eq!(CLOSE_STREAM_REFUSED, 4405);
        // Distinct from the control socket's revocation code, or a bad token
        // would look like a revocation and stop the node redialing forever.
        assert_ne!(CLOSE_STREAM_REFUSED, swarmbot_node_protocol::CLOSE_REVOKED);
    }

    #[test]
    fn the_frames_a_refusing_endpoint_sends_carry_their_reason() {
        let text = error_frame("stream_broken", "chunk 3 arrived before chunk 2");
        let frame = StreamFrame::decode(&text).expect("a decodable frame");
        match frame {
            StreamFrame::Error { code, reason } => {
                assert_eq!(code, "stream_broken");
                assert!(reason.contains("chunk 3"), "{reason}");
            }
            other => panic!("expected an error frame, got {other:?}"),
        }
        let ack = StreamFrame::decode(&ack_frame(2 * 1024 * 1024)).expect("a decodable frame");
        assert!(matches!(ack, StreamFrame::Ack { bytes } if bytes == 2 * 1024 * 1024));
    }

    #[test]
    fn a_received_value_hashes_what_it_holds() {
        let received = received_from(b"abc".to_vec());
        assert_eq!(
            received.sha256,
            swarmbot_node_protocol::chunk_sha256(b"abc")
        );
        assert_eq!(received.len(), 3);
    }
}
