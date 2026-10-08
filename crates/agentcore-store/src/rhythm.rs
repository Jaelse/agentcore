//! Organisation goals and scheduled check-ins.

use agentcore_core::{DepartmentId, GoalStatus, OrgAgentId, OrgGoal, OrgSchedule};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::{Result, Store, StoreError, admin_event};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GoalInput {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub department_id: Option<DepartmentId>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GoalUpdate {
    pub title: Option<String>,
    pub description: Option<String>,
    /// `Some(None)` clears the owner.
    #[serde(default, with = "double_option")]
    pub department_id: Option<Option<DepartmentId>>,
    pub status: Option<GoalStatus>,
}

/// Distinguishes a missing field from an explicit `null`.
mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Ok(Some(Option::deserialize(d)?))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScheduleInput {
    pub name: String,
    pub message: String,
    pub every_minutes: u32,
    #[serde(default)]
    pub agent_id: Option<OrgAgentId>,
    /// First run; default: one interval from now.
    #[serde(default)]
    pub first_run_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScheduleUpdate {
    pub name: Option<String>,
    pub message: Option<String>,
    pub every_minutes: Option<u32>,
    pub enabled: Option<bool>,
}

#[derive(sqlx::FromRow)]
struct GoalRow {
    id: Uuid,
    title: String,
    description: String,
    department_id: Option<Uuid>,
    status: String,
    progress: String,
    progress_by: Option<String>,
    progress_at: Option<DateTime<Utc>>,
    created_by: String,
    created_at: DateTime<Utc>,
}

impl From<GoalRow> for OrgGoal {
    fn from(r: GoalRow) -> Self {
        Self {
            id: r.id,
            title: r.title,
            description: r.description,
            department_id: r.department_id,
            status: r.status.parse().unwrap_or(GoalStatus::Active),
            progress: r.progress,
            progress_by: r.progress_by,
            progress_at: r.progress_at,
            created_by: r.created_by,
            created_at: r.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct ScheduleRow {
    id: Uuid,
    department_id: Uuid,
    agent_id: Option<Uuid>,
    name: String,
    message: String,
    every_minutes: i32,
    next_run_at: DateTime<Utc>,
    last_run_at: Option<DateTime<Utc>>,
    enabled: bool,
    created_by: String,
}

impl From<ScheduleRow> for OrgSchedule {
    fn from(r: ScheduleRow) -> Self {
        Self {
            id: r.id,
            department_id: r.department_id,
            agent_id: r.agent_id,
            name: r.name,
            message: r.message,
            every_minutes: r.every_minutes.max(1) as u32,
            next_run_at: r.next_run_at,
            last_run_at: r.last_run_at,
            enabled: r.enabled,
            created_by: r.created_by,
        }
    }
}

const GOAL_COLUMNS: &str = "id, title, description, department_id, status, progress, \
    progress_by, progress_at, created_by, created_at";
const SCHEDULE_COLUMNS: &str = "id, department_id, agent_id, name, message, every_minutes, \
    next_run_at, last_run_at, enabled, created_by";

fn invalid(msg: impl Into<String>) -> StoreError {
    StoreError::Invalid(msg.into())
}

/// Longest interval: a quarter of a year.
const MAX_EVERY_MINUTES: u32 = 60 * 24 * 92;

fn check_every(every: u32) -> Result<i32> {
    if every == 0 || every > MAX_EVERY_MINUTES {
        return Err(invalid(format!(
            "a check-in runs every 1 to {MAX_EVERY_MINUTES} minutes"
        )));
    }
    Ok(every as i32)
}

impl Store {
    // ---- goals ------------------------------------------------------------------

    pub async fn list_goals(&self) -> Result<Vec<OrgGoal>> {
        let rows: Vec<GoalRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {GOAL_COLUMNS} FROM org_goals
             ORDER BY (status = 'active') DESC, created_at"
        )))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_goal(&self, id: Uuid) -> Result<OrgGoal> {
        let row: Option<GoalRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {GOAL_COLUMNS} FROM org_goals WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Into::into)
            .ok_or_else(|| StoreError::NotFound(format!("goal {id}")))
    }

    pub async fn create_goal(&self, input: GoalInput, actor: &str) -> Result<OrgGoal> {
        let title = input.title.trim();
        if title.is_empty() || title.chars().count() > 200 {
            return Err(invalid("a goal needs a title of up to 200 characters"));
        }
        let id = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO org_goals (id, title, description, department_id, created_by)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(title)
        .bind(input.description.trim())
        .bind(input.department_id)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            "goal.create",
            title,
            serde_json::json!({ "id": id }),
        )
        .await?;
        tx.commit().await?;
        self.get_goal(id).await
    }

    pub async fn update_goal(&self, id: Uuid, update: GoalUpdate, actor: &str) -> Result<OrgGoal> {
        let current = self.get_goal(id).await?;
        let title = update
            .title
            .as_deref()
            .map(str::trim)
            .unwrap_or(&current.title);
        if title.is_empty() {
            return Err(invalid("a goal needs a title"));
        }
        let department = update.department_id.unwrap_or(current.department_id);
        let status = update.status.unwrap_or(current.status);
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE org_goals SET title = $2, description = $3, department_id = $4,
                 status = $5, updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(title)
        .bind(
            update
                .description
                .as_deref()
                .map(str::trim)
                .unwrap_or(&current.description),
        )
        .bind(department)
        .bind(status.as_str())
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            "goal.update",
            title,
            serde_json::json!({ "id": id, "status": status }),
        )
        .await?;
        tx.commit().await?;
        self.get_goal(id).await
    }

    pub async fn delete_goal(&self, id: Uuid, actor: &str) -> Result<()> {
        let goal = self.get_goal(id).await?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM org_goals WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        admin_event(
            &mut tx,
            actor,
            "goal.delete",
            &goal.title,
            serde_json::json!({ "id": id }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Record progress on an active goal (agents and people).
    pub async fn report_goal_progress(&self, id: Uuid, text: &str, by: &str) -> Result<OrgGoal> {
        let text = text.trim();
        if text.is_empty() || text.chars().count() > 4000 {
            return Err(invalid("progress must be 1-4000 characters"));
        }
        let updated = sqlx::query(
            "UPDATE org_goals SET progress = $2, progress_by = $3, progress_at = now(),
                 updated_at = now()
             WHERE id = $1 AND status = 'active'",
        )
        .bind(id)
        .bind(text)
        .bind(by)
        .execute(&self.pool)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(invalid("that goal does not exist or is no longer active"));
        }
        self.get_goal(id).await
    }

    // ---- check-ins ----------------------------------------------------------------

    pub async fn list_schedules(
        &self,
        department: Option<DepartmentId>,
    ) -> Result<Vec<OrgSchedule>> {
        let rows: Vec<ScheduleRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {SCHEDULE_COLUMNS} FROM org_schedules
             WHERE $1::uuid IS NULL OR department_id = $1 ORDER BY created_at"
        )))
        .bind(department)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_schedule(&self, id: Uuid) -> Result<OrgSchedule> {
        let row: Option<ScheduleRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {SCHEDULE_COLUMNS} FROM org_schedules WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Into::into)
            .ok_or_else(|| StoreError::NotFound(format!("check-in {id}")))
    }

    pub async fn create_schedule(
        &self,
        department: DepartmentId,
        input: ScheduleInput,
        actor: &str,
    ) -> Result<OrgSchedule> {
        let every = check_every(input.every_minutes)?;
        let name = input.name.trim();
        if name.is_empty() || input.message.trim().is_empty() {
            return Err(invalid("a check-in needs a name and a message"));
        }
        if let Some(agent) = input.agent_id {
            let (dept,): (Uuid,) =
                sqlx::query_as("SELECT department_id FROM org_agents WHERE id = $1")
                    .bind(agent)
                    .fetch_optional(&self.pool)
                    .await?
                    .ok_or_else(|| StoreError::NotFound(format!("agent {agent}")))?;
            if dept != department {
                return Err(invalid("that agent is in another department"));
            }
        }
        let id = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO org_schedules (id, department_id, agent_id, name, message,
                 every_minutes, next_run_at, created_by)
             VALUES ($1, $2, $3, $4, $5, $6,
                     COALESCE($7, now() + make_interval(mins => $6)), $8)",
        )
        .bind(id)
        .bind(department)
        .bind(input.agent_id)
        .bind(name)
        .bind(input.message.trim())
        .bind(every)
        .bind(input.first_run_at)
        .bind(actor)
        .execute(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_foreign_key_violation() => {
                StoreError::NotFound(format!("department {department}"))
            }
            _ => StoreError::Db(e),
        })?;
        admin_event(
            &mut tx,
            actor,
            "checkin.create",
            name,
            serde_json::json!({ "id": id, "department": department, "every_minutes": every }),
        )
        .await?;
        tx.commit().await?;
        self.get_schedule(id).await
    }

    pub async fn update_schedule(
        &self,
        id: Uuid,
        update: ScheduleUpdate,
        actor: &str,
    ) -> Result<OrgSchedule> {
        let current = self.get_schedule(id).await?;
        let every = check_every(update.every_minutes.unwrap_or(current.every_minutes))?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE org_schedules SET name = $2, message = $3, every_minutes = $4, enabled = $5
             WHERE id = $1",
        )
        .bind(id)
        .bind(
            update
                .name
                .as_deref()
                .map(str::trim)
                .unwrap_or(&current.name),
        )
        .bind(
            update
                .message
                .as_deref()
                .map(str::trim)
                .unwrap_or(&current.message),
        )
        .bind(every)
        .bind(update.enabled.unwrap_or(current.enabled))
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            "checkin.update",
            &current.name,
            serde_json::json!({ "id": id }),
        )
        .await?;
        tx.commit().await?;
        self.get_schedule(id).await
    }

    pub async fn delete_schedule(&self, id: Uuid, actor: &str) -> Result<()> {
        let current = self.get_schedule(id).await?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM org_schedules WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        admin_event(
            &mut tx,
            actor,
            "checkin.delete",
            &current.name,
            serde_json::json!({ "id": id }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Make a check-in due now (it fires on the next scheduler pass).
    pub async fn run_schedule_now(&self, id: Uuid) -> Result<()> {
        let updated = sqlx::query("UPDATE org_schedules SET next_run_at = now() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("check-in {id}")));
        }
        Ok(())
    }

    /// Claim the check-ins that are due, and move them to their next run.
    /// Each due check-in is claimed by exactly one node (row locks with
    /// `SKIP LOCKED`). A check-in that was due several times while nothing
    /// ran fires once, then continues from now.
    pub async fn claim_due_schedules(&self) -> Result<Vec<OrgSchedule>> {
        let rows: Vec<ScheduleRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE org_schedules s SET last_run_at = now(),
                 next_run_at = GREATEST(s.next_run_at, now() - make_interval(mins => s.every_minutes))
                               + make_interval(mins => s.every_minutes)
             WHERE s.id IN (SELECT id FROM org_schedules
                            WHERE enabled AND next_run_at <= now()
                            ORDER BY next_run_at LIMIT 50
                            FOR UPDATE SKIP LOCKED)
             RETURNING {}",
            SCHEDULE_COLUMNS
                .split(", ")
                .map(|c| format!("s.{}", c.trim()))
                .collect::<Vec<_>>()
                .join(", ")
        )))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
}
