//! The JSON-RPC 2.0 envelope: exactly the Effect-RPC dialect the node
//! protocol document pins in §4.
//!
//! Every rule of §4 is implemented here and cited where it is enforced:
//!
//! 1. one JSON message per text frame — a JSON array is a batch and is
//!    rejected ([`ProtocolError::BatchUnsupported`]); we do not do batches;
//! 2. direction — a frame with a `method` belongs to *your server*, the
//!    control methods excepted; everything else to *your client*
//!    ([`direction`]);
//! 3. requests: `{"jsonrpc","id","method","params"?,"headers"?}`; `traceId`,
//!    `spanId` and `sampled` are accepted and ignored (we do not propagate
//!    traces across the wire yet);
//! 4. results: `{"jsonrpc","id","result"}` — void RPCs return `null`;
//! 5. typed errors: `{"jsonrpc","id","error":{"_tag":"Cause",…,"data":[…]}}`
//!    with a flat list of `Fail` / `Die` / `Interrupt` reasons (§5.3 tags);
//! 6. streams: `{"jsonrpc","chunk":true,"id","result":[…]}`, each of which the
//!    receiver must answer with `@effect/rpc/Ack` whose `requestId` keeps the
//!    JSON type of the request id — a mismatched Ack silently stalls the
//!    stream, so [`Id`] never blurs number and string;
//! 7. `@effect/rpc/Interrupt` cancels a call;
//! 8. payload-less RPCs omit `params` or send `null`; `{}` is an error;
//! 9. `@effect/rpc/Ping` is answered with `@effect/rpc/Pong`;
//! 10. headers are `[[name, value], …]`.

use std::fmt;

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use thiserror::Error;

/// Application-level ping (§4.9). Distinct from WebSocket protocol pings,
/// which the tungstenite layer answers below us.
pub const METHOD_PING: &str = "@effect/rpc/Ping";
/// Answer to [`METHOD_PING`] (§4.9).
pub const METHOD_PONG: &str = "@effect/rpc/Pong";
/// Stream acknowledgement: the receiver must send one after **every** chunk
/// (§4.6), and the sender waits for it before sending the next one.
pub const METHOD_ACK: &str = "@effect/rpc/Ack";
/// Call cancellation (§4.7).
pub const METHOD_INTERRUPT: &str = "@effect/rpc/Interrupt";
/// The params key carrying the id being acked/interrupted.
pub const PARAM_REQUEST_ID: &str = "requestId";

const JSONRPC: &str = "2.0";
const CAUSE_TAG: &str = "Cause";

fn malformed(reason: impl Into<String>) -> ProtocolError {
    ProtocolError::MalformedFrame {
        reason: reason.into(),
    }
}

/// A JSON-RPC id that **preserves its JSON type** (§4.3: `id` is a number or a
/// string). This matters beyond cosmetics: `@effect/rpc/Ack` must echo the
/// request id with the same JSON type, and a number `2` never equals the
/// string `"2"` — a mismatched Ack silently stalls the stream (§4.6), so the
/// two shapes stay distinct values here and hash differently.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Id {
    Number(i64),
    Text(String),
}

impl Id {
    /// Decode an id, rejecting anything that is not a JSON number or string.
    pub fn from_value(value: &Value) -> Option<Id> {
        match value {
            Value::Number(n) => n.as_i64().map(Id::Number),
            Value::String(s) => Some(Id::Text(s.clone())),
            _ => None,
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Id::Number(n) => Value::from(*n),
            Id::Text(s) => Value::from(s.clone()),
        }
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Id::Number(n) => write!(f, "{n}"),
            Id::Text(s) => f.write_str(s),
        }
    }
}

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Id::from_value(&value).ok_or_else(|| D::Error::custom("id must be a JSON number or string"))
    }
}

/// One decoded WebSocket text frame (§4).
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    /// A request for the receiving side's server half (§4.3). `params` is
    /// [`Value::Null`] for payload-less RPCs (§4.8).
    Request {
        id: Id,
        method: String,
        params: Value,
        headers: Vec<(String, String)>,
    },
    /// A successful result; `result: null` ends a stream or answers a void RPC
    /// (§4.4, §4.6).
    Response { id: Id, result: Value },
    /// A typed error (§4.5).
    Error { id: Id, error: CauseError },
    /// One stream chunk (§4.6). The receiver must Ack it.
    Chunk { id: Id, result: Vec<Value> },
    /// Stream acknowledgement (§4.6).
    Ack { request_id: Id },
    /// Call cancellation (§4.7).
    Interrupt { request_id: Id },
    /// Liveness ping (§4.9).
    Ping,
    /// Answer to a ping (§4.9).
    Pong,
}

