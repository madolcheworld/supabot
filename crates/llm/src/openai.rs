//! The OpenAI-compatible adapter: SSE streaming plus native tool calls.
//!
//! # Transport injection
//!
//! All I/O goes through [`HttpTransport`], a two-method port over HTTP. The
//! real implementation is [`ReqwestTransport`]; tests inject
//! [`crate::test_util::ScriptedTransport`], so no test ever touches a socket.
//!
//! Design choice and its trade-off: the transport returns the **raw body as a
//! stream of byte chunks** (`Vec<u8>` items, not `bytes::Bytes`, which this
//! crate cannot name without depending on `bytes` directly — one small copy
//! per chunk is the price). SSE *framing* happens above the transport, in
//! [`OpenAiCompat`], via `eventsource-stream`. That placement matters for two
//! reasons:
//!
//! 1. Error bodies are plain JSON, **not** SSE — if the transport framed
//!    everything, a 402's `no_provider_under_bid` body would be discarded by
//!    the SSE parser instead of reaching the operator. So the transport
//!    buffers non-2xx bodies into [`HttpError::Status`] and streams 2xx bodies
//!    untouched.
//! 2. Framing above the transport means the *same* parser (line buffering,
//!    split JSON lines, comments, `[DONE]`) runs against `ScriptedTransport`
//!    in tests and against a live gateway in production.
//!
//! Streaming stays incremental: chunks flow off the socket and into
//! [`LlmEvent`] deltas as they arrive; nothing buffers the full completion.
//!
//! # Retry semantics
//!
//! A retry **restarts the stream**. Deltas emitted by a failed attempt were
//! already sent and cannot be retracted, so the contract is: callers treat
//! [`LlmEvent`] deltas as best-effort progress and the returned
//! [`ChatResponse`] as authoritative. Every retry is announced with
//! [`LlmEvent::Restart`] *before* the retried attempt produces its first byte:
//! that is the consumer's instruction to discard what it rendered for this
//! turn, so a live transcript shows one answer instead of two attempts glued
//! together.
//!
//! # What counts as a complete stream
//!
//! A turn is complete only when the server said so: `data: [DONE]` (the [OI]
//! terminator) or a `finish_reason` on a choice. A stream that simply stops —
//! EOF with neither — is refused as [`LlmError::Malformed`], because a proxy
//! closing mid-answer is byte-for-byte indistinguishable from a finished
//! stream, and committing the half reply would put text in the transcript that
//! the model never finished.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use http::Response;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use swarmbot_contracts::{BackendKind, BackendSpec, ToolSpec, Usage};

use crate::backend::{LlmBackend, LlmError, emit};
use crate::types::{
    BackendStatus, Capabilities, ChatMessage, ChatRequest, ChatResponse, FinishReason, LlmEvent,
    ModelList, ModelOption, RedactedHeaders, Role, ToolCall,
};

/// Error bodies are quoted in operator-facing messages: cap what we keep.
const ERROR_BODY_LIMIT: usize = 2048;

/// The transport buffers failed-response bodies; cap that generously so a
/// gateway returning a huge error page cannot exhaust memory.
const TRANSPORT_ERROR_BODY_LIMIT: usize = 16 * 1024;

/// Exponential backoff floor and ceiling.
const BASE_BACKOFF_MS: u64 = 500;
const BACKOFF_CAP_MS: u64 = 8_000;

/// A server asking for an absurd wait gets capped: a turn must not freeze
/// longer than the operator's own timeout would allow anyway.
const RETRY_AFTER_CAP_SECS: u64 = 30;

/// A response body as it arrives: raw byte chunks. `Vec<u8>` instead of
/// `bytes::Bytes` because `bytes` is not a direct dependency of this crate
/// (see the module docs for the trade-off).
pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, HttpError>> + Send + 'static>>;

/// Transport-level HTTP failures. Everything above this layer speaks
/// [`LlmError`]; this enum is the boundary.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum HttpError {
    /// The exchange failed before or during (DNS, connect, TLS, a reset
    /// mid-stream), or the transport itself misbehaved. Always retryable.
    #[error("transport failure: {reason}")]
    Transport { reason: String },

    /// A non-2xx response. The transport buffered the (small) error body so
    /// the adapter can quote it, and parsed `Retry-After` when it was the
    /// numeric-seconds form.
    #[error("http {status}: {body}")]
    Status {
        status: u16,
        body: String,
        retry_after_secs: Option<u64>,
    },
}

impl HttpError {
    pub fn transport(reason: impl Into<String>) -> Self {
        Self::Transport {
            reason: reason.into(),
        }
    }
}

/// The HTTP port. One implementation talks to real servers
/// ([`ReqwestTransport`]); tests script it
/// ([`crate::test_util::ScriptedTransport`]).
///
/// Convention (both implementations honour it, and [`OpenAiCompat`] relies on
/// it): a non-2xx response comes back as `Err(HttpError::Status)` with the
/// body buffered; `Ok` carries a 2xx response whose body streams as it
/// arrives.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// One request → the raw SSE body as a byte-chunk stream, or an HTTP
    /// error with its body.
    async fn post_sse(
        &self,
        url: &str,
        headers: Vec<(String, String)>,
        body: Value,
    ) -> Result<Response<BodyStream>, HttpError>;

    /// One GET with a fully buffered text body. Used by `probe` only, which
    /// must never spend a completion and never streams.
    async fn get_text(
        &self,
        url: &str,
        headers: Vec<(String, String)>,
    ) -> Result<Response<String>, HttpError>;
}

