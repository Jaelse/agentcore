//! Organisations (departments, agents, messages, department files) and the
//! node registry. PostgreSQL is the shared state of every node: what each
//! agent *should* be doing lives here, and nodes converge to it.

use agentcore_core::{
    AgentKind, Department, DepartmentId, DepartmentState, Desired, MessageScope, NodeInfo,
    OrgAgent, OrgAgentId, OrgMessage, OrgSettings, SessionId,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Result, Store, StoreError, admin_event};

/// Channel every node listens on for organisation changes.
pub const ORG_CHANNEL: &str = "agentcore_org";
/// Name every communicator has inside its department.
pub const COMMUNICATOR_NAME: &str = "communicator";
/// Names agents use to address their communicator and the whole room.
pub const RESERVED_NAMES: [&str; 3] = [COMMUNICATOR_NAME, "everyone", "all"];

pub const MAX_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_FILES_PER_DEPARTMENT: i64 = 500;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DepartmentInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub mission: String,
    /// Empty = the server's default for departments.
    #[serde(default)]
    pub policy: String,
    #[serde(default)]
    pub tools: Vec<String>,
    pub communicator_agent: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentInput {
    pub name: String,
    pub agent: String,
    #[serde(default)]
    pub instructions: String,
}

/// A message to store, with its recipients already resolved.
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub scope: MessageScope,
    pub from_agent: Option<OrgAgentId>,
    pub from_department: Option<DepartmentId>,
    pub from_name: String,
    pub to_kind: &'static str,
    pub to_agent: Option<OrgAgentId>,
    pub to_department: Option<DepartmentId>,
    pub to_name: String,
    pub text: String,
    /// (agent, its department)
    pub recipients: Vec<(OrgAgentId, DepartmentId)>,
}

