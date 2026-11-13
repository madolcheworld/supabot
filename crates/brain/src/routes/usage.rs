//! Usage: `/api/usage` — what the team's turns actually cost, as the backends
//! reported it.
//!
//! The numbers come from [`TurnRepo::turns_since`](swarmbot_contracts::repo::TurnRepo)
//! and are folded by [`UsageReport::from_turns`]: one window of turns, summed
//! per bot and in total. Two things this module deliberately does **not** do:
//!
//! * it does not estimate. Every token here was reported by a backend on a model
//!   round trip; a backend that reports nothing contributes nothing, so its
//!   turns look free. `turns` is in the body precisely so a reader can tell "no
//!   turn ran" from "three turns whose backend reported no usage" — both are
//!   zeros, and only one of them means nothing was spent.
//! * it does not price. Turning tokens into money needs the owner's per-model
//!   rates; a built-in table would be a guess dressed up as a bill.
//!
//! The window is `[since, until]` with `since` **inclusive** (the same rule the
//! store pins): `?since=` takes an RFC 3339 timestamp (the same text
//! `DateTime<Utc>` serializes as), and the default is the last
//! [`DEFAULT_WINDOW_HOURS`] hours. `until` is the moment the answer was built
//! and is enforced as an upper bound, so a turn that started while the request
//! was in flight belongs to the *next* report instead of to both. One
//! consequence, stated rather than glossed: because the boundary is inclusive,
//! two windows that share an edge both contain a turn that started at exactly
//! that instant — a caller stitching reports together should overlap by an
//! instant, not abut.

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

use swarmbot_contracts::turn::{DEFAULT_WINDOW_HOURS, UsageReport};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Re-exported so a reader of this route sees the default without chasing it
/// into `contracts`: the CLI's embedded mode answers the same question from the
/// same constant, which is why the constant lives there.
pub use swarmbot_contracts::turn::DEFAULT_WINDOW_HOURS as WINDOW_HOURS;

/// `GET /api/usage?since=<RFC 3339>`
#[derive(Clone, Debug, Default, Deserialize)]
pub struct UsageQuery {
    /// Start of the window, RFC 3339. Absent means the last
    /// [`DEFAULT_WINDOW_HOURS`] hours.
    #[serde(default)]
    pub since: Option<String>,
}

/// `GET /api/usage` → a [`UsageReport`] as JSON.
pub async fn get(
    State(state): State<AppState>,
    Query(query): Query<UsageQuery>,
) -> ApiResult<Json<UsageReport>> {
    let until = Utc::now();
    let since = match query.since.as_deref() {
        Some(raw) => parse_since(raw)?,
        None => until - Duration::hours(DEFAULT_WINDOW_HOURS),
    };
    // A window that ends before it starts is a client bug, and an empty report
    // would hide it: say so instead of answering zeros with a straight face.
    if since > until {
        return Err(ApiError::invalid(format!(
            "`since` ({}) is in the future; the report would cover an empty window",
            since.to_rfc3339()
        )));
    }

    // The window rule (inclusive `since`, `until` as an enforced upper bound,
    // a deleted bot keeping its unnamed row) lives in `UsageReport::window` so
    // the CLI's embedded mode cannot answer the same question differently.
    let report = UsageReport::window(state.swarm.store.as_ref(), since, until)?;
    Ok(Json(report))
}

/// RFC 3339 text into UTC. The message names the field and shows the value,
/// because this is the one thing a caller gets wrong by hand.
fn parse_since(raw: &str) -> ApiResult<DateTime<Utc>> {
    let trimmed = raw.trim();
    DateTime::parse_from_rfc3339(trimmed)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|err| {
            ApiError::invalid(format!(
                "`since` must be an RFC 3339 timestamp (e.g. 2026-01-31T00:00:00Z), got {raw:?}: {err}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_since_timestamp_parses_as_rfc3339_into_utc() {
        let parsed = parse_since("2026-01-31T00:00:00Z").expect("a Z timestamp");
        assert_eq!(parsed.to_rfc3339(), "2026-01-31T00:00:00+00:00");
        // An explicit offset is the same instant in UTC.
        let offset = parse_since("2026-01-31T07:00:00+07:00").expect("an offset timestamp");
        assert_eq!(offset, parsed);
        assert!(
            offset.to_rfc3339().ends_with("+00:00"),
            "normalized to UTC: {}",
            offset.to_rfc3339()
        );
        // Whitespace from a query string is trimmed, not rejected.
        assert_eq!(
            parse_since(" 2026-01-31T00:00:00Z ").expect("trimmed"),
            parsed
        );
    }

    #[test]
    fn a_malformed_since_is_invalid_input_not_a_panic() {
        let err = parse_since("yesterday").expect_err("not a timestamp");
        assert_eq!(err.code(), "invalid_input");
        let message = err.0.to_string();
        assert!(message.contains("yesterday"), "{message}");
        assert!(message.contains("RFC 3339"), "{message}");
    }

    #[test]
    fn the_query_takes_an_optional_since() {
        let query: UsageQuery = serde_json::from_value(serde_json::json!({})).expect("parses");
        assert!(query.since.is_none());
        let query: UsageQuery =
            serde_json::from_value(serde_json::json!({"since": "2026-01-31T00:00:00Z"}))
                .expect("parses");
        assert_eq!(query.since.as_deref(), Some("2026-01-31T00:00:00Z"));
    }
}