/// The real transport: `reqwest` with a streaming body.
///
/// Note there is **no total request timeout here**: SSE completions can
/// legitimately run for minutes, so the whole-call deadline is enforced by
/// [`OpenAiCompat::chat`] (`BackendSpec::timeout_secs`) where cancellation and
/// retries live too. Only the connect phase gets its own timeout so a dead
/// host fails fast instead of eating the whole call budget.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, HttpError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|error| HttpError::transport(format!("cannot build http client: {error}")))?;
        Ok(Self { client })
    }

    /// Wrap an existing client (proxies, custom TLS, connection pooling
    /// shared with the rest of the process).
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    fn apply_headers(
        mut builder: reqwest::RequestBuilder,
        headers: &[(String, String)],
    ) -> reqwest::RequestBuilder {
        for (name, value) in headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn post_sse(
        &self,
        url: &str,
        headers: Vec<(String, String)>,
        body: Value,
    ) -> Result<Response<BodyStream>, HttpError> {
        let encoded = serde_json::to_vec(&body).map_err(|error| {
            HttpError::transport(format!("request body is not serializable: {error}"))
        })?;
        let builder = Self::apply_headers(self.client.post(url), &headers);
        let response = builder
            .body(encoded)
            .send()
            .await
            .map_err(|error| HttpError::transport(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(response).await);
        }
        let mut built = Response::builder().status(status.as_u16());
        for (name, value) in response.headers().iter() {
            built = built.header(name.clone(), value.clone());
        }
        // `bytes::Bytes` → `Vec<u8>`: the one copy the injection design pays
        // (see the module docs).
        let stream: BodyStream = Box::pin(
            response
                .bytes_stream()
                .map(|chunk| chunk.map(|bytes| bytes.to_vec()).map_err(HttpError::from)),
        );
        built
            .body(stream)
            .map_err(|error| HttpError::transport(format!("cannot assemble response: {error}")))
    }

    async fn get_text(
        &self,
        url: &str,
        headers: Vec<(String, String)>,
    ) -> Result<Response<String>, HttpError> {
        let builder = Self::apply_headers(self.client.get(url), &headers);
        let response = builder
            .send()
            .await
            .map_err(|error| HttpError::transport(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(response).await);
        }
        let text = response
            .text()
            .await
            .map_err(|error| HttpError::transport(error.to_string()))?;
        Response::builder()
            .status(status.as_u16())
            .body(text)
            .map_err(|error| HttpError::transport(format!("cannot assemble response: {error}")))
    }
}

impl From<reqwest::Error> for HttpError {
    fn from(error: reqwest::Error) -> Self {
        // reqwest errors never carry the request body or headers in Display,
        // and our Authorization header is the only secret in flight.
        HttpError::transport(error.to_string())
    }
}

/// Buffer a non-2xx response into [`HttpError::Status`], including a parsed
/// numeric `Retry-After`.
async fn status_error(response: reqwest::Response) -> HttpError {
    let status = response.status().as_u16();
    let retry_after_secs = response
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        // Only the delay-seconds form is honoured; the HTTP-date form is rare
        // enough not to be worth date parsing for a hint.
        .and_then(|text| text.trim().parse::<f64>().ok())
        .filter(|secs| secs.is_finite() && *secs >= 0.0)
        .map(|secs| secs as u64);
    let body = match response.text().await {
        Ok(text) => truncate_utf8(&text, TRANSPORT_ERROR_BODY_LIMIT),
        Err(error) => format!("<error body unreadable: {error}>"),
    };
    HttpError::Status {
        status,
        body,
        retry_after_secs,
    }
}

/// Frame a raw byte-chunk stream into SSE `data` payloads with
/// `eventsource-stream`. Shared by every transport, so the parser that runs
/// in production is the one tests exercise.
///
/// Trade-off (SSE spec, and `eventsource-stream` follows it): an event is
/// dispatched on its terminating blank line, so a stream that *ends* mid
/// event drops that final event. OpenAI-compatible servers terminate every
/// event; if one ever failed to, we would lose at most the trailing `usage`
/// chunk. The truncation itself cannot pass silently: `consume_sse` refuses a
/// stream that ends without `[DONE]` or a `finish_reason`, so a dropped tail is
/// an error rather than a short answer.
fn frame_sse(body: BodyStream) -> impl Stream<Item = Result<String, HttpError>> {
    use eventsource_stream::{EventStreamError, Eventsource};
    body.eventsource().map(|event| match event {
        Ok(event) => Ok(event.data),
        Err(EventStreamError::Transport(error)) => Err(error),
        Err(other) => Err(HttpError::transport(other.to_string())),
    })
}

/// The OpenAI-compatible backend: any `/chat/completions` endpoint that
/// speaks SSE deltas and native tool calls (OpenAI itself, inferhub, llama
/// servers, vLLM, …).
///
/// `Debug` is manual: the API key must never reach a log line, a transcript
/// or an error message — only its presence and its configured source.
pub struct OpenAiCompat {
    spec: BackendSpec,
    /// Normalized: no trailing slash, validated http(s).
    base_url: String,
    key: Option<String>,
    transport: Arc<dyn HttpTransport>,
}