#[derive(Debug, Clone, Default)]
pub struct MessageFilter {
    pub department: Option<DepartmentId>,
    pub agent: Option<OrgAgentId>,
    /// Only messages with an id greater than this (ids are time-ordered).
    pub after: Option<Uuid>,
    pub limit: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileInfo {
    pub path: String,
    pub size: i64,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

#[derive(sqlx::FromRow)]
struct DepartmentRow {
    id: Uuid,
    name: String,
    description: String,
    mission: String,
    policy: String,
    tools: Vec<String>,
    communicator_agent: String,
    state: String,
    created_at: DateTime<Utc>,
    updated_by: String,
}

impl From<DepartmentRow> for Department {
    fn from(r: DepartmentRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            description: r.description,
            mission: r.mission,
            policy: r.policy,
            tools: r.tools,
            communicator_agent: r.communicator_agent,
            state: r.state.parse().unwrap_or(DepartmentState::Active),
            created_at: r.created_at,
            updated_by: r.updated_by,
        }
    }
}

#[derive(sqlx::FromRow)]
struct AgentRow {
    id: Uuid,
    department_id: Uuid,
    name: String,
    kind: String,
    agent: String,
    instructions: String,
    desired: String,
    node: Option<String>,
    session_id: Option<Uuid>,
    status: Option<String>,
    note: Option<String>,
    changed_by: String,
    created_at: DateTime<Utc>,
}

impl From<AgentRow> for OrgAgent {
    fn from(r: AgentRow) -> Self {
        Self {
            id: r.id,
            department_id: r.department_id,
            name: r.name,
            kind: r.kind.parse().unwrap_or(AgentKind::Worker),
            agent: r.agent,
            instructions: r.instructions,
            desired: r.desired.parse().unwrap_or(Desired::Stopped),
            node: r.node,
            session_id: r.session_id,
            status: r.status,
            note: r.note,
            changed_by: r.changed_by,
            created_at: r.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct MessageRow {
    id: Uuid,
    created_at: DateTime<Utc>,
    scope: String,
    from_agent: Option<Uuid>,
    from_department: Option<Uuid>,
    from_name: String,
    to_kind: String,
    to_agent: Option<Uuid>,
    to_department: Option<Uuid>,
    to_name: String,
    text: String,
    recipients: Vec<Uuid>,
}

impl From<MessageRow> for OrgMessage {
    fn from(r: MessageRow) -> Self {
        Self {
            id: r.id,
            created_at: r.created_at,
            scope: r.scope.parse().unwrap_or(MessageScope::Human),
            from_agent: r.from_agent,
            from_department: r.from_department,
            from_name: r.from_name,
            to_kind: r.to_kind,
            to_agent: r.to_agent,
            to_department: r.to_department,
            to_name: r.to_name,
            text: r.text,
            recipients: r.recipients,
        }
    }
}

#[derive(sqlx::FromRow)]
struct NodeRow {
    name: String,
    internal_url: String,
    capacity: i32,
    version: String,
    started_at: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    agents: i64,
    alive: bool,
}

impl From<NodeRow> for NodeInfo {
    fn from(r: NodeRow) -> Self {
        Self {
            name: r.name,
            internal_url: r.internal_url,
            capacity: r.capacity.max(0) as u32,
            version: r.version,
            started_at: r.started_at,
            last_seen: r.last_seen,
            agents: r.agents.max(0) as u32,
            alive: r.alive,
        }
    }
}

const DEPARTMENT_COLUMNS: &str = "id, name, description, mission, policy, tools, \
    communicator_agent, state, created_at, updated_by";
const AGENT_COLUMNS: &str = "id, department_id, name, kind, agent, instructions, desired, \
    node, session_id, status, note, changed_by, created_at";
const MESSAGE_SELECT: &str = "SELECT m.id, m.created_at, m.scope, m.from_agent, \
    m.from_department, m.from_name, m.to_kind, m.to_agent, m.to_department, m.to_name, m.text, \
    COALESCE((SELECT array_agg(i.agent_id) FROM org_inbox i WHERE i.message_id = m.id), \
             '{}') AS recipients \
    FROM org_messages m";
/// Serialises placement decisions across nodes.
const PLACEMENT_LOCK: i64 = 0x6167_656e_7463_6f72; // "agentcor"

fn invalid(msg: impl Into<String>) -> StoreError {
    StoreError::Invalid(msg.into())
}

fn clean_department(input: &mut DepartmentInput) -> Result<()> {
    input.name = input.name.trim().to_string();
    if input.name.is_empty() || input.name.chars().count() > 64 {
        return Err(invalid("department name must be 1-64 characters"));
    }
    if input.name.eq_ignore_ascii_case("all") {
        return Err(invalid("`all` is reserved"));
    }
    if input.policy.trim().is_empty() {
        return Err(invalid("policy must not be empty"));
    }
    if input.communicator_agent.trim().is_empty() {
        return Err(invalid("communicator_agent must not be empty"));
    }
    input.tools.sort();
    input.tools.dedup();
    for tool in &input.tools {
        if !agentcore_core::org::TOOL_GROUPS.contains(&tool.as_str()) {
            return Err(invalid(format!(
                "unknown tool group `{tool}` (available: {})",
                agentcore_core::org::TOOL_GROUPS.join(", ")
            )));
        }
    }
    Ok(())
}

/// Agent names are how colleagues address each other in tool calls.
pub fn valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
}

fn map_unique(err: sqlx::Error, what: String) -> StoreError {
    match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => StoreError::Conflict(what),
        _ => StoreError::Db(err),
    }
}

impl Store {
    // ---- settings ---------------------------------------------------------------

    pub async fn org_settings(&self) -> Result<OrgSettings> {
        let (max_departments, max_agents): (i32, i32) =
            sqlx::query_as("SELECT max_departments, max_agents_per_department FROM org_settings")
                .fetch_one(&self.pool)
                .await?;
        Ok(OrgSettings {
            max_departments: max_departments.max(0) as u32,
            max_agents_per_department: max_agents.max(0) as u32,
        })
    }

