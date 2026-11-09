//! The node gateway's HTTP edge: `POST /node/pair` and the `GET /node` upgrade.
//!
//! Neither route is behind the owner's auth gate, and that is not an oversight: a
//! node has no credential until it has paired, and its credential afterwards is a
//! signature over a fresh nonce, not a bearer token. What protects them instead is
//! the protocol's own order —
//!
//! * **pairing** is redeemed with a single-use, ten-minute code the owner typed
//!   into the node (`swarmbot-node pair <brain-url> <CODE>`), stored only as
//!   `sha256("swarmbot-pairing-v1|<code>")`, so this route is the only place the
//!   plain code ever meets the database;
//! * **the upgrade** is refused with an HTTP 401 *before* anything is upgraded, so
//!   a node can report "the brain refused me" rather than "the socket died". The
//!   refusal is a normal response: axum's upgrade extractor only parses headers,
//!   and the 101 is built by `on_upgrade`, which a refused request never reaches.
//!
//! The verification order is the protocol's (§3.1) and every step has its own
//! error: header shape → paired and not revoked → clock skew → nonce replay →
//! signature. The nonce is burned only *after* the signature verifies, so an
//! observer cannot deny a node its own dial by replaying it first.

use std::sync::Arc;

use axum::Json;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde_json::json;
use tracing::{debug, info, warn};

use swarmbot_contracts::id::NodeId;
use swarmbot_contracts::node::{
    Capabilities, NodeInfo, NodeKind, PairRequest, PairResponse, PolicyMap, ProtocolRange,
};
use swarmbot_contracts::{BotId, ConversationId, Invocation};
use swarmbot_core::Change;
use swarmbot_node_protocol::PROTOCOL_VERSION;
use swarmbot_node_protocol::auth::{
    AUTH_SCHEME, KeyLookup, VerifiedUpgrade, unix_ms_now, verify_upgrade_auth,
};
use swarmbot_node_protocol::identity::parse_public_key_base64url;
use swarmbot_node_protocol::pairing::{constant_time_eq, hash_code, normalize_code};
use swarmbot_tools::{ApprovalGate, ToolContext};

use crate::error::{ApiError, ApiResult};
use crate::gateway::session;
use crate::routes::JsonBody;
use crate::state::AppState;

/// The message this route answered with while the gateway was a stub.
///
/// Pairing is implemented now, so `/node/pair` no longer says this — but
/// `crates/node`'s `brain_end_to_end` test builds its *fake* brain's 501 body from
/// `error_body(NOT_YET)` and asserts the operator sees this exact sentence, so the
/// constant stays until that test is rewritten against a real brain.
pub const NOT_YET: &str = "node pairing arrives in milestone M6";

/// The one refusal a bad pairing code gets, whichever way it is bad.
///
/// "Never minted here", "expired" and "already used" are indistinguishable to the
/// caller on purpose: an answer that separates them is an oracle for guessing
/// codes. The log records which one it was, because the owner is the only person
/// who can act on the difference.
const CODE_NOT_VALID: &str = "that pairing code is not valid here: it was never minted by this \
brain, it has expired, or it has already been used";

/// The close code for "the gateway itself broke" (1011, internal error).
///
/// Protocol-level refusals use their own codes — 4401 revoked, 4403 hello failed,
/// 4426 no common protocol — and a node reacts differently to each, so 1011 is
/// kept for the case where the brain, not the handshake, is at fault.
pub const CLOSE_CODE: u16 = 1011;