impl fmt::Debug for OpenAiCompat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompat")
            .field("id", &self.spec.id)
            .field("kind", &self.spec.kind)
            .field("base_url", &self.base_url)
            .field("model", &self.spec.model)
            .field("api_key_env", &self.spec.api_key_env)
            // Invariant: the key itself is never rendered by anything this
            // crate implements — Debug, Display, logs, errors or serde.
            .field("api_key", &self.key.as_ref().map(|_| "[redacted]"))
            .field("timeout_secs", &self.spec.timeout_secs)
            .field("max_retries", &self.spec.max_retries)
            .field("extra_body", &self.spec.extra_body)
            .field("extra_headers", &RedactedHeaders(&self.spec.extra_headers))
            .finish()
    }
}

impl OpenAiCompat {
    /// Build from a spec, an explicit key and an injected transport. An empty
    /// or whitespace key counts as absent.
    pub fn new(
        spec: BackendSpec,
        key: Option<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, LlmError> {
        if !matches!(spec.kind, BackendKind::OpenAiCompat) {
            return Err(LlmError::Unavailable {
                backend: spec.id.clone(),
                reason: format!("spec kind {:?} is not openai_compat", spec.kind),
            });
        }
        let base_url = spec
            .base_url
            .as_deref()
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string();
        let valid = url::Url::parse(&base_url)
            .ok()
            .is_some_and(|url| matches!(url.scheme(), "http" | "https"));
        if !valid {
            return Err(LlmError::Unavailable {
                backend: spec.id.clone(),
                reason: format!(
                    "openai_compat needs an http(s) base_url, got {:?}",
                    spec.base_url
                ),
            });
        }
        Ok(Self {
            spec,
            base_url,
            key: key.filter(|key| !key.trim().is_empty()),
            transport,
        })
    }

    /// Build from a spec plus the real transport, reading the key from
    /// whatever `env` resolves (the brain passes its secrets store falling
    /// back to the process environment).
    pub fn from_spec(
        spec: BackendSpec,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, LlmError> {
        let key = spec
            .api_key_env
            .as_deref()
            .and_then(env)
            .filter(|key| !key.trim().is_empty());
        let transport =
            Arc::new(
                ReqwestTransport::new().map_err(|error| LlmError::Unavailable {
                    backend: spec.id.clone(),
                    reason: error.to_string(),
                })?,
            );
        Self::new(spec, key, transport)
    }

    pub fn spec(&self) -> &BackendSpec {
        &self.spec
    }

    /// Whether a key is present. A spec without `api_key_env` (an unauthenticated
    /// local gateway) is "ready" without one.
    pub fn has_key(&self) -> bool {
        self.key.is_some() || self.spec.api_key_env.is_none()
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn models_url(&self) -> String {
        format!("{}/models", self.base_url)
    }

    fn missing_key(&self) -> LlmError {
        LlmError::MissingKey {
            env: self
                .spec
                .api_key_env
                .clone()
                .unwrap_or_else(|| "<no api_key_env configured>".to_string()),
        }
    }

    /// Headers in wire order; later entries replace earlier ones by name so a
    /// caller's `extra_headers` can even swap out the auth scheme.
    fn build_headers(&self, extra: &BTreeMap<String, String>, sse: bool) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = Vec::new();
        push_header(&mut headers, "content-type", "application/json");
        push_header(
            &mut headers,
            "accept",
            if sse {
                "text/event-stream"
            } else {
                "application/json"
            },
        );
        if let Some(key) = &self.key {
            push_header(&mut headers, "authorization", &format!("Bearer {key}"));
        }
        for (name, value) in &self.spec.extra_headers {
            push_header(&mut headers, name, value);
        }
        for (name, value) in extra {
            push_header(&mut headers, name, value);
        }
        headers
    }

    fn build_body(&self, request: &ChatRequest) -> Result<Value, LlmError> {
        let model = if request.model.trim().is_empty() {
            self.spec.model.clone().ok_or_else(|| LlmError::Malformed {
                backend: self.spec.id.clone(),
                reason: "no model: the request names none and the backend spec has no default"
                    .to_string(),
            })?
        } else {
            request.model.clone()
        };

        let mut messages: Vec<Value> = Vec::with_capacity(request.messages.len() + 1);
        if !request.instructions.trim().is_empty() {
            // The caller assembles the system prompt; it always goes first.
            messages.push(json!({ "role": "system", "content": request.instructions }));
        }
        messages.extend(request.messages.iter().map(message_to_json));

        let mut body = Map::new();
        body.insert("model".into(), Value::String(model));
        body.insert("messages".into(), Value::Array(messages));
        body.insert("stream".into(), Value::Bool(true));
        // Without this the final usage chunk is omitted by OpenAI-compatible
        // servers, and turns would be accounted as zero-cost.
        body.insert("stream_options".into(), json!({ "include_usage": true }));
        if let Some(max_tokens) = request.max_tokens {
            body.insert("max_tokens".into(), json!(max_tokens));
        }
        if let Some(temperature) = request.temperature {
            body.insert("temperature".into(), json!(temperature));
        }
        if !request.tools.is_empty() {
            body.insert(
                "tools".into(),
                Value::Array(request.tools.iter().map(tool_to_json).collect()),
            );
        }
        // Gateway parameters merge LAST so they win over every built-in key:
        // the invariant that lets e.g. inferhub's bid travel through this
        // crate without it knowing the parameter exists. Per-call
        // `extra_body` overrides spec-level `extra_body`.
        for (key, value) in &self.spec.extra_body {
            body.insert(key.clone(), value.clone());
        }
        for (key, value) in &request.extra_body {
            body.insert(key.clone(), value.clone());
        }
        Ok(Value::Object(body))
    }

    /// The retry loop around single attempts.
    async fn run(
        &self,
        request: ChatRequest,
        events: mpsc::Sender<LlmEvent>,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, LlmError> {
        let url = self.chat_url();
        let headers = self.build_headers(&request.extra_headers, true);
        let body = self.build_body(&request)?;
        let max_retries = self.spec.max_retries;

        let mut pending_delay: Option<Duration> = None;
        let mut last_error: Option<LlmError> = None;

        for attempt in 0..=max_retries {
            if let Some(delay) = pending_delay.take() {
                // Waiting for a retry must stay cancellable: Stop is instant
                // even mid-backoff.
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(LlmError::Cancelled),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            if cancel.is_cancelled() {
                return Err(LlmError::Cancelled);
            }
            if attempt > 0 {
                // A retry re-streams from zero. Everything a consumer rendered
                // for the failed attempt is void now, and it has to be told
                // *before* this attempt's first delta — otherwise the two
                // attempts' text is appended and reads as one answer. Emitted
                // after the backoff so the boundary lands immediately before
                // the request it describes.
                emit(&events, LlmEvent::Restart { attempt }).await?;
            }
            match self.attempt(&url, &headers, &body, &events, &cancel).await {
                Ok(response) => return Ok(response),
                Err(AttemptEnd::Retry { error, after }) => {
                    tracing::warn!(
                        backend = %self.spec.id,
                        attempt = attempt + 1,
                        max_retries,
                        error = %error,
                        "backend call failed; retrying"
                    );
                    pending_delay = Some(after.unwrap_or_else(|| backoff_delay(attempt)));
                    last_error = Some(error);
                }
                Err(AttemptEnd::Fatal(error)) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| LlmError::Unavailable {
            backend: self.spec.id.clone(),
            reason: "no attempt was made".to_string(),
        }))
    }

    /// One HTTP attempt: request, status handling, then stream consumption.
    async fn attempt(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
        events: &mpsc::Sender<LlmEvent>,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse, AttemptEnd> {
        let response = match self
            .transport
            .post_sse(url, headers.to_vec(), body.clone())
            .await
        {
            Ok(response) => response,
            Err(HttpError::Transport { reason }) => {
                return Err(AttemptEnd::Retry {
                    error: LlmError::Unavailable {
                        backend: self.spec.id.clone(),
                        reason,
                    },
                    after: None,
                });
            }
            Err(HttpError::Status {
                status,
                body,
                retry_after_secs,
            }) => {
                let error = refused_error(&self.spec.id, status, &body);
                return Err(if error.retryable() {
                    AttemptEnd::Retry {
                        error,
                        after: retry_after_secs.map(retry_after_delay),
                    }
                } else {
                    AttemptEnd::Fatal(error)
                });
            }
        };

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            // Defensive: transports are contracted to convert non-2xx into
            // `HttpError::Status`, but a third-party transport that hands back
            // the error response directly still gets the same treatment.
            let retry_after = response
                .headers()
                .get(http::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|text| text.trim().parse::<f64>().ok())
                .filter(|secs| secs.is_finite() && *secs >= 0.0)
                .map(|secs| retry_after_delay(secs as u64));
            let text = drain_to_string(response.into_body(), ERROR_BODY_LIMIT).await;
            let error = refused_error(&self.spec.id, status, &text);
            return Err(if error.retryable() {
                AttemptEnd::Retry {
                    error,
                    after: retry_after,
                }
            } else {
                AttemptEnd::Fatal(error)
            });
        }

        match self.consume_sse(response.into_body(), events, cancel).await {
            Ok(response) => Ok(response),
            // Retryable stream failures (a reset mid-response) restart the
            // stream; deltas are best-effort by contract, the final
            // ChatResponse is assembled from the successful attempt only.
            Err(error) if error.retryable() => Err(AttemptEnd::Retry { error, after: None }),
            Err(error) => Err(AttemptEnd::Fatal(error)),
        }
    }

    /// Parse the framed SSE stream into events plus the assembled response.
    ///
    /// Termination invariant: a turn is complete only when the server *said*
    /// it was — `data: [DONE]` or a `finish_reason`. EOF alone is not a
    /// terminator: a proxy that closes mid-answer produces exactly that byte
    /// stream, and inferring `Stop` from it would commit a half reply the
    /// model never finished. So a stream that ends without a terminator is
    /// refused as [`LlmError::Malformed`] naming what is missing, whether or
    /// not any text arrived.
    async fn consume_sse(
        &self,
        body: BodyStream,
        events: &mpsc::Sender<LlmEvent>,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse, LlmError> {
        let mut framed = Box::pin(frame_sse(body));
        let mut state = StreamState::default();
        let mut terminated = false;
        loop {
            let item = tokio::select! {
                biased;
                // Cancellation wins even when the next chunk is already ready:
                // Stop means stop now, not after the buffer drains.
                _ = cancel.cancelled() => return Err(LlmError::Cancelled),
                item = framed.next() => item,
            };
            match item {
                Some(Ok(data)) => {
                    let data = data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    if data == "[DONE]" {
                        terminated = true;
                        break;
                    }
                    state.handle_chunk(&self.spec.id, data, events).await?;
                }
                Some(Err(error)) => {
                    return Err(LlmError::Unavailable {
                        backend: self.spec.id.clone(),
                        reason: format!("SSE stream failed mid-response: {error}"),
                    });
                }
                // The server closed. That is only an ending when it already
                // sent a terminator (checked below); otherwise it is a
                // truncation and must not be dressed up as a complete turn.
                None => break,
            }
        }
        if !terminated && !state.has_finish_reason() {
            return Err(state.truncated_error(&self.spec.id));
        }
        state.finish(&self.spec.id, events).await
    }
}

/// How one attempt ended: retryable (with an optional server-requested wait)
/// or final.
enum AttemptEnd {
    Retry {
        error: LlmError,
        after: Option<Duration>,
    },
    Fatal(LlmError),
}

#[async_trait]
impl LlmBackend for OpenAiCompat {
    fn id(&self) -> &str {
        &self.spec.id
    }

    fn kind(&self) -> BackendKind {
        BackendKind::OpenAiCompat
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: true,
            tool_calls: true,
            // We forward `reasoning_content`/`reasoning` deltas when a model
            // emits them; servers that do not simply never send any.
            reasoning: true,
            // A POSTed completion cannot be steered mid-flight.
            steer: false,
            // Stateless HTTP: the history we send *is* the session.
            survives_restart: false,
        }
    }

    async fn chat(
        &self,
        request: ChatRequest,
        events: mpsc::Sender<LlmEvent>,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, LlmError> {
        if self.key.is_none() && self.spec.api_key_env.is_some() {
            // Fail before spending a request we know is unauthorized.
            return Err(self.missing_key());
        }
        // The whole call — every attempt, backoffs included — fits inside the
        // spec's timeout, so a hung socket cannot pin a bot's turn forever.
        // `timeout_secs: 0` is treated as 1s, never as "no timeout".
        let timeout = Duration::from_secs(self.spec.timeout_secs.max(1));
        match tokio::time::timeout(timeout, self.run(request, events, cancel)).await {
            Ok(result) => result,
            Err(_) => Err(LlmError::Timeout {
                secs: timeout.as_secs(),
            }),
        }
    }

    async fn probe(&self) -> Result<BackendStatus, LlmError> {
        let backend = self.spec.id.clone();
        if self.key.is_none() && self.spec.api_key_env.is_some() {
            // Honest without spending a request: the key we would send does
            // not exist.
            return Ok(BackendStatus {
                backend,
                ok: false,
                detail: self.missing_key().to_string(),
                model: self.spec.model.clone(),
                version: None,
            });
        }
        let headers = self.build_headers(&BTreeMap::new(), false);
        match self.transport.get_text(&self.models_url(), headers).await {
            Ok(response) => Ok(BackendStatus {
                backend,
                ok: true,
                detail: models_detail(response.body(), self.spec.model.as_deref()),
                model: self.spec.model.clone(),
                version: None,
            }),
            Err(HttpError::Status { status, body, .. }) => {
                let detail = if status == 401 || status == 403 {
                    format!(
                        "api key missing or rejected (HTTP {status}): {}",
                        truncate_utf8(&body, 256)
                    )
                } else {
                    format!(
                        "GET /models failed with HTTP {status}: {}",
                        truncate_utf8(&body, 256)
                    )
                };
                Ok(BackendStatus {
                    backend,
                    ok: false,
                    detail,
                    model: self.spec.model.clone(),
                    version: None,
                })
            }
            Err(HttpError::Transport { reason }) => Err(LlmError::Unavailable { backend, reason }),
        }
    }

    async fn available_models(&self) -> Result<ModelList, LlmError> {
        let backend = self.spec.id.clone();
        if self.key.is_none() && self.spec.api_key_env.is_some() {
            return Err(self.missing_key());
        }
        let headers = self.build_headers(&BTreeMap::new(), false);
        match self.transport.get_text(&self.models_url(), headers).await {
            Ok(response) => Ok(ModelList::authoritative(
                parse_model_ids(response.body())
                    .unwrap_or_default()
                    .into_iter()
                    .map(ModelOption::new)
                    .collect(),
            )),
            Err(HttpError::Status { status, body, .. }) => Err(LlmError::Refused {
                backend,
                status,
                body: truncate_utf8(&body, ERROR_BODY_LIMIT),
            }),
            Err(HttpError::Transport { reason }) => Err(LlmError::Unavailable { backend, reason }),
        }
    }
}

/// Accumulates one streamed turn: text, reasoning, tool calls by index,
/// finish reason and usage. Per-attempt state — a retry starts a fresh one.
#[derive(Default)]
struct StreamState {
    text: String,
    reasoning: String,
    /// Keyed by the wire `index` so fragments interleaved out of order (or
    /// parallel calls) assemble correctly; iteration order is index order,
    /// which is the order the model produced them in.
    tool_calls: BTreeMap<u32, ToolCallAccumulator>,
    finish_reason: Option<FinishReason>,
    usage: Option<Usage>,
}

#[derive(Default)]
struct ToolCallAccumulator {
    /// Whether `ToolCallStart` was already emitted for this index.
    started: bool,
    id: String,
    name: String,
    /// Raw argument JSON, assembled from many fragments.
    arguments: String,
}

impl StreamState {
    /// Whether the server told us why it stopped. A `finish_reason` is a
    /// terminator in its own right: some servers send it and then close
    /// without `data: [DONE]`.
    fn has_finish_reason(&self) -> bool {
        self.finish_reason.is_some()
    }

    /// The refusal for a stream that stopped without a terminator.
    ///
    /// Names exactly what is missing and how much arrived, because the
    /// operator's next question is "was that the model or the network?". The
    /// deltas that did arrive were already streamed as progress; the point of
    /// the refusal is that no [`ChatResponse`] exists, so nothing incomplete
    /// can be persisted or acted on.
    ///
    /// [`LlmError::Malformed`] rather than `Unavailable` on purpose: the
    /// retry policy in [`crate::backend::LlmError::retryable`] already lists
    /// "malformed streams" as not retryable, because a server that ends
    /// without a terminator will do it again — retrying only pays for the same
    /// truncated generation twice. A stream cut by a *transport* error takes
    /// the other branch in `consume_sse` and stays retryable.
    fn truncated_error(&self, backend: &str) -> LlmError {
        let reason =
            if self.text.is_empty() && self.reasoning.is_empty() && self.tool_calls.is_empty() {
                "the connection closed before any content: the SSE stream ended without \
             `data: [DONE]` or a finish_reason, so there is no reply to use"
                    .to_string()
            } else if self.text.is_empty() {
                format!(
                    "the connection closed mid-answer: the SSE stream ended after {} reasoning \
                 byte(s) and {} tool call(s) but no text, without `data: [DONE]` or a \
                 finish_reason; the reply is incomplete and was not used",
                    self.reasoning.len(),
                    self.tool_calls.len()
                )
            } else {
                format!(
                    "the connection closed mid-answer: the SSE stream ended after {} byte(s) of \
                 text without `data: [DONE]` or a finish_reason; the reply is incomplete and \
                 was not used",
                    self.text.len()
                )
            };
        LlmError::Malformed {
            backend: backend.to_string(),
            reason,
        }
    }

    async fn handle_chunk(
        &mut self,
        backend: &str,
        data: &str,
        events: &mpsc::Sender<LlmEvent>,
    ) -> Result<(), LlmError> {
        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(error) => {
                // Gateways interleave non-JSON payloads (proxied keep-alives,
                // HTML from a confused LB). An unreadable line carries nothing
                // actionable: log it and keep the turn alive.
                tracing::warn!(backend = %backend, %error, "skipping unreadable SSE data line");
                return Ok(());
            }
        };

        // The usage-only final chunk has `choices: []`; capture usage first.
        if let Some(usage) = value.get("usage") {
            self.usage = Some(parse_usage(usage));
        }
        let Some(choice) = value.pointer("/choices/0") else {
            return Ok(());
        };

        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                self.text.push_str(text);
                emit(events, LlmEvent::TextDelta(text.to_string())).await?;
            }
            // Different families name reasoning differently; forward both.
            for key in ["reasoning_content", "reasoning"] {
                if let Some(reasoning) = delta
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    self.reasoning.push_str(reasoning);
                    emit(events, LlmEvent::ReasoningDelta(reasoning.to_string())).await?;
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for (position, call) in calls.iter().enumerate() {
                    // `index` is the identity of a call across fragments;
                    // servers that omit it get the array position, which is
                    // stable for single-call deltas.
                    let index = call
                        .get("index")
                        .and_then(Value::as_u64)
                        .unwrap_or(position as u64) as u32;
                    let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
                    let name = call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let arguments = call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default();

                    let accumulator = self.tool_calls.entry(index).or_default();
                    if !accumulator.started {
                        accumulator.started = true;
                        emit(
                            events,
                            LlmEvent::ToolCallStart {
                                index,
                                id: id.to_string(),
                                name: name.to_string(),
                            },
                        )
                        .await?;
                    }
                    // Id/name usually arrive on the first fragment but are
                    // allowed to trickle: first non-empty value wins.
                    if accumulator.id.is_empty() && !id.is_empty() {
                        accumulator.id = id.to_string();
                    }
                    if accumulator.name.is_empty() && !name.is_empty() {
                        accumulator.name = name.to_string();
                    }
                    if !arguments.is_empty() {
                        accumulator.arguments.push_str(arguments);
                        emit(
                            events,
                            LlmEvent::ToolCallArgsDelta {
                                index,
                                fragment: arguments.to_string(),
                            },
                        )
                        .await?;
                    }
                }
            }
        }

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = map_finish_reason(backend, reason);
        }
        Ok(())
    }

    /// Assemble the final response and emit `Finished`.
    async fn finish(
        self,
        backend: &str,
        events: &mpsc::Sender<LlmEvent>,
    ) -> Result<ChatResponse, LlmError> {
        let mut tool_calls = Vec::with_capacity(self.tool_calls.len());
        for (index, accumulator) in self.tool_calls {
            let arguments = if accumulator.arguments.trim().is_empty() {
                // Argument-less calls (`get_time()`) are legal, and the empty
                // string is not valid JSON: normalize to `{}`.
                json!({})
            } else {
                match serde_json::from_str::<Value>(&accumulator.arguments) {
                    Ok(value) => value,
                    Err(error) if accumulator.id.is_empty() || accumulator.name.is_empty() => {
                        // Broken arguments *and* broken addressing: there is
                        // nothing a tool registry could act on, so the turn
                        // itself is malformed. Quote the raw string — it is
                        // model output, not a secret.
                        return Err(LlmError::Malformed {
                            backend: backend.to_string(),
                            reason: format!(
                                "tool call at index {index} has unparseable arguments ({error}) \
                                 and no {}: raw arguments: {}",
                                missing_parts(&accumulator),
                                truncate_utf8(&accumulator.arguments, 512)
                            ),
                        });
                    }
                    Err(error) => {
                        // A model that emits broken JSON must not kill a turn:
                        // hand the raw string through so the registry's schema
                        // validation rejects it and the loop's retry gives the
                        // model a second chance with the error text.
                        tracing::warn!(
                            backend = %backend,
                            tool = %accumulator.name,
                            %error,
                            "tool call arguments are not valid JSON; forwarding as `_raw`"
                        );
                        json!({ "_raw": accumulator.arguments })
                    }
                }
            };
            if accumulator.name.is_empty() {
                return Err(LlmError::Malformed {
                    backend: backend.to_string(),
                    reason: format!(
                        "tool call at index {index} has no function name (raw arguments: {})",
                        truncate_utf8(&accumulator.arguments, 512)
                    ),
                });
            }
            let id = if accumulator.id.is_empty() {
                // Non-conforming servers occasionally omit ids; the runner
                // needs one to address the tool result, so synthesize a
                // per-index id and say so in the log.
                let synthesized = format!("call_{index}");
                tracing::debug!(
                    backend = %backend,
                    tool = %accumulator.name,
                    id = %synthesized,
                    "server sent no tool call id; synthesized one"
                );
                synthesized
            } else {
                accumulator.id
            };
            tool_calls.push(ToolCall {
                id,
                name: accumulator.name,
                arguments,
            });
        }

        // `finish` only runs for a stream that carried a terminator (see
        // `consume_sse`). When the terminator was `[DONE]` with no
        // `finish_reason`, infer: assembled tool calls mean the model stopped
        // to call them, anything else is a normal stop.
        let finish_reason = self.finish_reason.unwrap_or(if tool_calls.is_empty() {
            FinishReason::Stop
        } else {
            FinishReason::ToolCalls
        });
        let mut usage = self.usage.unwrap_or_default();
        // One backend call happened, whatever the server reported; executed
        // tool calls are counted by the runner.
        usage.llm_calls = 1;

        let response = ChatResponse {
            text: self.text,
            reasoning: self.reasoning,
            tool_calls,
            finish_reason,
            usage,
        };
        emit(
            events,
            LlmEvent::Finished {
                finish_reason: response.finish_reason,
                usage: response.usage,
                text: response.text.clone(),
                reasoning: response.reasoning.clone(),
                tool_calls: response.tool_calls.clone(),
            },
        )
        .await?;
        Ok(response)
    }
}

fn missing_parts(accumulator: &ToolCallAccumulator) -> &'static str {
    match (accumulator.id.is_empty(), accumulator.name.is_empty()) {
        (true, true) => "id or name",
        (true, false) => "id",
        (false, true) => "name",
        (false, false) => "id or name",
    }
}