/// The tagged error objects of §5.3. All twelve tags of the protocol table are
/// modeled — even the ones our subset never produces (`Browser*`, `Lease*`,
/// `Desktop*`) — so a caller can decode anything a conforming peer sends and
/// tell "no" from "broken".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(tag = "_tag", rename_all_fields = "camelCase")]
pub enum TypedError {
    #[error("path denied: {path} ({reason})")]
    PathDenied { path: String, reason: String },

    #[error("refused by policy ({action}): {reason}")]
    PolicyRefused {
        action: String,
        reason: String,
        /// `false` tells the model retrying cannot help.
        retry: bool,
    },

    #[error("edit failed: {path} ({reason})")]
    EditFailed { path: String, reason: String },

    #[error("filesystem error {code} during {op} on {path}: {reason}")]
    FsError {
        op: String,
        path: String,
        /// An errno-style code: `ENOENT`, `EISDIR`, …
        code: String,
        reason: String,
    },

    #[error("tunnel {kind} failed: {reason}")]
    TunnelFailed { kind: String, reason: String },

    #[error("stream {kind} refused: {reason}")]
    StreamRefused { kind: String, reason: String },

    #[error("not authenticated: {reason}")]
    NotAuthenticated { reason: String },

    #[error("browser unavailable: {reason}")]
    BrowserUnavailable { reason: String },

    #[error("browser action failed ({action}): {reason}")]
    BrowserActionFailed { action: String, reason: String },

    #[error("lease for {bot_id} held by {holder}: {reason}")]
    LeaseHeld {
        bot_id: String,
        holder: String,
        reason: String,
    },

    #[error("desktop unavailable: {message}")]
    DesktopUnavailable {
        message: String,
        missing: Option<Vec<String>>,
    },

    #[error("node {node_id} is unavailable: {reason}")]
    NodeUnavailable { node_id: String, reason: String },
}

impl TypedError {
    /// The `_tag` this error carries on the wire (§5.3).
    pub fn tag(&self) -> &'static str {
        match self {
            TypedError::PathDenied { .. } => "PathDenied",
            TypedError::PolicyRefused { .. } => "PolicyRefused",
            TypedError::EditFailed { .. } => "EditFailed",
            TypedError::FsError { .. } => "FsError",
            TypedError::TunnelFailed { .. } => "TunnelFailed",
            TypedError::StreamRefused { .. } => "StreamRefused",
            TypedError::NotAuthenticated { .. } => "NotAuthenticated",
            TypedError::BrowserUnavailable { .. } => "BrowserUnavailable",
            TypedError::BrowserActionFailed { .. } => "BrowserActionFailed",
            TypedError::LeaseHeld { .. } => "LeaseHeld",
            TypedError::DesktopUnavailable { .. } => "DesktopUnavailable",
            TypedError::NodeUnavailable { .. } => "NodeUnavailable",
        }
    }

    pub fn path_denied(path: impl Into<String>, reason: impl Into<String>) -> Self {
        TypedError::PathDenied {
            path: path.into(),
            reason: reason.into(),
        }
    }

    pub fn policy_refused(
        action: impl Into<String>,
        reason: impl Into<String>,
        retry: bool,
    ) -> Self {
        TypedError::PolicyRefused {
            action: action.into(),
            reason: reason.into(),
            retry,
        }
    }

    pub fn fs_error(
        op: impl Into<String>,
        path: impl Into<String>,
        code: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        TypedError::FsError {
            op: op.into(),
            path: path.into(),
            code: code.into(),
            reason: reason.into(),
        }
    }

    pub fn not_authenticated(reason: impl Into<String>) -> Self {
        TypedError::NotAuthenticated {
            reason: reason.into(),
        }
    }

    pub fn stream_refused(kind: impl Into<String>, reason: impl Into<String>) -> Self {
        TypedError::StreamRefused {
            kind: kind.into(),
            reason: reason.into(),
        }
    }
}

/// One flat cause reason inside an error frame (§4.5): a typed failure, a
/// defect, or an interruption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CauseReason {
    Fail(TypedError),
    Die(String),
    Interrupt,
}