/// `POST /node/pair`
///
/// Redeems a pairing code for a node identity: the node's public key is stored, a
/// node id is minted, and the brain answers with its own public key and the origin
/// every later signature is bound to. A 2xx body that is not a `PairResponse`
/// makes the node write nothing, so the response shape is the contract.
pub async fn pair(
    State(state): State<AppState>,
    JsonBody(request): JsonBody<PairRequest>,
) -> ApiResult<Json<PairResponse>> {
    // 1. The code, normalized the way the protocol says: `abcd efgh`, `ABCD-EFGH`
    //    and `abcd-efgh` are one code, and a look-alike `I`/`O` maps to `1`/`0`.
    let normalized = normalize_code(&request.code).map_err(|error| {
        ApiError::invalid(format!("`{}` is not a pairing code: {error}", request.code))
    })?;
    let code_hash = hash_code(&normalized);

    // 2. A live record for that hash. One message covers "never minted here",
    //    "expired" and "already used" on purpose: telling a caller which of the
    //    three it was would be an oracle for guessing codes.
    let Some(record) = state.swarm.store.get_pairing(&code_hash)? else {
        warn!("a pairing attempt used a code this brain never minted");
        return Err(ApiError::conflict(CODE_NOT_VALID));
    };
    // The store looked the row up by this hash; comparing it again is cheap and
    // turns a store bug into a refusal instead of a pairing.
    if !constant_time_eq(&record.code_hash, &code_hash) {
        return Err(ApiError::internal(
            "the pairing record does not match the code it was found by",
        ));
    }
    if !record.is_live(Utc::now()) {
        // Which of "used" and "expired" it was goes to the log, not to the
        // caller: an answer that distinguishes them is an oracle for guessing
        // codes, and the operator is the only one who can act on the difference.
        info!(
            used = record.used,
            expired = !record.used,
            "a pairing attempt used a code that is no longer live"
        );
        return Err(ApiError::conflict(CODE_NOT_VALID));
    }

    // 3. The node's public key, in the shape the protocol pins: raw 32 bytes,
    //    base64url without padding.
    let _verifying_key = parse_public_key_base64url(&request.public_key).map_err(|error| {
        ApiError::invalid(format!(
            "`publicKey` is not a base64url-encoded 32-byte Ed25519 public key: {error}"
        ))
    })?;

    // 4. The protocol range has to overlap, or the socket that follows could not
    //    speak to us anyway.
    let offered = request.protocol.unwrap_or_else(ProtocolRange::current);
    let Some(protocol) = ProtocolRange::current().negotiate(offered) else {
        return Err(ApiError::conflict(format!(
            "this brain speaks node protocol v{PROTOCOL_VERSION} and the node offered {}-{}",
            offered.min, offered.max
        )));
    };

    // 5. Burn the code *before* creating anything: a crash halfway through must
    //    leave an unusable code behind, never a reusable one.
    let node_id = NodeId::new();
    let mut used = record.clone();
    used.used = true;
    used.node_id = Some(node_id.clone());
    state.swarm.store.update_pairing(&used)?;

    state
        .swarm
        .store
        .put_node_key(&node_id, &request.public_key)?;
    let now = Utc::now();
    let node = NodeInfo {
        id: node_id.clone(),
        // The hostname is the label until the node's own `node.connected` event
        // arrives with the `--label` its operator gave it.
        label: request.hostname.clone(),
        kind: NodeKind::Remote,
        hostname: request.hostname.clone(),
        platform: request.platform.clone(),
        arch: request.arch.clone().unwrap_or_else(|| "unknown".into()),
        version: request
            .node_version
            .clone()
            .unwrap_or_else(|| "unknown".into()),
        protocol,
        // Nothing is declared or approved yet: the hello brings the declarations,
        // and the pairing code's redemption is what approves them.
        declared: Capabilities::new(Vec::<String>::new()),
        approved: Capabilities::new(Vec::<String>::new()),
        roots: Vec::new(),
        policy: PolicyMap::default(),
        online: false,
        paired_at: now,
        last_seen_at: None,
        revoked: false,
    };
    state.swarm.store.insert_node(&node)?;
    state.swarm.hub.publish(Change::Node(node));
    info!(
        node = %node_id,
        hostname = %request.hostname,
        protocol,
        "a node paired; it may now dial the control socket"
    );

    Ok(Json(PairResponse {
        node_id,
        brain_public_key: state.identity.public_key_base64url(),
        // The origin the node pins and every later signature is bound to. It is
        // the *configured* origin, not the host being dialled: a brain behind a
        // reverse proxy is reached at one address and signs as another.
        brain_origin: state.gateway.origin().to_string(),
    }))
}

