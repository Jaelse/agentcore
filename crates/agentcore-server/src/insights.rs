//! How the organisation is doing, and making it better.
//!
//! * **Business data**: read-only data sources (a PostgreSQL database, an
//!   HTTP API, an uploaded table) that departments are granted; workers read
//!   them with `data_query`.
//! * **Metrics** of the organisation itself, and **signals**: patterns that
//!   usually mean something is inefficient.
//! * **Proposals**: improvements with the problem, the evidence, the solution
//!   and the concrete changes that apply it. The retrospective agent (a
//!   department granted `insights`) makes them; people apply, change, send
//!   back or reject them. Applying runs the changes with the permissions of
//!   the person who applies; kinds of change people allowed are applied
//!   without asking.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use agentcore_core::{
    ActionResult, AgentKind, DataSource, DataSourceKind, DepartmentId, OrgAgentId, OrgSchedule,
    Proposal, ProposalAction, ProposalStatus,
};
use agentcore_runtime::ToolHandler;
use agentcore_store::{
    AgentInput, AgentScope, DataSourceInput, DataSourceUpdate, DepartmentInput, GoalInput,
    MessageFilter, Metrics, NewProposal, ProposalRevision, ScheduleInput, ScheduleUpdate, Store,
};
use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Connection;
use uuid::Uuid;

use crate::AppState;
use crate::auth::Caller;
use crate::config::Role;
use crate::error::ApiError;
use crate::org::{self, Control, Org, Recipient, Sender, def, notify, text_arg};

type ApiResult<T> = Result<T, ApiError>;

const DEFAULT_ROWS: u64 = 200;
const MAX_ROWS: u64 = 1000;
const MAX_HTTP_BODY: usize = 256 * 1024;
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// Client for HTTP data sources: no redirects (a source cannot send the
/// API key elsewhere).
static DATA_HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default()
});

// ---- reading business data ----------------------------------------------------------

fn max_rows(source: &DataSource, args: &Value) -> u64 {
    let cap = source.config["max_rows"]
        .as_u64()
        .unwrap_or(DEFAULT_ROWS)
        .clamp(1, MAX_ROWS);
    args["limit"].as_u64().unwrap_or(cap).clamp(1, cap)
}

/// One read from a data source. Errors are for the agent (or the person
/// testing the source).
pub async fn query_source(
    store: &Store,
    source: &DataSource,
    args: &Value,
) -> Result<Value, String> {
    let limit = max_rows(source, args);
    match source.kind {
        DataSourceKind::Postgres => {
            let sql = clean_sql(&text_arg(args, "sql")?)?;
            let url = store
                .data_source_secret(source.id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("the source has no connection string")?;
            tokio::time::timeout(QUERY_TIMEOUT, postgres_query(&url, &sql, limit))
                .await
                .map_err(|_| "the query took too long".to_string())?
        }
        DataSourceKind::Http => {
            let path = text_arg(args, "path")?;
            let secret = store
                .data_source_secret(source.id)
                .await
                .map_err(|e| e.to_string())?;
            http_get(source, &path, secret.as_deref()).await
        }
        DataSourceKind::Table => {
            let content = store
                .data_source_content(source.id)
                .await
                .map_err(|e| e.to_string())?
                .unwrap_or_default();
            read_table(&content, args, limit)
        }
    }
}

/// One statement; a trailing `;` is allowed.
fn clean_sql(sql: &str) -> Result<String, String> {
    let sql = sql.trim().trim_end_matches(';').trim();
    if sql.is_empty() {
        return Err("the query is empty".into());
    }
    if sql.contains(';') {
        return Err("one statement at a time (no `;`)".into());
    }
    Ok(sql.to_string())
}

async fn postgres_query(url: &str, sql: &str, limit: u64) -> Result<Value, String> {
    let fail = |e: sqlx::Error| match e {
        sqlx::Error::Database(db) => db.message().to_string(),
        other => other.to_string(),
    };
    let mut conn = sqlx::PgConnection::connect(url)
        .await
        .map_err(|e| format!("cannot connect: {}", fail(e)))?;
    let mut tx = conn.begin().await.map_err(fail)?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(fail)?;
    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut *tx)
        .await
        .map_err(fail)?;
    // The query is on its own lines, so a `--` comment cannot hide the
    // limit; the transaction is read-only and rolled back.
    let wrapped = format!(
        "SELECT COALESCE(json_agg(t), '[]'::json) FROM (SELECT * FROM (\n{sql}\n) AS q LIMIT {}) AS t",
        limit + 1
    );
    let rows: sqlx::types::Json<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(wrapped))
        .fetch_one(&mut *tx)
        .await
        .map_err(fail)?;
    let _ = tx.rollback().await;
    let mut rows = match rows.0 {
        Value::Array(rows) => rows,
        _ => Vec::new(),
    };
    let truncated = rows.len() as u64 > limit;
    rows.truncate(limit as usize);
    Ok(json!({ "rows": rows, "truncated": truncated }))
}

async fn http_get(source: &DataSource, path: &str, secret: Option<&str>) -> Result<Value, String> {
    let path = path.trim();
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains("..")
        || path.contains('#')
        || path.contains('@')
        || path.chars().any(char::is_whitespace)
    {
        return Err(
            "`path` must be a path below the source's URL, like /v1/charges?limit=10".into(),
        );
    }
    if let Some(prefixes) = source.config["paths"].as_array()
        && !prefixes.is_empty()
        && !prefixes
            .iter()
            .filter_map(Value::as_str)
            .any(|p| path.starts_with(p))
    {
        return Err(format!(
            "this source only allows paths starting with {}",
            prefixes
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let base = source.config["base_url"]
        .as_str()
        .unwrap_or_default()
        .trim_end_matches('/');
    let mut request = DATA_HTTP.get(format!("{base}{path}"));
    if let Some(secret) = secret {
        let header = source.config["header"].as_str().unwrap_or("Authorization");
        request = request.header(header, secret);
    }
    let mut response = request
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let mut body = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if body.len() + chunk.len() > MAX_HTTP_BODY {
            body.extend_from_slice(&chunk[..MAX_HTTP_BODY - body.len()]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&body).to_string();
    let parsed = if truncated {
        None
    } else {
        serde_json::from_str::<Value>(&text).ok()
    };
    Ok(json!({
        "status": status,
        "content_type": content_type,
        "body": parsed.unwrap_or(Value::String(text)),
        "truncated": truncated,
    }))
}

/// Parse CSV (RFC 4180: quoted fields, `""` escapes, CRLF).
pub fn parse_csv(content: &str) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = content.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            (true, '"') => quoted = false,
            (true, c) => field.push(c),
            (false, '"') if field.is_empty() => quoted = true,
            (false, ',') => record.push(std::mem::take(&mut field)),
            (false, '\r') => {}
            (false, '\n') => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
            }
            (false, c) => field.push(c),
        }
    }
    if quoted {
        return Err("a quoted field is not closed".into());
    }
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    records.retain(|r| !(r.len() == 1 && r[0].is_empty()));
    let mut records = records.into_iter();
    let header = records.next().ok_or("the table is empty")?;
    if header.iter().any(|h| h.trim().is_empty()) {
        return Err("every column needs a name in the first row".into());
    }
    Ok((
        header.into_iter().map(|h| h.trim().to_string()).collect(),
        records.collect(),
    ))
}