impl CauseReason {
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        match self {
            CauseReason::Fail(error) => {
                map.insert("_tag".into(), Value::from("Fail"));
                // Serializing a TypedError cannot fail (plain data), but the
                // API returns Result — degrade to a defect instead of
                // panicking if that ever changes.
                let error = serde_json::to_value(error)
                    .unwrap_or_else(|e| Value::from(format!("unserializable typed error: {e}")));
                map.insert("error".into(), error);
            }
            CauseReason::Die(message) => {
                map.insert("_tag".into(), Value::from("Die"));
                map.insert("message".into(), Value::from(message.clone()));
            }
            CauseReason::Interrupt => {
                map.insert("_tag".into(), Value::from("Interrupt"));
            }
        }
        Value::Object(map)
    }

    pub fn from_value(value: &Value) -> Result<CauseReason, ProtocolError> {
        let obj = value
            .as_object()
            .ok_or_else(|| malformed("cause reason must be an object"))?;
        match obj.get("_tag").and_then(Value::as_str) {
            Some("Fail") => {
                let error = obj
                    .get("error")
                    .ok_or_else(|| malformed("Fail reason without `error`"))?;
                match serde_json::from_value::<TypedError>(error.clone()) {
                    Ok(typed) => Ok(CauseReason::Fail(typed)),
                    // A tag we do not know is still *a* failure: keep it as a
                    // defect so the caller sees something instead of silence.
                    Err(e) => Ok(CauseReason::Die(format!("undecodable typed error: {e}"))),
                }
            }
            Some("Die") => {
                let message = obj
                    .get("message")
                    .or_else(|| obj.get("defect"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Ok(CauseReason::Die(message.to_string()))
            }
            Some("Interrupt") => Ok(CauseReason::Interrupt),
            Some(other) => Ok(CauseReason::Die(format!("unknown cause reason `{other}`"))),
            None => Err(malformed("cause reason without `_tag`")),
        }
    }
}

impl Serialize for CauseReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CauseReason {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        CauseReason::from_value(&value).map_err(D::Error::custom)
    }
}

/// The `error` object of §4.5: `_tag: "Cause"` plus a flat list of reasons.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CauseError {
    /// Kept for wire fidelity; the reference implementation sends `0`.
    pub code: i32,
    pub message: String,
    pub data: Vec<CauseReason>,
}

impl CauseError {
    /// A cause carrying one typed failure.
    pub fn fail(error: TypedError) -> Self {
        let message = error.to_string();
        CauseError {
            code: 0,
            message,
            data: vec![CauseReason::Fail(error)],
        }
    }

    /// A cause carrying one defect (an untyped internal failure).
    pub fn die(message: impl Into<String>) -> Self {
        let message = message.into();
        CauseError {
            code: 0,
            message: message.clone(),
            data: vec![CauseReason::Die(message)],
        }
    }

    /// The cause a server answers an [`METHOD_INTERRUPT`] with (§4.7): `data`
    /// contains an `Interrupt` reason.
    pub fn interrupted() -> Self {
        CauseError {
            code: 0,
            message: "Interrupted".into(),
            data: vec![CauseReason::Interrupt],
        }
    }

    /// The first typed failure in `data`, if any — the usual thing a caller
    /// wants to read back out of an error frame.
    pub fn first_fail(&self) -> Option<&TypedError> {
        self.data.iter().find_map(|reason| match reason {
            CauseReason::Fail(error) => Some(error),
            _ => None,
        })
    }

    pub fn is_interrupted(&self) -> bool {
        self.data
            .iter()
            .any(|reason| matches!(reason, CauseReason::Interrupt))
    }

    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("_tag".into(), Value::from(CAUSE_TAG));
        map.insert("code".into(), Value::from(self.code));
        map.insert("message".into(), Value::from(self.message.clone()));
        map.insert(
            "data".into(),
            Value::Array(self.data.iter().map(CauseReason::to_value).collect()),
        );
        Value::Object(map)
    }

    pub fn from_value(value: &Value) -> Result<CauseError, ProtocolError> {
        // A bare string is not the dialect, but treat it as a defect rather
        // than dropping the whole frame: honesty over silence.
        if let Some(text) = value.as_str() {
            return Ok(CauseError::die(text.to_string()));
        }
        let obj = value
            .as_object()
            .ok_or_else(|| malformed("error must be an object"))?;
        let code = obj
            .get("code")
            .and_then(Value::as_i64)
            .map(|code| code as i32)
            .unwrap_or(0);
        let message = obj
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let data = match obj.get("data") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(CauseReason::from_value)
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(malformed("error `data` must be an array")),
        };
        Ok(CauseError {
            code,
            message,
            data,
        })
    }
}

