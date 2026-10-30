//! Organisations: departments of agents and who may talk to whom.
//!
//! A department (a "room") groups agents that work together. Agents talk
//! freely inside their department; departments only talk to each other
//! through the organisation's message router. [`route`] decides who may
//! address whom and [`RoutingPolicy`] decides, per department, what the
//! router lets through.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::SessionId;

pub type DepartmentId = Uuid;
pub type OrgAgentId = Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// Does the department's work.
    Worker,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Worker => "worker",
        }
    }
}

impl std::str::FromStr for AgentKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "worker" => Ok(Self::Worker),
            other => Err(format!("unknown agent kind `{other}`")),
        }
    }
}

/// What should be true for an agent; nodes converge their sessions to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Desired {
    Stopped,
    Running,
    Paused,
}

impl Desired {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Running => "running",
            Self::Paused => "paused",
        }
    }
}

impl std::str::FromStr for Desired {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "stopped" => Ok(Self::Stopped),
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            other => Err(format!("unknown desired state `{other}`")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepartmentState {
    Active,
    /// Every agent of the department is paused.
    Paused,
}

impl DepartmentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
        }
    }
}

impl std::str::FromStr for DepartmentState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            other => Err(format!("unknown department state `{other}`")),
        }
    }
}

/// Tool groups a department can grant its workers. Messaging is always on.
pub const TOOL_SANDBOX: &str = "sandbox";
pub const TOOL_FILES: &str = "files";
/// Read the organisation's metrics and structure, and propose improvements
/// (the retrospective).
pub const TOOL_INSIGHTS: &str = "insights";
pub const TOOL_GROUPS: [&str; 3] = [TOOL_SANDBOX, TOOL_FILES, TOOL_INSIGHTS];

/// Limits set by an admin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgSettings {
    pub max_departments: u32,
    /// Worker agents per department.
    pub max_agents_per_department: u32,
}

/// What the organisation is for and how it is meant to grow. Given to every
/// agent (company) and used for suggestions (blueprint).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgProfile {
    #[serde(default)]
    pub company_name: String,
    #[serde(default)]
    pub company_about: String,
    /// Growth path the organisation follows (a blueprint id).
    #[serde(default)]
    pub blueprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Department {
    pub id: DepartmentId,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Given to every agent of the department.
    #[serde(default)]
    pub mission: String,
    /// Guardrail policy for the department's workers.
    pub policy: String,
    /// Granted tool groups ([`TOOL_GROUPS`]).
    pub tools: Vec<String>,
    /// What the message router lets through to and from this department.
    #[serde(default)]
    pub routing: RoutingPolicy,
    pub state: DepartmentState,
    /// Template the department was created from, if any.
    #[serde(default)]
    pub template: Option<String>,
    /// Project (GitHub repository) the department works on.
    #[serde(default)]
    pub project_id: Option<uuid::Uuid>,
    /// Role (playbook) of its workers on the project; default: the
    /// project's role.
    #[serde(default)]
    pub role: Option<String>,
    /// Its workers cannot work without a linked project: they are held until
    /// one is linked.
    #[serde(default)]
    pub requires_repository: bool,
    pub created_at: DateTime<Utc>,
    pub updated_by: String,
}

impl Department {
    /// Why the department's workers are held, if they are.
    pub fn blocked(&self) -> Option<&'static str> {
        (self.requires_repository && self.project_id.is_none())
            .then_some("waiting for a GitHub repository to be linked")
    }

    pub fn grants(&self, group: &str) -> bool {
        self.tools.iter().any(|t| t == group)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgAgent {
    pub id: OrgAgentId,
    pub department_id: DepartmentId,
    pub name: String,
    pub kind: AgentKind,
    /// Configured agent (`[[agents]]`) this member runs, e.g. `opencode`.
    pub agent: String,
    /// Model provider this member uses instead of the agent's own.
    #[serde(default)]
    pub provider: Option<String>,
    /// Model this member uses instead of the agent's own.
    #[serde(default)]
    pub model: Option<String>,
    /// Member-specific instructions, added to the department mission.
    #[serde(default)]
    pub instructions: String,
    pub desired: Desired,
    /// Node the agent is placed on while it runs.
    pub node: Option<String>,
    /// Current (or last) session.
    pub session_id: Option<SessionId>,
    /// Last known session status (`running`, `awaiting_input`, ...).
    pub status: Option<String>,
    /// Why it last stopped, when not by a person.
    pub note: Option<String>,
    /// Who last started, paused or stopped it (`agentcore` for the system).
    pub changed_by: String,
    pub created_at: DateTime<Utc>,
}

/// One end of a message, as far as routing is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Party {
    Human,
    Agent {
        id: OrgAgentId,
        department: DepartmentId,
    },
}

