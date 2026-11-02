//! Bridges live sessions (in memory) and the session index in PostgreSQL.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agentcore_audit::AuditLog;
use agentcore_core::{Event, EventKind, SessionId, SessionInfo, SessionStatus};
use agentcore_runtime::{CreateSession, Resume, Session, SessionPaths};
use agentcore_store::SessionRecord;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;

use crate::AppState;
use crate::error::ApiError;

/// A session that is either running in this process or archived.
pub enum SessionRef {
    Live(Arc<Session>),
    Archived(Box<SessionRecord>),
}

impl SessionRef {
    pub fn info(&self) -> SessionInfo {
        match self {
            Self::Live(s) => s.info(),
            Self::Archived(r) => r.info.clone(),
        }
    }

    pub fn policy_digest(&self) -> String {
        match self {
            Self::Live(s) => s.policy_digest().to_string(),
            Self::Archived(r) => r.policy_digest.clone(),
        }
    }

    pub fn audit_path(&self) -> PathBuf {
        match self {
            Self::Live(s) => s.audit_path().to_path_buf(),
            Self::Archived(r) => r.audit_path.clone(),
        }
    }

    /// Where the session keeps its data on the data volume.
    pub fn paths(&self, data_dir: &std::path::Path) -> SessionPaths {
        match self {
            Self::Live(s) => s.paths().clone(),
            Self::Archived(r) => SessionPaths::of(data_dir, r.info.id, &r.audit_path),
        }
    }

    pub fn live(self) -> Result<Arc<Session>, ApiError> {
        match self {
            Self::Live(s) => Ok(s),
            Self::Archived(_) => Err(ApiError::new(
                StatusCode::CONFLICT,
                "this session is no longer running",
            )),
        }
    }

    /// All events of the session: from memory when live, otherwise read
    /// (and verified) from the audit file.
    pub async fn events(&self) -> Result<Vec<Event>, ApiError> {
        match self {
            Self::Live(s) => Ok(s.subscribe().0),
            Self::Archived(r) => {
                let path = r.audit_path.clone();
                let records =
                    tokio::task::spawn_blocking(move || agentcore_audit::read_events(&path))
                        .await
                        .map_err(|e| {
                            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
                        })??;
                Ok(records.into_iter().map(|r| r.event).collect())
            }
        }
    }
}

pub fn record(session: &Session, node: &str) -> SessionRecord {
    SessionRecord {
        info: session.info(),
        policy_digest: session.policy_digest().to_string(),
        audit_path: session.audit_path().to_path_buf(),
        node: Some(node.to_string()),
        resume_spec: None,
    }
}

/// How another node builds a session started by a person again, to carry
/// it on (department agents are built from their department instead).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResumeSpec {
    /// `POST /sessions`.
    Plain { request: CreateSession },
    /// `POST /projects/{id}/sessions`.
    Project {
        project_id: Uuid,
        request: crate::teamwork::StartRequest,
    },
}

impl AppState {
    pub async fn lookup(&self, id: SessionId) -> Result<SessionRef, ApiError> {
        if let Ok(session) = self.manager.get(id) {
            return Ok(SessionRef::Live(session));
        }
        match self.store.get_session(id).await? {
            Some(record) => Ok(SessionRef::Archived(Box::new(record))),
            None => Err(ApiError::new(
                StatusCode::NOT_FOUND,
                format!("session {id} not found"),
            )),
        }
    }

    pub async fn persist(&self, session: &Session) {
        self.persist_record(session, None).await;
    }

    /// Persist a new session with what another node needs to carry it on.
    pub async fn persist_new(&self, session: &Session, spec: ResumeSpec) {
        self.persist_record(session, serde_json::to_value(spec).ok())
            .await;
    }

    async fn persist_record(&self, session: &Session, resume_spec: Option<serde_json::Value>) {
        if session.is_fenced() {
            // Another node owns the session now.
            return;
        }
        let record = SessionRecord {
            resume_spec,
            ..record(session, &self.node)
        };
        if let Err(err) = self.store.upsert_session(&record).await {
            tracing::error!(session = %session.id(), error = %err, "failed to persist session");
        }
    }

