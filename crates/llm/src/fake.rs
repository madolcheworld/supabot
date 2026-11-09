//! The deterministic backend: scripted turns for tests and the offline demo.
//!
//! Why a fake lives in the production crate: the brain, the demo and every
//! downstream test suite need a backend that (a) never touches a network,
//! (b) plays back exactly what a test wrote, and (c) *cannot hang* — an empty
//! script answers with a fixed line instead of blocking a bot's turn forever.
//!
//! It also doubles as an inspection point: [`FakeBackend::requests`] returns
//! every [`ChatRequest`] it was handed, so a test can assert on the
//! instructions, history and tool list the orchestration assembled without
//! mocking anything at the HTTP layer.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use swarmbot_contracts::{BackendKind, Usage};

use crate::backend::{LlmBackend, LlmError, emit};
use crate::types::{
    BackendStatus, Capabilities, ChatRequest, ChatResponse, FinishReason, LlmEvent, Role, ToolCall,
};

/// The reply when the script ran out. Fixed and obvious, so a test that
/// forgot to push a turn fails on an assertion, not on a timeout.
pub const EMPTY_QUEUE_REPLY: &str = "[fake] no scripted turn left";

/// Text and arguments are split into deltas of this many characters, so
/// consumers exercise real streaming (many small events) rather than one
/// monolithic chunk.
const DELTA_CHARS: usize = 4;

/// One scripted assistant turn.
///
/// Everything is optional: a turn may emit only text, only tool calls, or
/// fail with a chosen [`LlmError`]. Field order of emission mirrors the real
/// adapter: reasoning deltas, text deltas, tool calls, `Finished`.
#[derive(Debug, Default)]
pub struct ScriptedTurn {
    /// Assistant text; streamed in [`DELTA_CHARS`]-character deltas.
    pub text: Option<String>,
    /// Reasoning text, streamed before the assistant text.
    pub reasoning: Option<String>,
    /// Tool calls to announce, in order.
    pub tool_calls: Vec<ScriptedToolCall>,
    /// Overrides the inferred finish reason (ToolCalls when calls exist, else
    /// Stop).
    pub finish_reason: Option<FinishReason>,
    /// Overrides the default usage (which only sets `llm_calls: 1`).
    pub usage: Option<Usage>,
    /// When set, `chat` returns this error and emits nothing.
    pub error: Option<LlmError>,
    /// Waited (cancellably) before anything is emitted. Default 0.
    pub delay: Duration,
}

/// A tool call a scripted turn announces. `id: None` gets a deterministic
/// generated id (`fakecall_N`), so tests can assert without inventing ids.
#[derive(Clone, Debug)]
pub struct ScriptedToolCall {
    pub id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

impl ScriptedTurn {
    /// A turn that says `text`.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::default()
        }
    }

    /// A turn that fails with `error` (nothing is emitted).
    pub fn error(error: LlmError) -> Self {
        Self {
            error: Some(error),
            ..Self::default()
        }
    }

    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.reasoning = Some(reasoning.into());
        self
    }

    pub fn with_tool_call(mut self, name: impl Into<String>, arguments: Value) -> Self {
        self.tool_calls.push(ScriptedToolCall {
            id: None,
            name: name.into(),
            arguments,
        });
        self
    }

    pub fn with_tool_call_id(
        mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
    ) -> Self {
        self.tool_calls.push(ScriptedToolCall {
            id: Some(id.into()),
            name: name.into(),
            arguments,
        });
        self
    }

    pub fn with_finish(mut self, finish_reason: FinishReason) -> Self {
        self.finish_reason = Some(finish_reason);
        self
    }

    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