fn read_table(content: &str, args: &Value, limit: u64) -> Result<Value, String> {
    let (columns, rows) = parse_csv(content)?;
    let index = |name: &str| {
        columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
            .ok_or_else(|| format!("no column `{name}` (columns: {})", columns.join(", ")))
    };
    let mut filters = Vec::new();
    if let Some(filter) = args["filter"].as_object() {
        for (key, value) in filter {
            let wanted = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            filters.push((index(key)?, wanted));
        }
    }
    let wanted: Vec<usize> = match args["columns"].as_array() {
        Some(names) if !names.is_empty() => names
            .iter()
            .filter_map(Value::as_str)
            .map(index)
            .collect::<Result<_, _>>()?,
        _ => (0..columns.len()).collect(),
    };
    let contains = args["contains"].as_str().map(str::to_lowercase);
    let matching: Vec<&Vec<String>> = rows
        .iter()
        .filter(|row| {
            filters.iter().all(|(i, v)| {
                row.get(*i)
                    .is_some_and(|cell| cell.trim().eq_ignore_ascii_case(v.trim()))
            })
        })
        .filter(|row| {
            contains
                .as_deref()
                .is_none_or(|needle| row.iter().any(|cell| cell.to_lowercase().contains(needle)))
        })
        .collect();
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
    let page: Vec<Value> = matching
        .iter()
        .skip(offset)
        .take(limit as usize)
        .map(|row| {
            let object: serde_json::Map<String, Value> = wanted
                .iter()
                .map(|&i| {
                    (
                        columns[i].clone(),
                        Value::String(row.get(i).cloned().unwrap_or_default()),
                    )
                })
                .collect();
            Value::Object(object)
        })
        .collect();
    Ok(json!({
        "columns": wanted.iter().map(|&i| &columns[i]).collect::<Vec<_>>(),
        "matching": matching.len(),
        "offset": offset,
        "rows": page,
    }))
}

fn describe_source(s: &DataSource) -> Value {
    let how = match s.kind {
        DataSourceKind::Postgres => {
            "PostgreSQL: data_query with `sql` (one SELECT; read-only; explore with information_schema.columns)"
        }
        DataSourceKind::Http => {
            "HTTP API: data_query with `path` (GET below the base URL, query string allowed)"
        }
        DataSourceKind::Table => {
            "Table: data_query with optional `filter` ({column: value}), `contains`, `columns`, `limit`, `offset`"
        }
    };
    json!({
        "name": s.name,
        "kind": s.kind,
        "description": s.description,
        "how_to_query": how,
        "rows": s.rows,
        "max_rows": s.config["max_rows"].as_u64().unwrap_or(DEFAULT_ROWS).min(MAX_ROWS),
    })
}

/// `data_*` tools of a worker whose department was granted data sources.
/// Grants are checked on every call.
pub struct DataTools {
    store: Store,
    agent: OrgAgentId,
    department: DepartmentId,
}

impl DataTools {
    pub fn new(store: Store, agent: OrgAgentId, department: DepartmentId) -> Self {
        Self {
            store,
            agent,
            department,
        }
    }
}

#[async_trait]
impl ToolHandler for DataTools {
    fn definitions(&self) -> Vec<Value> {
        vec![
            def(
                "data_list_sources",
                "Read-only business data your department may use (databases, APIs, tables): \
                 names, descriptions and how to query each.",
                json!({}),
                &[],
            ),
            def(
                "data_query",
                "Read from a data source. Give `source` and, depending on its kind: `sql` \
                 (PostgreSQL, one read-only SELECT), `path` (HTTP API, GET), or `filter` / \
                 `contains` / `columns` (table). `limit` and `offset` page through rows. \
                 State the source and the query when you use the numbers.",
                json!({
                    "source": { "type": "string" },
                    "sql": { "type": "string" },
                    "path": { "type": "string" },
                    "filter": { "type": "object" },
                    "contains": { "type": "string" },
                    "columns": { "type": "array", "items": { "type": "string" } },
                    "limit": { "type": "integer" },
                    "offset": { "type": "integer" },
                }),
                &["source"],
            ),
        ]
    }

    async fn call(&self, tool: &str, arguments: &Value) -> Result<Value, String> {
        let sources = self
            .store
            .data_sources_for(self.department)
            .await
            .map_err(|e| e.to_string())?;
        match tool {
            "data_list_sources" => Ok(json!({
                "sources": sources.iter().map(describe_source).collect::<Vec<_>>(),
            })),
            "data_query" => {
                let name = text_arg(arguments, "source")?;
                let source = sources
                    .iter()
                    .find(|s| s.name == name.trim())
                    .ok_or_else(|| {
                        format!(
                            "your department has no data source `{name}` (see data_list_sources)"
                        )
                    })?;
                let result = query_source(&self.store, source, arguments).await;
                let _ = self
                    .store
                    .record_activity(agentcore_store::Activity {
                        department: Some(self.department),
                        agent: Some(self.agent),
                        session: None,
                        kind: "data_query",
                        value: None,
                        detail: Some(&source.name),
                    })
                    .await;
                result
            }
            other => Err(format!("unknown tool `{other}`")),
        }
    }
}

// ---- signals ------------------------------------------------------------------------

/// A pattern in the metrics that usually means something is inefficient.
#[derive(Debug, Clone, Serialize)]
pub struct Signal {
    /// `high`, `medium` or `low`.
    pub severity: &'static str,
    pub kind: &'static str,
    pub title: String,
    pub detail: String,
    /// What usually helps.
    pub suggestion: String,
    pub department_id: Option<DepartmentId>,
    pub agent_id: Option<OrgAgentId>,
    pub goal_id: Option<Uuid>,
}

/// 1,284 → "1284"; 12_900 → "12.9K"; 4_200_000 → "4.2M".
fn compact(n: i64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 10_000 => format!("{:.0}K", n as f64 / 1e3),
        n => n.to_string(),
    }
}

fn hours(secs: f64) -> String {
    if secs >= 3600.0 {
        format!("{:.1} h", secs / 3600.0)
    } else {
        format!("{:.0} min", (secs / 60.0).max(1.0))
    }
}

