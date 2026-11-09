//! Routines: `/api/routines`.
//!
//! Routines are scheduled prompts that wake a bot on an interval or cron.
//! These endpoints allow listing, creating, pausing, resuming, and deleting routines,
//! as well as inspecting past runs.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::Utc;
use serde::Deserialize;

use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::RoutineId;
use swarmbot_contracts::routine::{Routine, RoutineRun, RoutineSchedule};

use crate::error::{ApiResult, parse_id};
use crate::routes::JsonBody;
use crate::routes::resolve_bot;
use crate::state::AppState;

/// `GET /api/routines?bot=<BotIdOrName>`
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RoutineQuery {
    #[serde(default)]
    pub bot: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct NewRoutine {
    pub bot: String,
    pub name: String,
    pub prompt: String,
    pub schedule: RoutineSchedule,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RunsQuery {
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `GET /api/routines`
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<RoutineQuery>,
) -> ApiResult<Json<Vec<Routine>>> {
    let routines = match query.bot.as_deref() {
        Some(key) => {
            let bot = resolve_bot(&state, key)?;
            state.swarm.routines.list_routines_for_bot(&bot.id)?
        }
        None => state.swarm.routines.list_routines()?,
    };
    Ok(Json(routines))
}

/// `POST /api/routines`
pub async fn create(
    State(state): State<AppState>,
    JsonBody(input): JsonBody<NewRoutine>,
) -> ApiResult<(StatusCode, Json<Routine>)> {
    let bot = resolve_bot(&state, &input.bot)?;
    let routine = state.swarm.routines.create_routine(
        bot.id,
        input.name,
        input.prompt,
        input.schedule,
        Utc::now(),
    )?;
    Ok((StatusCode::CREATED, Json(routine)))
}

/// `GET /api/routines/{id}`
pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Routine>> {
    let id = parse_id::<RoutineId>("routine", &id)?;
    let routine = state
        .swarm
        .routines
        .get_routine(&id)?
        .ok_or_else(|| DomainError::not_found("routine", &id))?;
    Ok(Json(routine))
}

/// `POST /api/routines/{id}/pause`
pub async fn pause(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Routine>> {
    let id = parse_id::<RoutineId>("routine", &id)?;
    let routine = state.swarm.routines.pause_routine(&id)?;
    Ok(Json(routine))
}

/// `POST /api/routines/{id}/resume`
pub async fn resume(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Routine>> {
    let id = parse_id::<RoutineId>("routine", &id)?;
    let routine = state.swarm.routines.resume_routine(&id, Utc::now())?;
    Ok(Json(routine))
}

/// `DELETE /api/routines/{id}`
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = parse_id::<RoutineId>("routine", &id)?;
    state.swarm.routines.delete_routine(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/routines/{id}/runs`
pub async fn list_runs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<RunsQuery>,
) -> ApiResult<Json<Vec<RoutineRun>>> {
    let id = parse_id::<RoutineId>("routine", &id)?;
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let runs = state.swarm.routines.list_runs(&id, limit)?;
    Ok(Json(runs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_routine_deserializes() {
        let value = json!({
            "bot": "scout",
            "name": "Sync",
            "prompt": "summarize",
            "schedule": {
                "kind": "interval",
                "every_secs": 60
            }
        });
        let req: NewRoutine = serde_json::from_value(value).expect("deserializes");
        assert_eq!(req.name, "Sync");
        assert_eq!(req.bot, "scout");
    }
}
