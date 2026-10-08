//! Running agentcore on several VMs.
//!
//! Every node is the same `agentcore serve` process with its own
//! `[cluster].node_name`, sharing one PostgreSQL database:
//!
//! * **Heartbeat**: each node keeps its row in `nodes` fresh; agents placed
//!   on a node that stopped responding are marked stopped by the others.
//! * **Notifications**: changes are announced with `NOTIFY agentcore_org`;
//!   every node listens, forwards them to its `/org/stream` clients and wakes
//!   its reconciler.
//! * **Reconciler**: compares what PostgreSQL says each agent placed on this
//!   node should be doing with the sessions the node runs, and acts (start,
//!   pause, resume, stop, deliver messages). It also runs periodically, so a
//!   lost notification only delays a command.
//! * **Forwarding**: session endpoints are served by the node that owns the
//!   session; other nodes forward requests (and streams) to it.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use agentcore_core::{DepartmentState, Desired, OrgAgent, SessionStatus};
use agentcore_runtime::{EndCause, Session};
use agentcore_store::ORG_CHANNEL;
use axum::body::Body;
use axum::extract::{FromRequestParts, OriginalUri, Request, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures::TryStreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::AppState;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::org::{self, Org};

/// Marks a request forwarded by another node (never forwarded again).
pub const FORWARDED: HeaderName = HeaderName::from_static("x-agentcore-forwarded-by");
const MAX_FORWARD_BODY: usize = 32 * 1024 * 1024;

fn status_str(status: SessionStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// Register this node and start its background work. Cancel the returned
/// token to stop it (tests use this to simulate a lost node).
pub async fn start(state: &AppState) -> anyhow::Result<CancellationToken> {
    let cluster = &state.config.cluster;
    state
        .store
        .heartbeat(
            &state.node,
            &state.config.internal_url(),
            cluster.max_agents,
            env!("CARGO_PKG_VERSION"),
            true,
        )
        .await?;
    tracing::info!(node = %state.node, url = %state.config.internal_url(), "node registered");
    let cancel = CancellationToken::new();
    tokio::spawn(heartbeat(state.clone(), cancel.clone()));
    tokio::spawn(listen(state.clone(), cancel.clone()));
    tokio::spawn(reconciler(state.clone(), cancel.clone()));
    Ok(cancel)
}

async fn heartbeat(state: AppState, cancel: CancellationToken) {
    let cluster = state.config.cluster.clone();
    let mut tick = tokio::time::interval(Duration::from_secs(cluster.heartbeat_secs));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            () = cancel.cancelled() => return,
        }
        if let Err(err) = state
            .store
            .heartbeat(
                &state.node,
                &state.config.internal_url(),
                cluster.max_agents,
                env!("CARGO_PKG_VERSION"),
                false,
            )
            .await
        {
            tracing::warn!(error = %err, "heartbeat failed");
            continue;
        }
        // Any node may notice that another one is gone.
        match state
            .store
            .fail_agents_on_dead_nodes(cluster.node_timeout_secs)
            .await
        {
            Ok(lost) if !lost.is_empty() => {
                for agent in &lost {
                    tracing::warn!(
                        agent = %agent.name,
                        node = agent.node.as_deref().unwrap_or("?"),
                        "agent lost with its node"
                    );
                }
                org::notify(&state.store, json!({ "kind": "agents" })).await;
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(error = %err, "checking for lost nodes failed"),
        }
        // Any node may send due check-ins; each is claimed by exactly one.
        if let Err(err) = run_check_ins(&state).await {
            tracing::warn!(error = %err, "sending check-ins failed");
        }
    }
}

/// Send the check-ins that are due. Their messages wake sleeping agents.
pub async fn run_check_ins(state: &AppState) -> anyhow::Result<usize> {
    let due = state.store.claim_due_schedules().await?;
    for check_in in &due {
        let to = match check_in.agent_id {
            Some(agent) => org::Recipient::Agent(agent),
            None => org::Recipient::Department(check_in.department_id),
        };
        let from = org::Sender::Human(format!("check-in: {}", check_in.name));
        match org::send(&state.store, &from, to, &check_in.message).await {
            Ok(_) => tracing::info!(check_in = %check_in.name, "check-in sent"),
            Err(err) => {
                tracing::warn!(check_in = %check_in.name, error = %err, "check-in not sent");
            }
        }
    }
    if !due.is_empty() {
        org::notify(&state.store, json!({ "kind": "checkins" })).await;
    }
    Ok(due.len())
}

async fn listen(state: AppState, cancel: CancellationToken) {
    loop {
        let mut listener = match sqlx::postgres::PgListener::connect_with(state.store.pool()).await
        {
            Ok(l) => l,
            Err(err) => {
                tracing::warn!(error = %err, "cannot listen for organisation changes; retrying");
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(2)) => continue,
                    () = cancel.cancelled() => return,
                }
            }
        };
        if let Err(err) = listener.listen(ORG_CHANNEL).await {
            tracing::warn!(error = %err, "LISTEN failed; retrying");
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        // Anything missed while disconnected: reconcile and tell the UIs.
        state.reconcile.notify_one();
        let _ = state.org_events.send(json!({ "kind": "resync" }));
        loop {
            let notification = tokio::select! {
                n = listener.recv() => n,
                () = cancel.cancelled() => return,
            };
            match notification {
                Ok(n) => {
                    let payload: Value =
                        serde_json::from_str(n.payload()).unwrap_or_else(|_| json!({}));
                    on_notification(&state, &payload).await;
                    let _ = state.org_events.send(payload);
                }
                Err(err) => {
                    tracing::warn!(error = %err, "lost the notification connection; reconnecting");
                    break;
                }
            }
        }
    }
}