/// Look for inefficiencies in the metrics.
pub fn signals(
    m: &Metrics,
    check_ins: &[OrgSchedule],
    budgets: &[agentcore_core::BudgetStatus],
    currency: &str,
    outbox: &agentcore_store::OutboxCounts,
) -> Vec<Signal> {
    let now = chrono::Utc::now();
    let mut out = Vec::new();
    if let Some(oldest) = outbox.oldest_pending
        && (now - oldest).num_hours() >= 24
    {
        out.push(Signal {
            severity: "medium",
            kind: "outbox_waiting",
            title: format!(
                "{} outgoing message(s) wait for approval, the oldest for {}",
                outbox.pending,
                hours((now - oldest).num_seconds() as f64)
            ),
            detail: "Customers, leads or partners are waiting while drafts sit in the outbox."
                .into(),
            suggestion: "Review the outbox, or let a trusted internal channel send without \
                         approval."
                .into(),
            department_id: None,
            agent_id: None,
            goal_id: None,
        });
    }
    if outbox.failed > 0 {
        out.push(Signal {
            severity: "medium",
            kind: "outbox_failed",
            title: format!("{} outgoing message(s) failed to send", outbox.failed),
            detail: "Their channel refused them or could not be reached.".into(),
            suggestion: "Check the channel's settings (Test), then retry the messages.".into(),
            department_id: None,
            agent_id: None,
            goal_id: None,
        });
    }
    let money = |micros: i64| crate::budgets::money(micros, currency);
    for b in budgets {
        let scope = b
            .budget
            .department_id
            .and_then(|id| m.departments.iter().find(|d| d.id == id))
            .map_or("The organisation".to_string(), |d| d.name.clone());
        let period = b.budget.period.as_str();
        if b.exhausted() {
            out.push(Signal {
                severity: "high",
                kind: "budget_used_up",
                title: format!(
                    "{scope} used up its {period} budget ({} of {})",
                    money(b.spent_micros),
                    money(b.budget.limit_micros)
                ),
                detail: if b.blocks() {
                    format!(
                        "Work is paused and model calls are refused until {}.",
                        b.period_end.format("%Y-%m-%d %H:%M UTC")
                    )
                } else {
                    "The budget only warns; work continues.".into()
                },
                suggestion: "Find what used the most (departments and agents below) before \
                             raising the budget."
                    .into(),
                department_id: b.budget.department_id,
                agent_id: None,
                goal_id: None,
            });
        } else if b.warning() {
            out.push(Signal {
                severity: "medium",
                kind: "budget_warning",
                title: format!(
                    "{scope} used {:.0}% of its {period} budget",
                    b.used() * 100.0
                ),
                detail: format!(
                    "{} of {}; the period ends {}.",
                    money(b.spent_micros),
                    money(b.budget.limit_micros),
                    b.period_end.format("%Y-%m-%d %H:%M UTC")
                ),
                suggestion: "Pause what is least important, or make check-ins less frequent."
                    .into(),
                department_id: b.budget.department_id,
                agent_id: None,
                goal_id: None,
            });
        }
    }
    let tokens: i64 = m.daily.iter().map(|d| d.tokens).sum();
    let cost: i64 = m.daily.iter().map(|d| d.cost_micros).sum();
    if tokens > 0 && cost == 0 {
        out.push(Signal {
            severity: "low",
            kind: "unpriced",
            title: format!("{} tokens used, but their cost is unknown", compact(tokens)),
            detail: "No prices are set for the models in use.".into(),
            suggestion: "Set prices under Spending, then price past calls.".into(),
            department_id: None,
            agent_id: None,
            goal_id: None,
        });
    }
    let dept_name = |id: Uuid| {
        m.departments
            .iter()
            .find(|d| d.id == id)
            .map_or("?".to_string(), |d| d.name.clone())
    };
    for d in &m.departments {
        let checked_in = check_ins
            .iter()
            .any(|c| c.department_id == d.id && c.enabled);
        if d.state == "active"
            && d.workers > 0
            && d.sessions == 0
            && d.messages_received == 0
            && !checked_in
        {
            out.push(Signal {
                severity: "medium",
                kind: "idle_department",
                title: format!("{} did nothing in {} days", d.name, m.days),
                detail: "No sessions, no messages and no check-ins.".into(),
                suggestion: "Add a check-in that gives it work towards a goal, or remove it if it \
                             is not needed yet."
                    .into(),
                department_id: Some(d.id),
                agent_id: None,
                goal_id: None,
            });
        }
        if d.waiting_now >= 5 && d.active_agents == 0 {
            out.push(Signal {
                severity: "high",
                kind: "unread_mail",
                title: format!(
                    "{} messages wait in {} and nobody is working",
                    d.waiting_now, d.name
                ),
                detail: "Its agents are stopped, so messages pile up.".into(),
                suggestion: "Start its agents (sleeping agents wake on messages).".into(),
                department_id: Some(d.id),
                agent_id: None,
                goal_id: None,
            });
        }
        if let Some(wait) = d.avg_wait_secs
            && wait > 3600.0
            && d.messages_received >= 3
        {
            out.push(Signal {
                severity: "medium",
                kind: "slow_responses",
                title: format!("Messages to {} wait {} on average", d.name, hours(wait)),
                detail: format!("{} messages received.", d.messages_received),
                suggestion: "Let its agents sleep instead of stopping them, or add a check-in, so \
                             messages wake them."
                    .into(),
                department_id: Some(d.id),
                agent_id: None,
                goal_id: None,
            });
        }
        if d.denied >= 5 {
            out.push(Signal {
                severity: "medium",
                kind: "policy_mismatch",
                title: format!("{} actions were denied in {}", d.denied, d.name),
                detail: "Agents keep trying things their guardrails do not allow.".into(),
                suggestion: "Make the instructions say what is allowed, or change the \
                             department's policy if the work needs it."
                    .into(),
                department_id: Some(d.id),
                agent_id: None,
                goal_id: None,
            });
        }
        if let Some(wait) = d.avg_approval_wait_secs
            && wait > 3600.0
            && d.approvals >= 2
        {
            out.push(Signal {
                severity: "medium",
                kind: "slow_approvals",
                title: format!("Approvals in {} wait {} on average", d.name, hours(wait)),
                detail: format!("{} approvals.", d.approvals),
                suggestion: "Allow routine, low-risk actions in the policy, or batch the work so \
                             fewer approvals are needed."
                    .into(),
                department_id: Some(d.id),
                agent_id: None,
                goal_id: None,
            });
        }
    }
    for a in &m.agents {
        if a.kind != "worker" {
            continue;
        }
        if a.failed_sessions >= 2 && a.failed_sessions * 2 >= a.sessions {
            out.push(Signal {
                severity: "high",
                kind: "failing_agent",
                title: format!(
                    "{} ({}) failed {} of {} sessions",
                    a.name,
                    dept_name(a.department_id),
                    a.failed_sessions,
                    a.sessions
                ),
                detail: "Failed sessions cost time and tokens without results.".into(),
                suggestion: "Look at a failed session; check the agent's configuration and \
                             instructions, or run another agent."
                    .into(),
                department_id: Some(a.department_id),
                agent_id: Some(a.id),
                goal_id: None,
            });
        }
        if (a.tokens > 200_000 || a.agent_hours > 8.0) && a.messages_sent == 0 {
            out.push(Signal {
                severity: "medium",
                kind: "spend_without_output",
                title: format!(
                    "{} ({}) used {} tokens and {:.1} agent-hours but sent no messages",
                    a.name,
                    dept_name(a.department_id),
                    compact(a.tokens),
                    a.agent_hours
                ),
                detail: "Nobody heard what it did.".into(),
                suggestion: "Make its instructions ask for a short report when a task is done, \
                             and progress reports on goals."
                    .into(),
                department_id: Some(a.department_id),
                agent_id: Some(a.id),
                goal_id: None,
            });
        }
    }
    for g in m.goals.iter().filter(|g| g.status == "active") {
        let last = g.progress_at.unwrap_or(g.created_at);
        let days = (now - last).num_days();
        if days >= 7 {
            out.push(Signal {
                severity: if days >= 14 { "high" } else { "medium" },
                kind: "stale_goal",
                title: format!("No progress on \"{}\" for {days} days", g.title),
                detail: if g.progress_at.is_some() {
                    "The latest progress report is old.".into()
                } else {
                    "Nobody has reported progress yet.".into()
                },
                suggestion: "Give a department the goal and a check-in that asks for the next \
                             step and a progress report."
                    .into(),
                department_id: g.department_id,
                agent_id: None,
                goal_id: Some(g.id),
            });
        }
    }
    let rank = |s: &str| match s {
        "high" => 0,
        "medium" => 1,
        _ => 2,
    };
    out.sort_by_key(|s| rank(s.severity));
    out
}

