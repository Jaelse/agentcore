//! Work items: the work a department does on its repository, and the leases
//! sessions hold on them. The department owns the work (branch, last
//! checkpoint, pull request); a session borrows it, and when the session
//! ends, its node restarts or its node is lost, the lease is released and
//! any worker can carry the item on from its last checkpoint.

use agentcore_core::{DepartmentId, OrgAgentId, SessionId, WorkItem, WorkItemState, WorkLease};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{Result, Store, StoreError, admin_event};

const COLUMNS: &str = "id, department_id, project_id, title, branch, base_branch, head_commit, \
    checkpoint_at, state, assignee, lease_id, lease_session, lease_node, lease_generation, \
    leased_at, pull_request_number, pull_request_url, note, created_by, created_at, updated_at, \
    closed_at, closed_by";

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    department_id: Uuid,
    project_id: Uuid,
    title: String,
    branch: String,
    base_branch: String,
    head_commit: Option<String>,
    checkpoint_at: Option<DateTime<Utc>>,
    state: String,
    assignee: Option<Uuid>,
    lease_id: Option<Uuid>,
    lease_session: Option<Uuid>,
    lease_node: Option<String>,
    lease_generation: i64,
    leased_at: Option<DateTime<Utc>>,
    pull_request_number: Option<i64>,
    pull_request_url: Option<String>,
    note: Option<String>,
    created_by: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    closed_at: Option<DateTime<Utc>>,
    closed_by: Option<String>,
}

impl From<Row> for WorkItem {
    fn from(r: Row) -> Self {
        let lease = match (r.lease_id, r.lease_node) {
            (Some(id), Some(node)) => Some(WorkLease {
                id,
                session_id: r.lease_session,
                node,
                generation: r.lease_generation,
                since: r.leased_at.unwrap_or(r.updated_at),
            }),
            _ => None,
        };
        Self {
            id: r.id,
            department_id: r.department_id,
            project_id: r.project_id,
            title: r.title,
            branch: r.branch,
            base_branch: r.base_branch,
            head_commit: r.head_commit,
            checkpoint_at: r.checkpoint_at,
            state: r.state.parse().unwrap_or(WorkItemState::Open),
            assignee: r.assignee,
            lease,
            pull_request_number: r.pull_request_number.map(|n| n as u64),
            pull_request_url: r.pull_request_url,
            note: r.note,
            created_by: r.created_by,
            created_at: r.created_at,
            updated_at: r.updated_at,
            closed_at: r.closed_at,
            closed_by: r.closed_by,
        }
    }
}

/// A new work item, created when a worker has none to carry on.
#[derive(Debug, Clone)]
pub struct NewWorkItem {
    pub department_id: DepartmentId,
    pub project_id: Uuid,
    pub title: String,
    pub branch: String,
    pub base_branch: String,
}

/// What a worker got when its session started.
#[derive(Debug, Clone)]
pub struct Acquired {
    pub item: WorkItem,
    /// It carries on earlier work (rather than being new).
    pub resumed: bool,
}

