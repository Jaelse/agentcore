//! Bridges live sessions (in memory) and the session index in PostgreSQL.

use std::path::PathBuf;
use std::sync::Arc;

use agentcore_audit::AuditLog;
use agentcore_core::{Event, EventKind, SessionId, SessionInfo, SessionStatus};
use agentcore_runtime::Session;
use agentcore_store::{SessionRecord, Store};
use axum::http::StatusCode;
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
    }
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
        if let Err(err) = self
            .store
            .upsert_session(&record(session, &self.node))
            .await
        {
            tracing::error!(session = %session.id(), error = %err, "failed to persist session");
        }
    }

    /// Keep the database row of a live session up to date until it ends.
    pub fn follow(&self, session: Arc<Session>) {
        let state = self.clone();
        tokio::spawn(async move {
            let (_, mut rx) = session.subscribe();
            state.persist(&session).await;
            loop {
                match rx.recv().await {
                    Ok(event) => match event.kind {
                        EventKind::SessionEnded { .. } => break,
                        EventKind::StatusChanged { status } => {
                            state.persist(&session).await;
                            // Department agents: status shown in the
                            // organisation, mail delivered when it waits.
                            state.reconcile.notify_one();
                            // A turn ended: keep the latest change snapshot.
                            if status == agentcore_core::SessionStatus::AwaitingInput {
                                state.persist_changes(&session).await;
                            }
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

    pub async fn persist_changes(&self, session: &Session) {
        if let Some(changes) = session.last_changes()
            && let Err(err) = self.store.save_changes(session.id(), &changes).await
        {
            tracing::error!(session = %session.id(), error = %err, "failed to store changes");
        }
    }
}

/// After a crash or restart: mark this node's sessions that were live as
/// failed, close their audit logs with a `session_ended` event, and remove
/// their sandboxes. Other nodes' sessions are not touched.
pub async fn recover(store: &Store, sandbox: &dyn agentcore_sandbox::SandboxProvider, node: &str) {
    match store.fail_interrupted_sessions(node).await {
        Ok(sessions) => {
            for record in sessions {
                let path = record.audit_path.clone();
                let id = record.info.id;
                let closed = tokio::task::spawn_blocking(move || close_audit(&path, id)).await;
                if !matches!(closed, Ok(Ok(()))) {
                    tracing::error!(session = %id, "could not close the audit log of an interrupted session");
                }
                tracing::warn!(session = %id, "session was interrupted by an agentcore restart");
            }
        }
        Err(err) => tracing::error!(error = %err, "failed to recover interrupted sessions"),
    }
    match sandbox.cleanup_orphans().await {
        Ok(0) => {}
        Ok(n) => tracing::warn!(
            removed = n,
            "removed sandboxes left behind by a previous run"
        ),
        Err(err) => tracing::error!(error = %err, "failed to clean up orphaned sandboxes"),
    }
}

fn close_audit(
    path: &std::path::Path,
    session_id: SessionId,
) -> Result<(), agentcore_audit::AuditError> {
    let next_seq = agentcore_audit::read_events(path)?
        .last()
        .map_or(0, |r| r.event.seq + 1);
    let log = AuditLog::open(path, true)?;
    log.append(&Event {
        id: Uuid::now_v7(),
        session_id,
        seq: next_seq,
        timestamp: chrono::Utc::now(),
        kind: EventKind::SessionEnded {
            status: SessionStatus::Failed,
            exit_code: None,
            reason: Some("interrupted: agentcore stopped while the session was running".into()),
        },
    })?;
    Ok(())
}