fn parse_usage(value: &Value) -> Usage {
    Usage {
        input_tokens: value
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        llm_calls: 1,
        tool_calls: 0,
    }
}

/// Map the wire string; an unknown reason is the server's future, not our
/// failure — log it and fall back to inference (treat as absent).
fn map_finish_reason(backend: &str, reason: &str) -> Option<FinishReason> {
    match reason {
        "stop" => Some(FinishReason::Stop),
        // `function_call` is the legacy spelling of `tool_calls`.
        "tool_calls" | "function_call" => Some(FinishReason::ToolCalls),
        "length" => Some(FinishReason::Length),
        "content_filter" => Some(FinishReason::ContentFilter),
        other => {
            tracing::warn!(backend = %backend, reason = %other, "unknown finish_reason; inferring");
            None
        }
    }
}

fn message_to_json(message: &ChatMessage) -> Value {
    let mut object = Map::new();
    object.insert(
        "role".into(),
        match message.role {
            Role::System => json!("system"),
            Role::User => json!("user"),
            Role::Assistant => json!("assistant"),
            Role::Tool => json!("tool"),
        },
    );
    match message.role {
        Role::Assistant => {
            // Tool-call-only turns carry `content: null` on the wire; some
            // strict gateways reject an empty string there.
            if message.content.is_empty() && !message.tool_calls.is_empty() {
                object.insert("content".into(), Value::Null);
            } else {
                object.insert("content".into(), json!(message.content));
            }
            if !message.tool_calls.is_empty() {
                let calls: Vec<Value> = message
                    .tool_calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                // Arguments travel as a JSON *string*, exactly
                                // as the model sent them (including a `_raw`
                                // fallback, which round-trips honestly).
                                "arguments": call.arguments.to_string(),
                            },
                        })
                    })
                    .collect();
                object.insert("tool_calls".into(), Value::Array(calls));
            }
        }
        Role::Tool => {
            if let Some(tool_call_id) = &message.tool_call_id {
                object.insert("tool_call_id".into(), json!(tool_call_id));
            }
            object.insert("content".into(), json!(message.content));
        }
        Role::System | Role::User => {
            object.insert("content".into(), json!(message.content));
        }
    }
    if let Some(name) = &message.name {
        object.insert("name".into(), json!(name));
    }
    Value::Object(object)
}

