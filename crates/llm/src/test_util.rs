//! Test doubles for the LLM layer — compiled into the crate on purpose, so
//! downstream crates (brain tests, the offline demo) can script HTTP-level
//! behaviour without touching a socket.
//!
//! [`ScriptedTransport`] is an [`HttpTransport`] that answers from a queue of
//! canned responses and records every request it received. The invariant it
//! guarantees: **a test can never hang on it** — when the script runs out it
//! fails loudly instead of pending forever.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use http::Response;
use serde_json::Value;

use crate::openai::{BodyStream, HttpError, HttpTransport};
use crate::types::RedactedHeaders;

/// One canned exchange. Consumed in order, one per attempt — so a retry test
/// scripts one response per expected attempt.
#[derive(Clone, Debug)]
pub enum ScriptedResponse {
    /// A 2xx-style response whose raw body chunks are delivered to the stream
    /// in order. The chunks are *raw SSE text*: framing happens above the
    /// transport (in `OpenAiCompat`), exactly like the real path, so a chunk
    /// may split a `data:` line anywhere.
    Sse { status: u16, chunks: Vec<String> },

    /// A non-2xx response with a buffered body, mirroring what
    /// [`crate::openai::ReqwestTransport`] does with error statuses.
    Status {
        status: u16,
        body: String,
        retry_after_secs: Option<u64>,
    },

    /// A transport-level failure (connect refused, DNS, TLS, …).
    Fail { reason: String },

    /// A 200 whose stream yields `chunks` and then pends forever: the fixture
    /// for cancellation and timeout tests.
    HangAfter { chunks: Vec<String> },

    /// A 200 whose stream yields `chunks` and then fails mid-body (a reset
    /// after partial output): exercises the retry-restarts-stream contract.
    StreamFail { chunks: Vec<String>, reason: String },
}

impl ScriptedResponse {
    /// A 200 SSE response from raw body chunks.
    pub fn sse(chunks: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::Sse {
            status: 200,
            chunks: chunks.into_iter().map(Into::into).collect(),
        }
    }

    /// A non-2xx response with a buffered body.
    pub fn status(status: u16, body: impl Into<String>) -> Self {
        Self::Status {
            status,
            body: body.into(),
            retry_after_secs: None,
        }
    }

    /// A non-2xx response carrying a numeric `Retry-After`.
    pub fn status_retry_after(status: u16, body: impl Into<String>, secs: u64) -> Self {
        Self::Status {
            status,
            body: body.into(),
            retry_after_secs: Some(secs),
        }
    }

    pub fn fail(reason: impl Into<String>) -> Self {
        Self::Fail {
            reason: reason.into(),
        }
    }

    /// Yield these chunks, then hang forever.
    pub fn hang_after(chunks: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::HangAfter {
            chunks: chunks.into_iter().map(Into::into).collect(),
        }
    }

    /// Yield these chunks, then fail mid-stream.
    pub fn stream_fail(
        chunks: impl IntoIterator<Item = impl Into<String>>,
        reason: impl Into<String>,
    ) -> Self {
        Self::StreamFail {
            chunks: chunks.into_iter().map(Into::into).collect(),
            reason: reason.into(),
        }
    }
}

/// One request as the transport saw it. Header values of secret-bearing names
/// are masked in `Debug` (the recorder must not become a leak), but remain
/// readable through [`RecordedRequest::header`] for assertions.
#[derive(Clone, PartialEq)]
pub struct RecordedRequest {
    pub method: String,
    pub url: String,
    headers: Vec<(String, String)>,
    pub body: Option<Value>,
}

impl RecordedRequest {
    /// Case-insensitive header lookup — how tests assert that an API key or a
    /// gateway header actually reached the wire.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Header names in wire order, for "exactly these headers" assertions.
    pub fn header_names(&self) -> Vec<&str> {
        self.headers.iter().map(|(name, _)| name.as_str()).collect()
    }
}

impl fmt::Debug for RecordedRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordedRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &RedactedHeaders(&self.headers))
            .field("body", &self.body)
            .finish()
    }
}

/// An [`HttpTransport`] answering from a script and recording what it was
/// asked. Interior mutability throughout: shared as `Arc<ScriptedTransport>`
/// while the adapter under test holds it as `Arc<dyn HttpTransport>`.
#[derive(Debug)]
pub struct ScriptedTransport {
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    responses: VecDeque<ScriptedResponse>,
    requests: Vec<RecordedRequest>,
}

