//! Operator REST API and live event stream.

use std::convert::Infallible;
use std::time::Duration;

use agentcore_core::{Event, SessionId};
use agentcore_runtime::{ApprovalDecision, CreateSession};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, sse};
use futures::{Stream, StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

use crate::AppState;
use crate::auth::Caller;
use crate::error::ApiError;

type ApiResult<T> = Result<T, ApiError>;

pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

pub async fn whoami(caller: Caller) -> Json<Value> {
    Json(json!({ "name": caller.name, "role": caller.role }))
}

/// Transparency information about the system (EU AI Act Art. 13 / 50).
pub async fn system_card(State(state): State<AppState>, _caller: Caller) -> Json<Value> {
    let config = &state.config;
    let policies: Vec<_> = state
        .manager
        .policies()
        .iter()
        .map(|p| {
            json!({
                "name": p.name(),
                "description": p.policy().description,
                "digest": p.digest(),
                "default": p.policy().default,
                "limits": p.limits(),
                "rules": p.policy().rules,
            })
        })
        .collect();
    let mut agents: Vec<_> = state
        .manager
        .agents()
        .map(|a| {
            json!({
                "name": a.name,
                "adapter": a.adapter,
                "description": a.description,
                "policy": a.policy,
            })
        })
        .collect();
    agents.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Json(json!({
        "ai_system": true,
        "transparency": config.transparency,
        "version": env!("CARGO_PKG_VERSION"),
        "sandbox_backend": state.manager.sandbox_backend(),
        "default_policy": config.policies.default,
        "audit_retention_days": config.storage.audit_retention_days,
        "agents": agents,
        "policies": policies,
    }))
}

pub async fn list_sessions(State(state): State<AppState>, _caller: Caller) -> Json<Value> {
    Json(json!(state.manager.list()))
}

pub async fn create_session(
    State(state): State<AppState>,
    caller: Caller,
    Json(request): Json<CreateSession>,
) -> ApiResult<impl IntoResponse> {
    caller.require_operator()?;
    if request.task.trim().is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "task must not be empty",
        ));
    }
    let session = state.manager.create(request, caller.principal())?;
    Ok((StatusCode::CREATED, Json(json!(session.info()))))
}

pub async fn get_session(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<SessionId>,
) -> ApiResult<Json<Value>> {
    let session = state.manager.get(id)?;
    Ok(Json(json!({
        "session": session.info(),
        "policy": {
            "name": session.policy().name(),
            "digest": session.policy().digest(),
        },
        "approvals": session.pending_approvals(),
    })))
}

#[derive(Debug, Default, Deserialize)]
pub struct StopRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

pub async fn stop_session(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<SessionId>,
    body: Option<Json<StopRequest>>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let session = state.manager.get(id)?;
    let reason = body
        .and_then(|Json(b)| b.reason)
        .unwrap_or_else(|| "stopped by operator".into());
    session.stop(caller.principal(), reason).await;
    Ok(Json(json!(session.info())))
}

pub async fn stop_all(
    State(state): State<AppState>,
    caller: Caller,
    body: Option<Json<StopRequest>>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let reason = body
        .and_then(|Json(b)| b.reason)
        .unwrap_or_else(|| "emergency stop".into());
    let stopped = state.manager.stop_all(caller.principal(), &reason).await;
    Ok(Json(json!({ "stopped": stopped })))
}

pub async fn list_approvals(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<SessionId>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(state.manager.get(id)?.pending_approvals())))
}

#[derive(Debug, Deserialize)]
pub struct DecisionRequest {
    pub approved: bool,
    #[serde(default)]
    pub comment: Option<String>,
}

pub async fn decide_approval(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, approval_id)): Path<(SessionId, Uuid)>,
    Json(decision): Json<DecisionRequest>,
) -> ApiResult<StatusCode> {
    caller.require_operator()?;
    state.manager.get(id)?.resolve_approval(
        approval_id,
        ApprovalDecision {
            approved: decision.approved,
            by: caller.principal(),
            comment: decision.comment.filter(|c| !c.trim().is_empty()),
        },
    )?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Default, Deserialize)]
pub struct EventsQuery {
    /// Only return events with `seq` greater than this.
    #[serde(default)]
    pub after: Option<u64>,
}

fn is_new(after: Option<u64>) -> impl Fn(&Event) -> bool {
    move |e| after.is_none_or(|a| e.seq > a)
}

pub async fn events(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<SessionId>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Json<Vec<Event>>> {
    let (history, _) = state.manager.get(id)?.subscribe();
    Ok(Json(
        history.into_iter().filter(is_new(query.after)).collect(),
    ))
}

/// Server-sent events: replays history then streams live events. If the
/// client falls too far behind, a `lagged` event is sent and the stream ends;
/// clients reconnect with `?after=<last seq>`.
pub async fn stream(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<SessionId>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<sse::Event, Infallible>>>> {
    let (history, rx) = state.manager.get(id)?.subscribe();
    let keep = is_new(query.after);
    let history: Vec<_> = history.into_iter().filter(keep).collect();
    let replay = stream::iter(history.into_iter().map(Ok));
    let live = BroadcastStream::new(rx).map(|r| r.map_err(|_| ()));
    let events = replay.chain(live).scan(false, |ended, item| {
        if *ended {
            return futures::future::ready(None);
        }
        let out = match item {
            Ok(event) => sse::Event::default()
                .event("event")
                .id(event.seq.to_string())
                .json_data(&event)
                .unwrap_or_else(|_| sse::Event::default().event("error")),
            Err(()) => {
                *ended = true;
                sse::Event::default().event("lagged").data("{}")
            }
        };
        futures::future::ready(Some(Ok(out)))
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

/// Verify the hash chain of a session's audit log.
pub async fn verify_audit(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<SessionId>,
) -> ApiResult<Json<Value>> {
    let path = state.manager.get(id)?.audit_path().to_path_buf();
    let result = tokio::task::spawn_blocking(move || agentcore_audit::verify_file(&path))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(match result {
        Ok(head) => json!({ "valid": true, "records": head.records, "head": head.hash }),
        Err(err) => json!({ "valid": false, "error": err.to_string() }),
    }))
}

/// Download the raw audit log (JSON Lines).
pub async fn download_audit(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<SessionId>,
) -> ApiResult<impl IntoResponse> {
    let path = state.manager.get(id)?.audit_path().to_path_buf();
    let body = tokio::fs::read(&path)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/x-ndjson".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"agentcore-audit-{id}.jsonl\""),
            ),
        ],
        body,
    ))
}
