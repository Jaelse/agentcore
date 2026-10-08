//! Organisations: departments of agents and who may talk to whom.
//!
//! A department (a "room") groups agents that work together. Agents talk
//! freely inside their department; departments only talk to each other
//! through their communicator agents. [`route`] is the single place where
//! that rule is decided.

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
    /// Only passes messages between its department and other departments.
    Communicator,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Communicator => "communicator",
        }
    }
}

impl std::str::FromStr for AgentKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "worker" => Ok(Self::Worker),
            "communicator" => Ok(Self::Communicator),
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
pub const TOOL_GROUPS: [&str; 2] = [TOOL_SANDBOX, TOOL_FILES];

/// Limits set by an admin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgSettings {
    pub max_departments: u32,
    /// Worker agents per department; the communicator is not counted.
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
    /// Configured agent (`[[agents]]`) the communicator runs.
    pub communicator_agent: String,
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
    pub created_at: DateTime<Utc>,
    pub updated_by: String,
}

impl Department {
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
        kind: AgentKind,
    },
}

/// Where a message is addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Agent {
        id: OrgAgentId,
        department: DepartmentId,
        kind: AgentKind,
    },
    /// A department: its room when sent from inside (or by a person), its
    /// communicator when sent by another department's communicator.
    Department(DepartmentId),
    /// Every other department (their communicators).
    AllDepartments,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageScope {
    /// Inside one department.
    Internal,
    /// Between communicators of different departments.
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
        "you cannot contact other departments directly: send your message to your \
         department's communicator (to: \"communicator\")"
    )]
    WorkerOutsideDepartment,
    #[error(
        "communicators reach other departments through their communicators: use \
         team_send_to_department"
    )]
    CommunicatorToForeignWorker,
    #[error("you cannot send a message to yourself")]
    ToSelf,
}

/// Decide whether `from` may send to `to`, and with which scope.
pub fn route(from: Party, to: Target) -> Result<MessageScope, RouteError> {
    let Party::Agent {
        id: from_id,
        department: from_dept,
        kind: from_kind,
    } = from
    else {
        return Ok(MessageScope::Human);
    };
    match (from_kind, to) {
        (_, Target::Agent { id, .. }) if id == from_id => Err(RouteError::ToSelf),
        (_, Target::Agent { department, .. }) if department == from_dept => {
            Ok(MessageScope::Internal)
        }
        (_, Target::Department(department)) if department == from_dept => {
            Ok(MessageScope::Internal)
        }
        (AgentKind::Worker, _) => Err(RouteError::WorkerOutsideDepartment),
        (
            AgentKind::Communicator,
            Target::Agent {
                kind: AgentKind::Communicator,
                ..
            },
        )
        | (AgentKind::Communicator, Target::Department(_) | Target::AllDepartments) => {
            Ok(MessageScope::InterDepartment)
        }
        (AgentKind::Communicator, Target::Agent { .. }) => {
            Err(RouteError::CommunicatorToForeignWorker)
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

    fn agent(dept: DepartmentId, kind: AgentKind) -> (Party, Target) {
        let id = Uuid::new_v4();
        (
            Party::Agent {
                id,
                department: dept,
                kind,
            },
            Target::Agent {
                id,
                department: dept,
                kind,
            },
        )
    }

    #[test]
    fn workers_stay_inside_their_department() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let (w1, _) = agent(a, AgentKind::Worker);
        let (_, w2) = agent(a, AgentKind::Worker);
        let (_, own_comm) = agent(a, AgentKind::Communicator);
        let (_, foreign_worker) = agent(b, AgentKind::Worker);
        let (_, foreign_comm) = agent(b, AgentKind::Communicator);

        assert_eq!(route(w1, w2), Ok(MessageScope::Internal));
        assert_eq!(route(w1, own_comm), Ok(MessageScope::Internal));
        assert_eq!(route(w1, Target::Department(a)), Ok(MessageScope::Internal));
        for to in [
            foreign_worker,
            foreign_comm,
            Target::Department(b),
            Target::AllDepartments,
        ] {
            assert_eq!(route(w1, to), Err(RouteError::WorkerOutsideDepartment));
        }
    }

    #[test]
    fn communicators_bridge_departments() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let (comm, comm_target) = agent(a, AgentKind::Communicator);
        let (_, own_worker) = agent(a, AgentKind::Worker);
        let (_, foreign_worker) = agent(b, AgentKind::Worker);
        let (_, foreign_comm) = agent(b, AgentKind::Communicator);

        assert_eq!(route(comm, own_worker), Ok(MessageScope::Internal));
        assert_eq!(
            route(comm, Target::Department(a)),
            Ok(MessageScope::Internal)
        );
        assert_eq!(route(comm, foreign_comm), Ok(MessageScope::InterDepartment));
        assert_eq!(
            route(comm, Target::Department(b)),
            Ok(MessageScope::InterDepartment)
        );
        assert_eq!(
            route(comm, Target::AllDepartments),
            Ok(MessageScope::InterDepartment)
        );
        assert_eq!(
            route(comm, foreign_worker),
            Err(RouteError::CommunicatorToForeignWorker)
        );
        assert_eq!(route(comm, comm_target), Err(RouteError::ToSelf));
    }

    #[test]
    fn people_reach_everyone() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let (_, w) = agent(a, AgentKind::Worker);
        let (_, c) = agent(b, AgentKind::Communicator);
        for to in [w, c, Target::Department(a), Target::AllDepartments] {
            assert_eq!(route(Party::Human, to), Ok(MessageScope::Human));
        }
    }
}
