//! Handoffs: work one department hands to another. A handoff waits for the
//! receiving department (however long it is paused, asleep or outside its
//! hours) until one of its workers takes it, and is closed with a result.

use agentcore_core::{DepartmentId, Handoff, HandoffStatus, OrgAgentId};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{Result, Store, StoreError};

const COLUMNS: &str = "id, from_department, from_agent, from_name, to_department, title, spec, \
    status, message_id, taken_by, taken_by_name, taken_at, result, closed_at, created_at";

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    from_department: Uuid,
    from_agent: Option<Uuid>,
    from_name: String,
    to_department: Uuid,
    title: String,
    spec: String,
    status: String,
    message_id: Option<Uuid>,
    taken_by: Option<Uuid>,
    taken_by_name: Option<String>,
    taken_at: Option<DateTime<Utc>>,
    result: String,
    closed_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl From<Row> for Handoff {
    fn from(r: Row) -> Self {
        Self {
            id: r.id,
            from_department: r.from_department,
            from_agent: r.from_agent,
            from_name: r.from_name,
            to_department: r.to_department,
            title: r.title,
            spec: r.spec,
            status: r.status.parse().unwrap_or(HandoffStatus::Waiting),
            message_id: r.message_id,
            taken_by: r.taken_by,
            taken_by_name: r.taken_by_name,
            taken_at: r.taken_at,
            result: r.result,
            closed_at: r.closed_at,
            created_at: r.created_at,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewHandoff {
    pub id: Uuid,
    pub from_department: DepartmentId,
    pub from_agent: Option<OrgAgentId>,
    pub from_name: String,
    pub to_department: DepartmentId,
    pub title: String,
    pub spec: String,
    /// `Held` or `Waiting`, as the router decided about its notice.
    pub status: HandoffStatus,
    pub message_id: Uuid,
}

impl Store {
    pub async fn insert_handoff(&self, new: NewHandoff) -> Result<Handoff> {
        let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "INSERT INTO handoffs (id, from_department, from_agent, from_name, to_department,
                 title, spec, status, message_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             RETURNING {COLUMNS}"
        )))
        .bind(new.id)
        .bind(new.from_department)
        .bind(new.from_agent)
        .bind(&new.from_name)
        .bind(new.to_department)
        .bind(&new.title)
        .bind(&new.spec)
        .bind(new.status.as_str())
        .bind(new.message_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.into())
    }

    pub async fn get_handoff(&self, id: Uuid) -> Result<Handoff> {
        let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM handoffs WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Into::into)
            .ok_or_else(|| StoreError::NotFound(format!("handoff {id}")))
    }

    /// Handoffs a department sent or received (all of them without one),
    /// newest first.
    pub async fn list_handoffs(
        &self,
        department: Option<DepartmentId>,
        open_only: bool,
    ) -> Result<Vec<Handoff>> {
        let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM handoffs
             WHERE ($1::uuid IS NULL OR from_department = $1 OR to_department = $1)
               AND (NOT $2 OR status IN ('held', 'waiting', 'taken'))
             ORDER BY created_at DESC LIMIT 200"
        )))
        .bind(department)
        .bind(open_only)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// The router released (or refused) the held notice of a handoff.
    /// `None` when the message carries no held handoff.
    pub async fn release_handoff(
        &self,
        message: Uuid,
        delivered: bool,
        decision: &str,
    ) -> Result<Option<Handoff>> {
        let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE handoffs
             SET status = CASE WHEN $2 THEN 'waiting' ELSE 'cancelled' END,
                 result = CASE WHEN $2 THEN result ELSE $3 END,
                 closed_at = CASE WHEN $2 THEN NULL ELSE now() END
             WHERE message_id = $1 AND status = 'held'
             RETURNING {COLUMNS}"
        )))
        .bind(message)
        .bind(delivered)
        .bind(decision)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// A worker of the receiving department takes a handoff (also one a
    /// colleague took). `None` when it is not waiting or taken, or is not
    /// for `department`.
    pub async fn take_handoff(
        &self,
        id: Uuid,
        department: DepartmentId,
        agent: OrgAgentId,
        agent_name: &str,
    ) -> Result<Option<Handoff>> {
        let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE handoffs
             SET status = 'taken', taken_by = $3, taken_by_name = $4, taken_at = now()
             WHERE id = $1 AND to_department = $2 AND status IN ('waiting', 'taken')
             RETURNING {COLUMNS}"
        )))
        .bind(id)
        .bind(department)
        .bind(agent)
        .bind(agent_name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Close an open handoff as done, declined or cancelled. With
    /// `department`, only a handoff for that department (its receivers).
    /// `None` when it is not open (any more).
    pub async fn close_handoff(
        &self,
        id: Uuid,
        department: Option<DepartmentId>,
        status: HandoffStatus,
        result: &str,
    ) -> Result<Option<Handoff>> {
        if status.is_open() {
            return Err(StoreError::Invalid(format!(
                "a handoff cannot be closed as {}",
                status.as_str()
            )));
        }
        let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE handoffs SET status = $3, result = $4, closed_at = now()
             WHERE id = $1 AND ($2::uuid IS NULL OR to_department = $2)
               AND status IN ('held', 'waiting', 'taken')
             RETURNING {COLUMNS}"
        )))
        .bind(id)
        .bind(department)
        .bind(status.as_str())
        .bind(result)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }
}