    /// Keep the database row of a live session up to date until it ends.
    pub fn follow(&self, session: Arc<Session>) {
        let state = self.clone();
        tokio::spawn(async move {
            let (_, mut rx) = session.subscribe();
            state.persist(&session).await;
            // Department agents: what the metrics need beyond the index.
            let context = session.context();
            let org_member = context.department_id.map(|d| (d, context.org_agent_id));
            let mut approvals = crate::insights::ApprovalClock::default();
            // A pull request the agent proposed, for agentcore to deliver
            // once its turn has ended (roles with `delivery.auto`).
            let auto_delivers = session.role().is_some_and(|r| r.delivery.auto);
            let proposed = Arc::new(AtomicBool::new(false));
            loop {
                match rx.recv().await {
                    Ok(event) => match event.kind {
                        EventKind::SessionEnded { .. } => break,
                        EventKind::PolicyEvaluated {
                            verdict: agentcore_core::Verdict::Deny { rule, .. },
                            ..
                        } => {
                            if let Some((department, agent)) = org_member {
                                state
                                    .activity(
                                        department,
                                        agent,
                                        session.id(),
                                        "denied",
                                        None,
                                        rule.as_deref(),
                                    )
                                    .await;
                            }
                        }
                        EventKind::ApprovalRequested { approval_id, .. } => {
                            approvals.requested(approval_id);
                        }
                        EventKind::ApprovalResolved {
                            approval_id,
                            approved,
                            ..
                        } => {
                            if let Some((department, agent)) = org_member {
                                let waited = approvals.resolved(approval_id);
                                let outcome = if approved { "approved" } else { "rejected" };
                                state
                                    .activity(
                                        department,
                                        agent,
                                        session.id(),
                                        "approval",
                                        waited,
                                        Some(outcome),
                                    )
                                    .await;
                            }
                        }
                        EventKind::StatusChanged { status } => {
                            state.persist(&session).await;
                            // Department agents: status shown in the
                            // organisation, mail delivered when it waits.
                            state.reconcile.notify_one();
                            // A turn ended: keep the latest change snapshot.
                            if status == agentcore_core::SessionStatus::AwaitingInput {
                                state.persist_changes(&session).await;
                                if proposed.swap(false, Ordering::SeqCst) {
                                    let (state, session, proposed) =
                                        (state.clone(), session.clone(), proposed.clone());
                                    tokio::spawn(async move {
                                        if !crate::teamwork::auto_deliver(&state, &session).await {
                                            proposed.store(true, Ordering::SeqCst);
                                        }
                                    });
                                }
                            }
                        }
                        EventKind::PullRequestProposed { .. } if auto_delivers => {
                            proposed.store(true, Ordering::SeqCst);
                        }
                        EventKind::ActionCompleted { .. }
                        | EventKind::ModelCall { .. }
                        | EventKind::Delivered { .. } => state.persist(&session).await,
                        _ => {}
                    },
                    Err(RecvError::Lagged(_)) => state.persist(&session).await,
                    Err(RecvError::Closed) => break,
                }
            }
            state.persist(&session).await;
            state.persist_changes(&session).await;
            state.reconcile.notify_one();
        });
    }

    async fn activity(
        &self,
        department: uuid::Uuid,
        agent: Option<uuid::Uuid>,
        session: uuid::Uuid,
        kind: &str,
        value: Option<i64>,
        detail: Option<&str>,
    ) {
        let activity = agentcore_store::Activity {
            department: Some(department),
            agent,
            session: Some(session),
            kind,
            value,
            detail,
        };
        if let Err(err) = self.store.record_activity(activity).await {
            tracing::warn!(error = %err, "failed to record activity");
        }
    }

    pub async fn persist_changes(&self, session: &Session) {
        if let Some(changes) = session.last_changes()
            && let Err(err) = self.store.save_changes(session.id(), &changes).await
        {
            tracing::error!(session = %session.id(), error = %err, "failed to store changes");
        }
    }
}

/// At start: remove sandboxes left behind by a previous run of this node.
/// Its sessions are not ended: they are carried on ([`carry_on`] and the
/// reconciler), here or on another node.
pub async fn clean_up_sandboxes(sandbox: &dyn agentcore_sandbox::SandboxProvider) {
    match sandbox.cleanup_orphans().await {
        Ok(0) => {}
        Ok(n) => tracing::warn!(
            removed = n,
            "removed sandboxes left behind by a previous run"
        ),
        Err(err) => tracing::error!(error = %err, "failed to clean up orphaned sandboxes"),
    }
}

