use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    Action, ActionOutcome, ModelCallOutcome, Principal, SessionId, SessionStatus, Verdict,
};

/// A single, immutable fact about a session. Events are numbered per session
/// (`seq`) so consumers can detect gaps and resume streams.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub session_id: SessionId,
    pub seq: u64,
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventKind {
    SessionCreated {
        agent: String,
        task: String,
        policy: String,
        policy_digest: String,
        created_by: Principal,
    },
    SandboxStarted {
        backend: String,
        details: serde_json::Value,
    },
    AgentStarted {
        program: String,
        args: Vec<String>,
    },
    /// A line of output produced by the agent process.
    Output {
        stream: OutputStream,
        line: String,
    },
    ActionRequested {
        action_id: Uuid,
        action: Action,
        requested_by: Principal,
    },
    PolicyEvaluated {
        action_id: Uuid,
        verdict: Verdict,
    },
    ApprovalRequested {
        approval_id: Uuid,
        action_id: Uuid,
        action: Action,
        reason: String,
    },
    ApprovalResolved {
        approval_id: Uuid,
        action_id: Uuid,
        approved: bool,
        by: Principal,
        comment: Option<String>,
    },
    ActionCompleted {
        action_id: Uuid,
        outcome: ActionOutcome,
    },
    /// A call to an LLM through the model gateway. Full request and response
    /// bodies are kept in the database; the audit chain holds their hashes.
    ModelCall {
        call_id: Uuid,
        provider: String,
        model: Option<String>,
        path: String,
        http_status: Option<u16>,
        outcome: ModelCallOutcome,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        duration_ms: u64,
        request_sha256: String,
        response_sha256: Option<String>,
        detail: Option<String>,
    },
    StatusChanged {
        status: SessionStatus,
    },
    StopRequested {
        by: Principal,
        reason: String,
    },
    SessionEnded {
        status: SessionStatus,
        exit_code: Option<i32>,
        reason: Option<String>,
    },
}

impl EventKind {
    /// Stable machine name of the event type.
    pub fn name(&self) -> &'static str {
        match self {
            Self::SessionCreated { .. } => "session_created",
            Self::SandboxStarted { .. } => "sandbox_started",
            Self::AgentStarted { .. } => "agent_started",
            Self::Output { .. } => "output",
            Self::ActionRequested { .. } => "action_requested",
            Self::PolicyEvaluated { .. } => "policy_evaluated",
            Self::ApprovalRequested { .. } => "approval_requested",
            Self::ApprovalResolved { .. } => "approval_resolved",
            Self::ActionCompleted { .. } => "action_completed",
            Self::ModelCall { .. } => "model_call",
            Self::StatusChanged { .. } => "status_changed",
            Self::StopRequested { .. } => "stop_requested",
            Self::SessionEnded { .. } => "session_ended",
        }
    }
}
