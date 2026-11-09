//! Tasks: `/api/tasks`.
//!
//! The store has no "every task" query — it indexes tasks by owner and by origin
//! conversation, which are the two questions a runner asks. An owner looking at a
//! task pane wants neither, so this module sweeps both indexes and deduplicates:
//! a task whose bot was deleted still belongs to the conversation that asked for
//! it, and dropping it from the list would hide work that was started.
//!
//! The list is newest-first. That is not documented in the API table, and it is
//! the one ordering choice here a client could reasonably disagree with; a task
//! pane wants the work that just started at the top.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::TaskId;
use swarmbot_contracts::task::{Task, TaskStep};

use crate::error::{ApiResult, parse_id};
use crate::routes::resolve_bot;
use crate::state::AppState;

/// `GET /api/tasks?bot=<BotId>`
#[derive(Clone, Debug, Default, Deserialize)]
pub struct TaskQuery {
    /// A bot id or handle; absent means every bot.
    #[serde(default)]
    pub bot: Option<String>,
}

/// `GET /api/tasks`
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<TaskQuery>,
) -> ApiResult<Json<Vec<Task>>> {
    let tasks = match query.bot.as_deref() {
        Some(key) => {
            let bot = resolve_bot(&state, key)?;
            state.swarm.tasks.list(&bot.id)?
        }
        None => all_tasks(&state)?,
    };
    Ok(Json(tasks))
}

/// `GET /api/tasks/{id}` → `{"task":…,"steps":[…]}`
pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let id = parse_id::<TaskId>("task", &id)?;
    let task = state
        .swarm
        .tasks
        .get(&id)?
        .ok_or_else(|| DomainError::not_found("task", &id))?;
    let steps = state.swarm.tasks.steps(&id)?;
    Ok(Json(detail_body(&task, &steps)))
}

/// `POST /api/tasks/{id}/cancel`
///
/// `by_owner` is true: the bot that owned the work is told, and the origin
/// conversation gets an event line. A silent disappearance would look like a bug
/// in the delegation graph.
pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Task>> {
    let id = parse_id::<TaskId>("task", &id)?;
    let task = state.swarm.tasks.cancel(&id, true)?;
    Ok(Json(task))
}

/// Every task the store knows about, newest first.
fn all_tasks(state: &AppState) -> Result<Vec<Task>, DomainError> {
    let store = state.swarm.store.clone();
    let mut tasks: Vec<Task> = Vec::new();
    for bot in store.list_bots()? {
        for task in state.swarm.tasks.list(&bot.id)? {
            push_unique(&mut tasks, task);
        }
    }
    for conversation in store.list_conversations()? {
        for task in state.swarm.tasks.in_conversation(&conversation.id)? {
            push_unique(&mut tasks, task);
        }
    }
    tasks.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(tasks)
}

fn push_unique(tasks: &mut Vec<Task>, task: Task) {
    if !tasks.iter().any(|known| known.id == task.id) {
        tasks.push(task);
    }
}

/// `GET /api/tasks/{id}`'s body: the task and its own progress in one document,
/// so a pane renders without a second round trip (and cannot show a task whose
/// steps came from an older read).
fn detail_body(task: &Task, steps: &[TaskStep]) -> Value {
    json!({ "task": task, "steps": steps })
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::id::{BotId, ConversationId};
    use swarmbot_contracts::task::StepKind;

    #[test]
    fn the_task_filter_takes_a_bot_or_nothing() {
        let query: TaskQuery = serde_json::from_value(json!({})).expect("parses");
        assert!(query.bot.is_none());
        let query: TaskQuery =
            serde_json::from_value(json!({"bot": "bot_0189fbd3a4d37f0e9c0e6a3f8f1c9d2e"}))
                .expect("parses");
        assert!(query.bot.is_some());
    }

    #[test]
    fn the_detail_body_is_a_task_and_its_steps() {
        let task = Task::own(
            BotId::new(),
            ConversationId::new(),
            "audit the diff",
            "look for risky commands",
        );
        let steps = vec![TaskStep::new(
            task.id.clone(),
            1,
            StepKind::Notice,
            "queued for the bot",
        )];
        let detail = detail_body(&task, &steps);
        assert_eq!(detail["task"]["title"], "audit the diff");
        assert_eq!(detail["steps"][0]["kind"], "notice");
        assert_eq!(detail["steps"][0]["seq"], 1);
        // The shape the client decodes into `TaskDetail`.
        assert_eq!(detail.as_object().expect("an object").len(), 2);
    }
}
