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

/// What a budget counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetUnit {
    /// What calls cost, in the organisation's currency.
    #[default]
    Money,
    /// Input plus output tokens, whatever the model costs.
    Tokens,
}

impl BudgetUnit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Money => "money",
            Self::Tokens => "tokens",
        }
    }
}

impl std::str::FromStr for BudgetUnit {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "money" => Ok(Self::Money),
            "tokens" => Ok(Self::Tokens),
            other => Err(format!("unknown budget unit `{other}`")),
        }
    }
}

/// A limit on model calls for the organisation (no department) or one
/// department, per calendar period (UTC): in money or in tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    pub id: Uuid,
    pub department_id: Option<DepartmentId>,
    pub period: BudgetPeriod,
    pub unit: BudgetUnit,
    /// The limit: millionths of the organisation's currency for a money
    /// budget, tokens for a token budget.
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
    /// What the period's calls cost (micros), whatever the unit.
    pub spent_micros: i64,
    /// Input plus output tokens of the period's calls, whatever the unit.
    pub spent_tokens: i64,
    /// Departments this budget paused and will resume next period.
    pub paused_departments: Vec<DepartmentId>,
    /// Start of the period in which it ran out (until released).
    pub exhausted_period: Option<DateTime<Utc>>,
}

impl BudgetStatus {
    /// What the period used, in the budget's unit.
    pub fn spent(&self) -> i64 {
        match self.budget.unit {
            BudgetUnit::Money => self.spent_micros,
            BudgetUnit::Tokens => self.spent_tokens,
        }
    }

    pub fn used(&self) -> f64 {
        self.spent() as f64 / self.budget.limit_micros.max(1) as f64
    }

    pub fn exhausted(&self) -> bool {
        self.spent() >= self.budget.limit_micros
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

/// One stretch of a department's working week.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoursWindow {
    /// ISO weekdays: 1 = Monday ... 7 = Sunday.
    pub days: Vec<u32>,
    /// `HH:MM`, local time.
    pub start: String,
    /// `HH:MM`, local time. Not after `start` means it ends the next day.
    pub end: String,
}

impl HoursWindow {
    fn times(&self) -> Result<(chrono::NaiveTime, chrono::NaiveTime), String> {
        let parse = |s: &str| {
            chrono::NaiveTime::parse_from_str(s, "%H:%M")
                .map_err(|_| format!("`{s}` is not a time like 09:00"))
        };
        Ok((parse(&self.start)?, parse(&self.end)?))
    }

    pub fn check(&self) -> Result<(), String> {
        if self.days.is_empty() || self.days.iter().any(|d| !(1..=7).contains(d)) {
            return Err("a window needs weekdays from 1 (Monday) to 7 (Sunday)".into());
        }
        let (start, end) = self.times()?;
        if start == end {
            return Err("a window cannot start and end at the same time".into());
        }
        Ok(())
    }

    /// Is this local moment inside the window.
    fn contains(&self, local: chrono::NaiveDateTime) -> bool {
        use chrono::{Datelike, Timelike};
        let Ok((start, end)) = self.times() else {
            return false;
        };
        let day = local.weekday().number_from_monday();
        let yesterday = if day == 1 { 7 } else { day - 1 };
        let t = local.time().with_nanosecond(0).unwrap_or(local.time());
        if start < end {
            self.days.contains(&day) && t >= start && t < end
        } else {
            // Past midnight: the evening of a listed day, or the morning after it.
            (self.days.contains(&day) && t >= start) || (self.days.contains(&yesterday) && t < end)
        }
    }
}

/// When a department works. Outside its windows the department is paused
/// and its model calls are refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DepartmentHours {
    pub department_id: DepartmentId,
    /// Off: the department works at any time.
    pub enabled: bool,
    /// IANA name such as `Europe/Berlin`.
    pub timezone: String,
    pub windows: Vec<HoursWindow>,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

impl DepartmentHours {
    /// May the department work at this local moment.
    pub fn is_open(&self, local: chrono::NaiveDateTime) -> bool {
        !self.enabled || self.windows.iter().any(|w| w.contains(local))
    }
}

/// Where agents can say something to the outside world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    /// SMTP.
    Email,
    /// A Slack incoming webhook.
    Slack,
    /// An HTTP POST with JSON (Zapier, Make, n8n, your own service, ...).
    Webhook,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Slack => "slack",
            Self::Webhook => "webhook",
        }
    }
}