/// `GET /node` — the control socket.
pub async fn connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(header) = authorization_of(&headers) else {
        return refuse(format!(
            "the upgrade carries no `Authorization: {AUTH_SCHEME} …` header"
        ));
    };
    if let Some(wanted) = headers
        .get("x-swarmbot-protocol")
        .and_then(|value| value.to_str().ok())
    {
        debug!(protocol = %wanted, "a node offered a protocol range");
    }
    let now = unix_ms_now();
    let verified: VerifiedUpgrade = match verify_upgrade_auth(
        header,
        state.gateway.origin(),
        &|node_id| key_lookup(&state, node_id),
        state.gateway.replay(),
        now,
    ) {
        Ok(verified) => verified,
        Err(error) => {
            // The reason is logged in full and reported in full: a node operator
            // staring at "the brain refused me" needs the step that failed, and
            // none of these strings carry a secret (the signature is over a nonce
            // that is already burned).
            warn!(error = %error, brain_origin = state.gateway.origin(), "a node upgrade was refused");
            return refuse(error.to_string());
        }
    };
    info!(node = %verified.node_id, "a node's upgrade signature verified; serving the control socket");
    upgrade.on_upgrade(move |socket| session::serve_node(state, socket, verified))
}

/// Create an MCP HTTP service for the internal listener.
pub fn create_mcp_service(state: &AppState) -> Arc<swarmbot_mcp::McpHttpService> {
    let swarm = &state.swarm;
    let workspace = swarm.data_dir.join("workspaces").join("mcp");
    let spool_dir = swarm.data_dir.join("workspaces").join(".spool");
    let _ = std::fs::create_dir_all(&workspace);
    let _ = std::fs::create_dir_all(&spool_dir);
    let invocation = Invocation::new(
        BotId::from_suffix("mcp"),
        ConversationId::from_suffix("mcp"),
    );
    let ctx = ToolContext::local(
        invocation,
        workspace,
        Arc::clone(&swarm.gate) as Arc<dyn ApprovalGate>,
        Arc::clone(&swarm.nodes),
        spool_dir,
    );
    let server = swarmbot_mcp::SwarmMcpServer::new(Arc::clone(&swarm.tools), ctx);
    Arc::new(swarmbot_mcp::create_default_http_service(server))
}

/// `POST /mcp`, `GET /mcp`, and `DELETE /mcp` on the internal listener.
///
/// Drives the Streamable HTTP transport of [`swarmbot_mcp`].
pub async fn mcp_handler(
    State(mcp): State<Arc<swarmbot_mcp::McpHttpService>>,
    req: Request,
) -> Response {
    mcp.handle(req).await.into_response()
}

/// The documented envelope, for the routes whose status is not one a
/// [`swarmbot_contracts::DomainError`] carries.
pub fn error_body(message: &str) -> serde_json::Value {
    json!({ "error": { "code": "internal", "message": message } })
}

/// A 401 with the envelope, and no upgrade.
fn refuse(reason: String) -> Response {
    ApiError::unauthorized(reason).into_response()
}