/// Where a message is addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Agent {
        id: OrgAgentId,
        department: DepartmentId,
    },
    /// A department: its room when sent from inside (or by a person), its
    /// workers (as the router's delivery rule picks them) from outside.
    Department(DepartmentId),
    /// Every other department.
    AllDepartments,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageScope {
    /// Inside one department.
    Internal,
    /// Between departments, through the router.
    InterDepartment,
    /// Sent by a person.
    Human,
}

impl MessageScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::InterDepartment => "inter_department",
            Self::Human => "human",
        }
    }
}

impl std::str::FromStr for MessageScope {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "internal" => Ok(Self::Internal),
            "inter_department" => Ok(Self::InterDepartment),
            "human" => Ok(Self::Human),
            other => Err(format!("unknown message scope `{other}`")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    #[error(
        "you cannot contact agents of other departments directly: send your message with \
         team_send_to_department"
    )]
    WorkerOutsideDepartment,
    #[error("you cannot send a message to yourself")]
    ToSelf,
}

/// Decide whether `from` may send to `to`, and with which scope. A message
/// that leaves its department can only be addressed to a department; the
/// router's [`RoutingPolicy`] then decides whether it gets through.
pub fn route(from: Party, to: Target) -> Result<MessageScope, RouteError> {
    let Party::Agent {
        id: from_id,
        department: from_dept,
    } = from
    else {
        return Ok(MessageScope::Human);
    };
    match to {
        Target::Agent { id, .. } if id == from_id => Err(RouteError::ToSelf),
        Target::Agent { department, .. } if department == from_dept => Ok(MessageScope::Internal),
        Target::Department(department) if department == from_dept => Ok(MessageScope::Internal),
        Target::Agent { .. } => Err(RouteError::WorkerOutsideDepartment),
        Target::Department(_) | Target::AllDepartments => Ok(MessageScope::InterDepartment),
    }
}

/// Which departments a department may exchange messages with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRule {
    /// Only these departments; `None` = every department.
    #[serde(default)]
    pub allow: Option<Vec<DepartmentId>>,
    /// Never these, even when allowed above.
    #[serde(default)]
    pub deny: Vec<DepartmentId>,
}

impl PeerRule {
    pub fn permits(&self, peer: DepartmentId) -> bool {
        !self.deny.contains(&peer) && self.allow.as_ref().is_none_or(|a| a.contains(&peer))
    }
}

/// When messages that leave a department wait for a person.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteApproval {
    /// Straight through.
    #[default]
    None,
    /// Only a message to all departments waits.
    Broadcast,
    /// Every message waits.
    All,
}

/// Which workers of the receiving department get a message from outside.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "worker")]
pub enum Delivery {
    /// Every worker (the room).
    #[default]
    All,
    /// One worker (by name), who passes it on; every worker when that name
    /// does not exist (any more).
    Lead(String),
}

