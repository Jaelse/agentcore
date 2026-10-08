//! Agents added from the agent catalogue (shared by every node).

use agentcore_core::AgentSpec;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::types::Json;

use crate::{Result, Store, StoreError, admin_event};

#[derive(Debug, Clone, Serialize)]
pub struct InstalledAgent {
    pub name: String,
    pub catalog: String,
    pub spec: AgentSpec,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

#[derive(sqlx::FromRow)]
struct Row {
    name: String,
    catalog: String,
    spec: Json<AgentSpec>,
    enabled: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    updated_by: String,
}

impl From<Row> for InstalledAgent {
    fn from(r: Row) -> Self {
        Self {
            name: r.name,
            catalog: r.catalog,
            spec: r.spec.0,
            enabled: r.enabled,
            created_at: r.created_at,
            updated_at: r.updated_at,
            updated_by: r.updated_by,
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
}

impl Store {
    pub async fn list_installed_agents(&self) -> Result<Vec<InstalledAgent>> {
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT name, catalog, spec, enabled, created_at, updated_at, updated_by
             FROM installed_agents ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Add an agent; the name must be new.
    pub async fn install_agent(
        &self,
        catalog: &str,
        spec: &AgentSpec,
        actor: &str,
    ) -> Result<InstalledAgent> {
        if !valid_name(&spec.name) {
            return Err(StoreError::Invalid(
                "agent name must be 1-63 characters of a-z, 0-9, - or _".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO installed_agents (name, catalog, spec, updated_by)
             VALUES ($1, $2, $3, $4) ON CONFLICT (name) DO NOTHING",
        )
        .bind(&spec.name)
        .bind(catalog)
        .bind(Json(spec))
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            return Err(StoreError::Conflict(format!("agent `{}`", spec.name)));
        }
        admin_event(
            &mut tx,
            actor,
            "agent.install",
            &spec.name,
            serde_json::json!({
                "catalog": catalog, "provider": spec.provider, "model": spec.model,
                "policy": spec.policy,
            }),
        )
        .await?;
        tx.commit().await?;
        self.installed_agent(&spec.name).await
    }

    pub async fn update_installed_agent(
        &self,
        spec: &AgentSpec,
        enabled: bool,
        actor: &str,
    ) -> Result<InstalledAgent> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE installed_agents SET spec = $2, enabled = $3, updated_at = now(),
                 updated_by = $4
             WHERE name = $1",
        )
        .bind(&spec.name)
        .bind(Json(spec))
        .bind(enabled)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("agent `{}`", spec.name)));
        }
        admin_event(
            &mut tx,
            actor,
            "agent.update",
            &spec.name,
            serde_json::json!({
                "provider": spec.provider, "model": spec.model, "policy": spec.policy,
                "enabled": enabled,
            }),
        )
        .await?;
        tx.commit().await?;
        self.installed_agent(&spec.name).await
    }

    pub async fn uninstall_agent(&self, name: &str, actor: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("DELETE FROM installed_agents WHERE name = $1")
            .bind(name)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("agent `{name}`")));
        }
        admin_event(
            &mut tx,
            actor,
            "agent.uninstall",
            name,
            serde_json::json!({}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn installed_agent(&self, name: &str) -> Result<InstalledAgent> {
        let row: Option<Row> = sqlx::query_as(
            "SELECT name, catalog, spec, enabled, created_at, updated_at, updated_by
             FROM installed_agents WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Into::into)
            .ok_or_else(|| StoreError::NotFound(format!("agent `{name}`")))
    }
}