impl Store {
    /// Take the lease on the work item a worker carries on in `project`: its
    /// most recently updated open item nobody holds, else a new one. `lease`
    /// is the session's lease id until the session exists (see
    /// [`Store::bind_work_lease`]). Fails if the worker's items are all held.
    pub async fn acquire_work_item(
        &self,
        agent: OrgAgentId,
        node: &str,
        lease: Uuid,
        new: NewWorkItem,
    ) -> Result<Acquired> {
        let mut tx = self.pool.begin().await?;
        let found: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM work_items
             WHERE assignee = $1 AND project_id = $2 AND department_id = $3
               AND state = 'open' AND lease_id IS NULL
             ORDER BY updated_at DESC LIMIT 1
             FOR UPDATE SKIP LOCKED",
        )
        .bind(agent)
        .bind(new.project_id)
        .bind(new.department_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (row, resumed): (Row, bool) = match found {
            Some((id,)) => (
                sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "UPDATE work_items SET lease_id = $2, lease_node = $3, lease_session = NULL,
                         lease_generation = lease_generation + 1, leased_at = now(),
                         updated_at = now()
                     WHERE id = $1 RETURNING {COLUMNS}"
                )))
                .bind(id)
                .bind(lease)
                .bind(node)
                .fetch_one(&mut *tx)
                .await?,
                true,
            ),
            None => (
                sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "INSERT INTO work_items (id, department_id, project_id, title, branch,
                         base_branch, assignee, lease_id, lease_node, lease_generation, leased_at,
                         created_by)
                     SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, 1, now(), a.name
                     FROM org_agents a WHERE a.id = $7
                     RETURNING {COLUMNS}"
                )))
                .bind(Uuid::now_v7())
                .bind(new.department_id)
                .bind(new.project_id)
                .bind(&new.title)
                .bind(&new.branch)
                .bind(&new.base_branch)
                .bind(agent)
                .bind(lease)
                .bind(node)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| StoreError::NotFound(format!("agent {agent}")))?,
                false,
            ),
        };
        tx.commit().await?;
        Ok(Acquired {
            item: row.into(),
            resumed,
        })
    }

    /// Record the session that holds a lease, once it exists.
    pub async fn bind_work_lease(&self, lease: Uuid, session: SessionId) -> Result<bool> {
        let done = sqlx::query(
            "UPDATE work_items SET lease_session = $2, updated_at = now() WHERE lease_id = $1",
        )
        .bind(lease)
        .bind(session)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Give up a lease that never got a session (the session could not start).
    pub async fn drop_work_lease(&self, lease: Uuid) -> Result<()> {
        sqlx::query(
            "UPDATE work_items SET lease_id = NULL, lease_node = NULL, lease_session = NULL,
                 leased_at = NULL, updated_at = now()
             WHERE lease_id = $1",
        )
        .bind(lease)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_work_item(&self, id: Uuid) -> Result<WorkItem> {
        let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM work_items WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Into::into)
            .ok_or_else(|| StoreError::NotFound(format!("work item {id}")))
    }

    /// The work item a session holds, if any.
    pub async fn work_item_of_session(&self, session: SessionId) -> Result<Option<WorkItem>> {
        let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM work_items WHERE lease_session = $1"
        )))
        .bind(session)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Whether the worker has open work to carry on.
    pub async fn has_open_work_items(&self, agent: OrgAgentId) -> Result<bool> {
        let (open,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM work_items WHERE assignee = $1 AND state = 'open')",
        )
        .bind(agent)
        .fetch_one(&self.pool)
        .await?;
        Ok(open)
    }

    /// A department's work items (all departments with `None`), open ones
    /// first, most recently updated first.
    pub async fn list_work_items(
        &self,
        department: Option<DepartmentId>,
        include_closed: bool,
    ) -> Result<Vec<WorkItem>> {
        let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM work_items
             WHERE ($1::uuid IS NULL OR department_id = $1) AND ($2 OR state = 'open')
             ORDER BY state = 'closed', updated_at DESC
             LIMIT 500"
        )))
        .bind(department)
        .bind(include_closed)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Record a checkpoint pushed by the lease holder. False if the lease
    /// (generation) is no longer the item's: the push must not count.
    pub async fn checkpoint_work_item(
        &self,
        id: Uuid,
        generation: i64,
        head: &str,
    ) -> Result<bool> {
        let done = sqlx::query(
            "UPDATE work_items SET head_commit = $3, checkpoint_at = now(), updated_at = now()
             WHERE id = $1 AND lease_generation = $2 AND lease_id IS NOT NULL",
        )
        .bind(id)
        .bind(generation)
        .bind(head)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// The holder delivered the item as a pull request.
    pub async fn work_item_delivered(
        &self,
        id: Uuid,
        generation: i64,
        head: &str,
        pr_number: u64,
        pr_url: &str,
    ) -> Result<bool> {
        let done = sqlx::query(
            "UPDATE work_items SET head_commit = $3, checkpoint_at = now(),
                 pull_request_number = $4, pull_request_url = $5, updated_at = now()
             WHERE id = $1 AND lease_generation = $2 AND lease_id IS NOT NULL",
        )
        .bind(id)
        .bind(generation)
        .bind(head)
        .bind(pr_number as i64)
        .bind(pr_url)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// The holder renamed its item.
    pub async fn rename_work_item(&self, id: Uuid, generation: i64, title: &str) -> Result<bool> {
        let title = check_title(title)?;
        let done = sqlx::query(
            "UPDATE work_items SET title = $3, updated_at = now()
             WHERE id = $1 AND lease_generation = $2 AND lease_id IS NOT NULL",
        )
        .bind(id)
        .bind(generation)
        .bind(title)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// The session ended: its item stays with its worker, free to carry on.
    pub async fn release_work_item(
        &self,
        session: SessionId,
        note: &str,
    ) -> Result<Option<WorkItem>> {
        let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE work_items SET lease_id = NULL, lease_node = NULL, lease_session = NULL,
                 leased_at = NULL, note = $2, updated_at = now()
             WHERE lease_session = $1
             RETURNING {COLUMNS}"
        )))
        .bind(session)
        .bind(note)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Release the leases held on nodes whose heartbeat is older than the
    /// timeout: their sessions are gone with the node. Returns the items.
    pub async fn expire_work_leases_on_dead_nodes(
        &self,
        node_timeout_secs: u64,
    ) -> Result<Vec<WorkItem>> {
        let columns: String = COLUMNS
            .split(", ")
            .map(|c| format!("w.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE work_items w SET lease_id = NULL, lease_session = NULL, leased_at = NULL,
                 note = 'its session was lost with node ' || w.lease_node, lease_node = NULL,
                 updated_at = now()
             FROM nodes n
             WHERE w.lease_node = n.name AND w.lease_id IS NOT NULL
               AND n.last_seen < now() - make_interval(secs => $1)
             RETURNING {columns}"
        )))
        .bind(node_timeout_secs as f64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Hand an item to another worker of its department; it carries the item
    /// on in its next session. Refused while a session holds it.
    pub async fn assign_work_item(
        &self,
        id: Uuid,
        agent: OrgAgentId,
        by: &str,
    ) -> Result<WorkItem> {
        let mut tx = self.pool.begin().await?;
        let item: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM work_items WHERE id = $1 FOR UPDATE"
        )))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let item: WorkItem = item
            .ok_or_else(|| StoreError::NotFound(format!("work item {id}")))?
            .into();
        if item.state == WorkItemState::Closed {
            return Err(StoreError::Invalid("the work item is closed".into()));
        }
        if item.lease.is_some() && item.assignee != Some(agent) {
            return Err(StoreError::Busy(
                "a session is working on this item; it can be handed over once that session \
                 ends"
                    .into(),
            ));
        }
        let member: Option<(String,)> = sqlx::query_as(
            "SELECT name FROM org_agents
             WHERE id = $1 AND department_id = $2 AND kind = 'worker'",
        )
        .bind(agent)
        .bind(item.department_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((name,)) = member else {
            return Err(StoreError::Invalid(
                "work items go to workers of the item's own department".into(),
            ));
        };
        let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE work_items SET assignee = $2, note = $3, updated_at = now()
             WHERE id = $1 RETURNING {COLUMNS}"
        )))
        .bind(id)
        .bind(agent)
        .bind(format!("assigned to {name} by {by}"))
        .fetch_one(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            by,
            "work_item.assign",
            &item.title,
            serde_json::json!({ "id": id, "agent": agent }),
        )
        .await?;
        tx.commit().await?;
        Ok(row.into())
    }

    /// Close an item: nobody carries it on any more (its branch and pull
    /// request stay on GitHub). The holder (`generation`) may close its own
    /// item; others only one nobody holds.
    pub async fn close_work_item(
        &self,
        id: Uuid,
        generation: Option<i64>,
        note: &str,
        by: &str,
    ) -> Result<WorkItem> {
        let mut tx = self.pool.begin().await?;
        let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM work_items WHERE id = $1 FOR UPDATE"
        )))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let item: WorkItem = row
            .ok_or_else(|| StoreError::NotFound(format!("work item {id}")))?
            .into();
        if item.state == WorkItemState::Closed {
            return Ok(item);
        }
        match (&item.lease, generation) {
            (Some(lease), Some(g)) if lease.generation == g => {}
            (Some(_), _) => {
                return Err(StoreError::Busy(
                    "a session is working on this item; stop that session first".into(),
                ));
            }
            (None, _) => {}
        }
        let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE work_items SET state = 'closed', closed_at = now(), closed_by = $2,
                 note = $3, lease_id = NULL, lease_node = NULL, lease_session = NULL,
                 leased_at = NULL, updated_at = now()
             WHERE id = $1 RETURNING {COLUMNS}"
        )))
        .bind(id)
        .bind(by)
        .bind(note)
        .fetch_one(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            by,
            "work_item.close",
            &item.title,
            serde_json::json!({ "id": id, "note": note }),
        )
        .await?;
        tx.commit().await?;
        Ok(row.into())
    }
}

fn check_title(title: &str) -> Result<&str> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(StoreError::Invalid(
            "a work item title has 1 to 200 characters".into(),
        ));
    }
    Ok(title)
}
