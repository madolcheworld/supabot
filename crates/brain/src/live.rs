//! `/ws/live`: the live event socket.
//!
//! The hub does the replication work — [`swarmbot_core::LiveHub::subscribe`]
//! sends a snapshot (or a replay from a cursor), then a `synchronized` marker,
//! then live events — so this module is a forwarder with three jobs of its own:
//!
//! * **serialize one [`LiveEvent`] per text frame.** JSON text, never binary:
//!   a binary frame is a decode error to the client, by design, so it notices a
//!   protocol change instead of guessing at one.
//! * **end with a close frame.** Dropping the socket sends a bare TCP FIN, which
//!   a WebSocket client reports as `ResetWithoutClosingHandshake` — an error
//!   where the truth is "the brain stopped". A 1001 close says it plainly.
//! * **stop when either side goes away**: the client closes, the socket write
//!   fails, or the daemon was asked to shut down. A leaked subscriber would keep
//!   the hub's forwarder task alive for the life of the process.
//!
//! Authorization already happened in the route layer, including the `Origin`
//! check that a browser upgrade needs (see [`crate::routes::require_auth`]).

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use futures::{SinkExt as _, StreamExt as _};
use serde::Deserialize;
use tracing::{debug, info, warn};

use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::ConversationId;
use swarmbot_contracts::live::{LiveCursor, LiveEvent};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// How often an idle socket is pinged.
///
/// A reverse proxy in front of the brain drops a connection that carries nothing
/// for a minute or two, and the client would see a transport error rather than a
/// quiet stream. 30 s is inside every default idle timeout this crate knows of.
const KEEPALIVE: Duration = Duration::from_secs(30);

/// The `?cursor=&conversation=` query.
///
/// Both are optional and both arrive percent-encoded (`3:42` → `3%3A42`), which
/// axum's `Query` extractor decodes before serde sees it.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LiveQuery {
    /// `<epoch>:<sequence>`; resume from here instead of taking a snapshot.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Narrow the stream to one conversation.
    #[serde(default)]
    pub conversation: Option<String>,
}

/// `GET /ws/live`
pub async fn handler(
    State(state): State<AppState>,
    Query(query): Query<LiveQuery>,
    upgrade: WebSocketUpgrade,
) -> ApiResult<Response> {
    let cursor = match query.cursor.as_deref() {
        Some(text) => Some(text.parse::<LiveCursor>().map_err(ApiError::from)?),
        None => None,
    };
    let conversation = match query.conversation.as_deref() {
        Some(text) => Some(
            ConversationId::parse(text)
                .map_err(|err| ApiError::from(DomainError::not_found("conversation", err.value)))?,
        ),
        None => None,
    };
    debug!(
        cursor = %cursor.map(|c| c.to_string()).unwrap_or_else(|| "<snapshot>".into()),
        conversation = %conversation.as_ref().map(ConversationId::to_string).unwrap_or_else(|| "<all>".into()),
        "a live subscriber is upgrading"
    );
    Ok(upgrade.on_upgrade(move |socket| forward(state, socket, cursor, conversation)))
}

/// Why a socket ended, as the close frame's reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// The client sent a close frame, or the TCP connection went away.
    Client,
    /// The daemon was asked to stop.
    Shutdown,
    /// Writing failed: the peer is gone.
    Broken,
    /// The hub's forwarder stopped, which only happens when the hub goes away.
    HubGone,
    /// A frame could not be serialized — a bug, and reported as one.
    Internal,
}

impl Ending {
    /// 1001 "going away" for an orderly end, 1011 for one that was not.
    fn close_frame(self) -> CloseFrame {
        match self {
            Self::Client => CloseFrame {
                code: 1000,
                reason: "the client closed the socket".into(),
            },
            Self::Shutdown => CloseFrame {
                code: 1001,
                reason: "the brain is stopping".into(),
            },
            Self::HubGone => CloseFrame {
                code: 1001,
                reason: "the brain's event log went away".into(),
            },
            Self::Broken | Self::Internal => CloseFrame {
                code: 1011,
                reason: "the live socket broke".into(),
            },
        }
    }
}