fn tool_to_json(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        },
    })
}

fn push_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    // Replace-by-name keeps exactly one value per header: appending would
    // send two `content-type`s (or two authorizations) and confuse gateways.
    if let Some(position) = headers
        .iter()
        .position(|(existing, _)| existing.eq_ignore_ascii_case(name))
    {
        headers.remove(position);
    }
    headers.push((name.to_string(), value.to_string()));
}

/// Build the operator-facing refusal, including the inferhub auction hint.
fn refused_error(backend: &str, status: u16, body: &str) -> LlmError {
    let mut body = truncate_utf8(body, ERROR_BODY_LIMIT);
    if status == 402 && body.contains("no_provider_under_bid") {
        // Real, current behaviour of the api.inferhub.dev gateway: requests
        // are auctioned and 402 means no provider took the configured bid.
        // The bid is a gateway-specific parameter, so the fix lives in the
        // operator's config — say exactly that instead of leaving them to
        // guess what "payment required" could mean.
        body.push_str(
            "\nhint: this gateway auctions each request between providers and none accepted \
             the current bid; raise its bid parameter (for example `max_bid_per_mtok`) in this \
             backend's `extra_body`, which is merged into every request verbatim.",
        );
    }
    LlmError::Refused {
        backend: backend.to_string(),
        status,
        body,
    }
}