impl ScriptedTransport {
    pub fn new(responses: Vec<ScriptedResponse>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                responses: VecDeque::from(responses),
                requests: Vec::new(),
            }),
        })
    }

    /// Every request received so far, in order (POST attempts and GET probes).
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state
            .lock()
            .map(|state| state.requests.clone())
            .unwrap_or_default()
    }

    /// POST attempts (chat calls, including retries), excluding probes.
    pub fn post_count(&self) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.method == "POST")
            .count()
    }

    fn record(&self, method: &str, url: &str, headers: Vec<(String, String)>, body: Option<Value>) {
        if let Ok(mut state) = self.state.lock() {
            state.requests.push(RecordedRequest {
                method: method.to_string(),
                url: url.to_string(),
                headers,
                body,
            });
        }
    }

    fn next_response(&self) -> Option<ScriptedResponse> {
        self.state
            .lock()
            .ok()
            .and_then(|mut state| state.responses.pop_front())
    }
}

#[async_trait]
impl HttpTransport for ScriptedTransport {
    async fn post_sse(
        &self,
        url: &str,
        headers: Vec<(String, String)>,
        body: Value,
    ) -> Result<Response<BodyStream>, HttpError> {
        self.record("POST", url, headers, Some(body));
        match self.next_response() {
            // Script exhausted: fail loudly so a miscounted retry shows up as
            // an error instead of a hung test.
            None => Err(HttpError::transport(
                "scripted responses exhausted (one response per attempt)",
            )),
            Some(ScriptedResponse::Sse { status, chunks }) => Response::builder()
                .status(status)
                .body(body_stream(chunks, false))
                .map_err(|error| HttpError::transport(error.to_string())),
            Some(ScriptedResponse::Status {
                status,
                body,
                retry_after_secs,
            }) => Err(HttpError::Status {
                status,
                body,
                retry_after_secs,
            }),
            Some(ScriptedResponse::Fail { reason }) => Err(HttpError::Transport { reason }),
            Some(ScriptedResponse::HangAfter { chunks }) => Response::builder()
                .status(200)
                .body(body_stream(chunks, true))
                .map_err(|error| HttpError::transport(error.to_string())),
            Some(ScriptedResponse::StreamFail { chunks, reason }) => {
                let items = chunks
                    .into_iter()
                    .map(|chunk| Ok(chunk.into_bytes()))
                    .chain([Err(HttpError::Transport { reason })])
                    .collect::<Vec<Result<Vec<u8>, HttpError>>>();
                let stream: BodyStream = Box::pin(futures::stream::iter(items));
                Response::builder()
                    .status(200)
                    .body(stream)
                    .map_err(|error| HttpError::transport(error.to_string()))
            }
        }
    }

    async fn get_text(
        &self,
        url: &str,
        headers: Vec<(String, String)>,
    ) -> Result<Response<String>, HttpError> {
        self.record("GET", url, headers, None);
        match self.next_response() {
            None => Err(HttpError::transport(
                "scripted responses exhausted (one response per attempt)",
            )),
            Some(ScriptedResponse::Sse { status, chunks }) => Response::builder()
                .status(status)
                .body(chunks.join(""))
                .map_err(|error| HttpError::transport(error.to_string())),
            Some(ScriptedResponse::Status {
                status,
                body,
                retry_after_secs,
            }) => Err(HttpError::Status {
                status,
                body,
                retry_after_secs,
            }),
            Some(ScriptedResponse::Fail { reason }) => Err(HttpError::Transport { reason }),
            Some(ScriptedResponse::HangAfter { .. })
            | Some(ScriptedResponse::StreamFail { .. }) => {
                // A hung probe must behave like a hung stream: pend forever
                // (a timeout/cancel test owns the way out).
                std::future::pending::<()>().await;
                Err(HttpError::transport("unreachable"))
            }
        }
    }
}

fn body_stream(chunks: Vec<String>, hang: bool) -> BodyStream {
    let items = chunks
        .into_iter()
        .map(|chunk| Ok(chunk.into_bytes()))
        .collect::<Vec<Result<Vec<u8>, HttpError>>>();
    if hang {
        Box::pin(futures::stream::iter(items).chain(futures::stream::pending()))
    } else {
        Box::pin(futures::stream::iter(items))
    }
}

/// Build SSE wire text from `data` payloads: each becomes `data: <payload>`
/// terminated by a blank line, the framing an OpenAI-compatible server sends.
pub fn sse_text(payloads: impl IntoIterator<Item = impl AsRef<str>>) -> String {
    let mut out = String::new();
    for payload in payloads {
        out.push_str("data: ");
        out.push_str(payload.as_ref());
        out.push_str("\n\n");
    }
    out
}