// ---- checking and applying proposals ------------------------------------------------

/// Check that what the actions refer to exists and that values are sane.
pub async fn validate_actions(state: &AppState, actions: &[ProposalAction]) -> Result<(), String> {
    let org = Org::load(&state.store).await.map_err(|e| e.to_string())?;
    let check_ins = state
        .store
        .list_schedules(None)
        .await
        .map_err(|e| e.to_string())?;
    let agent = |id: OrgAgentId| {
        org.agent(id)
            .ok_or_else(|| format!("no agent with id {id} (see insights_org)"))
    };
    let dept = |id: DepartmentId| {
        org.department(id)
            .ok_or_else(|| format!("no department with id {id} (see insights_org)"))
    };
    for (i, action) in actions.iter().enumerate() {
        let at = |e: String| format!("change {} ({}): {e}", i + 1, action.kind());
        match action {
            ProposalAction::UpdateInstructions { agent: a, .. }
            | ProposalAction::RemoveAgent { agent: a } => {
                let a = agent(*a).map_err(at)?;
                if matches!(action, ProposalAction::RemoveAgent { .. })
                    && a.kind == AgentKind::Communicator
                {
                    return Err(at("a communicator cannot be removed".into()));
                }
            }
            ProposalAction::ControlAgent { agent: a, control } => {
                agent(*a).map_err(at)?;
                serde_json::from_value::<Control>(json!(control))
                    .map_err(|_| at("`control` is start, pause, resume or stop".into()))?;
            }
            ProposalAction::AddAgent {
                department, agent, ..
            } => {
                dept(*department).map_err(at)?;
                if let Some(spec) = agent
                    && state.manager.agent(spec).is_none()
                {
                    return Err(at(format!("no configured agent `{spec}`")));
                }
            }
            ProposalAction::UpdateDepartment {
                department, policy, ..
            } => {
                dept(*department).map_err(at)?;
                if let Some(p) = policy
                    && state.manager.policies().get(p).is_none()
                {
                    return Err(at(format!("no policy `{p}`")));
                }
            }
            ProposalAction::CreateCheckIn {
                department,
                agent: a,
                ..
            } => {
                dept(*department).map_err(at)?;
                if let Some(a) = a {
                    agent(*a).map_err(at)?;
                }
            }
            ProposalAction::UpdateCheckIn { check_in, .. } => {
                if !check_ins.iter().any(|c| c.id == *check_in) {
                    return Err(at(format!("no check-in with id {check_in}")));
                }
            }
            ProposalAction::CreateGoal { department, .. } => {
                if let Some(d) = department {
                    dept(*d).map_err(at)?;
                }
            }
            ProposalAction::SendMessage {
                department,
                agent: a,
                ..
            } => match (department, a) {
                (Some(d), None) => {
                    dept(*d).map_err(at)?;
                }
                (None, Some(a)) => {
                    agent(*a).map_err(at)?;
                }
                _ => return Err(at("give either `department` or `agent`".into())),
            },
            ProposalAction::SetBudget {
                department, limit, ..
            } => {
                if let Some(d) = department {
                    dept(*d).map_err(at)?;
                }
                if !limit.is_finite() || *limit <= 0.0 {
                    return Err(at("`limit` is an amount above zero".into()));
                }
            }
            ProposalAction::SetLimits { .. } => {}
        }
    }
    Ok(())
}

fn require_role(actions: &[ProposalAction], role: Role) -> ApiResult<()> {
    if role == Role::Viewer {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "viewers cannot apply changes",
        ));
    }
    if role != Role::Admin
        && let Some(a) = actions.iter().find(|a| a.needs_admin())
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            format!("`{}` changes need an admin", a.kind()),
        ));
    }
    Ok(())
}

/// Apply a proposal's changes in order; the first failure stops the rest.
/// `by` is who applies (a person, or the auto-apply allowance).
pub async fn apply(state: &AppState, id: Uuid, revision: u32, by: &str) -> ApiResult<Proposal> {
    let proposal = state.store.claim_proposal(id, revision, by).await?;
    let mut results = Vec::new();
    let mut failed = false;
    for action in &proposal.actions {
        if failed {
            results.push(ActionResult {
                kind: action.kind().into(),
                ok: false,
                detail: "not applied: an earlier change failed".into(),
            });
            continue;
        }
        let result = apply_one(state, action, by).await;
        failed = result.is_err();
        results.push(ActionResult {
            kind: action.kind().into(),
            ok: result.is_ok(),
            detail: result.unwrap_or_else(|e| e),
        });
    }
    let proposal = state.store.finish_proposal(id, &results, by).await?;
    notify(&state.store, json!({ "kind": "proposals" })).await;
    notify(&state.store, json!({ "kind": "department" })).await;
    state.reconcile.notify_one();
    if let Some(proposer) = proposal.proposer_agent {
        let text = if failed {
            format!(
                "Your proposal \"{}\" was applied by {by} but a change failed: {}. Look at the \
                 result (insights_proposals) and propose a fix if needed.",
                proposal.title,
                results
                    .iter()
                    .find(|r| !r.ok)
                    .map_or("", |r| r.detail.as_str())
            )
        } else {
            format!(
                "Your proposal \"{}\" was applied by {by}. Check in the metrics whether it \
                 helps.",
                proposal.title
            )
        };
        tell(state, proposer, &text).await;
    }
    Ok(proposal)
}

async fn tell(state: &AppState, agent: OrgAgentId, text: &str) {
    if let Err(err) = org::send(
        &state.store,
        &Sender::Human("proposals".into()),
        Recipient::Agent(agent),
        text,
    )
    .await
    {
        tracing::debug!(error = %err, "could not tell the proposer");
    }
}

