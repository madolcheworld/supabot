//! [`LiveSubscription`]: the brain's live event socket, blocking.
//!
//! The socket delivers a snapshot, then upserts, then a `synchronized`
//! marker; every event carries a [`LiveCursor`]. **Reconnection is the
//! caller's job on purpose** — the CLI wants a simple fixed-delay retry, the
//! GPUI app wants exponential backoff on the UI thread's terms — so this
//! module only makes resuming easy: [`LiveSubscription::cursor`] always knows
//! the newest cursor seen, and passing it back into [`BrainClient::live`]
//! resumes (or re-snapshots, when the epoch changed) without a hole.
//!
//! [`BrainClient::live`]: crate::BrainClient::live

use tungstenite::Message;
use tungstenite::client::ClientRequestBuilder;
use tungstenite::http::Uri;
use tungstenite::protocol::WebSocket;
use tungstenite::stream::MaybeTlsStream;
use url::Url;

use swarmbot_contracts::{ConversationId, LiveCursor, LiveEvent};

use crate::error::ClientError;

/// How much of an undecodable frame is quoted in the error: enough to
/// recognize what the brain sent, short enough for one terminal line.
const DECODE_PREVIEW_CHARS: usize = 200;

/// Map a failed handshake: an HTTP refusal keeps its status (401 when the
/// token is wrong, 404 on a brain too old to serve `/ws/live`, …); anything
/// else never got an HTTP answer at all and is a transport failure.
fn handshake_error(url: &str, err: tungstenite::Error) -> ClientError {
    match err {
        tungstenite::Error::Http(response) => ClientError::from_response(
            response.status().as_u16(),
            "the brain refused the live upgrade",
        ),
        other => ClientError::Transport {
            url: url.to_string(),
            reason: other.to_string(),
        },
    }
}

/// An open `/ws/live` socket. Created by [`BrainClient::live`](crate::BrainClient::live).
#[derive(Debug)]
pub struct LiveSubscription {
    socket: WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    /// Kept for error messages only.
    url: String,
    /// The newest cursor any received event carried.
    cursor: Option<LiveCursor>,
    /// Set once the peer closed cleanly; later calls short-circuit to `None`
    /// instead of re-reading a terminated socket.
    done: bool,
}

/// Build the `/ws/live` URL: cursor and conversation travel as query
/// parameters, percent-encoded (`3:42` → `3%3A42`) so the brain parses back
/// exactly what was sent.
pub(crate) fn live_url(
    ws_base: &str,
    cursor: Option<LiveCursor>,
    conversation: Option<&ConversationId>,
) -> Result<String, ClientError> {
    let mut url =
        Url::parse(&format!("{}/ws/live", ws_base.trim_end_matches('/'))).map_err(|err| {
            ClientError::Transport {
                url: ws_base.to_string(),
                reason: format!("cannot build the live URL: {err}"),
            }
        })?;
    // Only touch the query when there is something to say: `query_pairs_mut`
    // would otherwise leave a bare `?` on the URL.
    if cursor.is_some() || conversation.is_some() {
        let mut pairs = url.query_pairs_mut();
        if let Some(cursor) = cursor {
            pairs.append_pair("cursor", &cursor.to_string());
        }
        if let Some(conversation) = conversation {
            pairs.append_pair("conversation", conversation.as_str());
        }
    }
    Ok(url.to_string())
}

impl LiveSubscription {
    /// Dial `/ws/live` on the brain.
    ///
    /// The token travels as an `Authorization` header: a WebSocket client
    /// cannot read ureq's cookie jar, so `login()`'s cookie does not help
    /// here — this is why [`BrainClient::login`](crate::BrainClient::login)
    /// remembers the token.
    pub(crate) fn connect(
        ws_base: &str,
        cursor: Option<LiveCursor>,
        conversation: Option<&ConversationId>,
        token: Option<&str>,
    ) -> Result<Self, ClientError> {
        let url = live_url(ws_base, cursor, conversation)?;
        Self::open(&url, token)
    }

    /// Open an arbitrary `ws://`/`wss://` URL. Split out from [`Self::connect`]
    /// so tests can point a subscription at stub paths that are not `/ws/live`.
    pub(crate) fn open(url: &str, token: Option<&str>) -> Result<Self, ClientError> {
        let uri: Uri = url.parse().map_err(|err| ClientError::Transport {
            url: url.to_string(),
            reason: format!("not a parseable WebSocket URL: {err}"),
        })?;
        let mut request = ClientRequestBuilder::new(uri);
        if let Some(token) = token {
            request = request.with_header("authorization", format!("Bearer {token}"));
        }
        match tungstenite::connect(request) {
            Ok((socket, _handshake)) => Ok(Self {
                socket,
                url: url.to_string(),
                cursor: None,
                done: false,
            }),
            Err(err) => Err(handshake_error(url, err)),
        }
    }

