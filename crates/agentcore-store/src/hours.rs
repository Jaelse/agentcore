//! When departments work: weekly windows in a timezone.

use agentcore_core::{DepartmentHours, DepartmentId, HoursWindow};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use sqlx::types::Json;
use uuid::Uuid;

use crate::{Result, Store, StoreError, admin_event};

/// Most windows a department can have.
const MAX_WINDOWS: usize = 28;

#[derive(Debug, Clone, Deserialize)]
pub struct HoursInput {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_zone")]
    pub timezone: String,
    #[serde(default)]
    pub windows: Vec<HoursWindow>,
}

fn default_true() -> bool {
    true
}

fn default_zone() -> String {
    "UTC".into()
}

#[derive(sqlx::FromRow)]
struct HoursRow {
    department_id: Uuid,
    enabled: bool,
    timezone: String,
    windows: Json<Vec<HoursWindow>>,
    updated_at: DateTime<Utc>,
    updated_by: String,
}

impl From<HoursRow> for DepartmentHours {
    fn from(r: HoursRow) -> Self {
        Self {
            department_id: r.department_id,
            enabled: r.enabled,
            timezone: r.timezone,
            windows: r.windows.0,
            updated_at: r.updated_at,
            updated_by: r.updated_by,
        }
    }
}

const HOURS_COLUMNS: &str = "department_id, enabled, timezone, windows, updated_at, updated_by";

impl Store {
    /// The wall-clock time now in an IANA timezone; the database knows the
    /// zone rules.
    pub async fn local_now(&self, timezone: &str) -> Result<NaiveDateTime> {
        sqlx::query_scalar("SELECT now() AT TIME ZONE $1")
            .bind(timezone)
            .fetch_one(self.pool())
            .await
            .map_err(|_| {
                StoreError::Invalid(format!("`{timezone}` is not a timezone like Europe/Berlin"))
            })
    }

    pub async fn list_hours(&self) -> Result<Vec<DepartmentHours>> {
        let rows: Vec<HoursRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {HOURS_COLUMNS} FROM department_hours ORDER BY department_id"
        )))
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_hours(&self, department: DepartmentId) -> Result<Option<DepartmentHours>> {
        let row: Option<HoursRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {HOURS_COLUMNS} FROM department_hours WHERE department_id = $1"
        )))
        .bind(department)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(Into::into))
    }

    /// Set (or replace) a department's working hours.
    pub async fn set_hours(
        &self,
        department: DepartmentId,
        input: HoursInput,
        actor: &str,
    ) -> Result<DepartmentHours> {
        let timezone = input.timezone.trim().to_string();
        if input.windows.len() > MAX_WINDOWS {
            return Err(StoreError::Invalid(format!(
                "at most {MAX_WINDOWS} working windows"
            )));
        }
        for w in &input.windows {
            w.check().map_err(StoreError::Invalid)?;
        }
        if input.enabled && input.windows.is_empty() {
            return Err(StoreError::Invalid(
                "working hours need a window, or turn them off".into(),
            ));
        }
        self.local_now(&timezone).await?;
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "INSERT INTO department_hours (department_id, enabled, timezone, windows, updated_by)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (department_id) DO UPDATE SET enabled = EXCLUDED.enabled,
                 timezone = EXCLUDED.timezone, windows = EXCLUDED.windows,
                 updated_at = now(), updated_by = EXCLUDED.updated_by",
        )
        .bind(department)
        .bind(input.enabled)
        .bind(&timezone)
        .bind(Json(&input.windows))
        .bind(actor)
        .execute(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_foreign_key_violation() => {
                StoreError::NotFound("department".into())
            }
            _ => StoreError::Db(e),
        })?;
        admin_event(
            &mut tx,
            actor,
            "hours.set",
            &department.to_string(),
            json!({ "enabled": input.enabled, "timezone": timezone, "windows": input.windows }),
        )
        .await?;
        tx.commit().await?;
        self.get_hours(department)
            .await?
            .ok_or_else(|| StoreError::NotFound("working hours".into()))
    }

    pub async fn delete_hours(&self, department: DepartmentId, actor: &str) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        let deleted = sqlx::query("DELETE FROM department_hours WHERE department_id = $1")
            .bind(department)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(StoreError::NotFound("working hours".into()));
        }
        admin_event(
            &mut tx,
            actor,
            "hours.delete",
            &department.to_string(),
            json!({}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Is the department outside its working hours right now. False for a
    /// department without hours.
    pub async fn outside_hours(&self, department: DepartmentId) -> Result<bool> {
        let Some(hours) = self.get_hours(department).await? else {
            return Ok(false);
        };
        if !hours.enabled {
            return Ok(false);
        }
        Ok(!hours.is_open(self.local_now(&hours.timezone).await?))
    }

    /// The working hour starts: pause an active department (marked as paused
    /// by the hours). True when this call paused it.
    pub async fn pause_for_hours(&self, department: DepartmentId) -> Result<bool> {
        Ok(sqlx::query(
            "UPDATE departments SET state = 'paused', updated_at = now(), updated_by = 'hours'
             WHERE id = $1 AND state = 'active'",
        )
        .bind(department)
        .execute(self.pool())
        .await?
        .rows_affected()
            > 0)
    }

    /// Working hours begin: resume a department the hours paused (not one a
    /// person or a budget paused). True when this call resumed it.
    pub async fn resume_for_hours(&self, department: DepartmentId) -> Result<bool> {
        Ok(sqlx::query(
            "UPDATE departments SET state = 'active', updated_at = now(), updated_by = 'hours'
             WHERE id = $1 AND state = 'paused' AND updated_by = 'hours'",
        )
        .bind(department)
        .execute(self.pool())
        .await?
        .rows_affected()
            > 0)
    }
}