async fn on_notification(state: &AppState, payload: &Value) {
    if payload["kind"] == "agents_installed" {
        // An admin added, changed or removed a catalogue agent somewhere.
        state.reload_agents().await;
    }
    if payload["kind"] == "stop_all" && payload["origin"].as_str() != Some(&state.node) {
        // The emergency stop also covers sessions outside departments.
        let by = payload["by"].as_str().unwrap_or("agentcore");
        let reason = payload["reason"].as_str().unwrap_or("emergency stop");
        let n = state.manager.stop_all(org::principal(by), reason).await;
        tracing::warn!(stopped = n, by, "emergency stop from another node");
    }
    state.reconcile.notify_one();
}

async fn reconciler(state: AppState, cancel: CancellationToken) {
    let every = Duration::from_millis(state.config.cluster.reconcile_millis);
    loop {
        tokio::select! {
            () = tokio::time::sleep(every) => {}
            () = state.reconcile.notified() => {}
            () = cancel.cancelled() => return,
        }
        if let Err(err) = reconcile(&state).await {
            tracing::warn!(error = %err, "reconcile pass failed");
        }
    }
}

/// One pass: make the sessions on this node match the desired state.
pub async fn reconcile(state: &AppState) -> anyhow::Result<()> {
    let store = &state.store;
    let mine = store.agents_on_node(&state.node).await?;
    if mine.is_empty() {
        return Ok(());
    }
    let org = Org::load(store).await?;
    let mut changed = false;
    let mut waiting: Vec<(Uuid, Arc<Session>)> = Vec::new();
    let mut models = None;

    // Agents whose session ended for lack of work, and that may be woken.
    let mut sleepers: Vec<&OrgAgent> = Vec::new();

    for agent in &mine {
        let Some(dept) = org.department(agent.department_id) else {
            continue;
        };
        let session = agent.session_id.and_then(|id| state.manager.get(id).ok());
        if agent.status.as_deref() == Some("asleep")
            && !session.as_ref().is_some_and(|s| !s.status().is_terminal())
        {
            // Asleep: nothing runs (also after a node restart).
            match agent.desired {
                Desired::Stopped => {
                    store
                        .agent_ended(agent.id, agent.session_id, "stopped", None)
                        .await?;
                    changed = true;
                }
                Desired::Running if dept.state == DepartmentState::Active => {
                    sleepers.push(agent);
                }
                _ => {}
            }
            continue;
        }
        match session {
            Some(session) if !session.status().is_terminal() => {
                let id = session.id();
                if agent.desired == Desired::Stopped {
                    let reason = agent
                        .note
                        .clone()
                        .unwrap_or_else(|| "stopped in the organisation".into());
                    session
                        .stop(org::principal(&agent.changed_by), reason)
                        .await;
                    continue;
                }
                let dept_paused = dept.state == DepartmentState::Paused;
                let pause = agent.desired == Desired::Paused || dept_paused;
                let by = if agent.desired == Desired::Paused {
                    org::principal(&agent.changed_by)
                } else {
                    org::principal(&dept.updated_by)
                };
                if pause && !session.is_paused() {
                    if let Err(err) = session.pause(by).await {
                        tracing::warn!(agent = %agent.name, error = %err, "pause failed");
                    }
                } else if !pause
                    && session.is_paused()
                    && let Err(err) = session.resume(by).await
                {
                    tracing::warn!(agent = %agent.name, error = %err, "resume failed");
                }
                changed |= store
                    .agent_status(agent.id, id, &status_str(session.status()))
                    .await?;
                if session.status() == SessionStatus::AwaitingInput && !session.is_paused() {
                    waiting.push((agent.id, session));
                }
            }
            Some(session) => {
                // The session is over. A long-running agent carries on: in a
                // fresh session when time ran out, asleep until the next
                // message when there was nothing to do. Otherwise it is
                // stopped now.
                let status = status_str(session.status());
                let wanted = agent.desired == Desired::Running;
                match session.end_cause() {
                    Some(EndCause::TimeBudget) if wanted => {
                        if dept.state == DepartmentState::Active {
                            let models = endpoints(store, &mut models).await?;
                            start_agent(
                                state,
                                &org,
                                agent,
                                models,
                                Some(
                                    "Your previous session reached its time limit, so it was \
                                     ended.",
                                ),
                            )
                            .await?;
                        } else {
                            // Paused department: sleep; resumed work wakes it.
                            store.agent_asleep(agent.id, session.id()).await?;
                        }
                        changed = true;
                    }
                    Some(EndCause::Idle | EndCause::Finished) if wanted => {
                        changed |= store.agent_asleep(agent.id, session.id()).await?;
                    }
                    _ => {
                        if agent.desired != Desired::Stopped
                            || agent.status.as_deref() != Some(&status)
                        {
                            store
                                .agent_ended(agent.id, Some(session.id()), &status, None)
                                .await?;
                            changed = true;
                        }
                    }
                }
            }
            None => match agent.session_id {
                // Its session is not in this process: the node restarted.
                Some(id) => {
                    if agent.desired != Desired::Stopped
                        || !matches!(
                            agent.status.as_deref(),
                            Some("stopped" | "completed" | "failed")
                        )
                    {
                        store
                            .agent_ended(
                                agent.id,
                                Some(id),
                                "failed",
                                Some("interrupted: its node restarted"),
                            )
                            .await?;
                        changed = true;
                    }
                }
                None if agent.desired == Desired::Running
                    && dept.state == DepartmentState::Active =>
                {
                    // Started by a person: the loop guard starts over.
                    forget_starts(agent.id);
                    let models = endpoints(store, &mut models).await?;
                    start_agent(state, &org, agent, models, None).await?;
                    changed = true;
                }
                None => {}
            },
        }
    }

    // Wake sleeping agents that have mail.
    if !sleepers.is_empty() {
        let ids: Vec<Uuid> = sleepers.iter().map(|a| a.id).collect();
        let with_mail = store.agents_with_mail(&ids).await?;
        for agent in sleepers.into_iter().filter(|a| with_mail.contains(&a.id)) {
            let models = endpoints(store, &mut models).await?;
            start_agent(state, &org, agent, models, Some("You were asleep: there was nothing to do, so your previous session ended. New messages woke you.")).await?;
            changed = true;
        }
    }

    // Wake agents that wait for input and have mail.
    if !waiting.is_empty() {
        let ids: Vec<Uuid> = waiting.iter().map(|(id, _)| *id).collect();
        let with_mail = store.agents_with_mail(&ids).await?;
        for (agent, session) in waiting.iter().filter(|(id, _)| with_mail.contains(id)) {
            let pending = store.pending_messages(*agent).await?;
            let delivered: Vec<_> = pending.iter().map(org::delivered).collect();
            match session.deliver_messages(delivered) {
                Ok(()) => {
                    let ids: Vec<Uuid> = pending.iter().map(|m| m.id).collect();
                    store.mark_delivered(*agent, &ids).await?;
                    changed = true;
                }
                // Busy again (a person sent a message first): next pass.
                Err(err) => tracing::debug!(error = %err, "delivery postponed"),
            }
        }
    }

    if changed {
        org::notify(store, json!({ "kind": "agents", "node": &*state.node })).await;
    }
    Ok(())
}

