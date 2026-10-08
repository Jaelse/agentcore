//! The agentcore runtime.
//!
//! A [`SessionManager`] owns every running [`Session`]. A session couples one
//! agent, one task, one sandbox, one policy and one audit log. Everything that
//! happens is emitted as an [`agentcore_core::Event`], which is
//!
//! 1. appended to the hash-chained audit log (fail-closed: if the event cannot
//!    be recorded, the session is stopped),
//! 2. emitted as a `tracing` event for operational observability, and
//! 3. broadcast to live subscribers such as the web UI.

pub mod adapters;
mod approvals;
pub mod live;
mod manager;
mod session;
pub mod work;

pub use adapters::AdapterRegistry;
pub use approvals::{ApprovalDecision, PendingApproval};
pub use manager::{CreateSession, RuntimeConfig, SessionManager};
pub use session::{EndCause, IDLE_REASON, Session, TIME_BUDGET_REASON};
pub use work::{PreparedWorkspace, SessionOptions, ToolHandler, WorkspaceSetup};

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("unknown agent `{0}`")]
    UnknownAgent(String),
    #[error("unknown policy `{0}`")]
    UnknownPolicy(String),
    #[error("unknown adapter `{0}`")]
    UnknownAdapter(String),
    #[error("session {0} not found")]
    SessionNotFound(uuid::Uuid),
    #[error("approval {0} not found or already resolved")]
    ApprovalNotFound(uuid::Uuid),
    #[error("session is not running")]
    NotRunning,
    #[error("the agent is not waiting for input")]
    NotAwaitingInput,
    #[error("workspace: {0}")]
    Workspace(String),
    #[error(transparent)]
    Audit(#[from] agentcore_audit::AuditError),
    #[error(transparent)]
    Adapter(#[from] agentcore_core::agent::AdapterError),
    #[error(transparent)]
    Sandbox(#[from] agentcore_sandbox::SandboxError),
}
