//! What model calls cost, and budgets.

use agentcore_core::{Budget, BudgetAction, BudgetPeriod, BudgetStatus, DepartmentId, ModelPrice};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::{Result, Store, StoreError, admin_event};

fn invalid(msg: impl Into<String>) -> StoreError {
    StoreError::Invalid(msg.into())
}

/// The cost of a call in micros, from the most specific price of its
/// provider and model (exact name, then the longest `prefix*`, then `*`).
/// `{p}`, `{m}`, `{i}`, `{o}`: SQL for provider, model, input and output
/// tokens.
pub(crate) fn price_sql(p: &str, m: &str, i: &str, o: &str) -> String {
    format!(
        "(SELECT round(COALESCE({i}, 0) * mp.input_per_mtok + COALESCE({o}, 0) * mp.output_per_mtok)::bigint
            FROM model_prices mp
           WHERE mp.provider = {p}
             AND (mp.model = '*' OR mp.model = {m}
                  OR (mp.model LIKE '%*' AND {m} LIKE replace(replace(replace(replace(mp.model,
                        '\\', '\\\\'), '%', '\\%'), '_', '\\_'), '*', '%')))
           ORDER BY COALESCE(mp.model = {m}, false) DESC, (mp.model = '*') ASC,
                    length(mp.model) DESC
           LIMIT 1)"
    )
}

#[derive(Debug, Clone, Deserialize)]
pub struct PriceInput {
    pub provider: String,
    pub model: String,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BudgetInput {
    #[serde(default)]
    pub department_id: Option<DepartmentId>,
    pub period: BudgetPeriod,
    pub limit_micros: i64,
    pub action: BudgetAction,
    #[serde(default = "default_warn")]
    pub warn_percent: u32,
}

fn default_warn() -> u32 {
    80
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BudgetUpdate {
    pub limit_micros: Option<i64>,
    pub action: Option<BudgetAction>,
    pub warn_percent: Option<u32>,
}

#[derive(sqlx::FromRow)]
struct BudgetRow {
    id: Uuid,
    department_id: Option<Uuid>,
    period: String,
    limit_micros: i64,
    action: String,
    warn_percent: i32,
    updated_at: DateTime<Utc>,
    updated_by: String,
}

impl TryFrom<BudgetRow> for Budget {
    type Error = StoreError;

    fn try_from(r: BudgetRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            department_id: r.department_id,
            period: r.period.parse().map_err(StoreError::Invalid)?,
            limit_micros: r.limit_micros,
            action: r.action.parse().map_err(StoreError::Invalid)?,
            warn_percent: r.warn_percent.clamp(1, 100) as u32,
            updated_at: r.updated_at,
            updated_by: r.updated_by,
        })
    }
}

const BUDGET_COLUMNS: &str =
    "id, department_id, period, limit_micros, action, warn_percent, updated_at, updated_by";

/// Model calls in a scope since a time: `$1` department (NULL = all calls),
/// `$2` since.
const SPENT_SQL: &str = "SELECT COALESCE(sum(c.cost_micros), 0)::bigint FROM model_calls c
     WHERE c.started_at >= $2
       AND ($1::uuid IS NULL OR EXISTS (SELECT 1 FROM sessions s WHERE s.id = c.session_id
                                         AND s.context->>'department_id' = $1::text))";

fn check_budget(limit_micros: i64, warn_percent: u32) -> Result<()> {
    if limit_micros <= 0 {
        return Err(invalid("a budget needs a limit above zero"));
    }
    if !(1..=100).contains(&warn_percent) {
        return Err(invalid("warn at 1 to 100 percent"));
    }
    Ok(())
}

impl Store {
    // ---- prices -----------------------------------------------------------------