/// Enabled model endpoints, loaded once per pass.
async fn endpoints(
    store: &agentcore_store::Store,
    cache: &mut Option<Vec<agentcore_core::ModelEndpoint>>,
) -> anyhow::Result<Vec<agentcore_core::ModelEndpoint>> {
    if cache.is_none() {
        *cache = Some(store.enabled_endpoints().await?);
    }
    Ok(cache.clone().unwrap_or_default())
}

/// At most this many automatic starts of one agent per hour: an agent that
/// keeps ending at once (a broken agent, two agents waking each other) is
/// stopped instead of burning money.
const MAX_STARTS_PER_HOUR: usize = 6;

static STARTS: LazyLock<Mutex<HashMap<Uuid, VecDeque<Instant>>>> = LazyLock::new(Default::default);

/// Record a start; false when the agent started too often recently.
fn may_start(agent: Uuid) -> bool {
    let now = Instant::now();
    let hour = Duration::from_secs(3600);
    let mut starts = STARTS.lock().unwrap_or_else(|e| e.into_inner());
    starts.retain(|_, s| s.back().is_some_and(|t| now.duration_since(*t) < hour));
    let recent = starts.entry(agent).or_default();
    while recent
        .front()
        .is_some_and(|t| now.duration_since(*t) >= hour)
    {
        recent.pop_front();
    }
    if recent.len() >= MAX_STARTS_PER_HOUR {
        return false;
    }
    recent.push_back(now);
    true
}

