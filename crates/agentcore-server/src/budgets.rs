//! Spending: prices, budgets that warn or pause, and blocking model calls
//! once a pausing budget is used up.
//!
//! Every model call is priced when it is recorded. Each node checks the
//! budgets on its heartbeat: crossing the warning level tells people;
//! reaching the limit pauses the departments in the budget's scope (for a
//! `pause` budget) and the model gateway refuses their calls. When the next
//! period starts, or people raise the limit, the departments the budget
//! paused are resumed.

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use agentcore_core::{BudgetStatus, DepartmentId};
use agentcore_store::{BudgetInput, BudgetUpdate, PriceInput};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::AppState;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::org::notify;

type ApiResult<T> = Result<T, ApiError>;

/// How long budget usage is cached for the model gateway.
const CACHE_TTL: Duration = Duration::from_secs(5);

/// Budget usage and when it was loaded.
type Cached = Option<(Instant, Vec<BudgetStatus>)>;

static CACHE: LazyLock<Mutex<Cached>> = LazyLock::new(Default::default);

fn forget() {
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

async fn statuses(state: &AppState) -> Vec<BudgetStatus> {
    if let Some((at, cached)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        && at.elapsed() < CACHE_TTL
    {
        return cached.clone();
    }
    match state.store.budget_statuses().await {
        Ok(fresh) => {
            *CACHE.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((Instant::now(), fresh.clone()));
            fresh
        }
        Err(err) => {
            tracing::warn!(error = %err, "could not load budgets");
            Vec::new()
        }
    }
}

/// Money for people and agents: micros → "12.34 USD".
pub fn money(micros: i64, currency: &str) -> String {
    format!("{:.2} {currency}", micros as f64 / 1e6)
}

/// Why a model call of work in this department must be refused, if it must.
pub async fn blocked(state: &AppState, department: Option<DepartmentId>) -> Option<String> {
    let all = statuses(state).await;
    let hit = all.iter().find(|b| b.blocks() && b.covers(department))?;
    let currency = state
        .store
        .currency()
        .await
        .unwrap_or_else(|_| "USD".into());
    let scope = if hit.budget.department_id.is_some() {
        "the department's"
    } else {
        "the organisation's"
    };
    Some(format!(
        "{scope} {} budget of {} is used up ({} spent); model calls resume at {} or when \
         people raise the budget",
        hit.budget.period.as_str(),
        money(hit.budget.limit_micros, &currency),
        money(hit.spent_micros, &currency),
        hit.period_end.format("%Y-%m-%d %H:%M UTC"),
    ))
}

/// Check every budget (each node, on its heartbeat; claims make each step
/// happen once).
pub async fn evaluate(state: &AppState) -> anyhow::Result<()> {
    let store = &state.store;
    let all = store.budget_statuses().await?;
    let mut changed = false;
    for b in &all {
        // Released: a new period started, or there is room again.
        if let Some(period) = b.exhausted_period
            && (period < b.period_start || !b.exhausted())
            && let Some(resumed) = store.release_budget(b.budget.id).await?
        {
            tracing::info!(budget = %b.budget.id, resumed = resumed.len(), "budget has room again");
            notify(
                store,
                json!({ "kind": "department", "departments": resumed }),
            )
            .await;
            changed = true;
            continue;
        }
        if b.exhausted() && b.exhausted_period != Some(b.period_start) {
            if let Some(paused) = store.exhaust_budget(&b.budget, b.period_start).await? {
                tracing::warn!(
                    budget = %b.budget.id,
                    department = ?b.budget.department_id,
                    paused = paused.len(),
                    "budget used up"
                );
                record(state, b, "budget_exhausted").await;
                notify(
                    store,
                    json!({ "kind": "department", "departments": paused }),
                )
                .await;
                notify(store, json!({ "kind": "budget", "budget": b.budget.id })).await;
                changed = true;
            }
        } else if b.warning()
            && !b.exhausted()
            && store
                .claim_budget_warning(b.budget.id, b.period_start)
                .await?
        {
            record(state, b, "budget_warning").await;
            notify(store, json!({ "kind": "budget", "budget": b.budget.id })).await;
        }
    }
    if changed {
        forget();
        state.reconcile.notify_one();
    }
    Ok(())
}

async fn record(state: &AppState, b: &BudgetStatus, kind: &str) {
    let detail = b.budget.id.to_string();
    let _ = state
        .store
        .record_activity(agentcore_store::Activity {
            department: b.budget.department_id,
            agent: None,
            session: None,
            kind,
            value: Some(b.spent_micros),
            detail: Some(&detail),
        })
        .await;
}

// ---- API ------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SpendingQuery {
    #[serde(default)]
    pub days: Option<u32>,
}

/// Currency, prices, budgets with their usage, and calls without a price.
pub async fn spending(
    State(state): State<AppState>,
    _caller: Caller,
    Query(q): Query<SpendingQuery>,
) -> ApiResult<Json<Value>> {
    let days = i64::from(q.days.unwrap_or(30).clamp(1, 365));
    let since = chrono::Utc::now() - chrono::Duration::days(days);
    Ok(Json(json!({
        "currency": state.store.currency().await?,
        "prices": state.store.list_prices().await?,
        "budgets": state.store.budget_statuses().await?,
        "unpriced_calls": state.store.unpriced_calls(since).await?,
        "days": days,
    })))
}

async fn changed(state: &AppState) {
    forget();
    notify(&state.store, json!({ "kind": "budget" })).await;
    // Apply a raised or lowered limit right away.
    if let Err(err) = evaluate(state).await {
        tracing::warn!(error = %err, "budget check failed");
    }
}

pub async fn put_price(
    State(state): State<AppState>,
    caller: Caller,
    Json(input): Json<PriceInput>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let price = state.store.set_price(input, &caller.name).await?;
    changed(&state).await;
    Ok(Json(json!(price)))
}

#[derive(Debug, Deserialize)]
pub struct PriceKey {
    pub provider: String,
    pub model: String,
}

pub async fn delete_price(
    State(state): State<AppState>,
    caller: Caller,
    Query(key): Query<PriceKey>,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    state
        .store
        .delete_price(&key.provider, &key.model, &caller.name)
        .await?;
    changed(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Price calls recorded before their price was set.
pub async fn price_past_calls(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let priced = state.store.price_unpriced_calls(&caller.name).await?;
    changed(&state).await;
    Ok(Json(json!({ "priced": priced })))
}

#[derive(Debug, Deserialize)]
pub struct CurrencyInput {
    pub currency: String,
}

pub async fn put_currency(
    State(state): State<AppState>,
    caller: Caller,
    Json(input): Json<CurrencyInput>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let currency = state
        .store
        .set_currency(&input.currency, &caller.name)
        .await?;
    changed(&state).await;
    Ok(Json(json!({ "currency": currency })))
}

pub async fn create_budget(
    State(state): State<AppState>,
    caller: Caller,
    Json(input): Json<BudgetInput>,
) -> ApiResult<impl IntoResponse> {
    caller.require_admin()?;
    let budget = state.store.set_budget(input, &caller.name).await?;
    changed(&state).await;
    Ok((StatusCode::CREATED, Json(json!(budget))))
}

pub async fn update_budget(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(update): Json<BudgetUpdate>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let budget = state.store.update_budget(id, update, &caller.name).await?;
    changed(&state).await;
    Ok(Json(json!(budget)))
}

pub async fn delete_budget(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    // Resume what it paused before it goes.
    if let Some(resumed) = state.store.release_budget(id).await? {
        notify(
            &state.store,
            json!({ "kind": "department", "departments": resumed }),
        )
        .await;
    }
    state.store.delete_budget(id, &caller.name).await?;
    changed(&state).await;
    state.reconcile.notify_one();
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_reads_well() {
        assert_eq!(money(12_345_678, "EUR"), "12.35 EUR");
        assert_eq!(money(0, "USD"), "0.00 USD");
    }
}
