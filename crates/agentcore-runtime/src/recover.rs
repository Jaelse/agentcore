//! Sessions outlive their node.
//!
//! Everything a session keeps lives in one directory on the data volume,
//! which every node mounts: the audit log, the terminal recording, the
//! workspace and the agent's `HOME` (its conversation). When a node goes
//! away, another one takes the session over: it reads the audit log back
//! ([`Recovered::read`]), continues the same log, starts a new sandbox on the
//! same workspace and home, and carries the conversation on.

use std::path::{Path, PathBuf};

use agentcore_core::{Event, EventKind, Principal, PullRequestProposal, SessionId, SessionStatus};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::RuntimeError;

const AUDIT_FILE: &str = "audit.jsonl";

/// Where a session keeps its data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPaths {
    pub audit: PathBuf,
    pub recording: PathBuf,
    pub workspace: PathBuf,
    /// The agent's `HOME`; `None` for sessions from before session
    /// directories (their home was thrown away with the sandbox).
    pub home: Option<PathBuf>,
    /// Bare mirror of the repository a session works on (outside the
    /// sandbox; pushes and deliveries go through it).
    pub mirror: PathBuf,
}

impl SessionPaths {
    /// The directory of a new session: inside its department's directory
    /// when it belongs to one (`departments/<id>/sessions/<session>`),
    /// otherwise `sessions/<session>`.
    pub fn new(data_dir: &Path, id: SessionId, department: Option<Uuid>) -> Self {
        let dir = match department {
            Some(department) => data_dir
                .join("departments")
                .join(department.to_string())
                .join("sessions"),
            None => data_dir.join("sessions"),
        }
        .join(id.to_string());
        Self::in_dir(&dir)
    }

    /// The paths of an existing session, found from its audit log.
    pub fn of(data_dir: &Path, id: SessionId, audit: &Path) -> Self {
        match audit.parent() {
            Some(dir) if audit.file_name().is_some_and(|n| n == AUDIT_FILE) => Self::in_dir(dir),
            // Before session directories: one directory per kind of data.
            _ => Self {
                audit: audit.to_path_buf(),
                recording: data_dir.join("recordings").join(format!("{id}.cast")),
                workspace: data_dir.join("workspaces").join(id.to_string()),
                home: None,
                mirror: data_dir.join("repos").join(format!("{id}.git")),
            },
        }
    }

    /// Keep the workspace, home and mirror in a directory that outlives the
    /// session (a department agent's); the audit log and recording stay
    /// with the session.
    pub fn kept_in(self, dir: &Path) -> Self {
        Self {
            workspace: dir.join("workspace"),
            home: Some(dir.join("home")),
            mirror: dir.join("repository.git"),
            ..self
        }
    }

    fn in_dir(dir: &Path) -> Self {
        Self {
            audit: dir.join(AUDIT_FILE),
            recording: dir.join("recording.cast"),
            workspace: dir.join("workspace"),
            home: Some(dir.join("home")),
            mirror: dir.join("repository.git"),
        }
    }
}

/// What a session that is taken over picks up from its audit log.
#[derive(Debug, Clone)]
pub(crate) struct Recovered {
    pub created_by: Principal,
    pub created_at: DateTime<Utc>,
    pub task: String,
    pub policy: String,
    /// Events in the log (the next one gets this sequence number).
    pub records: u64,
    pub history: Vec<Event>,
    /// Status when the last node lost it.
    pub status: SessionStatus,
    pub paused: bool,
    /// Last turn that was started (0: the agent never ran).
    pub turn: u32,
    /// That turn ended: the agent was waiting for input.
    pub turn_ended: bool,
    /// The checkout was prepared (its base commit).
    pub base_commit: Option<String>,
    pub proposal: Option<PullRequestProposal>,
    pub delivery: Option<(String, Option<String>)>,
    pub actions: u64,
    pub model_calls: u64,
}

