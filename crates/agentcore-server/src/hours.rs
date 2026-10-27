//! Working hours: when each department works.
//!
//! Each node checks the schedules on its heartbeat. When a department's
//! working hours end it is paused (marked as paused by the hours) and the
//! model gateway refuses its calls; when they begin again it is resumed.
//! A department a person or a budget paused is never resumed by the hours.

use agentcore_core::{DepartmentHours, DepartmentId};
use agentcore_store::HoursInput;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::org::notify;

type ApiResult<T> = Result<T, ApiError>;

/// Pause the departments whose hours ended and resume those whose hours
/// began (each node, on its heartbeat; the updates are conditional, so each
/// change happens once).
pub async fn evaluate(state: &AppState) -> anyhow::Result<()> {
    let store = &state.store;
    let mut changed = Vec::new();
    for h in store.list_hours().await? {
        let open = !h.enabled || h.is_open(store.local_now(&h.timezone).await?);
        let moved = if open {
            // A used-up budget keeps the department paused; the next check
            // resumes it once there is room.
            !crate::budgets::budget_blocks(state, h.department_id).await
                && store.resume_for_hours(h.department_id).await?
        } else {
            store.pause_for_hours(h.department_id).await?
        };
        if moved {
            tracing::info!(department = %h.department_id, open, "working hours changed the state");
            changed.push(h.department_id);
        }
    }
    if !changed.is_empty() {
        notify(
            store,
            json!({ "kind": "department", "departments": changed }),
        )
        .await;
        state.reconcile.notify_one();
    }
    Ok(())
}

async fn applied(state: &AppState, department: DepartmentId) {
    notify(
        &state.store,
        json!({ "kind": "hours", "department": department }),
    )
    .await;
    if let Err(err) = evaluate(state).await {
        tracing::warn!(error = %err, "working hours check failed");
    }
}

/// Every schedule, and whether each department is inside its hours now.
pub async fn list(State(state): State<AppState>, _caller: Caller) -> ApiResult<Json<Value>> {
    let mut out = Vec::new();
    for h in state.store.list_hours().await? {
        let open = !h.enabled || h.is_open(state.store.local_now(&h.timezone).await?);
        out.push(json!({ "hours": h, "open_now": open }));
    }
    Ok(Json(json!({ "hours": out })))
}

pub async fn put(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<DepartmentId>,
    Json(input): Json<HoursInput>,
) -> ApiResult<Json<DepartmentHours>> {
    caller.require_admin()?;
    let hours = state.store.set_hours(id, input, &caller.name).await?;
    applied(&state, id).await;
    Ok(Json(hours))
}

pub async fn delete(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<DepartmentId>,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    state.store.delete_hours(id, &caller.name).await?;
    // No schedule: resume what the hours paused.
    if state.store.resume_for_hours(id).await? {
        state.reconcile.notify_one();
    }
    notify(&state.store, json!({ "kind": "hours", "department": id })).await;
    notify(
        &state.store,
        json!({ "kind": "department", "departments": [id] }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
