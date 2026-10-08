use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type SessionId = Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Pending,
    Running,
    AwaitingApproval,
    /// The agent finished a turn and waits for a message from a human.
    AwaitingInput,
    /// Frozen by a human: no process in the sandbox runs, no call goes out.
    Paused,
    Stopped,
    Completed,
    Failed,
}

impl SessionStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped | Self::Completed | Self::Failed)
    }
}

/// Public, serialisable snapshot of a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub agent: String,
    pub task: String,
    pub policy: String,
    pub status: SessionStatus,
    pub created_by: crate::Principal,
    pub created_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub pending_approvals: usize,
    pub actions: u64,
    #[serde(default)]
    pub model_calls: u64,
    #[serde(default)]
    pub context: crate::SessionContext,
}