/// Why a session is carried on, as recorded in its log.
pub const CARRIED_ON: &str = "the node it ran on stopped; carried on here";

/// Carry on the live sessions assigned to this node that it does not run:
/// moved here from a node that went away, or this node's own from before it
/// restarted. Department agents' sessions are carried on by the reconciler,
/// with their agent. Returns whether anything changed.
pub async fn carry_on(state: &AppState) -> anyhow::Result<bool> {
    let mut changed = false;
    for (record, managed) in state.store.live_sessions_of(&state.node).await? {
        if managed || state.manager.get(record.info.id).is_ok() {
            continue;
        }
        changed = true;
        let id = record.info.id;
        let spec = record
            .resume_spec
            .clone()
            .and_then(|spec| serde_json::from_value::<ResumeSpec>(spec).ok());
        let resume = Resume {
            id,
            audit_path: record.audit_path.clone(),
            reason: CARRIED_ON.into(),
            node: state.node.to_string(),
        };
        let result = match spec {
            Some(ResumeSpec::Plain { request }) => {
                let options = agentcore_runtime::SessionOptions {
                    models: state.store.enabled_endpoints().await?,
                    ..Default::default()
                };
                state
                    .manager
                    .resume(request, resume, options)
                    .map_err(ApiError::from)
            }
            Some(ResumeSpec::Project {
                project_id,
                request,
            }) => match state.project(project_id).await {
                Ok(project) => {
                    let by = record.info.created_by.clone();
                    crate::teamwork::start_session(state, &project, request, by, Some(resume)).await
                }
                Err(err) => Err(err),
            },
            None => Err(ApiError::new(
                StatusCode::CONFLICT,
                "it was started before sessions could move between nodes",
            )),
        };
        match result {
            Ok(session) => {
                tracing::info!(session = %id, "carrying on a session");
                state.persist(&session).await;
                state.follow(session);
            }
            Err(err) => {
                tracing::warn!(session = %id, error = %err.message(), "cannot carry a session on");
                end_lost(state, &record, err.message()).await;
            }
        }
    }
    Ok(changed)
}

/// A live session that cannot be carried on is over: its audit log gets its
/// end (failed, interrupted) unless it already has one, and its row says so.
pub async fn end_lost(state: &AppState, record: &SessionRecord, why: &str) {
    let (path, id) = (record.audit_path.clone(), record.info.id);
    let reason = format!("interrupted: its node stopped, and it could not be carried on ({why})");
    let closed = tokio::task::spawn_blocking(move || close_audit(&path, id, reason)).await;
    let status = match closed {
        Ok(Ok(status)) => status,
        _ => {
            tracing::error!(session = %id, "could not close the audit log of an interrupted session");
            SessionStatus::Failed
        }
    };
    if let Err(err) = state.store.end_lost_session(id, &state.node, status).await {
        tracing::warn!(session = %id, error = %err, "could not record the end of a lost session");
    }
}

/// Append the end of an interrupted session, unless the log has it already.
/// Returns how the session ended.
fn close_audit(
    path: &std::path::Path,
    session_id: SessionId,
    reason: String,
) -> Result<SessionStatus, agentcore_audit::AuditError> {
    let events = agentcore_audit::read_events(path)?;
    if let Some(status) = events.iter().rev().find_map(|r| match r.event.kind {
        EventKind::SessionEnded { status, .. } => Some(status),
        _ => None,
    }) {
        return Ok(status);
    }
    let next_seq = events.last().map_or(0, |r| r.event.seq + 1);
    let log = AuditLog::open(path, true)?;
    log.append(&Event {
        id: Uuid::now_v7(),
        session_id,
        seq: next_seq,
        timestamp: chrono::Utc::now(),
        kind: EventKind::SessionEnded {
            status: SessionStatus::Failed,
            exit_code: None,
            reason: Some(reason),
        },
    })?;
    Ok(SessionStatus::Failed)
}
