//! The node-protocol [`Socket`] over axum's WebSocket.
//!
//! This adapter exists because of a version split, and it is the cheapest way
//! out of it: axum 0.8's `ws` feature speaks tungstenite 0.29 while
//! `swarmbot-node-protocol` speaks 0.30, so the crate's own
//! [`node_protocol::mux::WsSocket`] cannot wrap an axum socket. Rather than run a
//! second TCP listener outside the router (two places to bind, two places to shut
//! down, and no shared middleware), the mux's three-method [`Socket`] trait is
//! implemented over axum's socket directly.
//!
//! The alternative that was rejected: `accept_ws_hdr` on a raw `TcpStream`. It
//! would work, but it needs the HTTP upgrade answered by hand, which means
//! leaving the axum router — and the router is where the envelope, the trace
//! layer and the body limit live.

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::{SinkExt as _, StreamExt as _};

use swarmbot_node_protocol::mux::Socket;
use swarmbot_node_protocol::wire::ProtocolError;

/// A brain-side control socket: one connected node.
pub struct GatewaySocket {
    inner: WebSocket,
}

impl GatewaySocket {
    pub fn new(inner: WebSocket) -> Self {
        Self { inner }
    }
}

impl std::fmt::Debug for GatewaySocket {
    /// The wrapped socket has no useful `Debug`; a stable placeholder keeps
    /// error messages readable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GatewaySocket(..)")
    }
}

impl Socket for GatewaySocket {
    async fn send_text(&mut self, text: &str) -> Result<(), ProtocolError> {
        self.inner
            .send(Message::Text(text.to_string().into()))
            .await
            .map_err(|error| ProtocolError::ws(error.to_string()))
    }

    async fn recv_text(&mut self) -> Result<Option<String>, ProtocolError> {
        loop {
            match self.inner.next().await {
                Some(Ok(Message::Text(text))) => return Ok(Some(text.to_string())),
                // tungstenite queues the pong itself; flushing pushes it out even
                // when no data frame follows for a while. Sending a second pong
                // here would be a protocol violation.
                Some(Ok(Message::Ping(_))) => {
                    self.inner
                        .flush()
                        .await
                        .map_err(|error| ProtocolError::ws(error.to_string()))?;
                }
                // The control socket is text-only (§4.1): everything else is
                // ignored rather than worth killing a live connection over.
                Some(Ok(Message::Pong(_))) | Some(Ok(Message::Binary(_))) => {}
                Some(Ok(Message::Close(frame))) => {
                    tracing::debug!(
                        frame = ?frame.map(|frame| (frame.code, frame.reason.to_string())),
                        "the node closed the control socket"
                    );
                    return Ok(None);
                }
                Some(Err(error)) => return Err(ProtocolError::ws(error.to_string())),
                None => return Ok(None),
            }
        }
    }

    async fn close(&mut self, code: u16, reason: &str) -> Result<(), ProtocolError> {
        // Best effort by contract: the socket is going away, and a close frame
        // that cannot be delivered is not an error worth reporting.
        let frame = CloseFrame {
            code,
            reason: reason.to_string().into(),
        };
        let _ = self.inner.send(Message::Close(Some(frame))).await;
        let _ = self.inner.flush().await;
        let _ = self.inner.close().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_adapter_satisfies_the_trait_the_mux_needs() {
        // A compile-time contract, asserted so a change to either side fails here
        // with a name in the message rather than deep inside `Peer::new`.
        fn assert_socket<S: Socket>() {}
        assert_socket::<GatewaySocket>();
    }

    #[test]
    fn a_close_carries_the_code_the_node_reads() {
        // The node stops redialing permanently on 4401 and re-pairs on nothing
        // else, so the code is the whole message: assert the frame the adapter
        // builds is the one axum will put on the wire.
        let frame = CloseFrame {
            code: swarmbot_node_protocol::CLOSE_REVOKED,
            reason: "this node was revoked".to_string().into(),
        };
        assert_eq!(frame.code, 4401);
        assert_eq!(frame.reason.to_string(), "this node was revoked");
        assert_eq!(swarmbot_node_protocol::CLOSE_HELLO_FAILED, 4403);
        assert_eq!(swarmbot_node_protocol::CLOSE_NO_PROTOCOL, 4426);
        assert_eq!(swarmbot_node_protocol::CLOSE_REPLACED, 4000);
    }

    #[test]
    fn the_debug_impl_hides_the_socket() {
        // A socket holds a live connection; `WebSocket` has no `Debug` worth
        // printing and must never be rendered into a log line.
        fn assert_debug<T: std::fmt::Debug>() {}
        assert_debug::<GatewaySocket>();
    }
}
