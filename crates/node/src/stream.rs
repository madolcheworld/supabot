//! The node's half of a data stream (§8): dial the brain's stream socket and
//! send a file's bytes over it.
//!
//! The direction matters and is the whole reason this exists. A brain never
//! dials a node — every socket in this protocol is opened by the node, so a node
//! behind NAT or a firewall needs no inbound port. So the brain mints a token,
//! puts it **inside the RPC**, and this module spends it: dial out, send, close.
//!
//! Three rules keep a transfer honest, and each of them is a way the naive
//! version fails quietly:
//!
//! * **the sender waits for acks.** A node that writes as fast as the socket
//!   accepts lets a fast disk outrun a slow brain until something buffers
//!   without bound or drops a frame. The window is [`MAX_IN_FLIGHT_BYTES`], and
//!   the sender blocks on acks to stay inside it.
//! * **`Done` names the whole content.** `{sha256, size}` is computed by *this*
//!   side over exactly the bytes it sent. The brain recomputes it over what
//!   arrived and refuses a mismatch, so a dropped or reordered frame is a
//!   detected failure on both ends rather than a shorter file.
//! * **a refusal from the brain stops this side too.** An `Error` frame is
//!   reported as an error, not logged and ignored: the transfer did not happen.

use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};

use swarmbot_node_protocol::auth::AUTH_SCHEME;
use swarmbot_node_protocol::streams::{
    MAX_CHUNK_BYTES, MAX_IN_FLIGHT_BYTES, StreamFrame, chunk_sha256,
};
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::Message as WsMessage;
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::http::HeaderValue;
use swarmbot_node_protocol::tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes;
use swarmbot_node_protocol::tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::NodeError;

/// How long to wait for the brain's ack before giving up on the transfer.
///
/// An ack is sent every MiB, so this is not "how slow may a disk be" — it is
/// "how long may the brain be silent", and a brain that has not acked in this
/// long is not going to.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// What a completed transfer sent, so the caller can put it in the RPC answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentFile {
    pub size: u64,
    pub sha256: String,
    pub chunks: u64,
}

type StreamSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Send `bytes` over the stream `stream_id` with the bearer `token`.
///
/// The URL is derived from the brain the node paired with, never from a payload:
/// a node that dialled whatever address a frame named could be pointed at a
/// third machine.
pub async fn send_file(url: &str, token: &str, bytes: &[u8]) -> Result<SentFile, NodeError> {
    let mut request = url
        .into_client_request()
        .map_err(|error| NodeError::Unreachable {
            url: url.to_string(),
            reason: error.to_string(),
        })?;
    // The bearer rides the header, not the query string: a URL is written to
    // every access log, and this token is a credential.
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("{AUTH_SCHEME} {token}")).map_err(|error| {
            NodeError::Unreachable {
                url: url.to_string(),
                reason: format!("the stream token cannot be put in a header: {error}"),
            }
        })?,
    );
    let (mut socket, _response) =
        connect_async(request)
            .await
            .map_err(|error| NodeError::Unreachable {
                url: url.to_string(),
                reason: error.to_string(),
            })?;

    let sent = write_frames(&mut socket, bytes).await;
    // Close either way: a failed transfer should not leave the brain waiting on
    // a socket that will never speak again.
    let _ = socket.close(None).await;
    sent
}

async fn write_frames(socket: &mut StreamSocket, bytes: &[u8]) -> Result<SentFile, NodeError> {
    let mut sent = 0u64;
    let mut acked = 0u64;
    let mut chunks = 0u64;
    for (seq, chunk) in bytes.chunks(MAX_CHUNK_BYTES).enumerate() {
        // Backpressure: never more than the window un-acknowledged. The brain
        // acks every MiB, so this waits at most one ack per chunk.
        while sent.saturating_sub(acked) + chunk.len() as u64 > MAX_IN_FLIGHT_BYTES {
            acked = wait_for_ack(socket).await?;
        }
        let frame = StreamFrame::data(seq as u64, chunk).map_err(|error| {
            NodeError::config(format!("a stream chunk could not be built: {error}"))
        })?;
        send(socket, &frame).await?;
        sent += chunk.len() as u64;
        chunks += 1;
    }
    send(
        socket,
        &StreamFrame::done(chunk_sha256(bytes), bytes.len() as u64),
    )
    .await?;
    Ok(SentFile {
        size: sent,
        sha256: chunk_sha256(bytes),
        chunks,
    })
}