impl Recovered {
    /// Read and verify the audit log. A log that already records the end of
    /// the session yields [`RuntimeError::Ended`]: there is nothing to carry
    /// on.
    pub fn read(path: &Path) -> Result<Self, RuntimeError> {
        let records = agentcore_audit::read_events(path)?;
        let mut created = false;
        let mut out = Self {
            created_by: Principal::System,
            created_at: Utc::now(),
            task: String::new(),
            policy: String::new(),
            records: records.len() as u64,
            history: Vec::with_capacity(records.len()),
            status: SessionStatus::Pending,
            paused: false,
            turn: 0,
            turn_ended: false,
            base_commit: None,
            proposal: None,
            delivery: None,
            actions: 0,
            model_calls: 0,
        };
        for record in records {
            let event = record.event;
            match &event.kind {
                EventKind::SessionCreated {
                    task,
                    policy,
                    created_by,
                    ..
                } => {
                    out.created_by = created_by.clone();
                    out.created_at = event.timestamp;
                    out.task = task.clone();
                    out.policy = policy.clone();
                    created = true;
                }
                EventKind::StatusChanged { status } => out.status = *status,
                EventKind::SessionEnded { status, .. } => {
                    return Err(RuntimeError::Ended(*status));
                }
                EventKind::Paused { .. } => out.paused = true,
                EventKind::Resumed { .. } => out.paused = false,
                EventKind::TurnStarted { turn } => (out.turn, out.turn_ended) = (*turn, false),
                EventKind::TurnEnded { turn, .. } => out.turn_ended |= *turn == out.turn,
                EventKind::WorkspacePrepared { base_commit, .. } => {
                    out.base_commit = Some(base_commit.clone());
                }
                EventKind::PullRequestProposed { proposal } => {
                    out.proposal = Some(proposal.clone());
                }
                EventKind::Delivered {
                    branch,
                    pull_request_url,
                    ..
                } => {
                    let url = pull_request_url
                        .clone()
                        .or_else(|| out.delivery.as_ref().and_then(|d| d.1.clone()));
                    out.delivery = Some((branch.clone(), url));
                }
                EventKind::ActionRequested { .. } => out.actions += 1,
                EventKind::ModelCall { .. } => out.model_calls += 1,
                _ => {}
            }
            out.history.push(event);
        }
        if !created {
            return Err(RuntimeError::Workspace(format!(
                "{} does not record how the session was created",
                path.display()
            )));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_live_in_their_department() {
        let id = Uuid::now_v7();
        let dept = Uuid::now_v7();
        let paths = SessionPaths::new(Path::new("/data"), id, Some(dept));
        let dir = PathBuf::from(format!("/data/departments/{dept}/sessions/{id}"));
        assert_eq!(paths.audit, dir.join("audit.jsonl"));
        assert_eq!(paths.workspace, dir.join("workspace"));
        assert_eq!(paths.home, Some(dir.join("home")));
        assert_eq!(paths.recording, dir.join("recording.cast"));
        // Found again from the audit log alone, on any node.
        assert_eq!(
            SessionPaths::of(Path::new("/other"), id, &paths.audit),
            paths
        );

        let plain = SessionPaths::new(Path::new("/data"), id, None);
        assert_eq!(
            plain.audit,
            PathBuf::from(format!("/data/sessions/{id}/audit.jsonl"))
        );
    }

    #[test]
    fn old_sessions_keep_their_layout() {
        let id = Uuid::now_v7();
        let audit = PathBuf::from(format!("/data/audit/{id}.jsonl"));
        let paths = SessionPaths::of(Path::new("/data"), id, &audit);
        assert_eq!(paths.audit, audit);
        assert_eq!(
            paths.workspace,
            PathBuf::from(format!("/data/workspaces/{id}"))
        );
        assert_eq!(
            paths.recording,
            PathBuf::from(format!("/data/recordings/{id}.cast"))
        );
        assert_eq!(paths.mirror, PathBuf::from(format!("/data/repos/{id}.git")));
        assert_eq!(paths.home, None);
    }
}