/// What the message router lets through to and from one department. All
/// checks are deterministic: no model reads the messages.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingPolicy {
    /// Departments this one may write to.
    #[serde(default)]
    pub outbound: PeerRule,
    /// Departments that may write to this one.
    #[serde(default)]
    pub inbound: PeerRule,
    /// Outgoing messages that wait for a person.
    #[serde(default)]
    pub approval: RouteApproval,
    /// Longest message sent or received, in characters.
    #[serde(default)]
    pub max_message_chars: Option<usize>,
    /// Most messages this department may send to other departments per hour.
    #[serde(default)]
    pub max_per_hour: Option<u32>,
    /// Phrases (case-insensitive) a message may not contain, sent or
    /// received.
    #[serde(default)]
    pub blocked_phrases: Vec<String>,
    /// Which of its workers receive messages from other departments.
    #[serde(default)]
    pub delivery: Delivery,
}

impl RoutingPolicy {
    /// The first rule, if any, that stops `text` from being sent or
    /// received by this department.
    pub fn content_violation(&self, text: &str) -> Option<String> {
        if let Some(max) = self.max_message_chars
            && text.chars().count() > max
        {
            return Some(format!("longer than {max} characters"));
        }
        let lower = text.to_lowercase();
        self.blocked_phrases
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .find(|p| lower.contains(&p.to_lowercase()))
            .map(|p| format!("contains the blocked phrase `{p}`"))
    }
}

/// What the router decided about one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteVerdict {
    /// Delivered to these departments.
    Deliver(Vec<DepartmentId>),
    /// Held for a person; if approved it goes to these departments.
    Hold(Vec<DepartmentId>),
    /// Not delivered, and why.
    Deny(String),
}

/// Apply the sender's and the receivers' policies to one message from
/// `from` to `targets` (departments with their names and policies).
/// `sent_last_hour` is how many messages `from` already sent out in the
/// last hour; `broadcast` whether it is addressed to all departments.
pub fn decide_route(
    from: (DepartmentId, &RoutingPolicy),
    targets: &[(DepartmentId, &str, &RoutingPolicy)],
    text: &str,
    broadcast: bool,
    sent_last_hour: u32,
) -> RouteVerdict {
    let (from_id, out) = from;
    if let Some(max) = out.max_per_hour
        && sent_last_hour >= max
    {
        return RouteVerdict::Deny(format!(
            "your department has already sent {max} messages to other departments in the last \
             hour"
        ));
    }
    if let Some(why) = out.content_violation(text) {
        return RouteVerdict::Deny(format!(
            "your department's routing policy refuses it: {why}"
        ));
    }
    let mut reasons = Vec::new();
    let mut ok = Vec::new();
    for (id, name, inbound) in targets {
        let why = if !out.outbound.permits(*id) {
            Some("your department may not write to it".to_string())
        } else if !inbound.inbound.permits(from_id) {
            Some("it does not accept messages from your department".to_string())
        } else {
            inbound
                .content_violation(text)
                .map(|w| format!("its routing policy refuses it: {w}"))
        };
        match why {
            Some(why) => reasons.push(format!("{name}: {why}")),
            None => ok.push(*id),
        }
    }
    if ok.is_empty() {
        return RouteVerdict::Deny(if reasons.is_empty() {
            "there is no department to send it to".into()
        } else {
            reasons.join("; ")
        });
    }
    let hold = match out.approval {
        RouteApproval::None => false,
        RouteApproval::Broadcast => broadcast,
        RouteApproval::All => true,
    };
    if hold {
        RouteVerdict::Hold(ok)
    } else {
        RouteVerdict::Deliver(ok)
    }
}

/// What became of a message at the router.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    #[default]
    Delivered,
    /// Waiting for a person to approve it.
    Held,
    /// Not delivered: refused by a routing policy or by a person.
    Rejected,
}

impl MessageStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Held => "held",
            Self::Rejected => "rejected",
        }
    }
}

impl std::str::FromStr for MessageStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "delivered" => Ok(Self::Delivered),
            "held" => Ok(Self::Held),
            "rejected" => Ok(Self::Rejected),
            other => Err(format!("unknown message status `{other}`")),
        }
    }
}