    /// The next live event; `None` once the brain closed the socket cleanly.
    ///
    /// Blocks until a frame arrives. Pings are answered with pongs (the
    /// tungstenite layer queues the pong; this method flushes it) and pings,
    /// pongs and control frames never surface as events. A text frame that is
    /// not a decodable [`LiveEvent`] is a [`ClientError::Decode`] quoting its
    /// first 200 chars — usually a brain newer than this client. Any other
    /// socket failure is [`ClientError::Transport`], after which the caller
    /// should reconnect with [`Self::cursor`].
    pub fn next_event(&mut self) -> Result<Option<LiveEvent>, ClientError> {
        if self.done {
            return Ok(None);
        }
        loop {
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    let text = text.as_str();
                    match serde_json::from_str::<LiveEvent>(text) {
                        Ok(event) => {
                            if let Some(cursor) = event.cursor() {
                                // Last seen wins, not max: a brain restart
                                // begins a new epoch at sequence 0, and that
                                // cursor is the one a resume must use.
                                self.cursor = Some(cursor);
                            }
                            return Ok(Some(event));
                        }
                        Err(err) => {
                            let preview: String = text.chars().take(DECODE_PREVIEW_CHARS).collect();
                            return Err(ClientError::Decode {
                                reason: format!("bad live frame `{preview}`: {err}"),
                            });
                        }
                    }
                }
                Ok(Message::Ping(_)) => {
                    // tungstenite queued the pong automatically; flush pushes
                    // it out even if no further frame arrives for a while.
                    let _ = self.socket.flush();
                }
                Ok(Message::Pong(_)) => {}
                Ok(Message::Close(_)) => {
                    // Drive the close handshake out, then report a clean end.
                    self.done = true;
                    let _ = self.socket.flush();
                    return Ok(None);
                }
                Ok(Message::Binary(_)) => {
                    return Err(ClientError::Decode {
                        reason: "the brain sent a binary frame; live events are JSON text"
                            .to_string(),
                    });
                }
                // `read()` does not surface raw frames, but the enum has the
                // variant: treat it as a heartbeat, not an event.
                Ok(Message::Frame(_)) => {}
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    self.done = true;
                    return Ok(None);
                }
                Err(err) => {
                    self.done = true;
                    return Err(ClientError::Transport {
                        url: self.url.clone(),
                        reason: err.to_string(),
                    });
                }
            }
        }
    }

    /// The newest cursor seen so far. Pass it to
    /// [`BrainClient::live`](crate::BrainClient::live) after a drop to resume
    /// (or re-snapshot, when the brain's epoch moved on) without a silent
    /// hole; `None` when no event carried one yet.
    pub fn cursor(&self) -> Option<LiveCursor> {
        self.cursor
    }

    /// Close the socket (best effort) and consume the subscription.
    pub fn close(mut self) {
        if !self.done {
            let _ = self.socket.close(None);
            let _ = self.socket.flush();
        }
    }
}