    pub async fn set_org_settings(
        &self,
        settings: OrgSettings,
        actor: &str,
    ) -> Result<OrgSettings> {
        let cap = |v: u32| i32::try_from(v).map_err(|_| invalid("limit is too large"));
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE org_settings SET max_departments = $1, max_agents_per_department = $2,
                 updated_at = now(), updated_by = $3",
        )
        .bind(cap(settings.max_departments)?)
        .bind(cap(settings.max_agents_per_department)?)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            "org.settings",
            "organisation",
            serde_json::json!(settings),
        )
        .await?;
        tx.commit().await?;
        Ok(settings)
    }

    /// Initial limits from the configuration, applied only while nobody has
    /// changed them yet.
    pub async fn seed_org_settings(&self, settings: OrgSettings) -> Result<()> {
        sqlx::query(
            "UPDATE org_settings SET max_departments = $1, max_agents_per_department = $2
             WHERE updated_by = 'agentcore'",
        )
        .bind(settings.max_departments.min(i32::MAX as u32) as i32)
        .bind(settings.max_agents_per_department.min(i32::MAX as u32) as i32)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- departments ------------------------------------------------------------

    pub async fn list_departments(&self) -> Result<Vec<Department>> {
        let rows: Vec<DepartmentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {DEPARTMENT_COLUMNS} FROM departments ORDER BY created_at"
        )))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_department(&self, id: DepartmentId) -> Result<Department> {
        let row: Option<DepartmentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {DEPARTMENT_COLUMNS} FROM departments WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Into::into)
            .ok_or_else(|| StoreError::NotFound(format!("department {id}")))
    }

    /// Case-insensitive lookup by name.
    pub async fn department_by_name(&self, name: &str) -> Result<Option<Department>> {
        let row: Option<DepartmentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {DEPARTMENT_COLUMNS} FROM departments WHERE lower(name) = lower($1)"
        )))
        .bind(name.trim())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Create a department and its communicator, within the department limit.
    pub async fn create_department(
        &self,
        mut input: DepartmentInput,
        actor: &str,
    ) -> Result<Department> {
        clean_department(&mut input)?;
        let mut tx = self.pool.begin().await?;
        // The settings row lock serialises limit checks across nodes.
        let (max,): (i32,) = sqlx::query_as("SELECT max_departments FROM org_settings FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
        let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM departments")
            .fetch_one(&mut *tx)
            .await?;
        if count >= i64::from(max) {
            return Err(StoreError::Limit(format!(
                "the organisation already has the maximum of {max} departments"
            )));
        }
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO departments (id, name, description, mission, policy, tools,
                                      communicator_agent, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(&input.name)
        .bind(&input.description)
        .bind(&input.mission)
        .bind(input.policy.trim())
        .bind(&input.tools)
        .bind(input.communicator_agent.trim())
        .bind(actor)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_unique(e, format!("department `{}`", input.name)))?;
        sqlx::query(
            "INSERT INTO org_agents (id, department_id, name, kind, agent)
             VALUES ($1, $2, $3, 'communicator', $4)",
        )
        .bind(Uuid::now_v7())
        .bind(id)
        .bind(COMMUNICATOR_NAME)
        .bind(input.communicator_agent.trim())
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            "department.create",
            &input.name,
            serde_json::json!({
                "id": id, "policy": input.policy, "tools": input.tools,
                "communicator_agent": input.communicator_agent,
            }),
        )
        .await?;
        tx.commit().await?;
        self.get_department(id).await
    }

    pub async fn update_department(
        &self,
        id: DepartmentId,
        mut input: DepartmentInput,
        actor: &str,
    ) -> Result<Department> {
        clean_department(&mut input)?;
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE departments SET name = $2, description = $3, mission = $4, policy = $5,
                 tools = $6, communicator_agent = $7, updated_at = now(), updated_by = $8
             WHERE id = $1",
        )
        .bind(id)
        .bind(&input.name)
        .bind(&input.description)
        .bind(&input.mission)
        .bind(input.policy.trim())
        .bind(&input.tools)
        .bind(input.communicator_agent.trim())
        .bind(actor)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_unique(e, format!("department `{}`", input.name)))?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("department {id}")));
        }
        sqlx::query(
            "UPDATE org_agents SET agent = $2, updated_at = now()
             WHERE department_id = $1 AND kind = 'communicator'",
        )
        .bind(id)
        .bind(input.communicator_agent.trim())
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            "department.update",
            &input.name,
            serde_json::json!({ "id": id, "policy": input.policy, "tools": input.tools }),
        )
        .await?;
        tx.commit().await?;
        self.get_department(id).await
    }

    /// Delete a department whose agents are all stopped.
    pub async fn delete_department(&self, id: DepartmentId, actor: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let name: Option<(String,)> =
            sqlx::query_as("SELECT name FROM departments WHERE id = $1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((name,)) = name else {
            return Err(StoreError::NotFound(format!("department {id}")));
        };
        let (live,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM org_agents WHERE department_id = $1 AND desired <> 'stopped'",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if live > 0 {
            return Err(StoreError::Conflict(format!(
                "department `{name}` has {live} running agent(s); stop them first"
            )));
        }
        sqlx::query("DELETE FROM departments WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        admin_event(
            &mut tx,
            actor,
            "department.delete",
            &name,
            serde_json::json!({ "id": id }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Pause or reactivate one department (`Some`) or all of them (`None`).
    pub async fn set_department_state(
        &self,
        id: Option<DepartmentId>,
        state: DepartmentState,
        actor: &str,
    ) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE departments SET state = $2, updated_at = now(), updated_by = $3
             WHERE ($1::uuid IS NULL OR id = $1) AND state <> $2",
        )
        .bind(id)
        .bind(state.as_str())
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            &format!(
                "department.{}",
                if state == DepartmentState::Paused {
                    "pause"
                } else {
                    "resume"
                }
            ),
            &id.map_or_else(|| "all".to_string(), |i| i.to_string()),
            serde_json::json!({}),
        )
        .await?;
        tx.commit().await?;
        Ok(updated.rows_affected())
    }

    // ---- agents -----------------------------------------------------------------

    pub async fn list_agents(&self, department: Option<DepartmentId>) -> Result<Vec<OrgAgent>> {
        let rows: Vec<AgentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {AGENT_COLUMNS} FROM org_agents
             WHERE $1::uuid IS NULL OR department_id = $1
             ORDER BY department_id, kind DESC, created_at"
        )))
        .bind(department)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_agent(&self, id: OrgAgentId) -> Result<OrgAgent> {
        let row: Option<AgentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {AGENT_COLUMNS} FROM org_agents WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Into::into)
            .ok_or_else(|| StoreError::NotFound(format!("agent {id}")))
    }

    pub async fn agent_by_session(&self, session: SessionId) -> Result<Option<OrgAgent>> {
        let row: Option<AgentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {AGENT_COLUMNS} FROM org_agents WHERE session_id = $1"
        )))
        .bind(session)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Agents placed on a node (running, paused, or stopped but still
    /// remembering that node).
    pub async fn agents_on_node(&self, node: &str) -> Result<Vec<OrgAgent>> {
        let rows: Vec<AgentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {AGENT_COLUMNS} FROM org_agents WHERE node = $1"
        )))
        .bind(node)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Add a worker agent, within the per-department limit.
    pub async fn add_agent(
        &self,
        department: DepartmentId,
        mut input: AgentInput,
        actor: &str,
    ) -> Result<OrgAgent> {
        input.name = input.name.trim().to_ascii_lowercase();
        if !valid_agent_name(&input.name) {
            return Err(invalid(
                "agent name must be 1-32 characters of a-z, 0-9, - or _",
            ));
        }
        if RESERVED_NAMES.contains(&input.name.as_str()) {
            return Err(invalid(format!("`{}` is reserved", input.name)));
        }
        if input.agent.trim().is_empty() {
            return Err(invalid("agent must not be empty"));
        }
        let mut tx = self.pool.begin().await?;
        let (max,): (i32,) =
            sqlx::query_as("SELECT max_agents_per_department FROM org_settings FOR UPDATE")
                .fetch_one(&mut *tx)
                .await?;
        let dept: Option<(String,)> =
            sqlx::query_as("SELECT name FROM departments WHERE id = $1 FOR UPDATE")
                .bind(department)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((dept_name,)) = dept else {
            return Err(StoreError::NotFound(format!("department {department}")));
        };
        let (count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM org_agents WHERE department_id = $1 AND kind = 'worker'",
        )
        .bind(department)
        .fetch_one(&mut *tx)
        .await?;
        if count >= i64::from(max) {
            return Err(StoreError::Limit(format!(
                "department `{dept_name}` already has the maximum of {max} agents"
            )));
        }
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO org_agents (id, department_id, name, kind, agent, instructions)
             VALUES ($1, $2, $3, 'worker', $4, $5)",
        )
        .bind(id)
        .bind(department)
        .bind(&input.name)
        .bind(input.agent.trim())
        .bind(&input.instructions)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_unique(e, format!("agent `{}` in `{dept_name}`", input.name)))?;
        admin_event(
            &mut tx,
            actor,
            "agent.create",
            &format!("{dept_name}/{}", input.name),
            serde_json::json!({ "id": id, "agent": input.agent }),
        )
        .await?;
        tx.commit().await?;
        self.get_agent(id).await
    }

    pub async fn update_agent(
        &self,
        id: OrgAgentId,
        agent: Option<String>,
        instructions: Option<String>,
        actor: &str,
    ) -> Result<OrgAgent> {
        let current = self.get_agent(id).await?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE org_agents SET agent = COALESCE($2, agent),
                 instructions = COALESCE($3, instructions), updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(agent.as_deref().map(str::trim).filter(|a| !a.is_empty()))
        .bind(instructions.as_deref())
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            "agent.update",
            &current.name,
            serde_json::json!({ "id": id, "agent": agent }),
        )
        .await?;
        tx.commit().await?;
        self.get_agent(id).await
    }

    /// Remove a stopped worker.
    pub async fn delete_agent(&self, id: OrgAgentId, actor: &str) -> Result<()> {
        let agent = self.get_agent(id).await?;
        if agent.kind == AgentKind::Communicator {
            return Err(invalid(
                "a department always has its communicator; delete the department instead",
            ));
        }
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("DELETE FROM org_agents WHERE id = $1 AND desired = 'stopped'")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(StoreError::Conflict(format!(
                "agent `{}` is running; stop it first",
                agent.name
            )));
        }
        admin_event(
            &mut tx,
            actor,
            "agent.delete",
            &agent.name,
            serde_json::json!({ "id": id }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Start a stopped agent: place it on the least loaded live node.
    /// Returns `None` if it was not stopped (already running or paused).
    pub async fn start_agent(
        &self,
        id: OrgAgentId,
        node_timeout_secs: u64,
        by: &str,
    ) -> Result<Option<OrgAgent>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(PLACEMENT_LOCK)
            .execute(&mut *tx)
            .await?;
        let desired: Option<(String,)> =
            sqlx::query_as("SELECT desired FROM org_agents WHERE id = $1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        match desired {
            None => return Err(StoreError::NotFound(format!("agent {id}"))),
            Some((d,)) if d != "stopped" => return Ok(None),
            Some(_) => {}
        }
        let node: Option<(String,)> = sqlx::query_as(
            "SELECT n.name FROM nodes n
             LEFT JOIN org_agents a ON a.node = n.name AND a.desired <> 'stopped'
             WHERE n.last_seen > now() - make_interval(secs => $1) AND n.capacity > 0
             GROUP BY n.name, n.capacity
             HAVING count(a.id) < n.capacity
             ORDER BY count(a.id)::float8 / n.capacity, n.name
             LIMIT 1",
        )
        .bind(node_timeout_secs as f64)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((node,)) = node else {
            return Err(StoreError::Unavailable(
                "no node has free capacity to run another agent".into(),
            ));
        };
        sqlx::query(
            "UPDATE org_agents SET desired = 'running', node = $2, session_id = NULL,
                 status = 'starting', note = NULL, changed_by = $3, updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(&node)
        .bind(by)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(self.get_agent(id).await?))
    }

    /// Set what an agent should do. `from` restricts which current states
    /// change (e.g. only pause running agents). Returns the changed agents.
    pub async fn set_desired(
        &self,
        scope: AgentScope,
        from: &[Desired],
        to: Desired,
        note: Option<&str>,
        by: &str,
    ) -> Result<Vec<OrgAgent>> {
        let from: Vec<&str> = from.iter().map(|d| d.as_str()).collect();
        let (agent, department) = match scope {
            AgentScope::Agent(id) => (Some(id), None),
            AgentScope::Department(id) => (None, Some(id)),
            AgentScope::All => (None, None),
        };
        let rows: Vec<AgentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE org_agents SET desired = $4, note = COALESCE($5, note), changed_by = $6,
                 updated_at = now()
             WHERE ($1::uuid IS NULL OR id = $1) AND ($2::uuid IS NULL OR department_id = $2)
               AND desired = ANY($3)
             RETURNING {AGENT_COLUMNS}"
        )))
        .bind(agent)
        .bind(department)
        .bind(&from)
        .bind(to.as_str())
        .bind(note)
        .bind(by)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// The node started a session for the agent.
    pub async fn agent_session_started(
        &self,
        id: OrgAgentId,
        session: SessionId,
        status: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE org_agents SET session_id = $2, status = $3, updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(session)
        .bind(status)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Latest status of the agent's session. Returns whether it changed.
    pub async fn agent_status(
        &self,
        id: OrgAgentId,
        session: SessionId,
        status: &str,
    ) -> Result<bool> {
        let updated = sqlx::query(
            "UPDATE org_agents SET status = $3, updated_at = now()
             WHERE id = $1 AND session_id = $2 AND status IS DISTINCT FROM $3",
        )
        .bind(id)
        .bind(session)
        .bind(status)
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() > 0)
    }

    /// The agent's session is over (or could not start): it is stopped now.
    pub async fn agent_ended(
        &self,
        id: OrgAgentId,
        session: Option<SessionId>,
        status: &str,
        note: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE org_agents SET desired = 'stopped', status = $3,
                 note = COALESCE($4, note), updated_at = now()
             WHERE id = $1 AND session_id IS NOT DISTINCT FROM $2 AND desired <> 'stopped'
                OR id = $1 AND session_id IS NOT DISTINCT FROM $2 AND status IS DISTINCT FROM $3",
        )
        .bind(id)
        .bind(session)
        .bind(status)
        .bind(note)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Agents placed on nodes whose heartbeat is older than the timeout are
    /// stopped: their sandboxes are gone with the node.
    pub async fn fail_agents_on_dead_nodes(&self, node_timeout_secs: u64) -> Result<Vec<OrgAgent>> {
        let rows: Vec<AgentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE org_agents a SET desired = 'stopped', status = 'failed',
                 note = 'node ' || a.node || ' stopped responding', updated_at = now()
             FROM nodes n
             WHERE a.node = n.name AND a.desired <> 'stopped'
               AND n.last_seen < now() - make_interval(secs => $1)
             RETURNING {}",
            AGENT_COLUMNS
                .split(", ")
                .map(|c| format!("a.{}", c.trim()))
                .collect::<Vec<_>>()
                .join(", ")
        )))
        .bind(node_timeout_secs as f64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    // ---- messages ---------------------------------------------------------------

    pub async fn insert_message(&self, msg: NewMessage) -> Result<OrgMessage> {
        let id = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO org_messages (id, scope, from_agent, from_department, from_name,
                 to_kind, to_agent, to_department, to_name, text)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(id)
        .bind(msg.scope.as_str())
        .bind(msg.from_agent)
        .bind(msg.from_department)
        .bind(&msg.from_name)
        .bind(msg.to_kind)
        .bind(msg.to_agent)
        .bind(msg.to_department)
        .bind(&msg.to_name)
        .bind(&msg.text)
        .execute(&mut *tx)
        .await?;
        let (agents, departments): (Vec<Uuid>, Vec<Uuid>) = msg.recipients.iter().copied().unzip();
        sqlx::query(
            "INSERT INTO org_inbox (message_id, agent_id, department_id)
             SELECT $1, a, d FROM unnest($2::uuid[], $3::uuid[]) AS t(a, d)
             ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(&agents)
        .bind(&departments)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.get_message(id).await
    }

    pub async fn get_message(&self, id: Uuid) -> Result<OrgMessage> {
        let row: Option<MessageRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{MESSAGE_SELECT} WHERE m.id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Into::into)
            .ok_or_else(|| StoreError::NotFound(format!("message {id}")))
    }

    /// Messages of the organisation, a department (sent, received or posted
    /// in it) or an agent, oldest first.
    pub async fn list_messages(&self, filter: MessageFilter) -> Result<Vec<OrgMessage>> {
        let limit = if filter.limit <= 0 {
            200
        } else {
            filter.limit.min(1000)
        };
        // Newest `limit` messages, returned oldest first.
        let rows: Vec<MessageRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT * FROM ({MESSAGE_SELECT}
             WHERE ($1::uuid IS NULL OR m.from_department = $1 OR m.to_department = $1
                    OR EXISTS (SELECT 1 FROM org_inbox i
                               WHERE i.message_id = m.id AND i.department_id = $1))
               AND ($2::uuid IS NULL OR m.from_agent = $2 OR m.to_agent = $2
                    OR EXISTS (SELECT 1 FROM org_inbox i
                               WHERE i.message_id = m.id AND i.agent_id = $2))
               AND ($3::uuid IS NULL OR m.id > $3)
             ORDER BY m.id DESC LIMIT $4) recent ORDER BY id"
        )))
        .bind(filter.department)
        .bind(filter.agent)
        .bind(filter.after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Messages waiting for an agent, oldest first (not marked).
    pub async fn pending_messages(&self, agent: OrgAgentId) -> Result<Vec<OrgMessage>> {
        let rows: Vec<MessageRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{MESSAGE_SELECT}
             WHERE EXISTS (SELECT 1 FROM org_inbox i WHERE i.message_id = m.id
                           AND i.agent_id = $1 AND i.delivered_at IS NULL)
             ORDER BY m.id LIMIT 50"
        )))
        .bind(agent)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn mark_delivered(&self, agent: OrgAgentId, messages: &[Uuid]) -> Result<()> {
        sqlx::query(
            "UPDATE org_inbox SET delivered_at = now()
             WHERE agent_id = $1 AND message_id = ANY($2) AND delivered_at IS NULL",
        )
        .bind(agent)
        .bind(messages)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Agents (of the given ones) with undelivered messages.
    pub async fn agents_with_mail(&self, agents: &[OrgAgentId]) -> Result<Vec<OrgAgentId>> {
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT DISTINCT agent_id FROM org_inbox
             WHERE agent_id = ANY($1) AND delivered_at IS NULL",
        )
        .bind(agents)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    // ---- department files -------------------------------------------------------

    pub async fn list_department_files(&self, department: DepartmentId) -> Result<Vec<FileInfo>> {
        let rows: Vec<(String, i32, DateTime<Utc>, String)> = sqlx::query_as(
            "SELECT path, octet_length(content), updated_at, updated_by FROM department_files
             WHERE department_id = $1 ORDER BY path",
        )
        .bind(department)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(path, size, updated_at, updated_by)| FileInfo {
                path,
                size: i64::from(size),
                updated_at,
                updated_by,
            })
            .collect())
    }

    pub async fn read_department_file(
        &self,
        department: DepartmentId,
        path: &str,
    ) -> Result<Option<Vec<u8>>> {
        let row: Option<(Vec<u8>,)> = sqlx::query_as(
            "SELECT content FROM department_files WHERE department_id = $1 AND path = $2",
        )
        .bind(department)
        .bind(path)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(c,)| c))
    }

    pub async fn write_department_file(
        &self,
        department: DepartmentId,
        path: &str,
        content: &[u8],
        by: &str,
    ) -> Result<()> {
        if content.len() > MAX_FILE_BYTES {
            return Err(invalid(format!(
                "department files are limited to {MAX_FILE_BYTES} bytes"
            )));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT id FROM departments WHERE id = $1 FOR UPDATE")
            .bind(department)
            .execute(&mut *tx)
            .await?;
        let (count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM department_files WHERE department_id = $1 AND path <> $2",
        )
        .bind(department)
        .bind(path)
        .fetch_one(&mut *tx)
        .await?;
        if count >= MAX_FILES_PER_DEPARTMENT {
            return Err(StoreError::Limit(format!(
                "a department can hold at most {MAX_FILES_PER_DEPARTMENT} files"
            )));
        }
        sqlx::query(
            "INSERT INTO department_files (department_id, path, content, updated_by)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (department_id, path) DO UPDATE
             SET content = EXCLUDED.content, updated_at = now(), updated_by = EXCLUDED.updated_by",
        )
        .bind(department)
        .bind(path)
        .bind(content)
        .bind(by)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    // ---- nodes ------------------------------------------------------------------

    /// Register this node (at start) or refresh its heartbeat.
    pub async fn heartbeat(
        &self,
        name: &str,
        internal_url: &str,
        capacity: u32,
        version: &str,
        starting: bool,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO nodes (name, internal_url, capacity, version)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (name) DO UPDATE SET internal_url = EXCLUDED.internal_url,
                 capacity = EXCLUDED.capacity, version = EXCLUDED.version, last_seen = now(),
                 started_at = CASE WHEN $5 THEN now() ELSE nodes.started_at END",
        )
        .bind(name)
        .bind(internal_url)
        .bind(capacity.min(i32::MAX as u32) as i32)
        .bind(version)
        .bind(starting)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_nodes(&self, node_timeout_secs: u64) -> Result<Vec<NodeInfo>> {
        let rows: Vec<NodeRow> = sqlx::query_as(
            "SELECT n.name, n.internal_url, n.capacity, n.version, n.started_at, n.last_seen,
                    (SELECT count(*) FROM org_agents a
                     WHERE a.node = n.name AND a.desired <> 'stopped') AS agents,
                    n.last_seen > now() - make_interval(secs => $1) AS alive
             FROM nodes n ORDER BY n.name",
        )
        .bind(node_timeout_secs as f64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// The node that owns a session, with its URL and whether it is alive.
    pub async fn session_node(
        &self,
        session: SessionId,
        node_timeout_secs: u64,
    ) -> Result<Option<(String, String, bool)>> {
        Ok(sqlx::query_as(
            "SELECT n.name, n.internal_url, n.last_seen > now() - make_interval(secs => $2)
             FROM sessions s JOIN nodes n ON n.name = s.node WHERE s.id = $1",
        )
        .bind(session)
        .bind(node_timeout_secs as f64)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Tell every node that something changed (`payload` is a small JSON hint).
    pub async fn notify(&self, payload: &serde_json::Value) -> Result<()> {
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(ORG_CHANNEL)
            .bind(payload.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// Which agents a desired-state change applies to.
#[derive(Debug, Clone, Copy)]
pub enum AgentScope {
    Agent(OrgAgentId),
    Department(DepartmentId),
    All,
}
