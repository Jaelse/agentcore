//! Shared domain types for agentcore.
//!
//! Everything an agent does that has a side effect is modelled as an [`Action`].
//! Every state change of a session is modelled as an [`Event`]. Events are what
//! the audit log records, what the UI streams, and what tracing exports, so the
//! three views of a session can never disagree.

pub mod action;
pub mod agent;
pub mod event;
pub mod insights;
pub mod live;
pub mod model;
pub mod org;
pub mod principal;
pub mod session;
pub mod work;

pub use action::{Action, ActionKind, ActionOutcome, Verdict};
pub use agent::{AgentAdapter, AgentSpec, LaunchContext, LaunchPlan};
pub use event::{Event, EventKind, OutputStream};
pub use insights::{
    ActionResult, Budget, BudgetAction, BudgetPeriod, BudgetStatus, Channel, ChannelKind,
    DataSource, DataSourceKind, ModelPrice, OutboxItem, OutboxStatus, Proposal, ProposalAction,
    ProposalStatus,
};
pub use live::{FileChange, FileChangeKind, LiveFrame, ModelDeltaKind, ProcessInfo};
pub use model::{ModelCallOutcome, ModelEndpoint, ProviderKind};
pub use org::{
    AgentKind, DeliveredMessage, Department, DepartmentId, DepartmentState, Desired, GoalStatus,
    MessageScope, NodeInfo, OrgAgent, OrgAgentId, OrgGoal, OrgMessage, OrgProfile, OrgSchedule,
    OrgSettings,
};
pub use principal::Principal;
pub use session::{SessionId, SessionInfo, SessionStatus};
pub use work::{
    ChangedFile, Changes, CheckResult, Commit, IssueRef, PullRequestProposal, SessionContext,
};

/// Marker placed in the environment of every sandboxed agent and attached to
/// generated artefacts so downstream consumers can tell the content was
/// produced by an AI system (EU AI Act Art. 50 transparency obligations).
pub const AI_GENERATED_MARKER: &str = "agentcore/ai-generated";