/// Pump events until one side is done, then close cleanly.
async fn forward(
    state: AppState,
    socket: WebSocket,
    cursor: Option<LiveCursor>,
    conversation: Option<ConversationId>,
) {
    let mut events = state.swarm.hub.subscribe(cursor, conversation);
    let (mut writer, mut reader) = socket.split();
    // The first tick of a plain `interval` fires at once, which would ping a
    // socket that was opened a microsecond ago.
    let mut keepalive =
        tokio::time::interval_at(tokio::time::Instant::now() + KEEPALIVE, KEEPALIVE);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ending = Ending::Client;

    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else {
                    ending = Ending::HubGone;
                    break;
                };
                let text = match serde_json::to_string(&event) {
                    Ok(text) => text,
                    Err(err) => {
                        // A LiveEvent that will not serialize is a bug in this
                        // crate, not something to blame the client for; the
                        // socket says so and ends.
                        warn!(error = %err, "cannot serialize a live event");
                        ending = Ending::Internal;
                        break;
                    }
                };
                if let Err(err) = writer.send(Message::text(text)).await {
                    debug!(error = %err, "the live socket is gone");
                    ending = Ending::Broken;
                    break;
                }
            }
            frame = reader.next() => {
                match frame {
                    // A ping is answered by tungstenite itself, which queues the
                    // pong; flushing pushes it out even if no event follows for
                    // a while. Sending a second pong here would be a duplicate.
                    Some(Ok(Message::Ping(_))) => {
                        if writer.flush().await.is_err() {
                            ending = Ending::Broken;
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {
                        // Pongs, and any text or binary a client sends: this
                        // socket is a one-way feed, so there is nothing to do.
                    }
                    Some(Err(err)) => {
                        debug!(error = %err, "the live socket ended while reading");
                        break;
                    }
                }
            }
            _ = keepalive.tick() => {
                if writer.send(Message::Ping(Bytes::from_static(b"swarmbot"))).await.is_err() {
                    ending = Ending::Broken;
                    break;
                }
            }
            _ = state.wait_for_shutdown() => {
                ending = Ending::Shutdown;
                break;
            }
        }
    }

    if ending != Ending::Client {
        info!(reason = ?ending, "a live subscriber is being closed");
    }
    // The close frame is the difference between "the brain stopped" and "the
    // connection reset" on the client, so it is sent even when the ending was
    // orderly — and a failure here is only worth a debug line, because there is
    // nobody left to tell.
    let _ = writer
        .send(Message::Close(Some(ending.close_frame())))
        .await;
    let _ = writer.flush().await;
    let _ = writer.close().await;
}

/// A live event is always one JSON text frame; the tests assert the shape.
pub fn frame_of(event: &LiveEvent) -> ApiResult<String> {
    serde_json::to_string(event)
        .map_err(|err| ApiError::internal(format!("cannot serialize a live event: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_takes_both_parameters_and_needs_neither() {
        let empty: LiveQuery = serde_json::from_value(serde_json::json!({})).expect("parses");
        assert!(empty.cursor.is_none() && empty.conversation.is_none());
        let both: LiveQuery = serde_json::from_value(serde_json::json!({
            "cursor": "3:42",
            "conversation": "cnv_0189fbd3a4d37f0e9c0e6a3f8f1c9d2e",
        }))
        .expect("parses");
        assert_eq!(both.cursor.as_deref(), Some("3:42"));
    }

    #[test]
    fn a_cursor_string_is_parsed_by_the_domain_type() {
        assert_eq!(
            "3:42".parse::<LiveCursor>().expect("parses"),
            LiveCursor::new(3, 42)
        );
        assert!("3".parse::<LiveCursor>().is_err());
        // The percent-encoded form arrives decoded, so this is what the handler
        // sees for `cursor=3%3A42`.
        assert_eq!("3:42", "3%3A42".replace("%3A", ":"));
    }

    #[test]
    fn endings_close_with_an_honest_code() {
        assert_eq!(Ending::Client.close_frame().code, 1000);
        assert_eq!(Ending::Shutdown.close_frame().code, 1001);
        assert_eq!(Ending::Broken.close_frame().code, 1011);
        assert_eq!(Ending::Internal.close_frame().code, 1011);
        assert!(Ending::Shutdown.close_frame().reason.contains("stopping"));
    }

    #[test]
    fn one_event_is_one_text_frame() {
        let event = LiveEvent::Synchronized {
            cursor: LiveCursor::new(1, 7),
        };
        let frame = frame_of(&event).expect("serializes");
        assert_eq!(
            frame,
            r#"{"type":"synchronized","cursor":{"epoch":1,"sequence":7}}"#
        );
        assert_eq!(
            serde_json::from_str::<LiveEvent>(&frame).expect("decodes"),
            event
        );
    }
}
