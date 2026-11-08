//! What the log export reads: the operational logs nodes persist, and the
//! other records that tell what happened, by time window.

use agentcore_core::SessionId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::types::Json;
use uuid::Uuid;

use crate::{AdminEvent, Result, SessionRecord, SessionRow, Store};

/// One record of a node's operational (`tracing`) log.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct NodeLog {
    pub id: Uuid,
    pub at: DateTime<Utc>,
    pub node: String,
    pub level: String,
    pub target: String,
    pub message: String,
    pub fields: Json<serde_json::Value>,
}

/// A node of the cluster, as it last reported.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct NodeInfo {
    pub name: String,
    pub internal_url: String,
    pub capacity: i32,
    pub version: String,
    pub started_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// Seen within the node timeout.
    pub alive: bool,
}

/// One row of the organisation's activity log.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ActivityRecord {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub department_id: Option<Uuid>,
    pub agent_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub kind: String,
    pub value: Option<i64>,
    pub detail: Option<String>,
}

impl Store {
    /// Keep warnings and errors a node logged (ids make it idempotent).
    pub async fn save_node_logs(&self, logs: &[NodeLog]) -> Result<()> {
        if logs.is_empty() {
            return Ok(());
        }
        let mut ids = Vec::with_capacity(logs.len());
        let mut at = Vec::with_capacity(logs.len());
        let mut node = Vec::with_capacity(logs.len());
        let mut level = Vec::with_capacity(logs.len());
        let mut target = Vec::with_capacity(logs.len());
        let mut message = Vec::with_capacity(logs.len());
        let mut fields = Vec::with_capacity(logs.len());
        for log in logs {
            ids.push(log.id);
            at.push(log.at);
            node.push(log.node.clone());
            level.push(log.level.clone());
            target.push(log.target.clone());
            message.push(log.message.clone());
            fields.push(log.fields.0.clone());
        }
        sqlx::query(
            "INSERT INTO node_logs (id, at, node, level, target, message, fields)
             SELECT * FROM UNNEST($1::uuid[], $2::timestamptz[], $3::text[], $4::text[],
                                  $5::text[], $6::text[], $7::jsonb[])
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(ids)
        .bind(at)
        .bind(node)
        .bind(level)
        .bind(target)
        .bind(message)
        .bind(fields)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persisted node logs in a time window, oldest first.
    pub async fn node_logs(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<NodeLog>> {
        Ok(sqlx::query_as(
            "SELECT * FROM (SELECT * FROM node_logs WHERE at >= $1 AND at <= $2
                            ORDER BY at DESC LIMIT $3) l ORDER BY at",
        )
        .bind(since)
        .bind(until)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Forget node logs older than `before`. Returns how many were removed.
    pub async fn prune_node_logs(&self, before: DateTime<Utc>) -> Result<u64> {
        Ok(sqlx::query("DELETE FROM node_logs WHERE at < $1")
            .bind(before)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// The nodes of the cluster.
    pub async fn nodes(&self, node_timeout_secs: u64) -> Result<Vec<NodeInfo>> {
        Ok(sqlx::query_as(
            "SELECT name, internal_url, capacity, version, started_at, last_seen,
                    last_seen > now() - make_interval(secs => $1) AS alive
             FROM nodes ORDER BY name",
        )
        .bind(node_timeout_secs as f64)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Sessions (archived ones too) that ran at some point in a time window,
    /// newest first; or just `only`, if given.
    pub async fn sessions_between(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        only: Option<SessionId>,
        limit: i64,
    ) -> Result<Vec<SessionRecord>> {
        let rows: Vec<SessionRow> = sqlx::query_as(
            "SELECT id, agent, task, policy, policy_digest, status, created_by, created_at,
                    ended_at, actions, model_calls, audit_path, context, node, resume_spec
             FROM sessions
             WHERE CASE WHEN $3::uuid IS NULL
                        THEN created_at <= $2 AND (ended_at IS NULL OR ended_at >= $1)
                        ELSE id = $3 END
             ORDER BY created_at DESC LIMIT $4",
        )
        .bind(since)
        .bind(until)
        .bind(only)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Configuration changes in a time window, oldest first.
    pub async fn admin_events_between(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<AdminEvent>> {
        Ok(sqlx::query_as(
            "SELECT * FROM (SELECT * FROM admin_events WHERE at >= $1 AND at <= $2
                            ORDER BY id DESC LIMIT $3) e ORDER BY id",
        )
        .bind(since)
        .bind(until)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Organisation activity in a time window, oldest first.
    pub async fn activity_between(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        only: Option<SessionId>,
        limit: i64,
    ) -> Result<Vec<ActivityRecord>> {
        Ok(sqlx::query_as(
            "SELECT * FROM (
                 SELECT id, at, department_id, agent_id, session_id, kind, value, detail
                 FROM org_activity
                 WHERE at >= $1 AND at <= $2 AND ($3::uuid IS NULL OR session_id = $3)
                 ORDER BY id DESC LIMIT $4) a
             ORDER BY id",
        )
        .bind(since)
        .bind(until)
        .bind(only)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Record that someone exported the logs.
    pub async fn record_log_export(&self, actor: &str, details: serde_json::Value) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        crate::admin_event(&mut tx, actor, "logs.export", "logs", details).await?;
        tx.commit().await?;
        Ok(())
    }
}