/// The deterministic [`LlmBackend`].
///
/// Two modes: **scripted** ([`FakeBackend::new`]) plays a queue of
/// [`ScriptedTurn`]s in order, then falls back to [`EMPTY_QUEUE_REPLY`];
/// **echo** ([`FakeBackend::echo`]) replies with the last user message, which
/// is what plumbing tests and the offline demo want when nobody scripted
/// anything.
pub struct FakeBackend {
    id: String,
    echo: bool,
    queue: Mutex<VecDeque<ScriptedTurn>>,
    requests: Mutex<Vec<ChatRequest>>,
    /// Counter for generated tool-call ids: deterministic within a backend.
    calls: AtomicUsize,
}

impl std::fmt::Debug for FakeBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeBackend")
            .field("id", &self.id)
            .field("echo", &self.echo)
            // Queue depth and request count are useful; their contents are
            // noise (and requests may carry secret-bearing extra_headers,
            // whose Debug is redacted anyway).
            .field("queued_turns", &self.lock_queue().map_or(0, |q| q.len()))
            .field(
                "recorded_requests",
                &self.lock_requests().map_or(0, |r| r.len()),
            )
            .finish()
    }
}

impl FakeBackend {
    /// A scripted backend: push turns, they play back in order.
    pub fn new(id: impl Into<String>) -> Self {
        Self::with_mode(id, false)
    }

    /// A backend that replies with the last user message (id `fake`).
    pub fn echo() -> Self {
        Self::with_mode("fake", true)
    }

    /// An echo backend under a chosen id (the registry uses this for a
    /// configured fake with `options.echo = true`).
    pub fn echo_with_id(id: impl Into<String>) -> Self {
        Self::with_mode(id, true)
    }

    fn with_mode(id: impl Into<String>, echo: bool) -> Self {
        Self {
            id: id.into(),
            echo,
            queue: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        }
    }

    /// Queue one scripted turn (takes effect on the next `chat`).
    pub fn push_turn(&self, turn: ScriptedTurn) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.push_back(turn);
        }
    }

    /// Every request this backend received, in order — the inspection point
    /// for "what did the orchestration actually send?".
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.lock_requests().map(|r| r.clone()).unwrap_or_default()
    }

    pub fn request_count(&self) -> usize {
        self.lock_requests().map_or(0, |r| r.len())
    }

    /// Number of queued turns (for demos that script ahead).
    pub fn queued_turns(&self) -> usize {
        self.lock_queue().map_or(0, |q| q.len())
    }

    fn lock_queue(&self) -> Option<MutexGuard<'_, VecDeque<ScriptedTurn>>> {
        self.queue.lock().ok()
    }

    fn lock_requests(&self) -> Option<MutexGuard<'_, Vec<ChatRequest>>> {
        self.requests.lock().ok()
    }

    fn next_call_id(&self) -> String {
        format!(
            "fakecall_{}",
            self.calls.fetch_add(1, Ordering::Relaxed) + 1
        )
    }
}