    pub async fn list_prices(&self) -> Result<Vec<ModelPrice>> {
        let rows: Vec<(String, String, f64, f64, DateTime<Utc>, String)> = sqlx::query_as(
            "SELECT provider, model, input_per_mtok::float8, output_per_mtok::float8,
                 updated_at, updated_by
             FROM model_prices ORDER BY provider, model",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(provider, model, input_per_mtok, output_per_mtok, updated_at, updated_by)| {
                    ModelPrice {
                        provider,
                        model,
                        input_per_mtok,
                        output_per_mtok,
                        updated_at,
                        updated_by,
                    }
                },
            )
            .collect())
    }

    pub async fn set_price(&self, input: PriceInput, actor: &str) -> Result<ModelPrice> {
        let model = input.model.trim();
        if model.is_empty() || model.len() > 200 || model[..model.len() - 1].contains('*') {
            return Err(invalid(
                "a model is an exact name, a prefix ending in `*`, or `*`",
            ));
        }
        for v in [input.input_per_mtok, input.output_per_mtok] {
            if !v.is_finite() || !(0.0..=1_000_000.0).contains(&v) {
                return Err(invalid("prices are per million tokens, 0 or more"));
            }
        }
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "INSERT INTO model_prices (provider, model, input_per_mtok, output_per_mtok, updated_by)
             VALUES ($1, $2, $3::float8::numeric, $4::float8::numeric, $5)
             ON CONFLICT (provider, model) DO UPDATE SET
                 input_per_mtok = EXCLUDED.input_per_mtok,
                 output_per_mtok = EXCLUDED.output_per_mtok,
                 updated_at = now(), updated_by = EXCLUDED.updated_by",
        )
        .bind(&input.provider)
        .bind(model)
        .bind(input.input_per_mtok)
        .bind(input.output_per_mtok)
        .bind(actor)
        .execute(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_foreign_key_violation() => {
                StoreError::NotFound(format!("model provider `{}`", input.provider))
            }
            _ => StoreError::Db(e),
        })?;
        admin_event(
            &mut tx,
            actor,
            "price.set",
            &format!("{}/{model}", input.provider),
            json!({ "input_per_mtok": input.input_per_mtok, "output_per_mtok": input.output_per_mtok }),
        )
        .await?;
        tx.commit().await?;
        self.list_prices()
            .await?
            .into_iter()
            .find(|p| p.provider == input.provider && p.model == model)
            .ok_or_else(|| StoreError::NotFound("price".into()))
    }

    pub async fn delete_price(&self, provider: &str, model: &str, actor: &str) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        let deleted = sqlx::query("DELETE FROM model_prices WHERE provider = $1 AND model = $2")
            .bind(provider)
            .bind(model)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!(
                "price for {provider}/{model}"
            )));
        }
        admin_event(
            &mut tx,
            actor,
            "price.delete",
            &format!("{provider}/{model}"),
            json!({}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Price the calls that were recorded before a price was known. Returns
    /// how many were priced.
    pub async fn price_unpriced_calls(&self, actor: &str) -> Result<u64> {
        let mut tx = self.pool().begin().await?;
        let updated = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE model_calls c SET cost_micros = {}
             WHERE c.cost_micros IS NULL
               AND (c.input_tokens IS NOT NULL OR c.output_tokens IS NOT NULL)",
            price_sql("c.provider", "c.model", "c.input_tokens", "c.output_tokens")
        )))
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            "price.backfill",
            "model_calls",
            json!({ "priced": updated.rows_affected() }),
        )
        .await?;
        tx.commit().await?;
        Ok(updated.rows_affected())
    }

    /// Calls with tokens but no price since a time.
    pub async fn unpriced_calls(&self, since: DateTime<Utc>) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM model_calls WHERE started_at >= $1 AND cost_micros IS NULL
               AND (input_tokens IS NOT NULL OR output_tokens IS NOT NULL)",
        )
        .bind(since)
        .fetch_one(self.pool())
        .await?)
    }

    pub async fn currency(&self) -> Result<String> {
        Ok(sqlx::query_scalar("SELECT currency FROM org_settings")
            .fetch_one(self.pool())
            .await?)
    }

    pub async fn set_currency(&self, currency: &str, actor: &str) -> Result<String> {
        let currency = currency.trim().to_ascii_uppercase();
        if currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_uppercase()) {
            return Err(invalid("a currency is a three-letter code like USD or EUR"));
        }
        let mut tx = self.pool().begin().await?;
        sqlx::query("UPDATE org_settings SET currency = $1")
            .bind(&currency)
            .execute(&mut *tx)
            .await?;
        admin_event(&mut tx, actor, "org.currency", &currency, json!({})).await?;
        tx.commit().await?;
        Ok(currency)
    }

    /// What model calls in a scope cost since a time (micros).
    pub async fn spent(
        &self,
        department: Option<DepartmentId>,
        since: DateTime<Utc>,
    ) -> Result<i64> {
        Ok(sqlx::query_scalar(SPENT_SQL)
            .bind(department)
            .bind(since)
            .fetch_one(self.pool())
            .await?)
    }

    // ---- budgets ----------------------------------------------------------------

    pub async fn get_budget(&self, id: Uuid) -> Result<Budget> {
        let row: Option<BudgetRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {BUDGET_COLUMNS} FROM budgets WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.ok_or_else(|| StoreError::NotFound(format!("budget {id}")))?
            .try_into()
    }

    /// Create a budget, or replace the one with the same scope and period.
    pub async fn set_budget(&self, input: BudgetInput, actor: &str) -> Result<Budget> {
        check_budget(input.limit_micros, input.warn_percent)?;
        let mut tx = self.pool().begin().await?;
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO budgets (id, department_id, period, limit_micros, action, warn_percent,
                 updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (COALESCE(department_id, '00000000-0000-0000-0000-000000000000'::uuid), period)
             DO UPDATE SET limit_micros = EXCLUDED.limit_micros, action = EXCLUDED.action,
                 warn_percent = EXCLUDED.warn_percent, warned_period = NULL,
                 updated_at = now(), updated_by = EXCLUDED.updated_by
             RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(input.department_id)
        .bind(input.period.as_str())
        .bind(input.limit_micros)
        .bind(input.action.as_str())
        .bind(input.warn_percent as i32)
        .bind(actor)
        .fetch_one(&mut *tx)
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
            "budget.set",
            &id.to_string(),
            json!({
                "department": input.department_id, "period": input.period.as_str(),
                "limit_micros": input.limit_micros, "action": input.action.as_str(),
            }),
        )
        .await?;
        tx.commit().await?;
        self.get_budget(id).await
    }

    pub async fn update_budget(
        &self,
        id: Uuid,
        update: BudgetUpdate,
        actor: &str,
    ) -> Result<Budget> {
        let current = self.get_budget(id).await?;
        let limit = update.limit_micros.unwrap_or(current.limit_micros);
        let warn = update.warn_percent.unwrap_or(current.warn_percent);
        check_budget(limit, warn)?;
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "UPDATE budgets SET limit_micros = $2, action = $3, warn_percent = $4,
                 warned_period = NULL, updated_at = now(), updated_by = $5
             WHERE id = $1",
        )
        .bind(id)
        .bind(limit)
        .bind(update.action.unwrap_or(current.action).as_str())
        .bind(warn as i32)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            "budget.update",
            &id.to_string(),
            json!({ "limit_micros": limit }),
        )
        .await?;
        tx.commit().await?;
        self.get_budget(id).await
    }

    pub async fn delete_budget(&self, id: Uuid, actor: &str) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        let deleted = sqlx::query("DELETE FROM budgets WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("budget {id}")));
        }
        admin_event(&mut tx, actor, "budget.delete", &id.to_string(), json!({})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Every budget with the current period and what it used.
    pub async fn budget_statuses(&self) -> Result<Vec<BudgetStatus>> {
        #[derive(sqlx::FromRow)]
        struct Row {
            #[sqlx(flatten)]
            budget: BudgetRow,
            period_start: DateTime<Utc>,
            period_end: DateTime<Utc>,
            spent_micros: i64,
            paused_departments: Vec<Uuid>,
            exhausted_period: Option<DateTime<Utc>>,
        }
        let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "WITH b AS (
                 SELECT {BUDGET_COLUMNS}, paused_departments, exhausted_period,
                     date_trunc(period, now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' AS period_start
                 FROM budgets
             )
             SELECT b.*, b.period_start + ('1 ' || b.period)::interval AS period_end,
                 (SELECT COALESCE(sum(c.cost_micros), 0)::bigint FROM model_calls c
                   WHERE c.started_at >= b.period_start
                     AND (b.department_id IS NULL OR EXISTS (
                          SELECT 1 FROM sessions s WHERE s.id = c.session_id
                             AND s.context->>'department_id' = b.department_id::text))
                 ) AS spent_micros
             FROM b ORDER BY b.department_id NULLS FIRST, b.period"
        )))
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(BudgetStatus {
                    budget: r.budget.try_into()?,
                    period_start: r.period_start,
                    period_end: r.period_end,
                    spent_micros: r.spent_micros,
                    paused_departments: r.paused_departments,
                    exhausted_period: r.exhausted_period,
                })
            })
            .collect()
    }

    /// Mark that people were warned in this period (exactly one caller wins).
    pub async fn claim_budget_warning(&self, id: Uuid, period: DateTime<Utc>) -> Result<bool> {
        let updated = sqlx::query(
            "UPDATE budgets SET warned_period = $2
             WHERE id = $1 AND warned_period IS DISTINCT FROM $2",
        )
        .bind(id)
        .bind(period)
        .execute(self.pool())
        .await?;
        Ok(updated.rows_affected() > 0)
    }

    /// The budget ran out in this period: pause the active departments in its
    /// scope and remember them. Returns them (empty when another node did it).
    pub async fn exhaust_budget(
        &self,
        budget: &Budget,
        period: DateTime<Utc>,
    ) -> Result<Option<Vec<DepartmentId>>> {
        let mut tx = self.pool().begin().await?;
        let claimed = sqlx::query(
            "UPDATE budgets SET exhausted_period = $2
             WHERE id = $1 AND exhausted_period IS DISTINCT FROM $2",
        )
        .bind(budget.id)
        .bind(period)
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() == 0 {
            return Ok(None);
        }
        let mut paused = Vec::new();
        if budget.action == BudgetAction::Pause {
            paused = sqlx::query_scalar(
                "UPDATE departments SET state = 'paused', updated_at = now(), updated_by = 'budget'
                 WHERE ($1::uuid IS NULL OR id = $1) AND state = 'active' RETURNING id",
            )
            .bind(budget.department_id)
            .fetch_all(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE budgets SET paused_departments = ARRAY(
                     SELECT DISTINCT unnest(paused_departments || $2::uuid[]))
                 WHERE id = $1",
            )
            .bind(budget.id)
            .bind(&paused)
            .execute(&mut *tx)
            .await?;
        }
        admin_event(
            &mut tx,
            "budget",
            "budget.exhausted",
            &budget.id.to_string(),
            json!({ "paused": paused, "period": period }),
        )
        .await?;
        tx.commit().await?;
        Ok(Some(paused))
    }

    /// A new period started (or the budget now has room): resume the
    /// departments this budget paused that are still paused by it. Returns
    /// them (None when another node did it).
    pub async fn release_budget(&self, id: Uuid) -> Result<Option<Vec<DepartmentId>>> {
        let mut tx = self.pool().begin().await?;
        let row: Option<(Option<DateTime<Utc>>, Vec<Uuid>)> = sqlx::query_as(
            "SELECT exhausted_period, paused_departments FROM budgets WHERE id = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((Some(_), paused)) = row else {
            return Ok(None);
        };
        sqlx::query(
            "UPDATE budgets SET exhausted_period = NULL, paused_departments = '{}' WHERE id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        let resumed: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE departments SET state = 'active', updated_at = now(), updated_by = 'budget'
             WHERE id = ANY($1) AND state = 'paused' AND updated_by = 'budget' RETURNING id",
        )
        .bind(&paused)
        .fetch_all(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            "budget",
            "budget.released",
            &id.to_string(),
            json!({ "resumed": resumed }),
        )
        .await?;
        tx.commit().await?;
        Ok(Some(resumed))
    }
}
