//! Business data sources and improvement proposals.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{DepartmentId, OrgAgentId};

/// Where read-only business data comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataSourceKind {
    /// A PostgreSQL database, queried with SQL in a read-only transaction.
    Postgres,
    /// An HTTP API, read with `GET` below its base URL.
    Http,
    /// A table uploaded as CSV.
    Table,
}

impl DataSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Http => "http",
            Self::Table => "table",
        }
    }
}

impl std::str::FromStr for DataSourceKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "postgres" => Ok(Self::Postgres),
            "http" => Ok(Self::Http),
            "table" => Ok(Self::Table),
            other => Err(format!("unknown data source kind `{other}`")),
        }
    }
}

/// A read-only data source and the departments that may read it. Secrets
/// are never part of this type; `secret_hint` shows which one is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataSource {
    pub id: Uuid,
    pub name: String,
    pub kind: DataSourceKind,
    pub description: String,
    /// Non-secret settings: `base_url`, `header` (http); `max_rows` (all).
    pub config: Value,
    pub secret_hint: Option<String>,
    /// Rows of a `table` source.
    pub rows: Option<u64>,
    pub departments: Vec<DepartmentId>,
    pub enabled: bool,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    /// Waiting for a person.
    Open,
    /// A person asked the proposer to change it.
    ChangesRequested,
    Applied,
    Rejected,
    /// Applying it failed part-way; `result` says where.
    Failed,
}

impl ProposalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::ChangesRequested => "changes_requested",
            Self::Applied => "applied",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
        }
    }

    /// Still to be decided (can be changed, applied or rejected).
    pub fn is_pending(self) -> bool {
        matches!(self, Self::Open | Self::ChangesRequested)
    }
}

impl std::str::FromStr for ProposalStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(Self::Open),
            "changes_requested" => Ok(Self::ChangesRequested),
            "applied" => Ok(Self::Applied),
            "rejected" => Ok(Self::Rejected),
            "failed" => Ok(Self::Failed),
            other => Err(format!("unknown proposal status `{other}`")),
        }
    }
}

/// One concrete change a proposal makes when applied. The closed set is
/// what can be enforced: each is checked against the permissions of the
/// person who applies it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProposalAction {
    /// Replace an agent's instructions.
    UpdateInstructions {
        agent: OrgAgentId,
        instructions: String,
    },
    /// Add a worker to a department.
    AddAgent {
        department: DepartmentId,
        name: String,
        /// Configured agent it runs (default: what its colleagues run).
        #[serde(default)]
        agent: Option<String>,
        #[serde(default)]
        instructions: String,
    },
    /// Remove a worker (it is stopped first).
    RemoveAgent { agent: OrgAgentId },
    /// Start, pause, resume or stop an agent.
    ControlAgent { agent: OrgAgentId, control: String },
    /// Change a department's mission, policy or tool groups.
    UpdateDepartment {
        department: DepartmentId,
        #[serde(default)]
        mission: Option<String>,
        #[serde(default)]
        policy: Option<String>,
        #[serde(default)]
        tools: Option<Vec<String>>,
    },
    /// Add a scheduled check-in.
    CreateCheckIn {
        department: DepartmentId,
        #[serde(default)]
        agent: Option<OrgAgentId>,
        name: String,
        message: String,
        every_minutes: u32,
    },
    /// Change a check-in.
    UpdateCheckIn {
        check_in: Uuid,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        every_minutes: Option<u32>,
        #[serde(default)]
        enabled: Option<bool>,
    },
    /// Add a goal.
    CreateGoal {
        title: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        department: Option<DepartmentId>,
    },
    /// Send a message (as the person who applies the proposal).
    SendMessage {
        #[serde(default)]
        department: Option<DepartmentId>,
        #[serde(default)]
        agent: Option<OrgAgentId>,
        text: String,
    },
    /// Set (or replace) a spending budget; `limit` in the organisation's
    /// currency.
    SetBudget {
        #[serde(default)]
        department: Option<DepartmentId>,
        period: BudgetPeriod,
        limit: f64,
        action: BudgetAction,
    },
    /// Change the organisation's limits.
    SetLimits {
        #[serde(default)]
        max_departments: Option<u32>,
        #[serde(default)]
        max_agents_per_department: Option<u32>,
    },
}

impl ProposalAction {
    /// Every kind, for settings and validation.
    pub const KINDS: [&'static str; 11] = [
        "update_instructions",
        "add_agent",
        "remove_agent",
        "control_agent",
        "update_department",
        "create_check_in",
        "update_check_in",
        "create_goal",
        "send_message",
        "set_budget",
        "set_limits",
    ];