impl std::str::FromStr for ChannelKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "email" => Ok(Self::Email),
            "slack" => Ok(Self::Slack),
            "webhook" => Ok(Self::Webhook),
            other => Err(format!("unknown channel kind `{other}`")),
        }
    }
}

/// An outward channel and who may use it. Secrets are never part of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub id: Uuid,
    pub name: String,
    pub kind: ChannelKind,
    pub description: String,
    pub config: Value,
    pub secret_hint: Option<String>,
    pub departments: Vec<DepartmentId>,
    /// Every message waits for a person (the default).
    pub requires_approval: bool,
    pub max_per_day: u32,
    /// Appended to every message.
    pub disclosure: String,
    pub enabled: bool,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxStatus {
    /// Waiting for a person.
    Pending,
    /// A person asked the agent to change it.
    ChangesRequested,
    /// Approved; being sent.
    Sending,
    Sent,
    Rejected,
    /// Sending failed; `error` says why. It can be approved again.
    Failed,
}

impl OutboxStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::ChangesRequested => "changes_requested",
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
        }
    }
}

impl std::str::FromStr for OutboxStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "changes_requested" => Ok(Self::ChangesRequested),
            "sending" => Ok(Self::Sending),
            "sent" => Ok(Self::Sent),
            "rejected" => Ok(Self::Rejected),
            "failed" => Ok(Self::Failed),
            other => Err(format!("unknown outbox status `{other}`")),
        }
    }
}

/// A message an agent wants to send to the outside world.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboxItem {
    pub id: Uuid,
    pub channel_id: Uuid,
    pub department_id: Option<DepartmentId>,
    pub agent_id: Option<OrgAgentId>,
    pub drafted_by: String,
    pub recipients: Vec<String>,
    pub subject: String,
    pub body: String,
    pub status: OutboxStatus,
    pub revision: u32,
    pub history: Vec<Value>,
    pub feedback: Option<String>,
    pub decided_by: Option<String>,
    pub decided_at: Option<DateTime<Utc>>,
    pub sent_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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

#[cfg(test)]
mod hours_tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
    }

    fn hours(enabled: bool, windows: Vec<HoursWindow>) -> DepartmentHours {
        DepartmentHours {
            department_id: Uuid::nil(),
            enabled,
            timezone: "UTC".into(),
            windows,
            updated_at: Utc::now(),
            updated_by: "t".into(),
        }
    }

    fn window(days: &[u32], start: &str, end: &str) -> HoursWindow {
        HoursWindow {
            days: days.to_vec(),
            start: start.into(),
            end: end.into(),
        }
    }

    #[test]
    fn office_hours() {
        let h = hours(true, vec![window(&[1, 2, 3, 4, 5], "09:00", "17:00")]);
        // 2026-10-26 is a Monday, 2026-10-31 a Saturday.
        assert!(h.is_open(at(2026, 10, 26, 9, 0)));
        assert!(h.is_open(at(2026, 10, 26, 16, 59)));
        assert!(!h.is_open(at(2026, 10, 26, 17, 0)));
        assert!(!h.is_open(at(2026, 10, 26, 8, 59)));
        assert!(!h.is_open(at(2026, 10, 31, 12, 0)));
    }

    #[test]
    fn night_shift_runs_past_midnight() {
        let h = hours(true, vec![window(&[5], "22:00", "06:00")]);
        // Friday 2026-10-30 evening, and Saturday morning.
        assert!(h.is_open(at(2026, 10, 30, 23, 0)));
        assert!(h.is_open(at(2026, 10, 31, 5, 59)));
        assert!(!h.is_open(at(2026, 10, 31, 6, 0)));
        assert!(!h.is_open(at(2026, 10, 31, 23, 0)));
        // Sunday night into Monday needs day 7.
        let sun = hours(true, vec![window(&[7], "22:00", "06:00")]);
        assert!(sun.is_open(at(2026, 11, 2, 1, 0)));
    }

    #[test]
    fn off_means_always_open() {
        assert!(hours(false, vec![]).is_open(at(2026, 10, 31, 3, 0)));
        assert!(!hours(true, vec![]).is_open(at(2026, 10, 26, 10, 0)));
    }

    #[test]
    fn windows_are_checked() {
        assert!(window(&[1], "09:00", "17:00").check().is_ok());
        assert!(window(&[], "09:00", "17:00").check().is_err());
        assert!(window(&[8], "09:00", "17:00").check().is_err());
        assert!(window(&[1], "9am", "17:00").check().is_err());
        assert!(window(&[1], "09:00", "09:00").check().is_err());
    }
}
