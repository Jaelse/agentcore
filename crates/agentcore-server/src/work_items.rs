//! Department work items: saving a worker's committed work to its item's
//! branch on GitHub (checkpoints), and the operator API to see, hand over and
//! close items.
//!
//! The department owns the work; a session borrows it through a lease (see
//! `agentcore_store::work_items`). A checkpoint is pushed only while the
//! session holds the lease, and only over the item's last checkpoint, so a
//! session that lost its item can never overwrite newer work.

use std::sync::{Arc, LazyLock};

use agentcore_core::{SessionStatus, WorkItemState};
use agentcore_runtime::{EndHook, RuntimeError, Session};
use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::AppState;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::org::{Org, Sender, notify, tell_assignee, work_item_json};

type ApiResult<T> = Result<T, ApiError>;

/// One push at a time on this node: a checkpoint and a delivery of the same
/// item must not race each other's lease check.
pub(crate) static PUSHES: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(Default::default);

/// Save the session's committed work to its work item's branch, if it holds
/// one and something changed since the last checkpoint. Returns the new
/// checkpoint. Does nothing while the agent works (its turn is running).
pub async fn checkpoint(state: &AppState, session: &Session) -> ApiResult<Option<String>> {
    if session.context().work_item_id.is_none() {
        return Ok(None);
    }
    let _push = PUSHES.lock().await;
    let Some(item) = state.store.work_item_of_session(session.id()).await? else {
        return Ok(None);
    };
    let Some(lease) = item.lease.as_ref() else {
        return Ok(None);
    };
    let bundle = match session.checkpoint_bundle(item.head_commit.as_deref()).await {
        Ok(Some(bundle)) => bundle,
        Ok(None) | Err(RuntimeError::NotAwaitingInput | RuntimeError::NotRunning) => {
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };
    let (bytes, head) = bundle;
    let project = state.project(item.project_id).await?;
    let (_, config, token) = state.require_github().await?;
    crate::teamwork::push_work(
        &config,
        &token,
        &project,
        &state.mirror_path(session.id()),
        &item.branch,
        &bytes,
        &head,
        Some(item.head_commit.as_deref()),
    )
    .await?;
    if !state
        .store
        .checkpoint_work_item(item.id, lease.generation, &head)
        .await?
    {
        // Pushed, but the lease went away meanwhile: whoever holds the item
        // now starts from the checkpoint they know, and their push will be
        // refused until they look again. Nothing is lost on GitHub.
        tracing::warn!(item = %item.id, session = %session.id(), "checkpoint after the lease was lost");
        return Ok(None);
    }
    tracing::info!(item = %item.id, session = %session.id(), head = %head, "work item checkpoint");
    notify(&state.store, json!({ "kind": "work_items" })).await;
    Ok(Some(head))
}

/// Save the sessions on this node that wait for input. Runs every
/// `[cluster].checkpoint_secs`.
pub async fn checkpoint_node(state: &AppState) -> anyhow::Result<usize> {
    let mut saved = 0;
    for agent in state.store.agents_on_node(&state.node).await? {
        let Some(session) = agent.session_id.and_then(|id| state.manager.get(id).ok()) else {
            continue;
        };
        if session.status() != SessionStatus::AwaitingInput || session.is_paused() {
            continue;
        }
        match checkpoint(state, &session).await {
            Ok(Some(_)) => saved += 1,
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(agent = %agent.name, error = %err.message(), "checkpoint failed")
            }
        }
    }
    Ok(saved)
}

/// Saves a worker's work once more before the system ends its session.
pub struct SaveOnEnd(pub AppState);

#[async_trait]
impl EndHook for SaveOnEnd {
    async fn before_end(&self, session: &Session) {
        if let Err(err) = checkpoint(&self.0, session).await {
            tracing::warn!(session = %session.id(), error = %err.message(), "final checkpoint failed");
        }
    }
}

// ---- operator API -------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub department: Option<Uuid>,
    /// Include closed items.
    #[serde(default)]
    pub closed: bool,
}

pub async fn list(
    State(state): State<AppState>,
    _caller: Caller,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let org = Org::load(&state.store).await?;
    let items = state.store.list_work_items(q.department, q.closed).await?;
    Ok(Json(json!(
        items
            .iter()
            .map(|i| {
                let mut v = work_item_json(&org, i);
                v["department_id"] = json!(i.department_id);
                v
            })
            .collect::<Vec<_>>()
    )))
}

#[derive(Debug, Deserialize)]
pub struct AssignRequest {
    pub agent: Uuid,
}

/// Hand an item to a worker of its department; it carries the item on in
/// its next session (a sleeping worker wakes on the message it gets).
pub async fn assign(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(req): Json<AssignRequest>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let item = state
        .store
        .assign_work_item(id, req.agent, &caller.name)
        .await?;
    tell_assignee(
        &state.store,
        &Sender::Human(caller.name.clone()),
        req.agent,
        &item,
    )
    .await;
    notify(&state.store, json!({ "kind": "work_items" })).await;
    let org = Org::load(&state.store).await?;
    Ok(Json(work_item_json(&org, &item)))
}

#[derive(Debug, Default, Deserialize)]
pub struct CloseRequest {
    #[serde(default)]
    pub note: Option<String>,
}

/// Close an item nobody works on: nobody carries it on any more.
pub async fn close(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    body: Option<Json<CloseRequest>>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let note = body
        .and_then(|Json(b)| b.note)
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| format!("closed by {}", caller.name));
    let item = state
        .store
        .close_work_item(id, None, note.trim(), &caller.name)
        .await?;
    if item.state != WorkItemState::Closed {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the work item was not closed",
        ));
    }
    notify(&state.store, json!({ "kind": "work_items" })).await;
    let org = Org::load(&state.store).await?;
    Ok(Json(work_item_json(&org, &item)))
}

/// The end hook every department worker session on a work item gets.
pub fn end_hook(state: &AppState) -> Arc<dyn EndHook> {
    Arc::new(SaveOnEnd(state.clone()))
}