fn authorization_of(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// What the key store says about a node id (§3.1 step 2).
///
/// "Never paired" and "revoked" are different facts with different consequences —
/// a revoked node stops redialing for good — so they are reported separately
/// rather than collapsed into one refusal.
fn key_lookup(state: &AppState, node_id: &str) -> KeyLookup {
    let Ok(id) = NodeId::parse(node_id) else {
        return KeyLookup::Unknown;
    };
    match state.swarm.store.get_node(&id) {
        Ok(Some(node)) if node.revoked => return KeyLookup::Revoked,
        Ok(Some(_)) | Ok(None) => {}
        Err(error) => {
            warn!(node = %id, %error, "cannot read the roster while verifying an upgrade");
            return KeyLookup::Unknown;
        }
    }
    let Ok(Some(key)) = state.swarm.store.node_key(&id) else {
        return KeyLookup::Unknown;
    };
    match parse_public_key_base64url(&key) {
        Ok(key) => KeyLookup::Active(key),
        Err(error) => {
            warn!(node = %id, %error, "the stored node key cannot be parsed; treating the node as unpaired");
            KeyLookup::Unknown
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::node::PairingRecord;
    use swarmbot_node_protocol::pairing::mint_code;

    #[test]
    fn the_stub_constants_the_node_crate_pins_are_still_there() {
        // `crates/node`'s end-to-end test builds its fake brain's answer from
        // `error_body(NOT_YET)` and asserts the operator sees that sentence, and it
        // asserts the control-socket close code. Both are this crate's to keep.
        assert_eq!(NOT_YET, "node pairing arrives in milestone M6");
        assert_eq!(CLOSE_CODE, 1011);
        let body = error_body(NOT_YET);
        assert_eq!(body["error"]["code"], json!("internal"));
        assert_eq!(body["error"]["message"], json!(NOT_YET));
    }

    #[test]
    fn a_pair_request_arrives_in_its_camel_case_shape() {
        let value = json!({
            "code": "ABCD-EFGH",
            "publicKey": "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo",
            "hostname": "workstation",
            "platform": "linux",
            "arch": "x86_64",
            "nodeVersion": "0.1.0",
            "protocol": {"min": 1, "max": 1},
        });
        let request: PairRequest = serde_json::from_value(value).expect("decodes");
        assert_eq!(request.code, "ABCD-EFGH");
        assert_eq!(request.protocol.expect("a range").max, 1);
        assert_eq!(request.node_version.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn a_pair_response_carries_the_three_facts_a_node_pins() {
        let response = PairResponse {
            node_id: NodeId::new(),
            brain_public_key: "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo".into(),
            brain_origin: "http://127.0.0.1:8787".into(),
        };
        let value = serde_json::to_value(&response).expect("serializes");
        assert_eq!(value["brainPublicKey"], json!(response.brain_public_key));
        assert_eq!(value["brainOrigin"], json!("http://127.0.0.1:8787"));
        assert_eq!(value["nodeId"], json!(response.node_id.to_string()));
        let back: PairResponse = serde_json::from_value(value).expect("round-trips");
        assert_eq!(back, response);
    }

    #[test]
    fn a_code_is_looked_up_by_hash_and_burned_by_flag() {
        let code = mint_code().expect("a code");
        let now = Utc::now();
        let record = PairingRecord {
            code_hash: hash_code(&normalize_code(&code.display).expect("normalized")),
            created_at: now,
            expires_at: now + chrono::Duration::minutes(10),
            used: false,
            node_id: None,
        };
        // The hash the route computes is the one a node's code produces, whatever
        // the operator typed.
        assert_eq!(
            record.code_hash,
            hash_code(&normalize_code(&code.raw).expect("normalized"))
        );
        assert!(record.is_live(now));
        assert!(
            !PairingRecord {
                used: true,
                ..record.clone()
            }
            .is_live(now)
        );
        assert!(
            !PairingRecord {
                expires_at: now - chrono::Duration::seconds(1),
                ..record
            }
            .is_live(now)
        );
    }

    #[test]
    fn a_protocol_range_has_to_overlap() {
        let current = ProtocolRange::current();
        assert_eq!(current.negotiate(current), Some(PROTOCOL_VERSION));
        assert_eq!(
            current.negotiate(ProtocolRange { min: 5, max: 9 }),
            None,
            "no overlap is a refusal, not a downgrade"
        );
    }

    #[test]
    fn an_authorization_header_is_read_trimmed_and_never_invented() {
        let mut headers = HeaderMap::new();
        assert!(authorization_of(&headers).is_none());
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "  Bearer x  ".parse().expect("a value"),
        );
        assert_eq!(authorization_of(&headers), Some("Bearer x"));
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "".parse().expect("an empty value"),
        );
        assert!(
            authorization_of(&headers).is_none(),
            "an empty header is no header"
        );
    }

    #[test]
    fn a_refusal_is_a_401_with_the_envelope() {
        let response = refuse("node nod_x is revoked".to_string());
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }
}