impl Serialize for CauseError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CauseError {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        CauseError::from_value(&value).map_err(D::Error::custom)
    }
}

impl From<TypedError> for CauseError {
    fn from(error: TypedError) -> Self {
        CauseError::fail(error)
    }
}

/// Everything that can go wrong at the protocol/transport layer. Remote
/// failures arrive as [`ProtocolError::Remote`] carrying the peer's
/// [`CauseError`]; local failures are transport or framing problems.
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("socket closed")]
    SocketClosed,

    #[error("websocket error: {reason}")]
    Ws { reason: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("malformed frame: {reason}")]
    MalformedFrame { reason: String },

    /// A JSON array frame (§4.1): batches are part of JSON-RPC but not of
    /// this dialect, so we refuse them loudly instead of half-serving them.
    #[error("batch requests are not supported (one JSON-RPC message per frame)")]
    BatchUnsupported,

    /// `{}` params on a payload-less RPC (§4.8): the reference implementation
    /// fails these with "Expected null".
    #[error("expected null params: payload-less RPCs must omit params or send null")]
    ExpectedNullParams,

    #[error("liveness timeout: no frame received within the deadline")]
    LivenessTimeout,

    #[error("call interrupted")]
    Interrupted,

    /// The peer answered with an error frame (§4.5).
    #[error("remote error: {}", error.message)]
    Remote { error: CauseError },

    /// A chunk was produced for a request that is no longer being served
    /// (already interrupted or finished).
    #[error("request {request_id} is no longer served: {reason}")]
    StreamGone { request_id: String, reason: String },

    /// The local consumer of a stream stopped reading, so the call was ended.
    #[error("stream consumer is gone")]
    ConsumerGone,
}

impl ProtocolError {
    /// Convenience constructor for [`ProtocolError::Ws`].
    pub fn ws(reason: impl Into<String>) -> Self {
        ProtocolError::Ws {
            reason: reason.into(),
        }
    }

    /// Convenience constructor for [`ProtocolError::Remote`].
    pub fn remote(error: CauseError) -> Self {
        ProtocolError::Remote { error }
    }

    /// The typed failure the peer reported, if this error carries one.
    pub fn typed(&self) -> Option<&TypedError> {
        match self {
            ProtocolError::Remote { error } => error.first_fail(),
            _ => None,
        }
    }
}

impl From<ProtocolError> for CauseError {
    /// Local protocol failures still have to cross the wire as a cause (§4.5)
    /// when we owe the peer an answer: remote causes pass through, an
    /// interrupt stays an `Interrupt` reason (§4.7), everything else becomes
    /// a defect.
    fn from(error: ProtocolError) -> Self {
        match error {
            ProtocolError::Remote { error } => error,
            ProtocolError::Interrupted => CauseError::interrupted(),
            other => CauseError::die(other.to_string()),
        }
    }
}

/// Which half of a peer owns an incoming frame (§4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Dispatch to the request handler this peer serves.
    Server,
    /// Belongs to the call lifecycle this peer's client half drives.
    Client,
}

/// The direction rule of §4.2, in one place.
///
/// A frame with a `method` is a request for *your server* — except the four
/// `@effect/rpc` control methods (`Ping`, `Pong`, `Ack`, `Interrupt`), which
/// belong to the lifecycle of calls and are consumed by the mux's control
/// path instead of ever reaching a user handler: a `Pong` settles a ping our
/// client sent, an `Ack` releases the next chunk of a stream we serve, an
/// `Interrupt` cancels a handler we are running, and a `Ping` is answered
/// with a `Pong`. Everything without a `method` (`result`, `error`, `chunk`)
/// completes a request *our client* sent.
pub fn direction(frame: &Frame) -> Direction {
    match frame {
        Frame::Request { .. } => Direction::Server,
        Frame::Response { .. }
        | Frame::Error { .. }
        | Frame::Chunk { .. }
        | Frame::Ack { .. }
        | Frame::Interrupt { .. }
        | Frame::Ping
        | Frame::Pong => Direction::Client,
    }
}

fn expect_null_params(obj: &Map<String, Value>) -> Result<(), ProtocolError> {
    match obj.get("params") {
        None | Some(Value::Null) => Ok(()),
        // §4.8: payload-less RPCs omit params or send null; `{}` is an error.
        _ => Err(ProtocolError::ExpectedNullParams),
    }
}

