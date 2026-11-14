//! Nodes: `/api/nodes`.
//!
//! The local node is created at boot by the swarm; a remote one arrives by
//! pairing, which is M6's work. What exists today is the owner's view of the
//! roster, the minting of a pairing code, an owner's edit of one row
//! ([`patch`]), and revocation.
//!
//! A pairing code is shown once and stored as `sha256("swarmbot-pairing-v1|<code>")`
//! (decision D1.3), so a stolen database cannot hand out node registrations: the
//! plain code only ever exists in this response and in the operator's hands.
//!
//! ## Why `PATCH` exists at all
//!
//! Until it did, the first `node.hello` approved every capability a node declared
//! (`gateway::session::apply_hello`), so handing out a pairing code was the whole
//! consent step: the owner never saw the scope they granted. `PATCH` is the one
//! place an owner can look at `declared` and say which subset is allowed, and it
//! can only ever **narrow** that subset — `approved ⊆ declared` is enforced here
//! and the effective set is the intersection, so no request can raise a node's
//! ceiling from the brain.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::node::{Capabilities, NodeInfo, PairingRecord};
use swarmbot_core::Change;

use swarmbot_node_protocol::pairing::mint_code;

use crate::auth::PAIRING_CODE_TTL_SECS;
use crate::error::{ApiError, ApiResult};
use crate::gateway::session;
use crate::routes::{JsonBody, node_id};
use crate::state::AppState;

/// `GET /api/nodes`
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<NodeInfo>>> {
    // Through the async facade: a route runs on a runtime worker, and the sync
    // port would park that worker for the whole SQLite call — a contended store
    // would then stall every other request sharing it.
    let nodes = state.async_store().read(|store| store.list_nodes()).await?;
    Ok(Json(nodes))
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
    let pairing = record.clone();
    state
        .async_store()
        .write(move |store| store.insert_pairing(&pairing))
        .await?;
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

/// `PATCH /api/nodes/{id}` body — every field optional, only what is present
/// changes.
///
/// `label` is the roster's display name. `approved` is the capability set the
/// owner allows, and it **replaces** the stored one rather than adding to it: an
/// approval list that could only grow would be unrevokable, and "one capability
/// off" is the most common thing an owner wants to say. Sending an empty list is
/// therefore meaningful — it approves nothing — which is why the field is
/// `Option<Vec<String>>` and not a plain `Vec`: absent and empty must stay
/// different.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchNodeRequest {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub approved: Option<Vec<String>>,
}

/// `PATCH /api/nodes/{id}`
///
/// The write goes through [`session::mutate_node`], the gateway's guarded writer,
/// so an owner's edit and a heartbeat cannot lose each other: the row is re-read
/// inside a revision-checked retry loop and the change is re-applied to whatever
/// the row says now. A bare `update_node` here would hand a heartbeat a stale row
/// to restore — the lost-update shape `AGENTS.md` records.
///
/// Revoking is *not* undone by this route: a revoked row is refused with 409, and
/// the way back is pairing again. Re-approving a machine the owner cut off would
/// be the one write that silently re-grants trust.
pub async fn patch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<PatchNodeRequest>,
) -> ApiResult<Json<NodeInfo>> {
    let id = node_id(&id)?;
    let wanted = id.clone();
    let Some(node) = state
        .async_store()
        .read(move |store| store.get_node(&wanted))
        .await?
    else {
        return Err(ApiError::from(DomainError::not_found("node", &id)));
    };
    if node.revoked {
        return Err(ApiError::conflict(
            "this node was revoked and cannot be re-approved through PATCH; pair it again",
        ));
    }
    // Validation runs on this read, and the write below re-reads on conflict. A
    // `node.hello` landing in between can change `declared`, and that is
    // deliberately not a problem: the effective set is `declared ∩ approved`, so
    // a capability the node no longer declares is no longer allowed either —
    // this route can only ever narrow, never widen, what a node may do.
    let approved = match body.approved {
        Some(requested) => Some(narrow_approved(&node.declared, &requested).map_err(
            |unknown| {
                ApiError::invalid(format!(
                    "capabilit{} {} not declared by node `{}` (it declared: {}); \
                     a node's ceiling cannot be raised from the brain",
                    if unknown.len() == 1 { "y" } else { "ies" },
                    quote_list(&unknown),
                    node.label,
                    declared_list(&node.declared),
                ))
            },
        )?),
        None => None,
    };
    let label = match body.label {
        Some(label) => {
            let trimmed = label.trim();
            if trimmed.is_empty() {
                return Err(ApiError::invalid(
                    "a node label cannot be empty: the roster shows it as the machine's name",
                ));
            }
            Some(trimmed.to_string())
        }
        None => None,
    };
    if label.is_none() && approved.is_none() {
        // Nothing to change. Answering the current row is the honest answer, and
        // it costs the store no write at all.
        return Ok(Json(node));
    }
    let written = session::mutate_node(state.swarm.store.as_ref(), &id, |row| {
        if let Some(label) = &label {
            row.label = label.clone();
        }
        if let Some(approved) = &approved {
            row.approved = approved.clone();
        }
    })
    .map_err(ApiError::from)?
    .ok_or_else(|| ApiError::from(DomainError::not_found("node", &id)))?;
    // The roster row is a live fact like any other: a client showing the old
    // approval set would go on showing a trust the owner has already withdrawn.
    state.swarm.hub.publish(Change::Node(written.clone()));
    info!(
        node = %id,
        label = %written.label,
        approved = ?written.approved.as_slice(),
        "a node row was edited by the owner"
    );
    Ok(Json(written))
}

