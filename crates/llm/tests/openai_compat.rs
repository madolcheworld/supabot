//! OpenAI-compatible adapter tests, entirely against `ScriptedTransport`.
//!
//! No test here touches a socket: the transport is injected and answers from
//! canned SSE text. Live-gateway tests would have to be `#[ignore]`d and
//! gated on `PROTO_LIVE_LLM=1`; none are needed for the behaviours below.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use swarmbot_contracts::{BackendSpec, ToolSpec, Toolset, Usage};
use swarmbot_llm::test_util::{ScriptedResponse, ScriptedTransport, sse_text};
use swarmbot_llm::{
    ChatMessage, ChatRequest, ChatResponse, FinishReason, LlmBackend, LlmError, LlmEvent,
    OpenAiCompat, ToolCall,
};

/// A key that must never surface in a Debug impl, a log line or an error.
const KEY: &str = "sk-test-DO-NOT-LEAK-9f3k";

const CHAT_URL: &str = "https://api.example.invalid/v1/chat/completions";
const MODELS_URL: &str = "https://api.example.invalid/v1/models";

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn spec() -> BackendSpec {
    let mut spec = BackendSpec::openai_compat(
        "inferhub",
        "https://api.example.invalid/v1",
        "TEST_KEY_ENV",
        "test-model",
    );
    spec.timeout_secs = 60;
    spec.max_retries = 3;
    spec
}

fn build(
    spec: BackendSpec,
    key: Option<&str>,
    responses: Vec<ScriptedResponse>,
) -> (OpenAiCompat, Arc<ScriptedTransport>) {
    let transport = ScriptedTransport::new(responses);
    let backend = OpenAiCompat::new(spec, key.map(str::to_string), transport.clone())
        .expect("the fixture spec is valid");
    (backend, transport)
}

fn scripted(responses: Vec<ScriptedResponse>) -> (OpenAiCompat, Arc<ScriptedTransport>) {
    build(spec(), Some(KEY), responses)
}

fn chat_request() -> ChatRequest {
    let mut request = ChatRequest::new("test-model", "You are lead.");
    request
        .messages
        .push(ChatMessage::user("weather in Jakarta?"));
    request.tools.push(ToolSpec::new(
        "get_weather",
        "look up weather",
        json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}),
        Toolset::Bot,
    ));
    request
}

async fn chat_collect(
    backend: &OpenAiCompat,
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

// --- SSE payload builders (one `data:` event each, via `sse_text`) ---------

fn role_chunk() -> String {
    json!({"choices":[{"index":0,"delta":{"role":"assistant"}}]}).to_string()
}

fn text_delta(text: &str) -> String {
    json!({"choices":[{"index":0,"delta":{"content":text}}]}).to_string()
}

fn reasoning_delta(text: &str) -> String {
    json!({"choices":[{"index":0,"delta":{"reasoning_content":text}}]}).to_string()
}

fn tool_start(index: u32, id: &str, name: &str) -> String {
    json!({"choices":[{"index":0,"delta":{"tool_calls":[
        {"index": index, "id": id, "function": {"name": name, "arguments": ""}}
    ]}}]})
    .to_string()
}

fn tool_args(index: u32, fragment: &str) -> String {
    json!({"choices":[{"index":0,"delta":{"tool_calls":[
        {"index": index, "function": {"arguments": fragment}}
    ]}}]})
    .to_string()
}

fn finish_chunk(reason: &str, usage: Option<(u64, u64)>) -> String {
    match usage {
        Some((prompt, completion)) => json!({
            "choices":[{"index":0,"delta":{},"finish_reason":reason}],
            "usage":{"prompt_tokens":prompt,"completion_tokens":completion},
        })
        .to_string(),
        None => json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]}).to_string(),
    }
}

/// The final `data:` payload of an OpenAI-style stream, as a String so the
/// SSE builder arrays stay homogeneous.
fn done() -> String {
    "[DONE]".to_string()
}

/// One 200 response whose body is the given `data` payloads.
fn sse(payloads: impl IntoIterator<Item = impl AsRef<str>>) -> ScriptedResponse {
    ScriptedResponse::sse([sse_text(payloads)])
}

/// Split text into fixed-size character chunks — every chunk boundary lands
/// mid-line, proving the framing buffers partial JSON.
fn ragged_chunks(text: &str, size: usize) -> Vec<String> {
    text.chars()
        .collect::<Vec<_>>()
        .chunks(size)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect()
}

fn minimal_ok(text: &str) -> ScriptedResponse {
    sse([text_delta(text), finish_chunk("stop", None), done()])
}