/// A message as stored and shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgMessage {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub scope: MessageScope,
    /// `None` for a person.
    pub from_agent: Option<OrgAgentId>,
    pub from_department: Option<DepartmentId>,
    /// Display name: `analyst (Research)` or the person's name.
    pub from_name: String,
    /// `agent`, `department` or `all_departments`.
    pub to_kind: String,
    pub to_agent: Option<OrgAgentId>,
    pub to_department: Option<DepartmentId>,
    /// Display name of the addressee.
    pub to_name: String,
    pub text: String,
    #[serde(default)]
    pub status: MessageStatus,
    /// Why the router held or refused it, or who decided.
    #[serde(default)]
    pub decision: String,
    /// Agents it was delivered to (inbox entries).
    #[serde(default)]
    pub recipients: Vec<OrgAgentId>,
}

/// A message as handed to an agent (recorded in its audit log).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredMessage {
    pub message_id: Uuid,
    pub from: String,
    pub to: String,
    pub scope: MessageScope,
    pub text: String,
    pub sent_at: DateTime<Utc>,
}

impl DeliveredMessage {
    /// How the message is shown to the agent.
    pub fn render(&self) -> String {
        format!(
            "[{}] from {} to {}:\n{}",
            self.sent_at.format("%H:%M:%S"),
            self.from,
            self.to,
            self.text
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Achieved,
    Dropped,
}

impl GoalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Achieved => "achieved",
            Self::Dropped => "dropped",
        }
    }
}

impl std::str::FromStr for GoalStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "achieved" => Ok(Self::Achieved),
            "dropped" => Ok(Self::Dropped),
            other => Err(format!("unknown goal status `{other}`")),
        }
    }
}

/// A goal people set for the organisation (optionally owned by a
/// department). Agents read goals and report progress; people decide when a
/// goal is achieved or dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgGoal {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub department_id: Option<DepartmentId>,
    pub status: GoalStatus,
    /// Latest progress report (by an agent or a person).
    pub progress: String,
    pub progress_by: Option<String>,
    pub progress_at: Option<DateTime<Utc>>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

/// A check-in: a message sent to a department (or one agent) on a schedule,
/// which wakes the agents it reaches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgSchedule {
    pub id: Uuid,
    pub department_id: DepartmentId,
    /// `None`: everyone in the department.
    pub agent_id: Option<OrgAgentId>,
    pub name: String,
    pub message: String,
    pub every_minutes: u32,
    pub next_run_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub enabled: bool,
    pub created_by: String,
}

/// Work a department does on its repository. The department owns it (its
/// branch, last checkpoint and pull request); a session borrows it through a
/// lease, and any worker can carry it on from the last checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: Uuid,
    pub department_id: DepartmentId,
    pub project_id: Uuid,
    pub title: String,
    /// Branch on GitHub that holds the work between sessions.
    pub branch: String,
    pub base_branch: String,
    /// Last checkpoint pushed to `branch` (`None`: nothing pushed yet).
    pub head_commit: Option<String>,
    pub checkpoint_at: Option<DateTime<Utc>>,
    pub state: WorkItemState,
    /// The worker that carries it on.
    pub assignee: Option<OrgAgentId>,
    /// The session working on it now, if any.
    pub lease: Option<WorkLease>,
    pub pull_request_number: Option<u64>,
    pub pull_request_url: Option<String>,
    /// What last happened to it, e.g. "lost with node n2".
    pub note: Option<String>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub closed_by: Option<String>,
}

/// Who holds a work item. Checkpoints carry the generation: a session whose
/// lease was taken away cannot write any more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkLease {
    pub id: Uuid,
    pub session_id: Option<SessionId>,
    pub node: String,
    pub generation: i64,
    pub since: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemState {
    Open,
    Closed,
}

impl WorkItemState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

impl std::str::FromStr for WorkItemState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            other => Err(format!("unknown work item state `{other}`")),
        }
    }
}