/// Resolve a requested approval list against what the node declared.
///
/// Two things have to happen here, and skipping either is a silent bug:
///
/// * **an undeclared capability is refused**, naming it, because approving what a
///   node never declared is a no-op dressed up as consent;
/// * **a bare name is resolved to the declared spelling** (`fs` → `fs@1`).
///   [`Capabilities::effective`] compares strings exactly, so storing the bare
///   name would approve nothing at all while telling the owner it worked.
///
/// Duplicates collapse, and the result follows the declared order, so two
/// requests that asked for the same set store the same row.
fn narrow_approved(
    declared: &Capabilities,
    requested: &[String],
) -> Result<Capabilities, Vec<String>> {
    let mut unknown: Vec<String> = Vec::new();
    let mut approved: Vec<String> = Vec::new();
    for name in requested {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            if !unknown.iter().any(String::is_empty) {
                unknown.push(String::new());
            }
            continue;
        }
        let full = if trimmed.contains('@') {
            trimmed.to_string()
        } else {
            format!("{trimmed}@1")
        };
        match declared
            .as_slice()
            .iter()
            .find(|declared| **declared == full || **declared == trimmed)
        {
            Some(spelling) if !approved.contains(spelling) => approved.push(spelling.clone()),
            Some(_) => {}
            None => {
                if !unknown.contains(&full) {
                    unknown.push(full);
                }
            }
        }
    }
    if !unknown.is_empty() {
        return Err(unknown);
    }
    approved.sort_by_key(|name| {
        declared
            .as_slice()
            .iter()
            .position(|declared| declared == name)
            .unwrap_or(usize::MAX)
    });
    Ok(Capabilities::new(approved))
}

/// The declared set as readable text, for the refusal above.
fn declared_list(declared: &Capabilities) -> String {
    if declared.as_slice().is_empty() {
        return "nothing".to_string();
    }
    declared.as_slice().join(", ")
}

/// `` `a`, `b` ``, quoted because an unquoted list would read as prose. An empty
/// entry is spelled out: a blank inside backticks is invisible in a terminal and
/// in a UI toast.
fn quote_list(names: &[String]) -> String {
    names
        .iter()
        .map(|name| {
            if name.trim().is_empty() {
                "<empty name>".to_string()
            } else {
                format!("`{name}`")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
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
    let wanted = id.clone();
    let Some(node) = state
        .async_store()
        .read(move |store| store.get_node(&wanted))
        .await?
    else {
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

    fn declared() -> Capabilities {
        Capabilities::new(["fs@1", "shell@1"])
    }

    #[test]
    fn a_bare_capability_name_is_resolved_to_the_declared_spelling() {
        // `Capabilities::effective` compares strings exactly, so storing `fs`
        // would approve nothing while telling the owner it worked. This is the
        // bug the normalisation exists for.
        let approved =
            narrow_approved(&declared(), &["fs".to_string()]).expect("declared capability");
        assert_eq!(approved.as_slice(), ["fs@1"]);
        assert!(
            declared().effective(&approved).has("fs"),
            "the stored form actually approves `fs`"
        );
        assert!(
            !declared().effective(&approved).has("shell"),
            "and it approves nothing else"
        );
    }

    #[test]
    fn an_undeclared_capability_is_refused_and_named() {
        let unknown = narrow_approved(
            &declared(),
            &["fs".to_string(), "pty".to_string(), "pty@1".to_string()],
        )
        .expect_err("pty was never declared");
        // Deduplicated, and shown in the form the node would have had to declare.
        assert_eq!(unknown, ["pty@1"]);
        assert_eq!(quote_list(&unknown), "`pty@1`");
        assert_eq!(declared_list(&declared()), "fs@1, shell@1");
    }

    #[test]
    fn an_empty_request_approves_nothing_and_says_so() {
        // `approved: []` is a meaningful write — it turns every capability off —
        // which is why the field is an `Option` and not a defaulted `Vec`.
        let approved = narrow_approved(&declared(), &[]).expect("an empty set is valid");
        assert!(approved.as_slice().is_empty());
        assert!(declared().effective(&approved).as_slice().is_empty());
    }

    #[test]
    fn duplicates_collapse_and_the_declared_order_is_kept() {
        let approved = narrow_approved(
            &declared(),
            &["shell".to_string(), "fs@1".to_string(), "fs".to_string()],
        )
        .expect("all declared");
        assert_eq!(
            approved.as_slice(),
            ["fs@1", "shell@1"],
            "one entry each, in the order the node declared them"
        );
    }

    #[test]
    fn an_empty_name_is_refused_rather_than_silently_dropped() {
        let unknown =
            narrow_approved(&declared(), &["  ".to_string()]).expect_err("nothing was named");
        assert!(unknown.iter().any(String::is_empty));
    }

    #[test]
    fn the_patch_body_distinguishes_absent_from_empty() {
        let absent: PatchNodeRequest =
            serde_json::from_value(serde_json::json!({})).expect("parses");
        assert!(absent.label.is_none() && absent.approved.is_none());
        let empty: PatchNodeRequest =
            serde_json::from_value(serde_json::json!({"approved": []})).expect("parses");
        assert_eq!(empty.approved.as_deref(), Some([].as_slice()));
        // A typo must be a 400, not a field this API silently ignores.
        assert!(
            serde_json::from_value::<PatchNodeRequest>(serde_json::json!({"approve": ["fs"]}))
                .is_err()
        );
    }
}