async fn send(socket: &mut StreamSocket, frame: &StreamFrame) -> Result<(), NodeError> {
    let text = frame.encode().map_err(|error| {
        NodeError::config(format!("a stream frame could not be encoded: {error}"))
    })?;
    socket
        .send(WsMessage::Text(Utf8Bytes::from(text)))
        .await
        .map_err(|error| NodeError::Unreachable {
            url: "the data stream".to_string(),
            reason: error.to_string(),
        })
}

/// Read frames until an `Ack` arrives; return the byte total it carried.
///
/// Returns the *cumulative* total rather than adding anything, so a duplicated
/// or reordered ack cannot inflate the window — the same reason the brain sends
/// a cumulative total instead of a per-chunk delta.
async fn wait_for_ack(socket: &mut StreamSocket) -> Result<u64, NodeError> {
    loop {
        let next = tokio::time::timeout(ACK_TIMEOUT, socket.next()).await;
        let message = match next {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(error))) => {
                return Err(NodeError::Unreachable {
                    url: "the data stream".to_string(),
                    reason: error.to_string(),
                });
            }
            Ok(None) => {
                return Err(NodeError::Unreachable {
                    url: "the data stream".to_string(),
                    reason: "the brain closed the stream before acknowledging the transfer".into(),
                });
            }
            Err(_) => {
                return Err(NodeError::Unreachable {
                    url: "the data stream".to_string(),
                    reason: format!(
                        "no acknowledgement within {ACK_TIMEOUT:?}; the brain is not reading the stream"
                    ),
                });
            }
        };
        let WsMessage::Text(text) = message else {
            continue; // pings/pongs are tungstenite's business
        };
        let frame = StreamFrame::decode(text.as_str()).map_err(|error| NodeError::Unreachable {
            url: "the data stream".to_string(),
            reason: format!("the brain sent an unreadable frame: {error}"),
        })?;
        match frame {
            StreamFrame::Ack { bytes } => return Ok(bytes),
            StreamFrame::Error { code, reason } => {
                return Err(NodeError::Unreachable {
                    url: "the data stream".to_string(),
                    reason: format!("the brain refused the transfer: {code}: {reason}"),
                });
            }
            other => {
                return Err(NodeError::Unreachable {
                    url: "the data stream".to_string(),
                    reason: format!(
                        "the brain sent a `{}` frame while this node was sending; only `ack` and \
                         `error` are expected there",
                        other.tag()
                    ),
                });
            }
        }
    }
}

/// The URL a node dials for one stream.
///
/// Derived from the paired brain's origin, never from a payload: a transfer must
/// go to the brain that minted the token, not to whatever address a frame names.
pub fn stream_url(brain_url: &str, stream_id: &str) -> Result<String, NodeError> {
    crate::identity::stream_socket_url(brain_url, stream_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_url_is_the_brains_origin_plus_the_path() {
        let url = stream_url("http://127.0.0.1:8787", "abc123").expect("a valid URL");
        assert_eq!(url, "ws://127.0.0.1:8787/node/stream/abc123");
        // The secure scheme follows the brain's: a `wss` brain must not be
        // dialled over `ws` because a token rides this socket.
        let secure = stream_url("https://brain.example", "abc123").expect("a valid URL");
        assert_eq!(secure, "wss://brain.example/node/stream/abc123");
        // An unknown scheme is a ref usable error, not a guess.
        assert!(stream_url("ftp://brain", "abc").is_err());
    }
}