/// Shrink to at most `limit` bytes without splitting a UTF-8 sequence.
pub(crate) fn truncate_utf8(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &text[..end])
}

/// Exponential backoff from [`BASE_BACKOFF_MS`], capped at [`BACKOFF_CAP_MS`],
/// with "equal jitter" (half fixed, half random): concurrent retries spread
/// out without ever waiting the full cap.
fn backoff_delay(retry_index: u32) -> Duration {
    let full = BASE_BACKOFF_MS
        .saturating_mul(1u64 << retry_index.min(4))
        .min(BACKOFF_CAP_MS);
    let half = full / 2;
    Duration::from_millis(half + pseudo_random_u64() % (full - half + 1))
}

/// Honour the server's requested wait, capped: a junk or absurd `Retry-After`
/// must not freeze a bot's turn for an hour.
fn retry_after_delay(secs: u64) -> Duration {
    if secs > RETRY_AFTER_CAP_SECS {
        tracing::warn!(
            retry_after = secs,
            cap = RETRY_AFTER_CAP_SECS,
            "capping Retry-After"
        );
    }
    Duration::from_secs(secs.min(RETRY_AFTER_CAP_SECS))
}

/// Jitter source without a `rand` dependency: `RandomState` is OS-seeded per
/// instantiation, so hashing a clock reading is plenty for backoff spread
/// (it is not cryptographic, and must never be used as such).
fn pseudo_random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    hasher.write_u128(nanos);
    hasher.finish()
}