impl crate::BrainClient {
    /// Subscribe to the brain's live event stream over `/ws/live`.
    ///
    /// `cursor` resumes from a previous subscription
    /// ([`LiveSubscription::cursor`]); `None` asks for a full snapshot.
    /// `conversation` narrows the stream to one conversation when given. The
    /// login token rides along as a bearer header — see
    /// [`BrainClient::login`](crate::BrainClient::login).
    pub fn live(
        &self,
        cursor: Option<LiveCursor>,
        conversation: Option<&ConversationId>,
    ) -> Result<LiveSubscription, ClientError> {
        LiveSubscription::connect(
            &self.ws_url(),
            cursor,
            conversation,
            self.live_token().as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::BrainClient;
    use crate::testing::StubBrain;

    #[test]
    fn the_live_url_percent_encodes_cursor_and_conversation() {
        let conversation = ConversationId::from_suffix("000000000000000000000000000000c1");
        let url = live_url(
            "ws://127.0.0.1:9",
            Some(LiveCursor::new(3, 42)),
            Some(&conversation),
        )
        .expect("url builds");
        // `3:42` survives the round trip in its encoded form …
        assert!(url.contains("cursor=3%3A42"), "{url}");
        // … and so does the conversation id.
        assert!(
            url.contains(&format!("conversation={conversation}")),
            "{url}"
        );
        assert!(url.starts_with("ws://127.0.0.1:9/ws/live?"), "{url}");

        // No cursor, no conversation: no query at all.
        let bare = live_url("wss://brain.example/", None, None).expect("url builds");
        assert_eq!(bare, "wss://brain.example/ws/live");
    }

    #[test]
    fn events_arrive_in_order_and_the_cursor_tracks_the_newest() {
        let stub = StubBrain::start();
        let client = BrainClient::new(&stub.url()).expect("valid stub url");
        let mut live = client
            .live(Some(LiveCursor::new(1, 0)), None)
            .expect("connects");

        // The handshake request carried the (encoded) resume cursor.
        let recorded = stub.last_request("GET", "/ws/live").expect("recorded");
        assert_eq!(recorded.query, "cursor=1%3A0");

        let first = live.next_event().expect("event").expect("not closed");
        assert!(matches!(first, LiveEvent::Snapshot(_)));
        assert_eq!(live.cursor(), Some(LiveCursor::new(3, 1)));

        let second = live.next_event().expect("event").expect("not closed");
        match &second {
            LiveEvent::EntryUpsert { entry, .. } => {
                assert_eq!(entry.seq, 2);
            }
            other => panic!("expected an entry upsert, got {other:?}"),
        }
        assert_eq!(live.cursor(), Some(LiveCursor::new(3, 2)));

        let third = live.next_event().expect("event").expect("not closed");
        assert!(matches!(third, LiveEvent::Synchronized { .. }));
        assert_eq!(live.cursor(), Some(LiveCursor::new(3, 3)));

        // The stub closes after its canned frames: a clean end, not an error,
        // and repeated reads stay `None` instead of touching a dead socket.
        assert!(live.next_event().expect("clean close").is_none());
        assert!(live.next_event().expect("stays closed").is_none());
        assert_eq!(live.cursor(), Some(LiveCursor::new(3, 3)));
        live.close();
    }

    #[test]
    fn login_provides_the_bearer_token_for_the_upgrade() {
        let stub = StubBrain::start();
        let client = BrainClient::new(&stub.url()).expect("valid stub url");
        client.login("one-time-token").expect("login");
        let conversation = ConversationId::from_suffix("000000000000000000000000000000c1");
        let mut live = client
            .live(None, Some(&conversation))
            .expect("connects with the remembered token");
        let recorded = stub.last_request("GET", "/ws/live").expect("recorded");
        assert_eq!(recorded.auth.as_deref(), Some("Bearer one-time-token"));
        assert_eq!(recorded.query, format!("conversation={conversation}"));
        // Drain the canned frames so the stub's WS thread ends cleanly.
        while let Ok(Some(_)) = live.next_event() {}
        live.close();
    }

    #[test]
    fn an_immediately_closed_socket_yields_none() {
        let stub = StubBrain::start();
        let url = format!("{}/ws/silent", stub.ws_url());
        let mut live = LiveSubscription::open(&url, None).expect("handshake succeeds");
        assert!(live.next_event().expect("clean close").is_none());
        assert_eq!(live.cursor(), None, "no event ever carried a cursor");
        live.close();
    }

    #[test]
    fn a_frame_that_is_not_json_quotes_its_beginning() {
        let stub = StubBrain::start();
        let url = format!("{}/ws/garbage", stub.ws_url());
        let mut live = LiveSubscription::open(&url, None).expect("handshake succeeds");
        let err = live.next_event().expect_err("garbage cannot decode");
        match err {
            ClientError::Decode { reason } => {
                assert!(reason.contains("this is not json {{{"), "{reason}");
            }
            other => panic!("expected Decode, got {other:?}"),
        }
        live.close();
    }

    #[test]
    fn a_refused_upgrade_becomes_unauthorized() {
        let stub = StubBrain::start();
        let url = format!("{}/ws/denied", stub.ws_url());
        let err = LiveSubscription::open(&url, Some("bad-token")).expect_err("refused");
        assert!(matches!(err, ClientError::Unauthorized), "{err:?}");
    }

    #[test]
    fn a_refused_upgrade_keeps_any_other_http_status() {
        let refusal = Box::new(
            tungstenite::http::Response::builder()
                .status(404)
                .body(None)
                .expect("response builds"),
        );
        let err = handshake_error(
            "ws://127.0.0.1:1/ws/live",
            tungstenite::Error::Http(refusal),
        );
        match err {
            ClientError::Http { status, .. } => assert_eq!(status, 404),
            other => panic!("expected Http, got {other:?}"),
        }
    }
}