    pub fn kind(&self) -> &'static str {
        match self {
            Self::UpdateInstructions { .. } => "update_instructions",
            Self::AddAgent { .. } => "add_agent",
            Self::RemoveAgent { .. } => "remove_agent",
            Self::ControlAgent { .. } => "control_agent",
            Self::UpdateDepartment { .. } => "update_department",
            Self::CreateCheckIn { .. } => "create_check_in",
            Self::UpdateCheckIn { .. } => "update_check_in",
            Self::CreateGoal { .. } => "create_goal",
            Self::SendMessage { .. } => "send_message",
            Self::SetBudget { .. } => "set_budget",
            Self::SetLimits { .. } => "set_limits",
        }
    }

    /// Changes that need an admin (structure, guardrails, limits).
    pub fn needs_admin(&self) -> bool {
        matches!(
            self,
            Self::AddAgent { .. }
                | Self::RemoveAgent { .. }
                | Self::UpdateDepartment { .. }
                | Self::SetBudget { .. }
                | Self::SetLimits { .. }
        )
    }
}

/// What applying one action did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    pub kind: String,
    pub ok: bool,
    pub detail: String,
}

/// An improvement: the problem, the evidence, the solution and the changes
/// that apply it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub id: Uuid,
    pub title: String,
    pub problem: String,
    pub evidence: String,
    pub solution: String,
    pub actions: Vec<ProposalAction>,
    pub status: ProposalStatus,
    pub revision: u32,
    /// Earlier revisions with the feedback that changed them.
    pub history: Vec<Value>,
    pub feedback: Option<String>,
    pub proposed_by: String,
    pub proposer_agent: Option<OrgAgentId>,
    pub decided_by: Option<String>,
    pub decided_at: Option<DateTime<Utc>>,
    pub result: Option<Vec<ActionResult>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Price of a model per million tokens. `model` is an exact name, a prefix
/// ending in `*`, or `*` for every model of the provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    pub provider: String,
    pub model: String,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPeriod {
    Day,
    Week,
    Month,
}

impl BudgetPeriod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }
}

impl std::str::FromStr for BudgetPeriod {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "day" => Ok(Self::Day),
            "week" => Ok(Self::Week),
            "month" => Ok(Self::Month),
            other => Err(format!("unknown budget period `{other}`")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetAction {
    /// Tell people when the warning level and the limit are reached.
    Warn,
    /// Also pause the work and block model calls when the limit is reached.
    Pause,
}

impl BudgetAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Pause => "pause",
        }
    }
}

impl std::str::FromStr for BudgetAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "warn" => Ok(Self::Warn),
            "pause" => Ok(Self::Pause),
            other => Err(format!("unknown budget action `{other}`")),
        }
    }
}

/// A spending limit on model calls for the organisation (no department)
/// or one department, per calendar period (UTC).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    pub id: Uuid,
    pub department_id: Option<DepartmentId>,
    pub period: BudgetPeriod,
    /// In millionths of the organisation's currency.
    pub limit_micros: i64,
    pub action: BudgetAction,
    pub warn_percent: u32,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

/// A budget and how much of it the current period used.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetStatus {
    #[serde(flatten)]
    pub budget: Budget,
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub spent_micros: i64,
    /// Departments this budget paused and will resume next period.
    pub paused_departments: Vec<DepartmentId>,
    /// Start of the period in which it ran out (until released).
    pub exhausted_period: Option<DateTime<Utc>>,
}

impl BudgetStatus {
    pub fn used(&self) -> f64 {
        self.spent_micros as f64 / self.budget.limit_micros.max(1) as f64
    }

    pub fn exhausted(&self) -> bool {
        self.spent_micros >= self.budget.limit_micros
    }

    pub fn warning(&self) -> bool {
        self.used() * 100.0 >= f64::from(self.budget.warn_percent)
    }

    /// Model calls in this budget's scope are refused.
    pub fn blocks(&self) -> bool {
        self.budget.action == BudgetAction::Pause && self.exhausted()
    }

    /// Applies to work of this department (an organisation budget applies
    /// to everything).
    pub fn covers(&self, department: Option<DepartmentId>) -> bool {
        self.budget.department_id.is_none() || self.budget.department_id == department
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_are_tagged_and_strict() {
        let a: ProposalAction = serde_json::from_value(serde_json::json!({
            "kind": "update_check_in", "check_in": Uuid::nil(), "every_minutes": 60,
        }))
        .unwrap();
        assert_eq!(a.kind(), "update_check_in");
        assert!(!a.needs_admin());
        assert!(
            serde_json::from_value::<ProposalAction>(serde_json::json!({
                "kind": "drop_database"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ProposalAction>(serde_json::json!({
                "kind": "set_limits", "max_departments": 3, "sudo": true
            }))
            .is_err(),
            "unknown fields are refused"
        );
    }
}
