//! Approval rules: `/api/rules`.
//!
//! Rules are the saved answers that keep the same question from being asked
//! twice; "allow always" on a card writes one (see
//! [`swarmbot_core::ApprovalService::respond`]). These routes are how the owner
//! reads and unsaves them.
//!
//! A rule is accepted as the domain type it is — no parallel DTO — because the
//! client builds it with [`swarmbot_contracts::request::ApprovalRule::new`] and
//! posts it verbatim. What this route adds is the check the wire cannot make: a
//! bot-scoped rule has to name a bot that exists.
//!
//! A rule with no matcher at all ("deny everything, always") is *accepted*, not
//! refused: it is a legitimate kill switch and refusing a shape the client crate
//! builds would be a broken API, not a safety feature. It is logged loudly,
//! because a blanket rule is the kind of thing an owner forgets they saved.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use tracing::{info, warn};

use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::RuleId;
use swarmbot_contracts::request::{ApprovalRule, RuleScope};

use crate::error::{ApiError, ApiResult, parse_id};
use crate::routes::JsonBody;
use crate::state::AppState;

/// `GET /api/rules`
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<ApprovalRule>>> {
    Ok(Json(state.swarm.approval.rules()?))
}

/// `POST /api/rules`
pub async fn create(
    State(state): State<AppState>,
    JsonBody(rule): JsonBody<ApprovalRule>,
) -> ApiResult<(StatusCode, Json<ApprovalRule>)> {
    if let RuleScope::Bot(bot) = &rule.scope
        && state.swarm.store.get_bot(bot)?.is_none()
    {
        return Err(ApiError::from(DomainError::not_found("bot", bot)));
    }
    let blanket = rule.surface.is_none() && rule.tool.is_none() && rule.command_prefix.is_none();
    let saved = state.swarm.approval.add_rule(rule)?;
    if blanket {
        warn!(
            rule = %saved.id,
            effect = ?saved.effect,
            scope = ?saved.scope,
            "a rule with no matcher was saved: it decides every request in its scope"
        );
    } else {
        info!(
            rule = %saved.id,
            effect = ?saved.effect,
            scope = ?saved.scope,
            "an approval rule was saved"
        );
    }
    Ok((StatusCode::CREATED, Json(saved)))
}

/// `DELETE /api/rules/{id}`
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = parse_id::<RuleId>("rule", &id)?;
    // The store's delete is a silent no-op for a missing row; an owner who asked
    // to unsave a rule deserves to hear that there was nothing to unsave.
    if !state
        .swarm
        .approval
        .rules()?
        .iter()
        .any(|rule| rule.id == id)
    {
        return Err(ApiError::from(DomainError::not_found("rule", &id)));
    }
    state.swarm.approval.delete_rule(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use swarmbot_contracts::node::Surface;
    use swarmbot_contracts::request::RuleEffect;

    #[test]
    fn a_rule_arrives_in_its_domain_shape() {
        let mut rule = ApprovalRule::new(RuleScope::Global, RuleEffect::Allow);
        rule.surface = Some(Surface::Shell);
        rule.command_prefix = Some("cargo".into());
        let value = serde_json::to_value(&rule).expect("serializes");
        let back: ApprovalRule = serde_json::from_value(value.clone()).expect("round-trips");
        assert_eq!(back, rule);
        assert_eq!(value["scope"], json!("global"));
        assert_eq!(value["surface"], json!("shell"));

        let bot_rule = ApprovalRule::new(
            RuleScope::Bot(swarmbot_contracts::id::BotId::new()),
            RuleEffect::Deny,
        );
        let value = serde_json::to_value(&bot_rule).expect("serializes");
        assert!(value["scope"]["bot"].is_string(), "{value}");
    }
}
