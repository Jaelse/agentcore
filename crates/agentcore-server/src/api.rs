//! Operator REST API and live event stream.

use std::convert::Infallible;
use std::time::Duration;

use agentcore_core::{Event, SessionId, SessionInfo};
use agentcore_runtime::{ApprovalDecision, CreateSession};
use agentcore_store::{NewProvider, ProviderUpdate};
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
use crate::sessions::SessionRef;

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
        .into_iter()
        .map(|a| {
            json!({
                "name": a.name,
                "adapter": a.adapter,
                "description": a.description,
                "policy": a.policy,
                "catalog": a.catalog,
                "provider": a.provider,
                "model": a.model,
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
        "model_gateway": {
            "log_bodies": config.model_gateway.log_bodies,
            "max_logged_body_bytes": config.model_gateway.max_logged_body_bytes,
        },
        "agents": agents,
        "policies": policies,
    }))
}

pub async fn list_sessions(
    State(state): State<AppState>,
    _caller: Caller,
) -> ApiResult<Json<Value>> {
    // Archived sessions come from the database; live ones from memory, which
    // has fresher state (pending approvals, counters).
    let mut sessions: Vec<SessionInfo> = state
        .store
        .list_sessions(200)
        .await?
        .into_iter()
        .map(|r| r.info)
        .collect();
    for live in state.manager.list() {
        match sessions.iter_mut().find(|s| s.id == live.id) {
            Some(slot) => *slot = live,
            None => sessions.push(live),
        }
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.created_at));
    Ok(Json(json!(sessions)))
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
    let models = state.store.enabled_endpoints().await?;
    let options = agentcore_runtime::SessionOptions {
        models,
        ..Default::default()
    };
    let session = state.manager.create(request, caller.principal(), options)?;
    state.persist(&session).await;
    state.follow(session.clone());
    Ok((StatusCode::CREATED, Json(json!(session.info()))))
}

pub async fn get_session(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<SessionId>,
) -> ApiResult<Json<Value>> {
    let session = state.lookup(id).await?;
    let info = session.info();
    let (approvals, proposal) = match &session {
        SessionRef::Live(s) => (json!(s.pending_approvals()), json!(s.proposal())),
        SessionRef::Archived(_) => {
            let proposal = session
                .events()
                .await?
                .into_iter()
                .rev()
                .find_map(|e| match e.kind {
                    agentcore_core::EventKind::PullRequestProposed { proposal } => Some(proposal),
                    _ => None,
                });
            (json!([]), json!(proposal))
        }
    };
    Ok(Json(json!({
        "session": info,
        "live": matches!(session, SessionRef::Live(_)),
        "policy": { "name": info.policy, "digest": session.policy_digest() },
        "approvals": approvals,
        "proposal": proposal,
        "role": info.context.role.as_ref().and_then(|r| state.roles.get(r)).map(|r| json!({
            "name": r.name, "title": r.display_title(), "delivery": r.delivery.kind,
            "checks": r.checks.iter().map(|c| &c.name).collect::<Vec<_>>(),
        })),
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
    let session = state.lookup(id).await?;
    let SessionRef::Live(session) = session else {
        // Already over: stopping is a no-op.
        return Ok(Json(json!(session.info())));
    };
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
    // Department agents on every node, then everything running here; other
    // nodes stop their own sessions when they hear about it.
    let agents = crate::org::stop_all_agents(&state, &caller.name, &reason).await?;
    let stopped = state.manager.stop_all(caller.principal(), &reason).await;
    Ok(Json(
        json!({ "stopped": stopped, "department_agents": agents }),
    ))
}

pub async fn list_approvals(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<SessionId>,
) -> ApiResult<Json<Value>> {
    Ok(Json(match state.lookup(id).await? {
        SessionRef::Live(s) => json!(s.pending_approvals()),
        SessionRef::Archived(_) => json!([]),
    }))
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
    state.lookup(id).await?.live()?.resolve_approval(
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
    let events = state.lookup(id).await?.events().await?;
    Ok(Json(
        events.into_iter().filter(is_new(query.after)).collect(),
    ))
}

/// Server-sent events: replays history then streams live events. If the
/// client falls too far behind, a `lagged` event is sent and the stream ends;
/// clients reconnect with `?after=<last seq>`. For archived sessions the
/// stream replays the audit log and then stays idle.
pub async fn stream(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<SessionId>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<sse::Event, Infallible>>>> {
    let keep = is_new(query.after);
    let (history, live) = match state.lookup(id).await? {
        SessionRef::Live(session) => {
            let (history, rx) = session.subscribe();
            (history, Some(rx))
        }
        archived @ SessionRef::Archived(_) => (archived.events().await?, None),
    };
    let history: Vec<_> = history.into_iter().filter(keep).collect();
    let replay = stream::iter(history.into_iter().map(Ok));
    let live = match live {
        Some(rx) => BroadcastStream::new(rx).map(|r| r.map_err(|_| ())).boxed(),
        None => stream::pending().boxed(),
    };
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
    let path = state.lookup(id).await?.audit_path();
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
    let path = state.lookup(id).await?.audit_path();
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

/// Full request and response of one model call.
pub async fn model_call(
    State(state): State<AppState>,
    _caller: Caller,
    Path((id, call_id)): Path<(SessionId, Uuid)>,
) -> ApiResult<Json<Value>> {
    match state.store.get_model_call(id, call_id).await? {
        Some(call) => Ok(Json(json!(call))),
        None => Err(ApiError::new(StatusCode::NOT_FOUND, "model call not found")),
    }
}

// ---- settings: model providers ------------------------------------------------

pub async fn list_providers(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    Ok(Json(json!(state.store.list_providers().await?)))
}

pub async fn create_provider(
    State(state): State<AppState>,
    caller: Caller,
    Json(provider): Json<NewProvider>,
) -> ApiResult<impl IntoResponse> {
    caller.require_admin()?;
    let created = state.store.create_provider(provider, &caller.name).await?;
    Ok((StatusCode::CREATED, Json(json!(created))))
}

pub async fn update_provider(
    State(state): State<AppState>,
    caller: Caller,
    Path(name): Path<String>,
    Json(update): Json<ProviderUpdate>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    Ok(Json(json!(
        state
            .store
            .update_provider(&name, update, &caller.name)
            .await?
    )))
}

pub async fn delete_provider(
    State(state): State<AppState>,
    caller: Caller,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    state.store.delete_provider(&name, &caller.name).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Configuration change history.
pub async fn admin_events(State(state): State<AppState>, caller: Caller) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    Ok(Json(json!(state.store.admin_events(200).await?)))
}