#[async_trait]
impl LlmBackend for FakeBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> BackendKind {
        BackendKind::Fake
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: true,
            tool_calls: true,
            reasoning: true,
            // Steering and durable sessions are harness/backend features the
            // fake deliberately does not pretend to have.
            steer: false,
            survives_restart: false,
        }
    }

    async fn available_models(&self) -> Result<crate::types::ModelList, LlmError> {
        // Exactly the one model the fake runs: naming more would be a lie.
        Ok(crate::types::ModelList::authoritative(vec![
            crate::types::ModelOption::new("fake"),
        ]))
    }

    async fn chat(
        &self,
        request: ChatRequest,
        events: mpsc::Sender<LlmEvent>,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, LlmError> {
        // Record first, even for turns that will fail: a test asserting "the
        // runner sent the right request" must see the attempt.
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request.clone());
        }
        let events = &events;

        let scripted = self.lock_queue().and_then(|mut queue| queue.pop_front());
        let turn = scripted.unwrap_or_else(|| {
            // Anti-hang invariant: an unscripted fake still answers.
            let text = if self.echo {
                last_user_message(&request)
            } else {
                EMPTY_QUEUE_REPLY.to_string()
            };
            ScriptedTurn::text(text)
        });

        if turn.delay > Duration::ZERO {
            // Cancellable wait, like a slow real backend.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(LlmError::Cancelled),
                _ = tokio::time::sleep(turn.delay) => {}
            }
        } else if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }

        if let Some(error) = turn.error {
            return Err(error);
        }

        let mut reasoning = String::new();
        if let Some(scripted) = &turn.reasoning {
            for delta in char_chunks(scripted) {
                reasoning.push_str(&delta);
                emit(events, LlmEvent::ReasoningDelta(delta)).await?;
            }
        }

        let mut text = String::new();
        if let Some(scripted) = &turn.text {
            for delta in char_chunks(scripted) {
                text.push_str(&delta);
                emit(events, LlmEvent::TextDelta(delta)).await?;
            }
        }

        let mut tool_calls = Vec::with_capacity(turn.tool_calls.len());
        for (position, scripted) in turn.tool_calls.iter().enumerate() {
            let index = position as u32;
            let id = scripted.id.clone().unwrap_or_else(|| self.next_call_id());
            emit(
                events,
                LlmEvent::ToolCallStart {
                    index,
                    id: id.clone(),
                    name: scripted.name.clone(),
                },
            )
            .await?;
            let arguments = scripted.arguments.to_string();
            for delta in char_chunks(&arguments) {
                emit(
                    events,
                    LlmEvent::ToolCallArgsDelta {
                        index,
                        fragment: delta,
                    },
                )
                .await?;
            }
            tool_calls.push(ToolCall {
                id,
                name: scripted.name.clone(),
                arguments: scripted.arguments.clone(),
            });
        }

        let finish_reason = turn.finish_reason.unwrap_or(if tool_calls.is_empty() {
            FinishReason::Stop
        } else {
            FinishReason::ToolCalls
        });
        let usage = turn.usage.unwrap_or(Usage {
            llm_calls: 1,
            ..Usage::default()
        });
        let response = ChatResponse {
            text,
            reasoning,
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

    async fn probe(&self) -> Result<BackendStatus, LlmError> {
        Ok(BackendStatus {
            backend: self.id.clone(),
            ok: true,
            detail: if self.echo {
                "fake backend (echo mode): always healthy, never online".to_string()
            } else {
                "fake backend: always healthy, never online".to_string()
            },
            model: Some("fake".to_string()),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        })
    }
}

fn last_user_message(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(|message| message.content.clone())
        .unwrap_or_else(|| "(nothing to echo)".to_string())
}