// ---------------------------------------------------------------------------
// streaming round trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scripted_round_trip_orders_events_and_assembles_the_call() {
    // The whole stream, cut into 17-character chunks so nearly every JSON
    // line is split across a chunk boundary, with a keep-alive comment
    // interleaved and `data: [DONE]` at the end.
    let body = format!(
        "{}: keep-alive\n\n{}",
        sse_text([
            role_chunk(),
            reasoning_delta("hmm"),
            text_delta("Hel"),
            text_delta("lo "),
        ]),
        sse_text([
            tool_start(0, "call_w1", "get_weather"),
            tool_args(0, "{\"ci"),
            tool_args(0, "ty\":"),
            tool_args(0, " \"Ja"),
            tool_args(0, "karta\"}"),
            finish_chunk("tool_calls", Some((11, 7))),
            done(),
        ])
    );
    let (backend, transport) = scripted(vec![ScriptedResponse::sse(ragged_chunks(&body, 17))]);

    let (result, events) = chat_collect(&backend, chat_request()).await;
    let response = match result {
        Ok(response) => response,
        Err(error) => panic!("round trip failed: {error}"),
    };

    let expected_tool_calls = vec![ToolCall {
        id: "call_w1".into(),
        name: "get_weather".into(),
        arguments: json!({"city": "Jakarta"}),
    }];
    let expected_usage = Usage {
        input_tokens: 11,
        output_tokens: 7,
        llm_calls: 1,
        tool_calls: 0,
    };
    assert_eq!(
        events,
        vec![
            LlmEvent::ReasoningDelta("hmm".into()),
            LlmEvent::TextDelta("Hel".into()),
            LlmEvent::TextDelta("lo ".into()),
            LlmEvent::ToolCallStart {
                index: 0,
                id: "call_w1".into(),
                name: "get_weather".into()
            },
            LlmEvent::ToolCallArgsDelta {
                index: 0,
                fragment: "{\"ci".into()
            },
            LlmEvent::ToolCallArgsDelta {
                index: 0,
                fragment: "ty\":".into()
            },
            LlmEvent::ToolCallArgsDelta {
                index: 0,
                fragment: " \"Ja".into()
            },
            LlmEvent::ToolCallArgsDelta {
                index: 0,
                fragment: "karta\"}".into()
            },
            LlmEvent::Finished {
                finish_reason: FinishReason::ToolCalls,
                usage: expected_usage,
                text: "Hello ".into(),
                reasoning: "hmm".into(),
                tool_calls: expected_tool_calls.clone(),
            },
        ]
    );

    assert_eq!(response.text, "Hello ");
    assert_eq!(response.reasoning, "hmm");
    assert_eq!(response.tool_calls, expected_tool_calls);
    assert!(response.has_tool_calls());
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.usage, expected_usage);

    // Exactly one attempt, aimed at the chat completions endpoint.
    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].url, CHAT_URL);
}

#[tokio::test]
async fn tool_calls_assemble_per_index_even_out_of_order() {
    let (backend, _) = scripted(vec![sse([
        tool_start(1, "call_b", "second_tool"),
        tool_args(1, "{\"x\":\""),
        tool_start(0, "call_a", "first_tool"),
        tool_args(0, "{\"x\":\""),
        tool_args(1, "b\"}"),
        tool_args(0, "a\"}"),
        finish_chunk("tool_calls", None),
        done(),
    ])]);

    let (result, events) = chat_collect(&backend, chat_request()).await;
    let response = result.expect("assembles");

    // Assembled in index order regardless of arrival order.
    assert_eq!(
        response.tool_calls,
        vec![
            ToolCall {
                id: "call_a".into(),
                name: "first_tool".into(),
                arguments: json!({"x": "a"}),
            },
            ToolCall {
                id: "call_b".into(),
                name: "second_tool".into(),
                arguments: json!({"x": "b"}),
            },
        ]
    );
    // One start per index, in arrival order; argument deltas keep their index.
    let starts = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::ToolCallStart { index, id, .. } => Some((*index, id.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        starts,
        vec![(1, "call_b".to_string()), (0, "call_a".to_string())]
    );
    let args = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::ToolCallArgsDelta { index, fragment } => Some((*index, fragment.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        args,
        vec![
            (1, "{\"x\":\"".to_string()),
            (0, "{\"x\":\"".to_string()),
            (1, "b\"}".to_string()),
            (0, "a\"}".to_string()),
        ]
    );
}

/// A stream that simply stops is **not** a complete answer.
///
/// This inverts the test that used to pin the opposite behaviour ("EOF is a
/// normal end"): a proxy that closes mid-answer is byte-for-byte identical to a
/// finished stream, so inferring `Stop` from EOF committed a half reply into the
/// transcript that no retry could ever replace. A truncation must be a refusal
/// that names what is missing, and it must not be retried — retrying a stream
/// that looks complete burns money and cannot repair the answer.
#[tokio::test]
async fn a_stream_that_ends_without_done_or_finish_reason_is_refused() {
    // A comment line, then two text deltas, then EOF with neither a
    // `data: [DONE]` nor a finish_reason.
    let body = format!(
        "{}: keep-alive\n\n{}",
        sse_text([text_delta("split ")]),
        sse_text([text_delta("works")])
    );
    let (backend, transport) = scripted(vec![ScriptedResponse::sse(ragged_chunks(&body, 23))]);
    let (result, events) = chat_collect(&backend, chat_request()).await;
    match result {
        Err(LlmError::Malformed { reason, .. }) => {
            assert!(reason.contains("[DONE]"), "{reason}");
            assert!(reason.contains("finish_reason"), "{reason}");
            assert!(reason.contains("incomplete"), "{reason}");
            assert!(reason.contains("was not used"), "{reason}");
        }
        other => panic!("a truncated stream must be refused, got {other:?}"),
    }
    // No `Finished`: a consumer that only acts on `Finished` sees nothing, so
    // the half reply cannot be mistaken for a completed turn.
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LlmEvent::Finished { .. })),
        "{events:?}"
    );
    // Malformed is not retryable: exactly one attempt.
    assert_eq!(transport.post_count(), 1, "a truncation is not retried");
}