async fn apply_one(state: &AppState, action: &ProposalAction, by: &str) -> Result<String, String> {
    let store = &state.store;
    let e = |e: agentcore_store::StoreError| e.to_string();
    let api = |e: ApiError| e.message().to_string();
    match action {
        ProposalAction::UpdateInstructions {
            agent,
            instructions,
        } => {
            let a = store
                .update_agent(*agent, None, Some(instructions.clone()), by)
                .await
                .map_err(e)?;
            notify(
                store,
                json!({ "kind": "agents", "departments": [a.department_id] }),
            )
            .await;
            Ok(format!(
                "new instructions for {}; they take effect in its next session",
                a.name
            ))
        }
        ProposalAction::AddAgent {
            department,
            name,
            agent,
            instructions,
        } => {
            let current = store.list_agents(Some(*department)).await.map_err(e)?;
            let spec = match agent {
                Some(a) => a.clone(),
                None => current
                    .iter()
                    .find(|a| a.kind == AgentKind::Worker)
                    .or(current.first())
                    .map(|a| a.agent.clone())
                    .ok_or("the department has no agents to copy the agent from")?,
            };
            org::check_agent_spec(state, &spec).map_err(api)?;
            let a = store
                .add_agent(
                    *department,
                    AgentInput {
                        name: name.clone(),
                        agent: spec,
                        instructions: instructions.clone(),
                    },
                    by,
                )
                .await
                .map_err(e)?;
            notify(
                store,
                json!({ "kind": "agents", "departments": [department] }),
            )
            .await;
            Ok(format!("added {} (stopped; start it when ready)", a.name))
        }
        ProposalAction::RemoveAgent { agent } => {
            let a = store.get_agent(*agent).await.map_err(e)?;
            org::control_agents(state, AgentScope::Agent(*agent), Control::Stop, by)
                .await
                .map_err(api)?;
            // Wait for its session to end (any node), then delete it.
            for _ in 0..100 {
                let now = store.get_agent(*agent).await.map_err(e)?;
                let ended = now.session_id.is_none()
                    || matches!(
                        now.status.as_deref(),
                        Some("stopped" | "completed" | "failed" | "asleep")
                    );
                if ended {
                    store.delete_agent(*agent, by).await.map_err(e)?;
                    notify(
                        store,
                        json!({ "kind": "agents", "departments": [a.department_id] }),
                    )
                    .await;
                    return Ok(format!("removed {}", a.name));
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(format!(
                "{} was stopped but its session has not ended yet; remove it later",
                a.name
            ))
        }
        ProposalAction::ControlAgent { agent, control } => {
            let control: Control = serde_json::from_value(json!(control))
                .map_err(|_| format!("unknown control `{control}`"))?;
            let (changed, failed) =
                org::control_agents(state, AgentScope::Agent(*agent), control, by)
                    .await
                    .map_err(api)?;
            if let Some(f) = failed.first() {
                return Err(f["error"].as_str().unwrap_or("failed").to_string());
            }
            Ok(format!("{} agent(s) changed", changed.len()))
        }
        ProposalAction::UpdateDepartment {
            department,
            mission,
            policy,
            tools,
        } => {
            let d = store.get_department(*department).await.map_err(e)?;
            let mut input = DepartmentInput {
                name: d.name.clone(),
                description: d.description.clone(),
                mission: mission.clone().unwrap_or(d.mission.clone()),
                policy: policy.clone().unwrap_or(d.policy.clone()),
                tools: tools.clone().unwrap_or(d.tools.clone()),
                communicator_agent: d.communicator_agent.clone(),
                template: d.template.clone(),
                project_id: d.project_id,
                role: d.role.clone(),
            };
            org::check_department(state, &mut input)
                .await
                .map_err(api)?;
            store
                .update_department(*department, input, by)
                .await
                .map_err(e)?;
            notify(
                store,
                json!({ "kind": "department", "departments": [department] }),
            )
            .await;
            Ok(format!(
                "{} updated; running agents get it in their next session",
                d.name
            ))
        }
        ProposalAction::CreateCheckIn {
            department,
            agent,
            name,
            message,
            every_minutes,
        } => {
            let c = store
                .create_schedule(
                    *department,
                    ScheduleInput {
                        name: name.clone(),
                        message: message.clone(),
                        every_minutes: *every_minutes,
                        agent_id: *agent,
                        first_run_at: None,
                    },
                    by,
                )
                .await
                .map_err(e)?;
            notify(
                store,
                json!({ "kind": "checkins", "departments": [department] }),
            )
            .await;
            Ok(format!(
                "check-in `{}` every {} minutes",
                c.name, c.every_minutes
            ))
        }
        ProposalAction::UpdateCheckIn {
            check_in,
            message,
            every_minutes,
            enabled,
        } => {
            let c = store
                .update_schedule(
                    *check_in,
                    ScheduleUpdate {
                        name: None,
                        message: message.clone(),
                        every_minutes: *every_minutes,
                        enabled: *enabled,
                    },
                    by,
                )
                .await
                .map_err(e)?;
            notify(
                store,
                json!({ "kind": "checkins", "departments": [c.department_id] }),
            )
            .await;
            Ok(format!("check-in `{}` updated", c.name))
        }
        ProposalAction::CreateGoal {
            title,
            description,
            department,
        } => {
            let g = store
                .create_goal(
                    GoalInput {
                        title: title.clone(),
                        description: description.clone(),
                        department_id: *department,
                    },
                    by,
                )
                .await
                .map_err(e)?;
            notify(store, json!({ "kind": "goals" })).await;
            Ok(format!("goal \"{}\" added", g.title))
        }
        ProposalAction::SendMessage {
            department,
            agent,
            text,
        } => {
            let to = match (department, agent) {
                (_, Some(a)) => Recipient::Agent(*a),
                (Some(d), None) => Recipient::Department(*d),
                (None, None) => return Err("no recipient".into()),
            };
            let m = org::send(store, &Sender::Human(by.to_string()), to, text)
                .await
                .map_err(|e| e.to_string())?;
            Ok(format!("message sent to {}", m.to_name))
        }
        ProposalAction::SetBudget {
            department,
            period,
            limit,
            action,
        } => {
            let b = store
                .set_budget(
                    agentcore_store::BudgetInput {
                        department_id: *department,
                        period: *period,
                        limit_micros: (limit * 1e6).round() as i64,
                        action: *action,
                        warn_percent: 80,
                    },
                    by,
                )
                .await
                .map_err(e)?;
            let currency = store.currency().await.map_err(e)?;
            notify(store, json!({ "kind": "budget" })).await;
            Ok(format!(
                "budget of {} per {} ({})",
                crate::budgets::money(b.limit_micros, &currency),
                b.period.as_str(),
                b.action.as_str()
            ))
        }
        ProposalAction::SetLimits {
            max_departments,
            max_agents_per_department,
        } => {
            let mut s = store.org_settings().await.map_err(e)?;
            if let Some(v) = max_departments {
                s.max_departments = *v;
            }
            if let Some(v) = max_agents_per_department {
                s.max_agents_per_department = *v;
            }
            let s = store.set_org_settings(s, by).await.map_err(e)?;
            notify(store, json!({ "kind": "settings" })).await;
            Ok(format!(
                "limits: {} departments, {} agents each",
                s.max_departments, s.max_agents_per_department
            ))
        }
    }
}

/// Apply a new proposal right away when people allowed every kind of
/// change in it.
async fn maybe_auto_apply(state: &AppState, proposal: Proposal) -> Proposal {
    let Ok((kinds, allowed_by)) = state.store.auto_apply().await else {
        return proposal;
    };
    let allowed = !proposal.actions.is_empty()
        && proposal
            .actions
            .iter()
            .all(|a| kinds.iter().any(|k| k == a.kind()));
    if !allowed {
        return proposal;
    }
    let by = format!(
        "auto-apply (allowed by {})",
        allowed_by.as_deref().unwrap_or("an admin")
    );
    match apply(state, proposal.id, proposal.revision, &by).await {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err.message(), "auto-apply failed");
            proposal
        }
    }
}

// ---- the retrospective's tools -----------------------------------------------------

/// `insights_*` tools of a worker in a department granted `insights`.
pub struct InsightsTools {
    state: AppState,
    agent: OrgAgentId,
}

impl InsightsTools {
    pub fn new(state: AppState, agent: OrgAgentId) -> Self {
        Self { state, agent }
    }
}

const ACTIONS_HELP: &str = "Each change is an object with `kind` and its fields (ids from \
    insights_org): update_instructions {agent, instructions}; add_agent {department, name, \
    agent?, instructions}; remove_agent {agent}; control_agent {agent, control: \
    start|pause|resume|stop}; update_department {department, mission?, policy?, tools?}; \
    create_check_in {department, agent?, name, message, every_minutes}; update_check_in \
    {check_in, message?, every_minutes?, enabled?}; create_goal {title, description?, \
    department?}; send_message {department | agent, text}; set_budget {department?, period: \
    day|week|month, limit (in the organisation's currency), action: warn|pause}; set_limits \
    {max_departments?, max_agents_per_department?}.";

