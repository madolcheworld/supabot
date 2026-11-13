//! Nodes: `/api/nodes`.
//!
//! The local node is created at boot by the swarm; a remote one arrives by
//! pairing, which is M6's work. What exists today is the owner's view of the
//! roster, the minting of a pairing code, and revocation.
//!
//! A pairing code is shown once and stored as `sha256("swarmbot-pairing-v1|<code>")`
//! (decision D1.3), so a stolen database cannot hand out node registrations: the
//! plain code only ever exists in this response and in the operator's hands.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use tracing::info;

use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::node::{Capabilities, NodeInfo, PairingRecord};
use swarmbot_core::Change;

use swarmbot_node_protocol::pairing::mint_code;

use crate::auth::PAIRING_CODE_TTL_SECS;
use crate::error::{ApiError, ApiResult};
use crate::gateway::session;
use crate::routes::node_id;
use crate::state::AppState;

/// `GET /api/nodes`
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<NodeInfo>>> {
    Ok(Json(state.swarm.store.list_nodes()?))
}

/// `POST /api/nodes/pairing-codes`
///
/// `node_id` is always null here: it is filled in when a node redeems the code,
/// which is what turns "a code exists" into "a machine is paired".
pub async fn create_pairing_code(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    // Minted by `swarmbot-node-protocol`, which is also what will redeem it: one
    // implementation of the alphabet, the dash placement and the hash, so a code
    // printed today is a code a node can pair with in M6.
    let code = mint_code()
        .map_err(|err| ApiError::internal(format!("cannot mint a pairing code: {err}")))?;
    let now = Utc::now();
    let record = PairingRecord {
        code_hash: code.hash(),
        created_at: now,
        expires_at: now + Duration::seconds(PAIRING_CODE_TTL_SECS),
        used: false,
        node_id: None,
    };
    state.swarm.store.insert_pairing(&record)?;
    info!(
        expires_at = %record.expires_at,
        "a node pairing code was minted (shown once, stored as a hash)"
    );
    Ok(Json(json!({
        // `ABCD-EFGH`, the form the operator reads out loud.
        "code": code.display,
        // RFC 3339 text, which is how `DateTime<Utc>` serializes.
        "expires_at": record.expires_at,
        "node_id": record.node_id,
    })))
}

/// `DELETE /api/nodes/{id}` — revoke a pairing.
///
/// Revoking is not deleting: the row stays so the roster can say "this machine
/// was here and was cut off", and its key is dropped from the allow-list so it
/// cannot redial. Idempotent, because a client that retries a revoke should get
/// the same answer twice.
pub async fn revoke(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = node_id(&id)?;
    if id.is_local() {
        // The local node is this process. Revoking it would leave every bot's
        // tool call failing with `node_unavailable` and no way to undo it from
        // the API, so it is refused rather than done.
        return Err(ApiError::conflict(
            "the local node cannot be revoked: it runs inside this process",
        ));
    }
    let Some(node) = state.swarm.store.get_node(&id)? else {
        return Err(ApiError::from(DomainError::not_found("node", &id)));
    };
    if node.revoked {
        return Ok(StatusCode::NO_CONTENT);
    }
    // Through the gateway's guarded writer, not a bare `update_node`: a
    // revocation racing a heartbeat used to be able to lose either side, and
    // "the owner revoked this node" losing to a liveness ping is the worse of
    // the two. On conflict the closure is re-applied to the fresh row.
    let written = session::mutate_node(state.swarm.store.as_ref(), &id, |node| {
        node.revoked = true;
        node.online = false;
        node.last_seen_at = Some(Utc::now());
        // Drop the approved capabilities as well as the flag: a revoked node that
        // somehow reached the gateway should find nothing it is allowed to do.
        node.approved = Capabilities::new(Vec::<String>::new());
    })
    .map_err(ApiError::from)?
    .ok_or_else(|| ApiError::from(DomainError::not_found("node", &id)))?;
    state.swarm.hub.publish(Change::Node(written.clone()));
    // A node that is connected right now is told with the one close code that
    // makes it stop redialing for good: 4401. Anything else and it would knock
    // every 30 seconds forever, filling two logs with a decision the owner
    // already made.
    if session::revoke(&state.gateway, &id).await {
        info!(node = %id, label = %written.label, "a connected node was revoked and told to stop redialing");
    } else {
        info!(node = %id, label = %written.label, "a node was revoked");
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pairing_record_hashes_the_code_and_expires_in_ten_minutes() {
        let code = mint_code().expect("a code");
        assert_eq!(code.display.len(), 9, "{code:?}");
        assert_eq!(code.display.chars().nth(4), Some('-'), "{code:?}");
        assert_eq!(code.raw.len(), 8);

        let now = Utc::now();
        let record = PairingRecord {
            code_hash: code.hash(),
            created_at: now,
            expires_at: now + Duration::seconds(PAIRING_CODE_TTL_SECS),
            used: false,
            node_id: None,
        };
        assert_eq!(record.code_hash.len(), 64, "sha256, hex");
        assert!(
            !record.code_hash.contains(&code.raw),
            "the code is not stored"
        );
        assert!(
            !record.code_hash.contains(&code.display),
            "nor is its display form"
        );
        assert!(record.is_live(now));
        assert!(!record.is_live(record.expires_at));
        assert_eq!(PAIRING_CODE_TTL_SECS, 600);
        // Single use: the flag is what `POST /node/pair` will check.
        assert!(
            !PairingRecord {
                used: true,
                ..record
            }
            .is_live(now)
        );
    }
}