/// A node of the cluster (one agentcore process, usually one VM).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    pub name: String,
    pub internal_url: String,
    pub capacity: u32,
    pub version: String,
    pub started_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// Agents placed on the node and not stopped.
    #[serde(default)]
    pub agents: u32,
    #[serde(default)]
    pub alive: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(dept: DepartmentId) -> (Party, Target) {
        let id = Uuid::new_v4();
        (
            Party::Agent {
                id,
                department: dept,
            },
            Target::Agent {
                id,
                department: dept,
            },
        )
    }

    #[test]
    fn agents_talk_freely_inside_and_reach_others_only_as_departments() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let (w1, me) = agent(a);
        let (_, w2) = agent(a);
        let (_, foreign) = agent(b);

        assert_eq!(route(w1, w2), Ok(MessageScope::Internal));
        assert_eq!(route(w1, Target::Department(a)), Ok(MessageScope::Internal));
        assert_eq!(route(w1, me), Err(RouteError::ToSelf));
        assert_eq!(route(w1, foreign), Err(RouteError::WorkerOutsideDepartment));
        for to in [Target::Department(b), Target::AllDepartments] {
            assert_eq!(route(w1, to), Ok(MessageScope::InterDepartment));
        }
    }

    #[test]
    fn people_reach_everyone() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let (_, w) = agent(a);
        let (_, c) = agent(b);
        for to in [w, c, Target::Department(a), Target::AllDepartments] {
            assert_eq!(route(Party::Human, to), Ok(MessageScope::Human));
        }
    }

    fn open() -> RoutingPolicy {
        RoutingPolicy::default()
    }

    #[test]
    fn an_open_policy_delivers() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let p = open();
        assert_eq!(
            decide_route((a, &p), &[(b, "B", &p)], "hello", false, 0),
            RouteVerdict::Deliver(vec![b])
        );
    }

    #[test]
    fn peers_are_allowed_and_denied_by_both_sides() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let mut from = open();
        from.outbound.allow = Some(vec![b]);
        let mut b_policy = open();
        b_policy.inbound.deny = vec![a];
        let c_policy = open();
        // Out: only B; B refuses A; C is not on A's list.
        let v = decide_route(
            (a, &from),
            &[(b, "B", &b_policy), (c, "C", &c_policy)],
            "hi",
            true,
            0,
        );
        let RouteVerdict::Deny(why) = v else {
            panic!("expected a denial, got {v:?}")
        };
        assert!(why.contains("B: it does not accept"), "{why}");
        assert!(why.contains("C: your department may not write"), "{why}");
        // Broadcast delivers to whoever remains.
        from.outbound.allow = None;
        assert_eq!(
            decide_route(
                (a, &from),
                &[(b, "B", &b_policy), (c, "C", &c_policy)],
                "hi",
                true,
                0
            ),
            RouteVerdict::Deliver(vec![c])
        );
    }

    #[test]
    fn approval_holds_what_the_policy_asks_for() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let p = open();
        let mut from = open();
        from.approval = RouteApproval::Broadcast;
        assert_eq!(
            decide_route((a, &from), &[(b, "B", &p)], "x", false, 0),
            RouteVerdict::Deliver(vec![b])
        );
        assert_eq!(
            decide_route((a, &from), &[(b, "B", &p)], "x", true, 0),
            RouteVerdict::Hold(vec![b])
        );
        from.approval = RouteApproval::All;
        assert_eq!(
            decide_route((a, &from), &[(b, "B", &p)], "x", false, 0),
            RouteVerdict::Hold(vec![b])
        );
    }

    #[test]
    fn limits_and_phrases_refuse() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut from = open();
        from.max_per_hour = Some(2);
        let p = open();
        assert!(matches!(
            decide_route((a, &from), &[(b, "B", &p)], "x", false, 2),
            RouteVerdict::Deny(w) if w.contains("last hour")
        ));
        let mut to = open();
        to.blocked_phrases = vec!["Password".into()];
        to.max_message_chars = Some(10);
        assert!(matches!(
            decide_route((a, &open()), &[(b, "B", &to)], "the password is x", false, 0),
            RouteVerdict::Deny(w) if w.contains("longer than 10")
        ));
        assert!(matches!(
            decide_route((a, &open()), &[(b, "B", &to)], "password", false, 0),
            RouteVerdict::Deny(w) if w.contains("blocked phrase `Password`")
        ));
    }
}