/// Buffer a body stream to text, bounded (error bodies are quoted in
/// [`LlmError::Refused`], which is capped separately at 2 KB).
async fn drain_to_string(mut body: BodyStream, limit: usize) -> String {
    let mut buffer: Vec<u8> = Vec::new();
    while buffer.len() < limit {
        match body.next().await {
            Some(Ok(chunk)) => {
                let take = (limit - buffer.len()).min(chunk.len());
                buffer.extend_from_slice(&chunk[..take]);
            }
            // An unreadable or ended error body still yields what arrived.
            _ => break,
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// The model ids a `/models` body lists, in the gateway's order, de-duplicated.
fn parse_model_ids(body: &str) -> Option<Vec<String>> {
    let value: Value = serde_json::from_str(body).ok()?;
    let models = value.get("data")?.as_array()?;
    let mut seen = BTreeSet::new();
    let ids: Vec<String> = models
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .filter(|id| seen.insert((*id).to_string()))
        .map(str::to_string)
        .collect();
    Some(ids)
}

/// Turn a `/models` response body into a Settings-screen one-liner.
fn models_detail(body: &str, wanted: Option<&str>) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return "reachable, but /models did not return JSON".to_string();
    };
    let ids: BTreeSet<String> = value
        .get("data")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| model.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return "reachable, but /models listed no identifiable models".to_string();
    }
    match wanted {
        Some(model) if ids.contains(model) => {
            format!("{} models available; `{model}` present", ids.len())
        }
        Some(model) => format!("{} models available; `{model}` NOT listed", ids.len()),
        None => format!("{} models available", ids.len()),
    }
}