fn forget_starts(agent: Uuid) {
    STARTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&agent);
}

async fn start_agent(
    state: &AppState,
    org: &Org,
    agent: &OrgAgent,
    models: Vec<agentcore_core::ModelEndpoint>,
    continuing: Option<&str>,
) -> anyhow::Result<()> {
    let Some(dept) = org.department(agent.department_id) else {
        return Ok(());
    };
    if !may_start(agent.id) {
        tracing::warn!(agent = %agent.name, "agent started too often; stopping it");
        state
            .store
            .agent_ended(
                agent.id,
                agent.session_id,
                "stopped",
                Some(&format!(
                    "started {MAX_STARTS_PER_HOUR} times within an hour; stopped to stop a loop \
                     (start it again when ready)"
                )),
            )
            .await?;
        return Ok(());
    }
    match org::start_agent_session(state, org, agent, dept, models, continuing).await {
        Ok(session) => {
            tracing::info!(
                agent = %agent.name,
                department = %dept.name,
                kind = agent.kind.as_str(),
                session = %session.id(),
                "department agent started"
            );
            state
                .store
                .agent_session_started(agent.id, session.id(), &status_str(session.status()))
                .await?;
            state.persist(&session).await;
            state.follow(session);
        }
        Err(err) => {
            tracing::warn!(agent = %agent.name, error = ?err, "could not start agent");
            state
                .store
                .agent_ended(
                    agent.id,
                    agent.session_id,
                    "failed",
                    Some(&format!("could not start: {}", err.message())),
                )
                .await?;
        }
    }
    Ok(())
}