#[async_trait]
impl ToolHandler for InsightsTools {
    fn definitions(&self) -> Vec<Value> {
        vec![
            def(
                "insights_metrics",
                "How the organisation is doing over the last `days` (default 14): per day, per \
                 department and per agent (sessions, failures, agent-hours, model calls, tokens, \
                 cost, messages, waiting times, denied actions, approvals, progress reports, \
                 data queries), goals, proposals, budgets with how much is used, and signals of \
                 likely inefficiencies.",
                json!({ "days": { "type": "integer", "minimum": 1, "maximum": 90 } }),
                &[],
            ),
            def(
                "insights_org",
                "The organisation's structure with ids: departments (mission, policy, tools), \
                 agents (instructions, status), check-ins, goals, limits, and the policies and \
                 agents that exist.",
                json!({}),
                &[],
            ),
            def(
                "insights_messages",
                "Recent messages of the organisation or one department (`department`: name or \
                 id), newest last.",
                json!({
                    "department": { "type": "string" },
                    "limit": { "type": "integer", "maximum": 200 },
                }),
                &[],
            ),
            def(
                "insights_proposals",
                "Proposals and their status (open, changes_requested, applied, rejected, \
                 failed), with people's feedback and the result of applying them.",
                json!({ "status": { "type": "string" } }),
                &[],
            ),
            def(
                "insights_propose",
                &format!(
                    "Propose an improvement to the people who run the organisation: the \
                     `problem` (what is inefficient), the `evidence` (numbers from the \
                     metrics), the `solution`, and `actions` (the concrete changes; may be \
                     empty for advice). Nothing changes until a person applies it. \
                     {ACTIONS_HELP}"
                ),
                json!({
                    "title": { "type": "string" },
                    "problem": { "type": "string" },
                    "evidence": { "type": "string" },
                    "solution": { "type": "string" },
                    "actions": { "type": "array", "items": { "type": "object" } },
                }),
                &["title", "problem", "solution"],
            ),
            def(
                "insights_revise",
                &format!(
                    "Revise one of your proposals that is still open, usually after people \
                     asked for changes. Give the `proposal` id, the fields that change and a \
                     `note` saying what changed. {ACTIONS_HELP}"
                ),
                json!({
                    "proposal": { "type": "string" },
                    "title": { "type": "string" },
                    "problem": { "type": "string" },
                    "evidence": { "type": "string" },
                    "solution": { "type": "string" },
                    "actions": { "type": "array", "items": { "type": "object" } },
                    "note": { "type": "string" },
                }),
                &["proposal", "note"],
            ),
        ]
    }

    async fn call(&self, tool: &str, arguments: &Value) -> Result<Value, String> {
        let store = &self.state.store;
        let e = |e: agentcore_store::StoreError| e.to_string();
        let org = Org::load(store).await.map_err(e)?;
        let me = org
            .agent(self.agent)
            .cloned()
            .ok_or("you are no longer a member of the organisation")?;
        match tool {
            "insights_metrics" => {
                let days = arguments["days"].as_u64().unwrap_or(14).clamp(1, 90) as u32;
                let metrics = store.org_metrics(days).await.map_err(e)?;
                let check_ins = store.list_schedules(None).await.map_err(e)?;
                let budgets = store.budget_statuses().await.map_err(e)?;
                let currency = store.currency().await.map_err(e)?;
                let outbox = store.outbox_counts(metrics.since).await.map_err(e)?;
                let signals = signals(&metrics, &check_ins, &budgets, &currency, &outbox);
                Ok(json!({
                    "metrics": metrics, "signals": signals, "budgets": budgets,
                    "currency": currency, "outbox": outbox,
                }))
            }
            "insights_org" => {
                let check_ins = store.list_schedules(None).await.map_err(e)?;
                let mut policies: Vec<Value> = self
                    .state
                    .manager
                    .policies()
                    .iter()
                    .map(|p| json!({ "name": p.name(), "description": p.policy().description }))
                    .collect();
                policies.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                Ok(json!({
                    "profile": org.profile,
                    "limits": store.org_settings().await.map_err(e)?,
                    "departments": org.departments.iter().map(|d| json!({
                        "id": d.id, "name": d.name, "mission": d.mission, "policy": d.policy,
                        "tools": d.tools, "state": d.state, "template": d.template,
                        "agents": org.members(d.id).map(|a| json!({
                            "id": a.id, "name": a.name, "kind": a.kind, "agent": a.agent,
                            "instructions": a.instructions, "desired": a.desired,
                            "status": a.status,
                        })).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                    "check_ins": check_ins,
                    "goals": org.goals,
                    "policies": policies,
                    "agents_available": self.state.manager.agents().into_iter().map(|a| a.name).collect::<Vec<_>>(),
                }))
            }
            "insights_messages" => {
                let department = match arguments["department"].as_str() {
                    Some(key) if !key.trim().is_empty() => Some(
                        org.departments
                            .iter()
                            .find(|d| d.id.to_string() == key || d.name.eq_ignore_ascii_case(key))
                            .ok_or_else(|| format!("no department `{key}`"))?
                            .id,
                    ),
                    _ => None,
                };
                let limit = arguments["limit"].as_i64().unwrap_or(50).clamp(1, 200);
                let messages = store
                    .list_messages(MessageFilter {
                        department,
                        agent: None,
                        after: None,
                        limit,
                    })
                    .await
                    .map_err(e)?;
                Ok(json!({ "messages": messages.iter().map(|m| json!({
                    "at": m.created_at, "from": m.from_name, "to": m.to_name,
                    "scope": m.scope, "text": m.text,
                })).collect::<Vec<_>>() }))
            }
            "insights_proposals" => {
                let status = match arguments["status"].as_str() {
                    Some(s) if !s.is_empty() => Some(s.parse::<ProposalStatus>()?),
                    _ => None,
                };
                let proposals = store.list_proposals(status).await.map_err(e)?;
                Ok(json!({ "proposals": proposals }))
            }
            "insights_propose" => {
                let actions = parse_actions(&arguments["actions"])?;
                validate_actions(&self.state, &actions).await?;
                let proposal = store
                    .create_proposal(NewProposal {
                        title: text_arg(arguments, "title")?,
                        problem: text_arg(arguments, "problem")?,
                        evidence: arguments["evidence"].as_str().unwrap_or_default().into(),
                        solution: text_arg(arguments, "solution")?,
                        actions,
                        proposed_by: org.label(&me),
                        proposer_agent: Some(me.id),
                    })
                    .await
                    .map_err(e)?;
                notify(store, json!({ "kind": "proposals" })).await;
                let proposal = maybe_auto_apply(&self.state, proposal).await;
                Ok(json!({
                    "proposal": proposal.id,
                    "status": proposal.status,
                    "result": proposal.result,
                }))
            }
            "insights_revise" => {
                let id: Uuid = text_arg(arguments, "proposal")?
                    .trim()
                    .parse()
                    .map_err(|_| "`proposal` is a proposal id".to_string())?;
                let current = store.get_proposal(id).await.map_err(e)?;
                if current.proposer_agent != Some(me.id) {
                    return Err("you can only revise your own proposals".into());
                }
                let actions = match arguments.get("actions") {
                    Some(v) if !v.is_null() => Some(parse_actions(v)?),
                    _ => None,
                };
                if let Some(actions) = &actions {
                    validate_actions(&self.state, actions).await?;
                }
                let s = |k: &str| arguments[k].as_str().map(String::from);
                let proposal = store
                    .revise_proposal(
                        id,
                        ProposalRevision {
                            title: s("title"),
                            problem: s("problem"),
                            evidence: s("evidence"),
                            solution: s("solution"),
                            actions,
                            note: text_arg(arguments, "note")?,
                        },
                        &org.label(&me),
                    )
                    .await
                    .map_err(e)?;
                notify(store, json!({ "kind": "proposals" })).await;
                Ok(json!({ "proposal": proposal.id, "revision": proposal.revision }))
            }
            other => Err(format!("unknown tool `{other}`")),
        }
    }
}

fn parse_actions(value: &Value) -> Result<Vec<ProposalAction>, String> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| {
                serde_json::from_value(v.clone())
                    .map_err(|err| format!("change {}: {err}. {ACTIONS_HELP}", i + 1))
            })
            .collect(),
        _ => Err("`actions` is a list of changes".into()),
    }
}