/// An empty stream that ended without a terminator is a failure too — never an
/// empty success. "The model produced nothing" and "the connection died before
/// the model produced anything" are different facts, and only the first is a
/// turn.
#[tokio::test]
async fn an_empty_truncated_stream_is_an_error_not_an_empty_success() {
    let (backend, transport) = scripted(vec![ScriptedResponse::sse(Vec::<String>::new())]);
    let (result, events) = chat_collect(&backend, chat_request()).await;
    match result {
        Err(LlmError::Malformed { reason, .. }) => {
            assert!(reason.contains("[DONE]"), "{reason}");
            assert!(reason.contains("finish_reason"), "{reason}");
            assert!(reason.contains("no reply"), "{reason}");
        }
        other => panic!("an empty truncated stream must be refused, got {other:?}"),
    }
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(transport.post_count(), 1);
}

/// The healthy path, pinned so the truncation refusal above cannot be satisfied
/// by refusing everything: a stream that sends `[DONE]` is a complete turn.
#[tokio::test]
async fn a_stream_that_sends_done_is_a_complete_turn() {
    let (backend, transport) = scripted(vec![sse([
        text_delta("all "),
        text_delta("there"),
        finish_chunk("stop", Some((4, 2))),
        done(),
    ])]);
    let (result, events) = chat_collect(&backend, chat_request()).await;
    let response = result.expect("a terminated stream is a complete answer");
    assert_eq!(response.text, "all there");
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 4,
            output_tokens: 2,
            llm_calls: 1,
            tool_calls: 0,
        }
    );
    assert!(matches!(events.last(), Some(LlmEvent::Finished { .. })));
    // One attempt, and no attempt boundary: `Restart` marks a retry, never the
    // start of the turn itself.
    assert_eq!(transport.post_count(), 1);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LlmEvent::Restart { .. })),
        "{events:?}"
    );
}

/// `[DONE]` alone is a terminator: some servers omit `finish_reason` and rely on
/// the [OI] sentinel, and that stream is complete (Stop is inferred).
#[tokio::test]
async fn done_alone_terminates_a_stream_without_a_finish_reason() {
    let (backend, _) = scripted(vec![sse([text_delta("done-only"), done()])]);
    let (result, _) = chat_collect(&backend, chat_request()).await;
    let response = result.expect("[DONE] is a terminator");
    assert_eq!(response.text, "done-only");
    assert_eq!(response.finish_reason, FinishReason::Stop);
}

/// So is a `finish_reason` without a trailing `[DONE]`: the server said why it
/// stopped, so the turn ended even though the socket then closed.
#[tokio::test]
async fn a_finish_reason_without_done_terminates_the_stream() {
    let (backend, _) = scripted(vec![sse([text_delta("hi"), finish_chunk("length", None)])]);
    let (result, _) = chat_collect(&backend, chat_request()).await;
    let response = result.expect("a finish_reason is a terminator");
    assert_eq!(response.text, "hi");
    assert_eq!(response.finish_reason, FinishReason::Length);
}

#[tokio::test]
async fn unreadable_sse_lines_are_skipped_not_fatal() {
    // A `data:` line that is not JSON (a confused proxy, an HTML error page
    // forwarded verbatim) must not kill the turn: it is skipped with a
    // warning and the rest of the stream still assembles.
    let body = format!(
        "data: definitely-not-json\n\n{}{}",
        sse_text([text_delta("fine")]),
        sse_text([finish_chunk("stop", None), done()])
    );
    let (backend, _) = scripted(vec![ScriptedResponse::sse([body])]);
    let (result, _) = chat_collect(&backend, chat_request()).await;
    let response = result.expect("junk lines are skipped");
    assert_eq!(response.text, "fine");
    assert_eq!(response.finish_reason, FinishReason::Stop);
}

#[tokio::test]
async fn usage_arrives_in_a_choices_less_final_chunk() {
    let (backend, _) = scripted(vec![sse([
        text_delta("hi"),
        finish_chunk("stop", None),
        // The usage chunk OpenAI sends after finish_reason, with no choices.
        json!({"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4}}).to_string(),
        done(),
    ])]);
    let (result, _) = chat_collect(&backend, chat_request()).await;
    let response = result.expect("ok");
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 3,
            output_tokens: 4,
            llm_calls: 1,
            tool_calls: 0,
        }
    );
    assert_eq!(response.text, "hi");
}

#[tokio::test]
async fn argument_less_tool_calls_normalize_to_empty_object() {
    // Some models emit `arguments: ""` or omit the field entirely.
    let (backend, _) = scripted(vec![sse([
        json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call_t","function":{"name":"get_time"}}
        ]}}]})
        .to_string(),
        finish_chunk("tool_calls", None),
        done(),
    ])]);
    let (result, _) = chat_collect(&backend, chat_request()).await;
    let response = result.expect("ok");
    assert_eq!(response.tool_calls[0].arguments, json!({}));
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
}

// ---------------------------------------------------------------------------
// broken tool-call JSON
// ---------------------------------------------------------------------------

#[tokio::test]
async fn broken_arguments_fall_back_to_raw_without_failing_the_turn() {
    let (backend, _) = scripted(vec![sse([
        text_delta("calling "),
        tool_start(0, "call_x", "mystery_tool"),
        tool_args(0, "{\"a\": "),
        tool_args(0, "1,"), // <- never closed: invalid JSON
        finish_chunk("tool_calls", None),
        done(),
    ])]);
    let (result, events) = chat_collect(&backend, chat_request()).await;
    let response = result.expect("broken JSON must not kill the turn");
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        response.tool_calls,
        vec![ToolCall {
            id: "call_x".into(),
            name: "mystery_tool".into(),
            arguments: json!({"_raw": "{\"a\": 1,"}),
        }]
    );
    // The Finished event carries the same `_raw` payload.
    match events.last() {
        Some(LlmEvent::Finished { tool_calls, .. }) => {
            assert_eq!(tool_calls[0].arguments, json!({"_raw": "{\"a\": 1,"}));
        }
        other => panic!("expected Finished, got {other:?}"),
    }
}