// ---- forwarding session requests to their node -----------------------------------

/// Session id in `/sessions/{id}/...` (with or without the `/api/v1` prefix).
fn session_in_path(path: &str) -> Option<Uuid> {
    let rest = path.strip_prefix("/api/v1").unwrap_or(path);
    rest.strip_prefix("/sessions/")?
        .split('/')
        .next()?
        .parse()
        .ok()
}

/// Middleware: requests for a session owned by another node are forwarded
/// to that node, streams included.
pub async fn forward_sessions(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(id) = session_in_path(request.uri().path()) else {
        return next.run(request).await;
    };
    if request.headers().contains_key(&FORWARDED) || state.manager.get(id).is_ok() {
        return next.run(request).await;
    }
    // Authenticate here, so nothing is forwarded for unknown callers; the
    // owner checks the token again.
    let (mut parts, body) = request.into_parts();
    if let Err(err) = Caller::from_request_parts(&mut parts, &state).await {
        return err.into_response();
    }
    let request = Request::from_parts(parts, body);
    let owner = match state
        .store
        .session_node(id, state.config.cluster.node_timeout_secs)
        .await
    {
        Ok(Some(owner)) if owner.0 != *state.node => owner,
        Ok(_) => return next.run(request).await,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let (node, url, alive) = owner;
    if !alive {
        return ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("node {node}, which runs this session, is unreachable"),
        )
        .into_response();
    }
    forward(&state, &node, &url, request)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

async fn forward(
    state: &AppState,
    node: &str,
    base: &str,
    request: Request,
) -> Result<Response, ApiError> {
    let original = request
        .extensions()
        .get::<OriginalUri>()
        .map(|o| o.0.clone())
        .unwrap_or_else(|| request.uri().clone());
    let path = original
        .path_and_query()
        .map_or_else(|| original.path().to_string(), |p| p.as_str().to_string());
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, MAX_FORWARD_BODY)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
    let mut upstream = state
        .cluster_http
        .request(
            parts.method,
            format!("{}{path}", base.trim_end_matches('/')),
        )
        .header(&FORWARDED, &*state.node)
        .body(body);
    for name in [
        header::AUTHORIZATION,
        header::CONTENT_TYPE,
        header::ACCEPT,
        HeaderName::from_static("last-event-id"),
    ] {
        if let Some(value) = parts.headers.get(&name) {
            upstream = upstream.header(name, value);
        }
    }
    let response = upstream.send().await.map_err(|e| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("node {node} did not answer: {e}"),
        )
    })?;
    let mut out = Response::builder().status(response.status());
    for name in [
        header::CONTENT_TYPE,
        header::CONTENT_DISPOSITION,
        header::CACHE_CONTROL,
    ] {
        if let Some(value) = response.headers().get(&name) {
            out = out.header(name, value);
        }
    }
    let stream = response.bytes_stream().map_err(std::io::Error::other);
    out.body(Body::from_stream(stream))
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_session_ids_in_paths() {
        let id = Uuid::now_v7();
        assert_eq!(session_in_path(&format!("/sessions/{id}")), Some(id));
        assert_eq!(
            session_in_path(&format!("/api/v1/sessions/{id}/live")),
            Some(id)
        );
        assert_eq!(session_in_path("/sessions"), None);
        assert_eq!(session_in_path("/sessions/not-a-uuid/stop"), None);
        assert_eq!(session_in_path("/org/departments"), None);
    }
}