// ---- API -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MetricsQuery {
    #[serde(default)]
    pub days: Option<u32>,
}

pub async fn metrics(
    State(state): State<AppState>,
    _caller: Caller,
    Query(q): Query<MetricsQuery>,
) -> ApiResult<Json<Value>> {
    let metrics = state.store.org_metrics(q.days.unwrap_or(14)).await?;
    let check_ins = state.store.list_schedules(None).await?;
    let budgets = state.store.budget_statuses().await?;
    let currency = state.store.currency().await?;
    let outbox = state.store.outbox_counts(metrics.since).await?;
    let signals = signals(&metrics, &check_ins, &budgets, &currency, &outbox);
    Ok(Json(json!({
        "metrics": metrics, "signals": signals, "budgets": budgets, "currency": currency,
        "outbox": outbox,
    })))
}

#[derive(Debug, Deserialize)]
pub struct ProposalQuery {
    #[serde(default)]
    pub status: Option<ProposalStatus>,
}

pub async fn list_proposals(
    State(state): State<AppState>,
    _caller: Caller,
    Query(q): Query<ProposalQuery>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(state.store.list_proposals(q.status).await?)))
}

pub async fn get_proposal(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(state.store.get_proposal(id).await?)))
}

#[derive(Debug, Deserialize)]
pub struct ProposalInput {
    pub title: String,
    pub problem: String,
    #[serde(default)]
    pub evidence: String,
    pub solution: String,
    #[serde(default)]
    pub actions: Vec<ProposalAction>,
}

fn bad(msg: String) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, msg)
}

/// A person writes a proposal (to apply later or let others review).
pub async fn create_proposal(
    State(state): State<AppState>,
    caller: Caller,
    Json(input): Json<ProposalInput>,
) -> ApiResult<impl IntoResponse> {
    caller.require_operator()?;
    validate_actions(&state, &input.actions)
        .await
        .map_err(bad)?;
    let proposal = state
        .store
        .create_proposal(NewProposal {
            title: input.title,
            problem: input.problem,
            evidence: input.evidence,
            solution: input.solution,
            actions: input.actions,
            proposed_by: caller.name.clone(),
            proposer_agent: None,
        })
        .await?;
    notify(&state.store, json!({ "kind": "proposals" })).await;
    Ok((StatusCode::CREATED, Json(json!(proposal))))
}

/// A person changes a proposal before applying it.
pub async fn update_proposal(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(revision): Json<ProposalRevision>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    if let Some(actions) = &revision.actions {
        validate_actions(&state, actions).await.map_err(bad)?;
    }
    let proposal = state
        .store
        .revise_proposal(id, revision, &caller.name)
        .await?;
    notify(&state.store, json!({ "kind": "proposals" })).await;
    Ok(Json(json!(proposal)))
}

#[derive(Debug, Deserialize)]
pub struct ApplyInput {
    /// The revision the person reviewed.
    pub revision: u32,
}

pub async fn apply_proposal(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(input): Json<ApplyInput>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let proposal = state.store.get_proposal(id).await?;
    require_role(&proposal.actions, caller.role)?;
    validate_actions(&state, &proposal.actions)
        .await
        .map_err(|m| ApiError::new(StatusCode::CONFLICT, m))?;
    Ok(Json(json!(
        apply(&state, id, input.revision, &caller.name).await?
    )))
}

#[derive(Debug, Deserialize)]
pub struct FeedbackInput {
    #[serde(default)]
    pub text: String,
}

/// Send a proposal back to its proposer with what should change.
pub async fn request_changes(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(input): Json<FeedbackInput>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let proposal = state
        .store
        .request_proposal_changes(id, &input.text, &caller.name)
        .await?;
    notify(&state.store, json!({ "kind": "proposals" })).await;
    if let Some(proposer) = proposal.proposer_agent {
        tell(
            &state,
            proposer,
            &format!(
                "{} asked for changes to your proposal \"{}\" (id {}): {}\nRevise it with \
                 insights_revise.",
                caller.name,
                proposal.title,
                proposal.id,
                input.text.trim()
            ),
        )
        .await;
    }
    Ok(Json(json!(proposal)))
}

pub async fn reject_proposal(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(input): Json<FeedbackInput>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let proposal = state
        .store
        .reject_proposal(id, &input.text, &caller.name)
        .await?;
    notify(&state.store, json!({ "kind": "proposals" })).await;
    if let Some(proposer) = proposal.proposer_agent {
        let why = if input.text.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", input.text.trim())
        };
        tell(
            &state,
            proposer,
            &format!(
                "{} rejected your proposal \"{}\"{why}. Do not propose it again unless \
                 something changes.",
                caller.name, proposal.title
            ),
        )
        .await;
    }
    Ok(Json(json!(proposal)))
}

pub async fn get_auto_apply(
    State(state): State<AppState>,
    _caller: Caller,
) -> ApiResult<Json<Value>> {
    let (kinds, by) = state.store.auto_apply().await?;
    Ok(Json(json!({
        "kinds": kinds,
        "by": by,
        "available": ProposalAction::KINDS,
    })))
}

#[derive(Debug, Deserialize)]
pub struct AutoApplyInput {
    pub kinds: Vec<String>,
}

/// Kinds of change applied without asking (admin).
pub async fn put_auto_apply(
    State(state): State<AppState>,
    caller: Caller,
    Json(input): Json<AutoApplyInput>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let kinds = state
        .store
        .set_auto_apply(&input.kinds, &caller.name)
        .await?;
    notify(&state.store, json!({ "kind": "proposals" })).await;
    Ok(Json(json!({ "kinds": kinds, "by": caller.name })))
}

// ---- data sources API ------------------------------------------------------------------

pub async fn list_data_sources(
    State(state): State<AppState>,
    _caller: Caller,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(state.store.list_data_sources().await?)))
}

/// Count the rows of an uploaded table (and check it parses).
fn table_rows(content: &str) -> ApiResult<usize> {
    parse_csv(content)
        .map(|(_, rows)| rows.len())
        .map_err(|e| bad(format!("the table is not valid CSV: {e}")))
}

fn with_rows(mut config: Value, rows: usize) -> Value {
    if !config.is_object() {
        config = json!({});
    }
    config["rows"] = json!(rows);
    config
}