fn request_id_param(obj: &Map<String, Value>, method: &str) -> Result<Id, ProtocolError> {
    let params = obj
        .get("params")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            malformed(format!(
                "{method} requires params with `{PARAM_REQUEST_ID}`"
            ))
        })?;
    let id = params
        .get(PARAM_REQUEST_ID)
        .ok_or_else(|| malformed(format!("{method} params miss `{PARAM_REQUEST_ID}`")))?;
    Id::from_value(id).ok_or_else(|| {
        malformed(format!(
            "`{PARAM_REQUEST_ID}` must be a JSON number or string"
        ))
    })
}

/// Parse one WebSocket text frame into a [`Frame`].
///
/// Rejects batches (§4.1) and `{}` params (§4.8) with typed errors; accepts
/// and ignores the optional tracing fields (§4.3).
pub fn parse(text: &str) -> Result<Frame, ProtocolError> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| malformed(format!("invalid JSON: {e}")))?;
    if value.is_array() {
        // §4.1: a JSON array is a batch. We do not implement batches.
        return Err(ProtocolError::BatchUnsupported);
    }
    let obj = value
        .as_object()
        .ok_or_else(|| malformed("frame is not a JSON object"))?;
    match obj.get("jsonrpc").and_then(Value::as_str) {
        Some(JSONRPC) => {}
        Some(other) => return Err(malformed(format!("unsupported jsonrpc version `{other}`"))),
        None => return Err(malformed("frame misses `jsonrpc`")),
    }

    if let Some(method) = obj.get("method") {
        let method = method
            .as_str()
            .ok_or_else(|| malformed("`method` is not a string"))?;
        match method {
            METHOD_PING => {
                expect_null_params(obj)?;
                return Ok(Frame::Ping);
            }
            METHOD_PONG => {
                expect_null_params(obj)?;
                return Ok(Frame::Pong);
            }
            METHOD_ACK => {
                let request_id = request_id_param(obj, METHOD_ACK)?;
                return Ok(Frame::Ack { request_id });
            }
            METHOD_INTERRUPT => {
                let request_id = request_id_param(obj, METHOD_INTERRUPT)?;
                return Ok(Frame::Interrupt { request_id });
            }
            _ => {}
        }
        let id = Id::from_value(
            obj.get("id")
                .ok_or_else(|| malformed("request without `id`"))?,
        )
        .ok_or_else(|| malformed("`id` must be a JSON number or string"))?;
        let params = match obj.get("params") {
            None | Some(Value::Null) => Value::Null,
            Some(Value::Object(map)) if map.is_empty() => {
                // §4.8: `{}` fails with "Expected null".
                return Err(ProtocolError::ExpectedNullParams);
            }
            Some(other) => other.clone(),
        };
        let headers = match obj.get("headers") {
            None | Some(Value::Null) => Vec::new(),
            // §4.10: headers are [[name, value], …].
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    let pair = item.as_array()?;
                    let name = pair.first()?.as_str()?.to_string();
                    let value = pair.get(1)?.as_str()?.to_string();
                    Some((name, value))
                })
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| malformed("headers must be [[name, value], …]"))?,
            Some(_) => return Err(malformed("headers must be an array")),
        };
        return Ok(Frame::Request {
            id,
            method: method.to_string(),
            params,
            headers,
        });
    }

    // No `method`: a frame for our client half (§4.2).
    let id = Id::from_value(
        obj.get("id")
            .ok_or_else(|| malformed("response without `id`"))?,
    )
    .ok_or_else(|| malformed("`id` must be a JSON number or string"))?;
    if obj.get("chunk").and_then(Value::as_bool) == Some(true) {
        let result = obj
            .get("result")
            .and_then(Value::as_array)
            .ok_or_else(|| malformed("chunk frame without `result` array"))?;
        return Ok(Frame::Chunk {
            id,
            result: result.clone(),
        });
    }
    if let Some(result) = obj.get("result") {
        return Ok(Frame::Response {
            id,
            result: result.clone(),
        });
    }
    if let Some(error) = obj.get("error") {
        return Ok(Frame::Error {
            id,
            error: CauseError::from_value(error)?,
        });
    }
    Err(malformed(
        "frame has none of `method`, `result`, `error`, `chunk`",
    ))
}