#[tokio::test]
async fn broken_arguments_without_id_or_name_are_malformed() {
    let (backend, _) = scripted(vec![sse([
        json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"{oops"}}
        ]}}]})
        .to_string(),
        done(),
    ])]);
    let (result, _) = chat_collect(&backend, chat_request()).await;
    match result {
        Err(LlmError::Malformed { reason, .. }) => {
            assert!(reason.contains("{oops"), "{reason}");
            assert!(reason.contains("index 0"), "{reason}");
        }
        other => panic!("expected Malformed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_nameless_tool_call_is_malformed_even_with_valid_arguments() {
    let (backend, _) = scripted(vec![sse([
        json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call_id","function":{"arguments":"{}"}}
        ]}}]})
        .to_string(),
        done(),
    ])]);
    let (result, _) = chat_collect(&backend, chat_request()).await;
    match result {
        Err(LlmError::Malformed { reason, .. }) => {
            assert!(reason.contains("no function name"), "{reason}");
        }
        other => panic!("expected Malformed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_missing_id_is_synthesized_when_the_call_is_otherwise_sound() {
    let (backend, _) = scripted(vec![sse([
        json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"function":{"name":"get_weather","arguments":"{\"city\":\"X\"}"}}
        ]}}]})
        .to_string(),
        finish_chunk("tool_calls", None),
        done(),
    ])]);
    let (result, _) = chat_collect(&backend, chat_request()).await;
    let response = result.expect("ok");
    assert_eq!(response.tool_calls[0].id, "call_0");
    assert_eq!(response.tool_calls[0].name, "get_weather");
}

#[tokio::test]
async fn unknown_finish_reasons_fall_back_to_inference() {
    let (backend, _) = scripted(vec![sse([
        text_delta("hi"),
        finish_chunk("insufficient_system_resource", None),
        done(),
    ])]);
    let (result, _) = chat_collect(&backend, chat_request()).await;
    let response = result.expect("unknown reasons do not fail turns");
    assert_eq!(response.finish_reason, FinishReason::Stop);
}