pub async fn create_data_source(
    State(state): State<AppState>,
    caller: Caller,
    Json(mut input): Json<DataSourceInput>,
) -> ApiResult<impl IntoResponse> {
    caller.require_admin()?;
    if input.kind == DataSourceKind::Table {
        let rows = table_rows(input.content.as_deref().unwrap_or_default())?;
        input.config = with_rows(input.config, rows);
    }
    let source = state.store.create_data_source(input, &caller.name).await?;
    notify(&state.store, json!({ "kind": "data_sources" })).await;
    Ok((StatusCode::CREATED, Json(json!(source))))
}

pub async fn update_data_source(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(mut update): Json<DataSourceUpdate>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let current = state.store.get_data_source(id).await?;
    if current.kind == DataSourceKind::Table {
        let rows = match &update.content {
            Some(content) => Some(table_rows(content)?),
            None => current.rows.map(|r| r as usize),
        };
        if let Some(rows) = rows {
            update.config = Some(with_rows(
                update.config.take().unwrap_or(current.config.clone()),
                rows,
            ));
        }
    }
    let source = state
        .store
        .update_data_source(id, update, &caller.name)
        .await?;
    notify(&state.store, json!({ "kind": "data_sources" })).await;
    Ok(Json(json!(source)))
}

pub async fn delete_data_source(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    state.store.delete_data_source(id, &caller.name).await?;
    notify(&state.store, json!({ "kind": "data_sources" })).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Try a source with a small read: `SELECT 1`, the base URL (or the
/// `test_path` in its config), or the first rows of a table. Admins may
/// pass their own query in the body (`sql`, `path`, `filter`, ...).
pub async fn test_data_source(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    body: Option<Json<Value>>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let source = state.store.get_data_source(id).await?;
    let mut args = body.map(|b| b.0).unwrap_or_else(|| json!({}));
    if !args.is_object() {
        args = json!({});
    }
    match source.kind {
        DataSourceKind::Postgres if args.get("sql").is_none() => {
            args["sql"] = json!("SELECT current_database() AS database, now() AS time");
        }
        DataSourceKind::Http if args.get("path").is_none() => {
            args["path"] = json!(source.config["test_path"].as_str().unwrap_or("/"));
        }
        DataSourceKind::Table if args.get("limit").is_none() => args["limit"] = json!(5),
        _ => {}
    }
    Ok(Json(
        match query_source(&state.store, &source, &args).await {
            Ok(result) => json!({ "ok": true, "result": result }),
            Err(error) => json!({ "ok": false, "error": error }),
        },
    ))
}

/// Remember when approvals were requested, to measure how long they wait.
#[derive(Default)]
pub struct ApprovalClock(HashMap<Uuid, std::time::Instant>);

impl ApprovalClock {
    pub fn requested(&mut self, approval: Uuid) {
        self.0.insert(approval, std::time::Instant::now());
    }

    pub fn resolved(&mut self, approval: Uuid) -> Option<i64> {
        self.0
            .remove(&approval)
            .map(|t| t.elapsed().as_millis() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_and_table_reads() {
        let (cols, rows) = parse_csv(
            "\u{feff}customer,plan,mrr\r\nAcme,pro,\"1,200\"\n\"Big \"\"B\"\"\",free,0\n\n",
        )
        .unwrap();
        assert_eq!(cols, ["customer", "plan", "mrr"]);
        assert_eq!(rows[0], ["Acme", "pro", "1,200"]);
        assert_eq!(rows[1][0], "Big \"B\"");
        assert!(parse_csv("a,b\n\"x,1\n").is_err());
        assert!(parse_csv("a,\n1,2\n").is_err());

        let csv = "customer,plan,mrr\nAcme,pro,1200\nBeta,free,0\nCorp,PRO,900\n";
        let r = read_table(csv, &json!({ "filter": { "plan": "pro" } }), 10).unwrap();
        assert_eq!(r["matching"], 2);
        let r = read_table(
            csv,
            &json!({ "columns": ["customer"], "contains": "bet" }),
            10,
        )
        .unwrap();
        assert_eq!(r["rows"], json!([{ "customer": "Beta" }]));
        let r = read_table(csv, &json!({ "offset": 1 }), 1).unwrap();
        assert_eq!(r["rows"][0]["customer"], "Beta");
        assert!(read_table(csv, &json!({ "filter": { "nope": 1 } }), 10).is_err());
    }

    #[test]
    fn signals_find_waste() {
        use agentcore_store::{AgentMetrics, DepartmentMetrics, GoalMetrics, ProposalCounts};
        let now = chrono::Utc::now();
        let dept = |name: &str| DepartmentMetrics {
            id: Uuid::now_v7(),
            name: name.into(),
            state: "active".into(),
            workers: 1,
            active_agents: 0,
            asleep_agents: 0,
            sessions: 0,
            failed_sessions: 0,
            agent_hours: 0.0,
            model_calls: 0,
            tokens: 0,
            cost_micros: 0,
            messages_sent: 0,
            messages_received: 0,
            avg_wait_secs: None,
            waiting_now: 0,
            denied: 0,
            approvals: 0,
            avg_approval_wait_secs: None,
            progress_reports: 0,
            data_queries: 0,
            last_activity: None,
        };
        let idle = dept("Legal");
        let mut busy = dept("Support");
        busy.sessions = 4;
        busy.waiting_now = 6;
        busy.denied = 5;
        let agent = AgentMetrics {
            id: Uuid::now_v7(),
            department_id: busy.id,
            name: "agent".into(),
            kind: "worker".into(),
            desired: "running".into(),
            status: None,
            sessions: 4,
            failed_sessions: 3,
            agent_hours: 9.0,
            model_calls: 10,
            tokens: 300_000,
            cost_micros: 0,
            messages_sent: 0,
            last_active: None,
        };
        let goal = GoalMetrics {
            id: Uuid::now_v7(),
            title: "Grow".into(),
            status: "active".into(),
            department_id: None,
            created_at: now - chrono::Duration::days(20),
            progress_at: None,
            progress_reports: 0,
        };
        let m = Metrics {
            days: 14,
            since: now,
            daily: vec![],
            departments: vec![idle.clone(), busy],
            agents: vec![agent],
            goals: vec![goal],
            proposals: ProposalCounts::default(),
        };
        let kinds: Vec<&str> = signals(&m, &[], &[], "USD", &Default::default())
            .iter()
            .map(|s| s.kind)
            .collect();
        for kind in [
            "idle_department",
            "unread_mail",
            "policy_mismatch",
            "failing_agent",
            "spend_without_output",
            "stale_goal",
        ] {
            assert!(kinds.contains(&kind), "{kind} in {kinds:?}");
        }
        assert_eq!(
            signals(&m, &[], &[], "USD", &Default::default())[0].severity,
            "high",
            "most severe first"
        );
        // A check-in means the idle department is expected to wake up.
        let check_in = OrgSchedule {
            id: Uuid::now_v7(),
            department_id: idle.id,
            agent_id: None,
            name: "daily".into(),
            message: "go".into(),
            every_minutes: 1440,
            next_run_at: now,
            last_run_at: None,
            enabled: true,
            created_by: "a".into(),
        };
        assert!(
            !signals(&m, &[check_in], &[], "USD", &Default::default())
                .iter()
                .any(|s| s.kind == "idle_department")
        );
    }

    #[test]
    fn sql_is_one_statement() {
        assert_eq!(clean_sql(" select 1; ").unwrap(), "select 1");
        assert!(clean_sql("select 1; drop table x").is_err());
        assert!(clean_sql(" ; ").is_err());
    }
}