/// Split into chunks of at most `DELTA_CHARS` characters — char-boundary safe
/// (a multi-byte character is never split, unlike a byte-wise cut).
fn char_chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        current.push(character);
        if current.chars().count() >= DELTA_CHARS {
            chunks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChatMessage;
    use std::sync::Arc;
    use swarmbot_contracts::ToolSpec;
    use swarmbot_contracts::Toolset;

    async fn collect(
        backend: &FakeBackend,
        request: ChatRequest,
    ) -> (Result<ChatResponse, LlmError>, Vec<LlmEvent>) {
        let (tx, mut rx) = mpsc::channel(256);
        let result = backend.chat(request, tx, CancellationToken::new()).await;
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        (result, events)
    }

    fn request_with(user_text: &str) -> ChatRequest {
        let mut request = ChatRequest::new("fake", "You are a test bot.");
        request.messages.push(ChatMessage::user(user_text));
        request
    }

    #[tokio::test]
    async fn scripted_turns_play_in_order() {
        let backend = FakeBackend::new("fake");
        backend.push_turn(ScriptedTurn::text("first answer"));
        backend.push_turn(ScriptedTurn::text("second answer"));

        let (first, _) = collect(&backend, request_with("a")).await;
        let (second, _) = collect(&backend, request_with("b")).await;
        assert_eq!(first.expect("ok").text, "first answer");
        assert_eq!(second.expect("ok").text, "second answer");
    }

    #[tokio::test]
    async fn an_empty_queue_answers_instead_of_hanging() {
        let backend = FakeBackend::new("fake");
        let (result, events) = collect(&backend, request_with("hello?")).await;
        let response = result.expect("the empty queue must not hang or fail");
        assert_eq!(response.text, EMPTY_QUEUE_REPLY);
        assert_eq!(response.finish_reason, FinishReason::Stop);
        // Streaming still happens (the reply is longer than one delta).
        assert!(
            events
                .iter()
                .filter(|e| matches!(e, LlmEvent::TextDelta(_)))
                .count()
                >= 2,
            "{events:?}"
        );
        assert!(matches!(events.last(), Some(LlmEvent::Finished { .. })));
    }

    #[tokio::test]
    async fn text_streams_in_small_deltas() {
        let backend = FakeBackend::new("fake");
        // 10 chars / 4 per delta = 3 deltas.
        backend.push_turn(ScriptedTurn::text("abcdefghij"));
        let (_, events) = collect(&backend, request_with("x")).await;
        let deltas = events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::TextDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(deltas, vec!["abcd", "efgh", "ij"]);
    }

    #[tokio::test]
    async fn requests_are_inspectable() {
        let backend = FakeBackend::new("fake");
        backend.push_turn(ScriptedTurn::text("ok"));

        let mut request = ChatRequest::new("fake", "You are lead.");
        request.messages.push(ChatMessage::user("ping"));
        request.tools.push(ToolSpec::new(
            "message_bot",
            "message another bot",
            serde_json::json!({"type": "object"}),
            Toolset::Bot,
        ));
        let (tx, rx) = mpsc::channel(16);
        backend
            .chat(request, tx, CancellationToken::new())
            .await
            .expect("scripted turn succeeds");
        drop(rx);

        let seen = backend.requests();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].instructions, "You are lead.");
        assert_eq!(seen[0].messages, vec![ChatMessage::user("ping")]);
        assert_eq!(seen[0].tools.len(), 1);
        assert_eq!(seen[0].tools[0].name, "message_bot");
        assert_eq!(backend.request_count(), 1);
    }

    #[tokio::test]
    async fn a_scripted_error_surfaces_and_is_recorded() {
        let backend = FakeBackend::new("fake");
        backend.push_turn(ScriptedTurn::error(LlmError::Unavailable {
            backend: "fake".into(),
            reason: "scripted outage".into(),
        }));
        let (result, events) = collect(&backend, request_with("x")).await;
        let error = result.expect_err("scripted failure");
        assert!(
            matches!(&error, LlmError::Unavailable { reason, .. } if reason == "scripted outage")
        );
        assert!(events.is_empty(), "a failed turn emits nothing");
        // The request was still recorded.
        assert_eq!(backend.request_count(), 1);

        // The next call falls back to the empty-queue reply.
        let (result, _) = collect(&backend, request_with("x")).await;
        assert_eq!(result.expect("ok").text, EMPTY_QUEUE_REPLY);
    }

    #[tokio::test]
    async fn tool_calls_stream_start_args_and_finish() {
        let backend = FakeBackend::new("fake");
        backend.push_turn(
            ScriptedTurn::text("let me check")
                .with_reasoning("thinking about weather")
                .with_tool_call("get_weather", serde_json::json!({"city": "Jakarta"}))
                .with_usage(Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    llm_calls: 1,
                    tool_calls: 0,
                }),
        );
        let (result, events) = collect(&backend, request_with("weather?")).await;
        let response = result.expect("ok");

        assert!(response.has_tool_calls());
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
        assert_eq!(response.reasoning, "thinking about weather");
        assert_eq!(response.usage.input_tokens, 10);
        let call = &response.tool_calls[0];
        assert_eq!(call.name, "get_weather");
        assert_eq!(call.arguments, serde_json::json!({"city": "Jakarta"}));
        assert_eq!(call.id, "fakecall_1", "generated ids are deterministic");

        // Event order mirrors the real adapter: reasoning, text, tool call.
        let kinds = events
            .iter()
            .map(|e| match e {
                LlmEvent::ReasoningDelta(_) => "reasoning",
                LlmEvent::TextDelta(_) => "text",
                LlmEvent::ToolCallStart { .. } => "start",
                LlmEvent::ToolCallArgsDelta { .. } => "args",
                LlmEvent::Finished { .. } => "finished",
            })
            .collect::<Vec<_>>();
        assert_eq!(kinds[0], "reasoning");
        assert!(kinds.contains(&"text"));
        let start = kinds.iter().position(|k| *k == "start").expect("start");
        assert_eq!(kinds.last().copied(), Some("finished"), "{kinds:?}");
        // Between the start and Finished there are only argument fragments.
        assert!(
            !kinds[start + 1..kinds.len() - 1].is_empty()
                && kinds[start + 1..kinds.len() - 1]
                    .iter()
                    .all(|k| *k == "args"),
            "{kinds:?}"
        );
    }

    #[tokio::test]
    async fn echo_replies_with_the_last_user_message() {
        let backend = FakeBackend::echo();
        let mut request = ChatRequest::new("fake", "");
        request.messages.push(ChatMessage::user("older"));
        request.messages.push(ChatMessage::assistant("an answer"));
        request
            .messages
            .push(ChatMessage::user("newest").with_name("lead"));
        let (result, _) = collect(&backend, request).await;
        assert_eq!(result.expect("ok").text, "newest");

        // Without any user message it still answers (no hang, no failure).
        let (result, _) = collect(&backend, ChatRequest::new("fake", "")).await;
        assert_eq!(result.expect("ok").text, "(nothing to echo)");
    }

    #[tokio::test]
    async fn a_scripted_delay_is_cancellable() {
        let backend = Arc::new(FakeBackend::new("fake"));
        backend.push_turn(ScriptedTurn::text("eventually").with_delay(Duration::from_secs(3600)));
        let cancel = CancellationToken::new();
        let (tx, rx) = mpsc::channel(16);
        let task = {
            let backend = Arc::clone(&backend);
            let cancel = cancel.clone();
            tokio::spawn(async move { backend.chat(request_with("slow"), tx, cancel).await })
        };
        cancel.cancel();
        let result = task.await.expect("task completes");
        assert!(matches!(result, Err(LlmError::Cancelled)));
        drop(rx);
    }

    #[tokio::test]
    async fn pre_cancellation_is_honoured_before_any_event() {
        let backend = FakeBackend::new("fake");
        backend.push_turn(ScriptedTurn::text("never"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (tx, rx) = mpsc::channel(16);
        let result = backend.chat(request_with("x"), tx, cancel).await;
        assert!(matches!(result, Err(LlmError::Cancelled)));
        drop(rx);
    }

    #[tokio::test]
    async fn probe_is_always_ok_and_capabilities_are_honest() {
        let backend = FakeBackend::new("fake");
        let status = backend.probe().await.expect("probe never fails");
        assert!(status.ok);
        assert_eq!(status.backend, "fake");
        assert!(status.version.is_some());

        let capabilities = backend.capabilities();
        assert!(capabilities.streaming);
        assert!(capabilities.tool_calls);
        assert!(capabilities.reasoning);
        assert!(!capabilities.steer);
        assert!(!capabilities.survives_restart);
        assert_eq!(backend.kind(), BackendKind::Fake);
    }

    #[tokio::test]
    async fn a_dropped_receiver_reads_as_cancellation() {
        let backend = FakeBackend::new("fake");
        backend.push_turn(ScriptedTurn::text(
            "a longer scripted answer that needs many deltas",
        ));
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let result = backend
            .chat(request_with("x"), tx, CancellationToken::new())
            .await;
        assert!(matches!(result, Err(LlmError::Cancelled)));
    }
}