// ---------------------------------------------------------------------------
// retries, rate limits, refusals
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_500_then_200_succeeds_in_two_attempts() {
    let (backend, transport) = scripted(vec![
        ScriptedResponse::status(500, "{\"error\":\"internal\"}"),
        minimal_ok("recovered"),
    ]);
    let (result, _) = chat_collect(&backend, chat_request()).await;
    let response = result.expect("the retry recovers");
    assert_eq!(response.text, "recovered");
    assert_eq!(transport.post_count(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_transport_failure_then_200_succeeds_in_two_attempts() {
    let (backend, transport) = scripted(vec![
        ScriptedResponse::fail("connection refused"),
        minimal_ok("recovered"),
    ]);
    let (result, _) = chat_collect(&backend, chat_request()).await;
    assert_eq!(result.expect("retry recovers").text, "recovered");
    assert_eq!(transport.post_count(), 2);
}

#[tokio::test(start_paused = true)]
async fn retry_after_is_honoured_as_seconds() {
    let started = tokio::time::Instant::now();
    let (backend, transport) = scripted(vec![
        ScriptedResponse::status_retry_after(429, "{\"error\":\"slow down\"}", 2),
        minimal_ok("ok"),
    ]);
    let (result, _) = chat_collect(&backend, chat_request()).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(transport.post_count(), 2);
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "waited only {:?}",
        started.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn a_400_is_never_retried() {
    let (backend, transport) = scripted(vec![ScriptedResponse::status(
        400,
        "{\"error\":\"bad request\"}",
    )]);
    let (result, _) = chat_collect(&backend, chat_request()).await;
    match result {
        Err(error @ LlmError::Refused { status, .. }) => {
            assert_eq!(status, 400);
            assert!(!error.retryable());
        }
        other => panic!("expected Refused(400), got {other:?}"),
    }
    assert_eq!(transport.post_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn exceeding_max_retries_yields_the_last_refusal() {
    let mut spec = spec();
    spec.max_retries = 1;
    let (backend, transport) = build(
        spec,
        Some(KEY),
        vec![
            ScriptedResponse::status(500, "first"),
            ScriptedResponse::status(503, "second"),
        ],
    );
    let (result, _) = chat_collect(&backend, chat_request()).await;
    match result {
        Err(LlmError::Refused { status, body, .. }) => {
            assert_eq!(status, 503);
            assert_eq!(body, "second");
        }
        other => panic!("expected Refused(503), got {other:?}"),
    }
    // 1 initial attempt + 1 retry, never more.
    assert_eq!(transport.post_count(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_mid_stream_reset_restarts_and_the_response_is_authoritative() {
    let partial = sse_text([text_delta("partial from attempt one")]);
    let (backend, transport) = scripted(vec![
        ScriptedResponse::stream_fail([partial], "connection reset"),
        minimal_ok("clean second attempt"),
    ]);
    let (result, events) = chat_collect(&backend, chat_request()).await;
    let response = result.expect("the retry recovers");
    // The returned response carries ONLY the successful attempt.
    assert_eq!(response.text, "clean second attempt");
    // Deltas are best-effort: the failed attempt's delta was already emitted
    // and the successful attempt's follows. The `Restart` between them is what
    // tells a live consumer to discard the first — see the test below.
    let deltas = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        deltas,
        vec!["partial from attempt one", "clean second attempt"]
    );
    assert_eq!(transport.post_count(), 2);
}

/// The attempt boundary, at the exact place it matters: attempt one streams
/// text and then the connection resets; the retry re-streams from zero. A
/// consumer that rendered attempt one would otherwise append attempt two and
/// read the two as one answer. `Restart` must arrive **after** attempt one's
/// deltas and **before** attempt two's first byte.
#[tokio::test(start_paused = true)]
async fn a_retry_announces_the_attempt_boundary_before_it_streams() {
    let partial = sse_text([text_delta("partial from attempt one")]);
    let (backend, transport) = scripted(vec![
        ScriptedResponse::stream_fail([partial], "connection reset"),
        minimal_ok("clean second attempt"),
    ]);
    let (result, events) = chat_collect(&backend, chat_request()).await;
    assert_eq!(
        result.expect("the retry recovers").text,
        "clean second attempt"
    );

    assert_eq!(
        events,
        vec![
            LlmEvent::TextDelta("partial from attempt one".into()),
            // The boundary: everything above is void.
            LlmEvent::Restart { attempt: 1 },
            LlmEvent::TextDelta("clean second attempt".into()),
            LlmEvent::Finished {
                finish_reason: FinishReason::Stop,
                usage: Usage {
                    llm_calls: 1,
                    ..Usage::default()
                },
                text: "clean second attempt".into(),
                reasoning: String::new(),
                tool_calls: Vec::new(),
            },
        ]
    );
    assert_eq!(transport.post_count(), 2);
}

/// One `Restart` per retry, numbered by retry ordinal (1, 2, …), each landing
/// before the attempt it introduces. A first attempt never emits one: there is
/// nothing to discard yet, so a consumer has no leading no-op to special-case.
#[tokio::test(start_paused = true)]
async fn restart_counts_retries_and_only_retries() {
    let (backend, transport) = scripted(vec![
        ScriptedResponse::status(500, "first"),
        ScriptedResponse::status(503, "second"),
        minimal_ok("third time"),
    ]);
    let (result, events) = chat_collect(&backend, chat_request()).await;
    assert_eq!(result.expect("recovers").text, "third time");

    let restarts = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::Restart { attempt } => Some(*attempt),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(restarts, vec![1, 2], "{events:?}");
    // The successful attempt's output follows the last boundary.
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                LlmEvent::TextDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["third time"]
    );
    assert_eq!(transport.post_count(), 3);
}

#[tokio::test]
async fn payment_required_under_bid_explains_the_auction() {
    // The exact, current body api.inferhub.dev returns when no provider
    // takes the configured bid.
    let body = r#"{"error":{"code":"no_provider_under_bid","min_input_per_mtok":"0.0075","min_output_per_mtok":"0.0235"}}"#;
    let (backend, transport) = scripted(vec![ScriptedResponse::status(402, body)]);
    let (result, _) = chat_collect(&backend, chat_request()).await;
    match result {
        Err(error @ LlmError::Refused { status, .. }) => {
            assert_eq!(status, 402);
            assert!(!error.retryable(), "402 is not retryable");
            let text = error.to_string();
            assert!(text.contains("no_provider_under_bid"), "{text}");
            assert!(text.contains("auctions"), "{text}");
            assert!(text.contains("extra_body"), "{text}");
            assert!(text.contains("max_bid_per_mtok"), "{text}");
        }
        other => panic!("expected Refused(402), got {other:?}"),
    }
    assert_eq!(transport.post_count(), 1, "402 must not be retried");
}

// ---------------------------------------------------------------------------
// cancellation and timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_mid_stream_returns_cancelled() {
    let chunk = sse_text([text_delta("partial")]);
    let (backend, _) = scripted(vec![ScriptedResponse::hang_after([chunk])]);
    let (tx, mut rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let task = {
        let backend = Arc::new(backend);
        let cancel = cancel.clone();
        tokio::spawn(async move { backend.chat(chat_request(), tx, cancel).await })
    };
    // Wait until we are demonstrably mid-stream, then cancel.
    let first = rx.recv().await.expect("the first delta arrives");
    assert!(matches!(first, LlmEvent::TextDelta(_)));
    cancel.cancel();
    let result = task.await.expect("the task finishes");
    assert!(matches!(result, Err(LlmError::Cancelled)), "{result:?}");
}

#[tokio::test(start_paused = true)]
async fn a_hung_stream_times_out() {
    let mut spec = spec();
    spec.timeout_secs = 5;
    spec.max_retries = 0;
    let (backend, _) = build(
        spec,
        Some(KEY),
        vec![ScriptedResponse::hang_after(Vec::<String>::new())],
    );
    let (result, _) = chat_collect(&backend, chat_request()).await;
    assert!(
        matches!(result, Err(LlmError::Timeout { secs: 5 })),
        "{result:?}"
    );
}

// ---------------------------------------------------------------------------
// request building: extra_body, extra_headers, wire format
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extra_body_wins_and_extra_headers_reach_the_transport() {
    let mut spec = spec();
    spec.extra_body
        .insert("max_bid_per_mtok".into(), json!(0.5));
    spec.extra_body.insert("temperature".into(), json!(0.2));
    spec.extra_body
        .insert("stream_options".into(), json!({"include_usage": false}));
    spec.extra_headers
        .insert("X-Owner".into(), "swarmbot".into());

    let mut request = chat_request();
    request.temperature = Some(0.1);
    request.max_tokens = Some(64);
    request.extra_body.insert("temperature".into(), json!(0.9));
    request
        .extra_body
        .insert("model".into(), json!("override-model"));
    request.extra_headers.insert("X-Trace".into(), "t-1".into());

    let (backend, transport) = build(spec, Some(KEY), vec![minimal_ok("ok")]);
    let (result, _) = chat_collect(&backend, request).await;
    result.expect("call succeeds");

    let recorded = &transport.requests()[0];
    let body = recorded.body.as_ref().expect("POST carries a body");

    // extra_body beats every built-in key, per-call beats spec-level.
    assert_eq!(body["model"], json!("override-model"));
    assert_eq!(body["temperature"], json!(0.9));
    assert_eq!(body["max_bid_per_mtok"], json!(0.5));
    assert_eq!(body["stream_options"], json!({"include_usage": false}));
    // Built-ins the extra_body did not claim stay as built.
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["max_tokens"], json!(64));

    // Headers: defaults, auth from the key, spec extras, per-call extras.
    assert_eq!(recorded.header("content-type"), Some("application/json"));
    assert_eq!(recorded.header("accept"), Some("text/event-stream"));
    assert_eq!(
        recorded.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(recorded.header("x-owner"), Some("swarmbot"));
    assert_eq!(recorded.header("x-trace"), Some("t-1"));
}

#[tokio::test]
async fn history_serializes_to_the_openai_wire_format() {
    let mut request = ChatRequest::new("test-model", "You are lead.");
    request.max_tokens = Some(64);
    request.temperature = Some(0.3);
    request
        .messages
        .push(ChatMessage::user("hi").with_name("owner"));
    request.messages.push(ChatMessage::assistant_tool_calls(
        "",
        vec![ToolCall::new("call_1", "shell", json!({"cmd": "ls"}))],
    ));
    request
        .messages
        .push(ChatMessage::tool("call_1", "a.txt\nb.txt"));
    request.messages.push(ChatMessage::assistant("done"));
    request.tools.push(ToolSpec::new(
        "shell",
        "run a command",
        json!({"type": "object"}),
        Toolset::Node,
    ));

    let (backend, transport) = scripted(vec![minimal_ok("ok")]);
    let (result, _) = chat_collect(&backend, request).await;
    result.expect("call succeeds");

    let body = transport.requests()[0]
        .body
        .clone()
        .expect("POST carries a body");
    assert_eq!(
        body["messages"],
        json!([
            {"role": "system", "content": "You are lead."},
            {"role": "user", "content": "hi", "name": "owner"},
            {
                "role": "assistant",
                // Tool-call-only turns carry null, not "".
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"},
                }],
            },
            {"role": "tool", "tool_call_id": "call_1", "content": "a.txt\nb.txt"},
            {"role": "assistant", "content": "done"},
        ])
    );
    assert_eq!(
        body["tools"],
        json!([{
            "type": "function",
            "function": {
                "name": "shell",
                "description": "run a command",
                "parameters": {"type": "object"},
            },
        }])
    );
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(body["max_tokens"], json!(64));
    // `temperature` is an f32 on the wire-neutral type: compare against the
    // f32 value, not the f64 literal (0.3f32 widens to 0.30000001192…).
    assert_eq!(body["temperature"], json!(0.3f32));
}

#[tokio::test]
async fn empty_instructions_send_no_system_message_and_no_tools_send_no_tools_key() {
    let mut request = ChatRequest::new("test-model", "");
    request.messages.push(ChatMessage::user("hi"));
    let (backend, transport) = scripted(vec![minimal_ok("ok")]);
    let (result, _) = chat_collect(&backend, request).await;
    result.expect("call succeeds");

    let requests = transport.requests();
    let body = requests[0].body.as_ref().expect("POST carries a body");
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages[0]["role"], json!("user"));
    assert!(body.get("tools").is_none(), "no tools key when unused");
}

#[tokio::test]
async fn an_empty_request_model_falls_back_to_the_spec_model() {
    let (backend, transport) = scripted(vec![minimal_ok("ok")]);
    let mut request = chat_request();
    request.model = String::new();
    let (result, _) = chat_collect(&backend, request).await;
    result.expect("call succeeds");
    let requests = transport.requests();
    let body = requests[0].body.as_ref().expect("POST carries a body");
    assert_eq!(body["model"], json!("test-model"));
}

// ---------------------------------------------------------------------------
// secrets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_api_key_never_appears_in_debug_errors_or_recorded_debug() {
    let (backend, transport) = scripted(vec![
        ScriptedResponse::status(401, "{\"error\":\"invalid api key\"}"),
        minimal_ok("unused"),
    ]);

    // Debug of the backend itself.
    let debug = format!("{backend:?}");
    assert!(
        !debug.contains(KEY),
        "backend Debug leaked the key: {debug}"
    );
    assert!(debug.contains("[redacted]"), "{debug}");

    // The error from a refusal quotes the server body only.
    let mut spec401 = spec();
    spec401.max_retries = 0;
    let (backend401, transport401) = build(
        spec401,
        Some(KEY),
        vec![ScriptedResponse::status(
            401,
            "{\"error\":\"invalid api key\"}",
        )],
    );
    let (result, _) = chat_collect(&backend401, chat_request()).await;
    let error = result.expect_err("401 refuses");
    assert!(!error.to_string().contains(KEY), "{error}");
    assert!(error.to_string().contains("invalid api key"), "{error}");
    assert_eq!(transport401.post_count(), 1);

    // The key DID reach the wire (that is its job), but the recorder's Debug
    // masks it — assertions read it through `header()` instead.
    let recorded = &transport401.requests()[0];
    assert_eq!(
        recorded.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    let recorded_debug = format!("{recorded:?}");
    assert!(
        !recorded_debug.contains(KEY),
        "recorded request Debug leaked the key: {recorded_debug}"
    );
    assert!(recorded_debug.contains("[redacted]"), "{recorded_debug}");

    // A request-level secret header is masked by ChatRequest's Debug too.
    let mut request = chat_request();
    request
        .extra_headers
        .insert("X-Api-Key".into(), KEY.to_string());
    let request_debug = format!("{request:?}");
    assert!(!request_debug.contains(KEY), "{request_debug}");

    // And the transport's own Debug stays clean.
    let transport_debug = format!("{transport:?}");
    assert!(!transport_debug.contains(KEY), "{transport_debug}");
}

#[tokio::test]
async fn a_missing_key_fails_before_spending_a_request() {
    let (backend, transport) = build(spec(), None, vec![minimal_ok("never")]);
    let (result, _) = chat_collect(&backend, chat_request()).await;
    match result {
        Err(error) => {
            assert!(matches!(&error, LlmError::MissingKey { env } if env == "TEST_KEY_ENV"));
            assert!(!error.retryable());
            assert_eq!(error.to_string(), "no api key: set TEST_KEY_ENV");
        }
        other => panic!("expected MissingKey, got {other:?}"),
    }
    assert_eq!(transport.requests().len(), 0);
}

// ---------------------------------------------------------------------------
// probe
// ---------------------------------------------------------------------------

#[tokio::test]
async fn probe_counts_models_without_spending_a_completion() {
    let (backend, transport) = scripted(vec![ScriptedResponse::sse([
        "{\"object\":\"list\",\"data\":[{\"id\":\"test-model\"},{\"id\":\"other\"}]}",
    ])]);
    let status = backend.probe().await.expect("probe ok");
    assert!(status.ok);
    assert_eq!(status.backend, "inferhub");
    assert_eq!(status.detail, "2 models available; `test-model` present");
    assert_eq!(status.model.as_deref(), Some("test-model"));

    let recorded = &transport.requests()[0];
    assert_eq!(recorded.method, "GET");
    assert_eq!(recorded.url, MODELS_URL);
    assert_eq!(
        recorded.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert!(recorded.body.is_none());
}

#[tokio::test]
async fn probe_reports_an_unlisted_model_honestly() {
    let (backend, _) = scripted(vec![ScriptedResponse::sse([
        "{\"data\":[{\"id\":\"something-else\"}]}",
    ])]);
    let status = backend.probe().await.expect("probe reports");
    assert!(status.ok);
    assert!(status.detail.contains("NOT listed"), "{status:?}");
}

#[tokio::test]
async fn probe_reports_rejected_keys_honestly() {
    let (backend, _) = scripted(vec![ScriptedResponse::status(
        401,
        "{\"error\":\"bad key\"}",
    )]);
    let status = backend.probe().await.expect("probe reports, never throws");
    assert!(!status.ok);
    assert!(status.detail.contains("api key"), "{status:?}");
    assert!(status.detail.contains("bad key"), "{status:?}");
}

#[tokio::test]
async fn probe_reports_a_missing_key_without_a_request() {
    let (backend, transport) = build(spec(), None, Vec::new());
    let status = backend.probe().await.expect("probe reports");
    assert!(!status.ok);
    assert!(
        status.detail.contains("no api key: set TEST_KEY_ENV"),
        "{status:?}"
    );
    assert_eq!(transport.requests().len(), 0);
}

#[tokio::test]
async fn probe_maps_transport_failures_to_unavailable() {
    let (backend, _) = scripted(vec![ScriptedResponse::fail("dns exploded")]);
    let error = backend.probe().await.expect_err("unreachable");
    match error {
        LlmError::Unavailable { reason, .. } => assert_eq!(reason, "dns exploded"),
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// construction
// ---------------------------------------------------------------------------

#[test]
fn construction_validates_the_spec() {
    let transport = ScriptedTransport::new(Vec::new());

    let mut no_url = spec();
    no_url.base_url = None;
    assert!(OpenAiCompat::new(no_url, Some(KEY.into()), transport.clone()).is_err());

    let mut wrong_kind = spec();
    wrong_kind.kind = swarmbot_contracts::BackendKind::Fake;
    assert!(OpenAiCompat::new(wrong_kind, Some(KEY.into()), transport.clone()).is_err());

    let mut ftp = spec();
    ftp.base_url = Some("ftp://example.invalid/v1".into());
    assert!(OpenAiCompat::new(ftp, Some(KEY.into()), transport.clone()).is_err());

    // Trailing slashes are normalized, blank keys count as absent.
    let mut slashed = spec();
    slashed.base_url = Some("https://api.example.invalid/v1///".into());
    let backend = OpenAiCompat::new(slashed, Some("   ".into()), transport.clone()).expect("valid");
    assert!(!backend.has_key(), "a blank key is no key");
    let backend = OpenAiCompat::new(spec(), Some(KEY.into()), transport).expect("valid");
    assert!(backend.has_key());
    assert_eq!(backend.id(), "inferhub");
    assert_eq!(
        backend.kind(),
        swarmbot_contracts::BackendKind::OpenAiCompat
    );
    let capabilities = backend.capabilities();
    assert!(capabilities.streaming && capabilities.tool_calls && capabilities.reasoning);
    assert!(!capabilities.steer && !capabilities.survives_restart);
}

#[test]
fn from_spec_reads_the_key_through_the_env_closure() {
    // No network is touched by construction — only by chat/probe, which this
    // test never calls.
    let backend = OpenAiCompat::from_spec(spec(), &|name: &str| {
        (name == "TEST_KEY_ENV").then(|| KEY.to_string())
    })
    .expect("valid spec");
    assert!(backend.has_key());
    assert!(!format!("{backend:?}").contains(KEY));

    let keyless = OpenAiCompat::from_spec(spec(), &|_| None).expect("valid spec");
    assert!(!keyless.has_key());
}

// ---------------------------------------------------------------------------
// live gateway (ignored by default — the only network-touching test)
// ---------------------------------------------------------------------------

/// Live smoke test against a real OpenAI-compatible gateway. Ignored unless
/// explicitly requested, so the suite stays hermetic:
///
/// ```text
/// PROTO_LIVE_LLM=1 PROTO_LIVE_BASE_URL=https://…/v1 \
/// PROTO_LIVE_API_KEY=sk-... PROTO_LIVE_MODEL=… \
///   cargo test -p swarmbot-llm --test openai_compat -- --ignored
/// ```
#[tokio::test]
#[ignore = "live: needs PROTO_LIVE_LLM=1 plus PROTO_LIVE_BASE_URL/API_KEY/MODEL"]
async fn live_gateway_round_trip() {
    if std::env::var("PROTO_LIVE_LLM").as_deref() != Ok("1") {
        eprintln!("skipping live test: set PROTO_LIVE_LLM=1 to run it");
        return;
    }
    let base_url = std::env::var("PROTO_LIVE_BASE_URL").expect("set PROTO_LIVE_BASE_URL");
    let model = std::env::var("PROTO_LIVE_MODEL").expect("set PROTO_LIVE_MODEL");
    let spec = BackendSpec::openai_compat("live", base_url, "PROTO_LIVE_API_KEY", model.clone());
    let backend =
        OpenAiCompat::from_spec(spec, &|name| std::env::var(name).ok()).expect("valid spec");

    let probe = backend.probe().await.expect("probe reaches the gateway");
    assert!(probe.ok, "{probe:?}");

    let mut request = ChatRequest::new("", "You answer in one short sentence.");
    request
        .messages
        .push(ChatMessage::user("Say hello in one word."));
    let (tx, mut rx) = mpsc::channel(256);
    let task =
        tokio::spawn(async move { backend.chat(request, tx, CancellationToken::new()).await });
    let mut saw_delta = false;
    while let Some(event) = rx.recv().await {
        if matches!(event, LlmEvent::TextDelta(_) | LlmEvent::ReasoningDelta(_)) {
            saw_delta = true;
        }
    }
    let response = task.await.expect("task joins").expect("live call succeeds");
    assert!(
        !response.text.is_empty() || response.has_tool_calls(),
        "{response:?}"
    );
    assert!(saw_delta, "the live gateway must stream deltas");

    // What this turn cost, printed rather than only asserted: the gate rule for a
    // live test is that it stays trivially cheap *and* says what it spent.
    eprintln!(
        "live_gateway_round_trip spent: input={} output={} llm_calls={} tool_calls={} model={model}",
        response.usage.input_tokens,
        response.usage.output_tokens,
        response.usage.llm_calls,
        response.usage.tool_calls
    );

    // **A real adapter fills `Usage`.** This was on the README's unverified list
    // for several rounds — every number the gate had ever seen came from
    // `FakeBackend::with_usage` — so the assertion is the point of keeping this
    // test live: a `Usage` of zeros here would mean the gateway's `usage` object
    // is not being read, and every report the owner sees would understate the
    // spend while looking authoritative.
    assert!(
        response.usage.llm_calls >= 1,
        "one backend call must be counted: {:?}",
        response.usage
    );
    assert!(
        response.usage.input_tokens > 0,
        "the gateway reported no input tokens: {:?}. Either it sent no `usage` object \
         (then `UsageReport` understates every turn) or the adapter drops it",
        response.usage
    );
    assert!(
        response.usage.output_tokens > 0,
        "the gateway reported no output tokens: {:?}",
        response.usage
    );
}

/// `available_models` reads the gateway's own `/models`: de-duplicated, in the
/// gateway's order — the provider menu's right column is this list verbatim.
#[tokio::test]
async fn available_models_lists_the_gateways_ids() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::sse([
        r#"{"data":[{"id":"m-1"},{"id":"m-2"},{"id":"m-1"}]}"#,
    ])]);
    let backend = OpenAiCompat::new(spec(), Some("k".into()), Arc::clone(&transport) as Arc<_>)
        .expect("builds");
    let list = backend.available_models().await.expect("the gateway lists");
    assert!(
        list.authoritative,
        "the gateway runs whatever id it is sent"
    );
    assert_eq!(
        list.models
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m-1", "m-2"],
        "de-duplicated, gateway order"
    );
}

/// Without a key there is no honest way to ask the gateway: fail before
/// spending a request, exactly like `chat` and `probe` do.
#[tokio::test]
async fn available_models_without_a_key_is_a_missing_key_not_a_guess() {
    let transport = ScriptedTransport::new(Vec::new());
    let backend =
        OpenAiCompat::new(spec(), None, Arc::clone(&transport) as Arc<_>).expect("builds");
    let error = backend.available_models().await.expect_err("no key");
    assert!(matches!(error, LlmError::MissingKey { .. }), "{error}");
    assert_eq!(transport.requests().len(), 0);
}