/// Render a frame as one WebSocket text frame (§4.1). Infallible: everything
/// a [`Frame`] holds is already valid JSON.
pub fn render(frame: &Frame) -> String {
    let mut map = Map::new();
    map.insert("jsonrpc".into(), Value::from(JSONRPC));
    match frame {
        Frame::Request {
            id,
            method,
            params,
            headers,
        } => {
            map.insert("id".into(), id.to_value());
            map.insert("method".into(), Value::from(method.clone()));
            if !params.is_null() {
                // §4.8: payload-less RPCs omit params (or send null).
                map.insert("params".into(), params.clone());
            }
            if !headers.is_empty() {
                map.insert(
                    "headers".into(),
                    Value::Array(
                        headers
                            .iter()
                            .map(|(name, value)| {
                                Value::Array(vec![
                                    Value::from(name.clone()),
                                    Value::from(value.clone()),
                                ])
                            })
                            .collect(),
                    ),
                );
            }
        }
        Frame::Response { id, result } => {
            map.insert("id".into(), id.to_value());
            // §4.4: an explicit `null` result is meaningful (void RPCs, end of
            // a stream), so it is always rendered.
            map.insert("result".into(), result.clone());
        }
        Frame::Error { id, error } => {
            map.insert("id".into(), id.to_value());
            map.insert("error".into(), error.to_value());
        }
        Frame::Chunk { id, result } => {
            map.insert("chunk".into(), Value::from(true));
            map.insert("id".into(), id.to_value());
            map.insert("result".into(), Value::from(result.clone()));
        }
        Frame::Ack { request_id } => {
            map.insert("method".into(), Value::from(METHOD_ACK));
            // §4.6: requestId keeps the JSON type of the request id.
            map.insert(
                "params".into(),
                Value::Object({
                    let mut params = Map::new();
                    params.insert(PARAM_REQUEST_ID.into(), request_id.to_value());
                    params
                }),
            );
        }
        Frame::Interrupt { request_id } => {
            map.insert("method".into(), Value::from(METHOD_INTERRUPT));
            map.insert(
                "params".into(),
                Value::Object({
                    let mut params = Map::new();
                    params.insert(PARAM_REQUEST_ID.into(), request_id.to_value());
                    params
                }),
            );
        }
        Frame::Ping => {
            map.insert("method".into(), Value::from(METHOD_PING));
        }
        Frame::Pong => {
            map.insert("method".into(), Value::from(METHOD_PONG));
        }
    }
    Value::Object(map).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip(frame: Frame) {
        let text = render(&frame);
        let parsed = parse(&text).unwrap_or_else(|e| panic!("{e} for {text}"));
        assert_eq!(parsed, frame, "roundtrip changed the frame: {text}");
    }

    #[test]
    fn every_frame_kind_round_trips() {
        roundtrip(Frame::Request {
            id: Id::Number(2),
            method: "shell.exec".into(),
            params: json!({"inv": {"id": "inv_1"}, "command": "true"}),
            headers: vec![("x-swarmbot-node-id".into(), "nod_1".into())],
        });
        roundtrip(Frame::Request {
            id: Id::Text("abc".into()),
            method: "@effect/rpc/noop".into(),
            params: Value::Null,
            headers: vec![],
        });
        roundtrip(Frame::Response {
            id: Id::Number(0),
            result: Value::Null,
        });
        roundtrip(Frame::Response {
            id: Id::Number(4),
            result: json!({"sha256": "aa", "size": 2}),
        });
        roundtrip(Frame::Error {
            id: Id::Number(1),
            error: CauseError {
                code: 0,
                message: "boom".into(),
                data: vec![
                    CauseReason::Fail(TypedError::path_denied("/etc/x", "outside roots")),
                    CauseReason::Die("defect".into()),
                    CauseReason::Interrupt,
                ],
            },
        });
        roundtrip(Frame::Chunk {
            id: Id::Number(2),
            result: vec![json!({"a": 1}), json!({"b": 2})],
        });
        roundtrip(Frame::Ack {
            request_id: Id::Number(2),
        });
        roundtrip(Frame::Ack {
            request_id: Id::Text("2".into()),
        });
        roundtrip(Frame::Interrupt {
            request_id: Id::Number(7),
        });
        roundtrip(Frame::Ping);
        roundtrip(Frame::Pong);
    }

    #[test]
    fn ids_keep_their_json_type() {
        // §4.6: a mismatched Ack type silently stalls the stream, so number 2
        // and string "2" must stay distinct through render/parse and compare
        // unequal (they are different hash-map keys in the mux).
        let numeric = render(&Frame::Ack {
            request_id: Id::Number(2),
        });
        let textual = render(&Frame::Ack {
            request_id: Id::Text("2".into()),
        });
        assert!(numeric.contains("\"requestId\":2"), "{numeric}");
        assert!(textual.contains("\"requestId\":\"2\""), "{textual}");
        assert_ne!(numeric, textual);

        let parsed_numeric = parse(&numeric).unwrap();
        let parsed_textual = parse(&textual).unwrap();
        assert_eq!(
            parsed_numeric,
            Frame::Ack {
                request_id: Id::Number(2)
            }
        );
        assert_eq!(
            parsed_textual,
            Frame::Ack {
                request_id: Id::Text("2".into())
            }
        );
        assert_ne!(parsed_numeric, parsed_textual);
        assert_ne!(Id::Number(2), Id::Text("2".into()));

        // Request ids keep their type too.
        let text_id = parse(r#"{"jsonrpc":"2.0","id":"r-1","method":"m"}"#).unwrap();
        assert_eq!(
            text_id,
            Frame::Request {
                id: Id::Text("r-1".into()),
                method: "m".into(),
                params: Value::Null,
                headers: vec![],
            }
        );
    }

    #[test]
    fn payload_less_requests_omit_params_and_reject_empty_object() {
        // §4.8: omit params or send null; `{}` fails.
        let rendered = render(&Frame::Request {
            id: Id::Number(1),
            method: "m".into(),
            params: Value::Null,
            headers: vec![],
        });
        let as_value: Value = serde_json::from_str(&rendered).unwrap();
        assert!(!as_value.as_object().unwrap().contains_key("params"));
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"m","params":null}"#).unwrap(),
            Frame::Request {
                id: Id::Number(1),
                method: "m".into(),
                params: Value::Null,
                headers: vec![],
            }
        );
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"m","params":{}}"#),
            Err(ProtocolError::ExpectedNullParams)
        ));
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","method":"@effect/rpc/Ping","params":{}}"#),
            Err(ProtocolError::ExpectedNullParams)
        ));
    }

    #[test]
    fn a_batch_array_is_rejected_with_a_typed_error() {
        // §4.1: one JSON-RPC message per frame; we do not implement batches.
        let batch =
            r#"[{"jsonrpc":"2.0","id":1,"method":"a"},{"jsonrpc":"2.0","id":2,"method":"b"}]"#;
        assert!(matches!(parse(batch), Err(ProtocolError::BatchUnsupported)));
        assert!(matches!(parse("[]"), Err(ProtocolError::BatchUnsupported)));
    }

    #[test]
    fn typed_errors_keep_their_tags() {
        // §5.3: errors are `_tag`-ged objects inside `Fail` reasons.
        let error = CauseError::fail(TypedError::PolicyRefused {
            action: "shell".into(),
            reason: "ceiling never".into(),
            retry: false,
        });
        let rendered = render(&Frame::Error {
            id: Id::Number(3),
            error: error.clone(),
        });
        assert!(rendered.contains(r#""_tag":"Cause""#), "{rendered}");
        assert!(rendered.contains(r#""_tag":"Fail""#), "{rendered}");
        assert!(rendered.contains(r#""_tag":"PolicyRefused""#), "{rendered}");
        assert!(rendered.contains(r#""retry":false"#), "{rendered}");
        let parsed = parse(&rendered).unwrap();
        assert_eq!(
            parsed,
            Frame::Error {
                id: Id::Number(3),
                error
            }
        );

        // The documented example frame from §4.5 decodes.
        let doc_example = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "error": {
                "_tag": "Cause",
                "code": 0,
                "message": "…",
                "data": [{ "_tag": "Fail", "error": { "_tag": "PathDenied", "path": "/etc/x", "reason": "outside roots" } }]
            }
        }"#;
        match parse(doc_example).unwrap() {
            Frame::Error { id, error } => {
                assert_eq!(id, Id::Number(1));
                assert_eq!(
                    error.first_fail(),
                    Some(&TypedError::path_denied("/etc/x", "outside roots"))
                );
            }
            other => panic!("expected an error frame, got {other:?}"),
        }
    }

    #[test]
    fn node_unavailable_serializes_camel_case() {
        // §5.3 spells the field `nodeId`.
        let value = serde_json::to_value(TypedError::NodeUnavailable {
            node_id: "nod_1".into(),
            reason: "offline".into(),
        })
        .unwrap();
        assert_eq!(value["_tag"], json!("NodeUnavailable"));
        assert_eq!(value["nodeId"], json!("nod_1"));
    }

    #[test]
    fn ping_and_pong_render_exactly() {
        assert_eq!(
            render(&Frame::Ping),
            r#"{"jsonrpc":"2.0","method":"@effect/rpc/Ping"}"#
        );
        assert_eq!(
            render(&Frame::Pong),
            r#"{"jsonrpc":"2.0","method":"@effect/rpc/Pong"}"#
        );
        assert_eq!(
            render(&Frame::Ack {
                request_id: Id::Number(2)
            }),
            r#"{"jsonrpc":"2.0","method":"@effect/rpc/Ack","params":{"requestId":2}}"#
        );
        assert_eq!(
            render(&Frame::Interrupt {
                request_id: Id::Number(2)
            }),
            r#"{"jsonrpc":"2.0","method":"@effect/rpc/Interrupt","params":{"requestId":2}}"#
        );
    }

    #[test]
    fn direction_follows_the_rule() {
        // §4.2 as refined by our control-method set: only real RPC requests
        // go to the server half; control frames and completions belong to the
        // client half's call lifecycle.
        assert_eq!(
            direction(&Frame::Request {
                id: Id::Number(0),
                method: "fs.stat".into(),
                params: Value::Null,
                headers: vec![],
            }),
            Direction::Server
        );
        for frame in [
            Frame::Response {
                id: Id::Number(0),
                result: Value::Null,
            },
            Frame::Error {
                id: Id::Number(0),
                error: CauseError::die("x"),
            },
            Frame::Chunk {
                id: Id::Number(0),
                result: vec![],
            },
            Frame::Ack {
                request_id: Id::Number(0),
            },
            Frame::Interrupt {
                request_id: Id::Number(0),
            },
            Frame::Ping,
            Frame::Pong,
        ] {
            assert_eq!(direction(&frame), Direction::Client, "{frame:?}");
        }
    }

    #[test]
    fn malformed_frames_are_typed_errors() {
        assert!(matches!(
            parse("3"),
            Err(ProtocolError::MalformedFrame { .. })
        ));
        assert!(matches!(
            parse("{"),
            Err(ProtocolError::MalformedFrame { .. })
        ));
        assert!(matches!(
            parse(r#"{"id":1,"method":"m"}"#),
            Err(ProtocolError::MalformedFrame { .. })
        ));
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","method":3}"#),
            Err(ProtocolError::MalformedFrame { .. })
        ));
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","method":"m"}"#),
            Err(ProtocolError::MalformedFrame { .. })
        ));
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":1.5,"method":"m"}"#),
            Err(ProtocolError::MalformedFrame { .. })
        ));
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"m","headers":{"a":"b"}}"#),
            Err(ProtocolError::MalformedFrame { .. })
        ));
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":1,"result":nul}"#),
            Err(ProtocolError::MalformedFrame { .. })
        ));
        // Ack without requestId.
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","method":"@effect/rpc/Ack","params":{}}"#),
            Err(ProtocolError::MalformedFrame { .. })
        ));
    }

    #[test]
    fn unknown_cause_tags_degrade_to_defects_instead_of_vanishing() {
        let frame = parse(
            r#"{"jsonrpc":"2.0","id":1,"error":{"_tag":"Cause","code":0,"message":"m","data":[
                {"_tag":"SomeFutureTag","detail":1},{"_tag":"Die","defect":"crashed"}]}}"#,
        )
        .unwrap();
        match frame {
            Frame::Error { error, .. } => {
                assert_eq!(error.data.len(), 2);
                assert!(
                    matches!(&error.data[0], CauseReason::Die(m) if m.contains("SomeFutureTag"))
                );
                assert!(matches!(&error.data[1], CauseReason::Die(m) if m == "crashed"));
                assert!(error.first_fail().is_none());
            }
            other => panic!("expected error frame, got {other:?}"),
        }
    }

    #[test]
    fn a_bare_string_error_becomes_a_defect() {
        let frame = parse(r#"{"jsonrpc":"2.0","id":1,"error":"boom"}"#).unwrap();
        match frame {
            Frame::Error { error, .. } => {
                assert_eq!(error, CauseError::die("boom"));
            }
            other => panic!("expected error frame, got {other:?}"),
        }
    }

    #[test]
    fn protocol_errors_convert_to_causes() {
        let typed = TypedError::not_authenticated("no hello");
        let cause = CauseError::from(ProtocolError::remote(CauseError::fail(typed.clone())));
        assert_eq!(cause.first_fail(), Some(&typed));
        assert!(CauseError::from(ProtocolError::Interrupted).is_interrupted());
        let die = CauseError::from(ProtocolError::SocketClosed);
        assert!(matches!(die.data.first(), Some(CauseReason::Die(_))));
    }
}
