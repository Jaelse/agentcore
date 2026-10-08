//! Business data sources, operating activity, metrics and improvement
//! proposals.

use agentcore_core::{
    ActionResult, DataSource, DataSourceKind, DepartmentId, OrgAgentId, Proposal, ProposalAction,
    ProposalStatus,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::types::Json;
use uuid::Uuid;

use crate::{Result, Store, StoreError, admin_event, key_hint};

/// Largest uploaded table.
pub const MAX_TABLE_BYTES: usize = 5 * 1024 * 1024;

fn invalid(msg: impl Into<String>) -> StoreError {
    StoreError::Invalid(msg.into())
}

/// Ciphertext and nonce of a source's secret (both NULL when none is set).
type SealedSecret = (Option<Vec<u8>>, Option<Vec<u8>>);

fn secret_aad(id: Uuid) -> Vec<u8> {
    format!("data_source:{id}").into_bytes()
}

// ---- data sources -------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct DataSourceInput {
    pub name: String,
    pub kind: DataSourceKind,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub config: Value,
    /// Connection string (postgres) or API key (http).
    #[serde(default)]
    pub secret: Option<String>,
    /// CSV of a table source.
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub departments: Vec<DepartmentId>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DataSourceUpdate {
    pub description: Option<String>,
    pub config: Option<Value>,
    /// A new secret; an empty string removes it.
    pub secret: Option<String>,
    pub content: Option<String>,
    pub departments: Option<Vec<DepartmentId>>,
    pub enabled: Option<bool>,
}

#[derive(sqlx::FromRow)]
struct SourceRow {
    id: Uuid,
    name: String,
    kind: String,
    description: String,
    config: Json<Value>,
    secret_hint: Option<String>,
    departments: Vec<Uuid>,
    enabled: bool,
    updated_at: DateTime<Utc>,
    updated_by: String,
}

impl TryFrom<SourceRow> for DataSource {
    type Error = StoreError;

    fn try_from(r: SourceRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            name: r.name,
            kind: r.kind.parse().map_err(StoreError::Invalid)?,
            description: r.description,
            rows: r.config.0.get("rows").and_then(Value::as_u64),
            config: r.config.0,
            secret_hint: r.secret_hint,
            departments: r.departments,
            enabled: r.enabled,
            updated_at: r.updated_at,
            updated_by: r.updated_by,
        })
    }
}

const SOURCE_COLUMNS: &str = "id, name, kind, description, config, secret_hint, departments, enabled, updated_at, updated_by";

fn check_config(kind: DataSourceKind, config: &Value) -> Result<()> {
    if !config.is_object() {
        return Err(invalid("`config` must be an object"));
    }
    if kind == DataSourceKind::Http {
        let url = config["base_url"].as_str().unwrap_or_default();
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(invalid(
                "an http source needs `base_url` (http:// or https://)",
            ));
        }
    }
    Ok(())
}

fn check_secret(kind: DataSourceKind, secret: &str) -> Result<()> {
    if kind == DataSourceKind::Postgres
        && !(secret.starts_with("postgres://") || secret.starts_with("postgresql://"))
    {
        return Err(invalid(
            "a postgres source needs a connection string (postgres://user:password@host/db)",
        ));
    }
    Ok(())
}

fn check_content(content: &str) -> Result<()> {
    if content.len() > MAX_TABLE_BYTES {
        return Err(invalid(format!(
            "tables are limited to {} MB",
            MAX_TABLE_BYTES / 1024 / 1024
        )));
    }
    Ok(())
}

impl Store {
    pub async fn list_data_sources(&self) -> Result<Vec<DataSource>> {
        let rows: Vec<SourceRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {SOURCE_COLUMNS} FROM data_sources ORDER BY name"
        )))
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn get_data_source(&self, id: Uuid) -> Result<DataSource> {
        let row: Option<SourceRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {SOURCE_COLUMNS} FROM data_sources WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.ok_or_else(|| StoreError::NotFound(format!("data source {id}")))?
            .try_into()
    }

    /// Enabled sources a department may read.
    pub async fn data_sources_for(&self, department: DepartmentId) -> Result<Vec<DataSource>> {
        let rows: Vec<SourceRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {SOURCE_COLUMNS} FROM data_sources
             WHERE enabled AND $1 = ANY(departments) ORDER BY name"
        )))
        .bind(department)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn create_data_source(
        &self,
        input: DataSourceInput,
        actor: &str,
    ) -> Result<DataSource> {
        let name = input.name.trim();
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
            || name.is_empty()
            || name.len() > 63
            || name.starts_with(['-', '_'])
        {
            return Err(invalid(
                "a data source name uses a-z, 0-9, - and _ (up to 63 characters)",
            ));
        }
        let config = if input.config.is_null() {
            json!({})
        } else {
            input.config
        };
        check_config(input.kind, &config)?;
        let secret = input.secret.filter(|s| !s.trim().is_empty());
        match (&secret, input.kind) {
            (Some(s), kind) => check_secret(kind, s)?,
            (None, DataSourceKind::Postgres) => {
                return Err(invalid("a postgres source needs a connection string"));
            }
            _ => {}
        }
        if input.kind == DataSourceKind::Table {
            check_content(input.content.as_deref().unwrap_or_default())?;
        }
        let id = Uuid::now_v7();
        let sealed = match &secret {
            Some(s) => Some(self.cipher.encrypt(s.as_bytes(), &secret_aad(id))?),
            None => None,
        };
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "INSERT INTO data_sources (id, name, kind, description, config, secret_ciphertext,
                 secret_nonce, secret_hint, content, departments, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(id)
        .bind(name)
        .bind(input.kind.as_str())
        .bind(input.description.trim())
        .bind(Json(&config))
        .bind(sealed.as_ref().map(|(_, c)| c))
        .bind(sealed.as_ref().map(|(n, _)| n))
        .bind(secret.as_deref().map(key_hint))
        .bind(
            input
                .content
                .filter(|_| input.kind == DataSourceKind::Table),
        )
        .bind(&input.departments)
        .bind(actor)
        .execute(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                StoreError::Conflict(format!("data source `{name}`"))
            }
            _ => StoreError::Db(e),
        })?;
        admin_event(
            &mut tx,
            actor,
            "data_source.create",
            name,
            json!({ "id": id, "kind": input.kind.as_str(), "departments": input.departments }),
        )
        .await?;
        tx.commit().await?;
        self.get_data_source(id).await
    }

    pub async fn update_data_source(
        &self,
        id: Uuid,
        update: DataSourceUpdate,
        actor: &str,
    ) -> Result<DataSource> {
        let current = self.get_data_source(id).await?;
        let config = update.config.clone().unwrap_or(current.config.clone());
        check_config(current.kind, &config)?;
        if let Some(content) = &update.content {
            check_content(content)?;
        }
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "UPDATE data_sources SET description = $2, config = $3, departments = $4,
                 enabled = $5, content = COALESCE($6, content), updated_at = now(),
                 updated_by = $7
             WHERE id = $1",
        )
        .bind(id)
        .bind(
            update
                .description
                .as_deref()
                .map(str::trim)
                .unwrap_or(&current.description),
        )
        .bind(Json(&config))
        .bind(update.departments.as_ref().unwrap_or(&current.departments))
        .bind(update.enabled.unwrap_or(current.enabled))
        .bind(
            update
                .content
                .as_ref()
                .filter(|_| current.kind == DataSourceKind::Table),
        )
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        match update.secret.as_deref().map(str::trim) {
            Some("") if current.kind == DataSourceKind::Postgres => {
                return Err(invalid("a postgres source needs a connection string"));
            }
            Some("") => {
                sqlx::query(
                    "UPDATE data_sources SET secret_ciphertext = NULL, secret_nonce = NULL,
                         secret_hint = NULL WHERE id = $1",
                )
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
            Some(secret) => {
                check_secret(current.kind, secret)?;
                let (nonce, ciphertext) =
                    self.cipher.encrypt(secret.as_bytes(), &secret_aad(id))?;
                sqlx::query(
                    "UPDATE data_sources SET secret_ciphertext = $2, secret_nonce = $3,
                         secret_hint = $4 WHERE id = $1",
                )
                .bind(id)
                .bind(&ciphertext)
                .bind(&nonce)
                .bind(key_hint(secret))
                .execute(&mut *tx)
                .await?;
            }
            None => {}
        }
        admin_event(
            &mut tx,
            actor,
            "data_source.update",
            &current.name,
            json!({ "id": id, "secret_changed": update.secret.is_some() }),
        )
        .await?;
        tx.commit().await?;
        self.get_data_source(id).await
    }

    pub async fn delete_data_source(&self, id: Uuid, actor: &str) -> Result<()> {
        let current = self.get_data_source(id).await?;
        let mut tx = self.pool().begin().await?;
        sqlx::query("DELETE FROM data_sources WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        admin_event(
            &mut tx,
            actor,
            "data_source.delete",
            &current.name,
            json!({ "id": id }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The decrypted secret (server use only).
    pub async fn data_source_secret(&self, id: Uuid) -> Result<Option<String>> {
        let row: Option<SealedSecret> = sqlx::query_as(
            "SELECT secret_ciphertext, secret_nonce FROM data_sources WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        match row {
            Some((Some(ciphertext), Some(nonce))) => {
                let secret = self.cipher.decrypt(&nonce, &ciphertext, &secret_aad(id))?;
                Ok(Some(
                    String::from_utf8(secret).map_err(|_| StoreError::Crypto)?,
                ))
            }
            _ => Ok(None),
        }
    }

    /// The CSV of a table source.
    pub async fn data_source_content(&self, id: Uuid) -> Result<Option<String>> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT content FROM data_sources WHERE id = $1")
                .bind(id)
                .fetch_optional(self.pool())
                .await?;
        Ok(row.and_then(|(c,)| c))
    }

    // ---- activity -------------------------------------------------------------

    /// Record something a department agent did, for the metrics.
    pub async fn record_activity(&self, activity: Activity<'_>) -> Result<()> {
        sqlx::query(
            "INSERT INTO org_activity (department_id, agent_id, session_id, kind, value, detail)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(activity.department)
        .bind(activity.agent)
        .bind(activity.session)
        .bind(activity.kind)
        .bind(activity.value)
        .bind(activity.detail)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    // ---- proposals ------------------------------------------------------------

    pub async fn create_proposal(&self, new: NewProposal) -> Result<Proposal> {
        check_proposal(&new.title, &new.problem, &new.solution, &new.actions)?;
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO org_proposals (id, title, problem, evidence, solution, actions,
                 proposed_by, proposer_agent)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(new.title.trim())
        .bind(new.problem.trim())
        .bind(new.evidence.trim())
        .bind(new.solution.trim())
        .bind(Json(&new.actions))
        .bind(&new.proposed_by)
        .bind(new.proposer_agent)
        .execute(self.pool())
        .await?;
        self.get_proposal(id).await
    }

    pub async fn list_proposals(&self, status: Option<ProposalStatus>) -> Result<Vec<Proposal>> {
        let rows: Vec<ProposalRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {PROPOSAL_COLUMNS} FROM org_proposals
             WHERE $1::text IS NULL OR status = $1
             ORDER BY created_at DESC LIMIT 500"
        )))
        .bind(status.map(ProposalStatus::as_str))
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn get_proposal(&self, id: Uuid) -> Result<Proposal> {
        let row: Option<ProposalRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {PROPOSAL_COLUMNS} FROM org_proposals WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.ok_or_else(|| StoreError::NotFound(format!("proposal {id}")))?
            .try_into()
    }

    /// A new revision (by its proposer or a person); the previous one goes
    /// to the history. Only while the proposal is undecided.
    pub async fn revise_proposal(
        &self,
        id: Uuid,
        revision: ProposalRevision,
        by: &str,
    ) -> Result<Proposal> {
        let current = self.get_proposal(id).await?;
        if !current.status.is_pending() {
            return Err(StoreError::Conflict(format!(
                "a decision on proposal `{}` ({})",
                current.title,
                current.status.as_str()
            )));
        }
        let title = revision.title.unwrap_or(current.title.clone());
        let problem = revision.problem.unwrap_or(current.problem.clone());
        let evidence = revision.evidence.unwrap_or(current.evidence.clone());
        let solution = revision.solution.unwrap_or(current.solution.clone());
        let actions = revision.actions.unwrap_or(current.actions.clone());
        check_proposal(&title, &problem, &solution, &actions)?;
        let previous = json!({
            "revision": current.revision,
            "title": current.title,
            "problem": current.problem,
            "evidence": current.evidence,
            "solution": current.solution,
            "actions": current.actions,
            "feedback": current.feedback,
            "replaced_by": by,
            "note": revision.note,
            "at": Utc::now(),
        });
        let updated = sqlx::query(
            "UPDATE org_proposals SET title = $2, problem = $3, evidence = $4, solution = $5,
                 actions = $6, status = 'open', feedback = NULL, revision = revision + 1,
                 history = history || jsonb_build_array($7::jsonb), updated_at = now()
             WHERE id = $1 AND revision = $8 AND status IN ('open', 'changes_requested')",
        )
        .bind(id)
        .bind(title.trim())
        .bind(problem.trim())
        .bind(evidence.trim())
        .bind(solution.trim())
        .bind(Json(&actions))
        .bind(Json(previous))
        .bind(current.revision as i32)
        .execute(self.pool())
        .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::Conflict(
                "a newer revision of this proposal".into(),
            ));
        }
        self.get_proposal(id).await
    }

    /// A person asks the proposer to change the proposal.
    pub async fn request_proposal_changes(
        &self,
        id: Uuid,
        feedback: &str,
        by: &str,
    ) -> Result<Proposal> {
        let feedback = feedback.trim();
        if feedback.is_empty() {
            return Err(invalid("say what should change"));
        }
        let entry = json!({ "feedback": feedback, "by": by, "at": Utc::now() });
        let updated = sqlx::query(
            "UPDATE org_proposals SET status = 'changes_requested', feedback = $2,
                 history = history || jsonb_build_array($3::jsonb), updated_at = now()
             WHERE id = $1 AND status IN ('open', 'changes_requested')",
        )
        .bind(id)
        .bind(feedback)
        .bind(Json(entry))
        .execute(self.pool())
        .await?;
        if updated.rows_affected() == 0 {
            return self.decided(id).await;
        }
        self.get_proposal(id).await
    }

    pub async fn reject_proposal(&self, id: Uuid, reason: &str, by: &str) -> Result<Proposal> {
        let updated = sqlx::query(
            "UPDATE org_proposals SET status = 'rejected', feedback = NULLIF($2, ''),
                 decided_by = $3, decided_at = now(), updated_at = now()
             WHERE id = $1 AND status IN ('open', 'changes_requested')",
        )
        .bind(id)
        .bind(reason.trim())
        .bind(by)
        .execute(self.pool())
        .await?;
        if updated.rows_affected() == 0 {
            return self.decided(id).await;
        }
        self.get_proposal(id).await
    }

    /// Take an undecided proposal for applying (exactly one caller wins).
    /// `revision` guards against applying something other than what the
    /// person saw.
    pub async fn claim_proposal(&self, id: Uuid, revision: u32, by: &str) -> Result<Proposal> {
        let updated = sqlx::query(
            "UPDATE org_proposals SET status = 'applied', decided_by = $3, decided_at = now(),
                 updated_at = now()
             WHERE id = $1 AND revision = $2 AND status IN ('open', 'changes_requested')",
        )
        .bind(id)
        .bind(revision as i32)
        .bind(by)
        .execute(self.pool())
        .await?;
        if updated.rows_affected() == 0 {
            let current = self.get_proposal(id).await?;
            if current.status.is_pending() {
                return Err(StoreError::Conflict(format!(
                    "revision {} of this proposal (you saw revision {revision})",
                    current.revision
                )));
            }
            return self.decided(id).await;
        }
        self.get_proposal(id).await
    }

    /// Record what applying did.
    pub async fn finish_proposal(
        &self,
        id: Uuid,
        results: &[ActionResult],
        actor: &str,
    ) -> Result<Proposal> {
        let ok = results.iter().all(|r| r.ok);
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "UPDATE org_proposals SET status = $2, result = $3, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(if ok { "applied" } else { "failed" })
        .bind(Json(results))
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            "proposal.apply",
            &id.to_string(),
            json!({ "ok": ok, "results": results }),
        )
        .await?;
        tx.commit().await?;
        self.get_proposal(id).await
    }

    async fn decided(&self, id: Uuid) -> Result<Proposal> {
        let p = self.get_proposal(id).await?;
        Err(StoreError::Conflict(format!(
            "a decision on proposal `{}` ({})",
            p.title,
            p.status.as_str()
        )))
    }

    /// Kinds of change applied without asking, and who allowed them.
    pub async fn auto_apply(&self) -> Result<(Vec<String>, Option<String>)> {
        Ok(
            sqlx::query_as("SELECT auto_apply, auto_apply_by FROM org_settings")
                .fetch_one(self.pool())
                .await?,
        )
    }

    pub async fn set_auto_apply(&self, kinds: &[String], actor: &str) -> Result<Vec<String>> {
        for kind in kinds {
            if !ProposalAction::KINDS.contains(&kind.as_str()) {
                return Err(invalid(format!("unknown kind of change `{kind}`")));
            }
        }
        let mut tx = self.pool().begin().await?;
        sqlx::query("UPDATE org_settings SET auto_apply = $1, auto_apply_by = $2")
            .bind(kinds)
            .bind(actor)
            .execute(&mut *tx)
            .await?;
        admin_event(
            &mut tx,
            actor,
            "proposal.auto_apply",
            "organisation",
            json!({ "kinds": kinds }),
        )
        .await?;
        tx.commit().await?;
        Ok(kinds.to_vec())
    }
}

pub struct Activity<'a> {
    pub department: Option<DepartmentId>,
    pub agent: Option<OrgAgentId>,
    pub session: Option<Uuid>,
    pub kind: &'a str,
    pub value: Option<i64>,
    pub detail: Option<&'a str>,
}

pub struct NewProposal {
    pub title: String,
    pub problem: String,
    pub evidence: String,
    pub solution: String,
    pub actions: Vec<ProposalAction>,
    pub proposed_by: String,
    pub proposer_agent: Option<OrgAgentId>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProposalRevision {
    pub title: Option<String>,
    pub problem: Option<String>,
    pub evidence: Option<String>,
    pub solution: Option<String>,
    pub actions: Option<Vec<ProposalAction>>,
    /// What changed and why.
    #[serde(default)]
    pub note: String,
}

fn check_proposal(
    title: &str,
    problem: &str,
    solution: &str,
    actions: &[ProposalAction],
) -> Result<()> {
    if title.trim().is_empty() || title.chars().count() > 200 {
        return Err(invalid("a proposal needs a title of up to 200 characters"));
    }
    if problem.trim().is_empty() || solution.trim().is_empty() {
        return Err(invalid("a proposal needs a problem and a solution"));
    }
    if actions.len() > 20 {
        return Err(invalid("a proposal makes at most 20 changes"));
    }
    Ok(())
}

const PROPOSAL_COLUMNS: &str = "id, title, problem, evidence, solution, actions, status, \
    revision, history, feedback, proposed_by, proposer_agent, decided_by, decided_at, result, \
    created_at, updated_at";

#[derive(sqlx::FromRow)]
struct ProposalRow {
    id: Uuid,
    title: String,
    problem: String,
    evidence: String,
    solution: String,
    actions: Json<Vec<ProposalAction>>,
    status: String,
    revision: i32,
    history: Json<Vec<Value>>,
    feedback: Option<String>,
    proposed_by: String,
    proposer_agent: Option<Uuid>,
    decided_by: Option<String>,
    decided_at: Option<DateTime<Utc>>,
    result: Option<Json<Vec<ActionResult>>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<ProposalRow> for Proposal {
    type Error = StoreError;

    fn try_from(r: ProposalRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            title: r.title,
            problem: r.problem,
            evidence: r.evidence,
            solution: r.solution,
            actions: r.actions.0,
            status: r.status.parse().map_err(StoreError::Invalid)?,
            revision: r.revision.max(1) as u32,
            history: r.history.0,
            feedback: r.feedback,
            proposed_by: r.proposed_by,
            proposer_agent: r.proposer_agent,
            decided_by: r.decided_by,
            decided_at: r.decided_at,
            result: r.result.map(|j| j.0),
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

// ---- metrics ------------------------------------------------------------------

/// How the organisation did over the last `days` days.
#[derive(Debug, Clone, Serialize)]
pub struct Metrics {
    pub days: u32,
    pub since: DateTime<Utc>,
    pub daily: Vec<DayMetrics>,
    pub departments: Vec<DepartmentMetrics>,
    pub agents: Vec<AgentMetrics>,
    pub goals: Vec<GoalMetrics>,
    pub proposals: ProposalCounts,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DayMetrics {
    pub day: DateTime<Utc>,
    pub messages: i64,
    pub inter_department: i64,
    pub sessions: i64,
    pub failed_sessions: i64,
    pub model_calls: i64,
    pub tokens: i64,
    /// What the model calls cost (micros of the organisation's currency).
    pub cost_micros: i64,
    pub denied: i64,
    pub approvals: i64,
    pub progress_reports: i64,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DepartmentMetrics {
    pub id: Uuid,
    pub name: String,
    pub state: String,
    pub workers: i64,
    pub active_agents: i64,
    pub asleep_agents: i64,
    pub sessions: i64,
    pub failed_sessions: i64,
    /// Time agents' sessions ran in the window (sandboxes alive).
    pub agent_hours: f64,
    pub model_calls: i64,
    pub tokens: i64,
    pub cost_micros: i64,
    pub messages_sent: i64,
    pub messages_received: i64,
    /// Average time a message waited for its recipient (seconds).
    pub avg_wait_secs: Option<f64>,
    pub waiting_now: i64,
    pub denied: i64,
    pub approvals: i64,
    pub avg_approval_wait_secs: Option<f64>,
    pub progress_reports: i64,
    pub data_queries: i64,
    pub last_activity: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AgentMetrics {
    pub id: Uuid,
    pub department_id: Uuid,
    pub name: String,
    pub kind: String,
    pub desired: String,
    pub status: Option<String>,
    pub sessions: i64,
    pub failed_sessions: i64,
    pub agent_hours: f64,
    pub model_calls: i64,
    pub tokens: i64,
    pub cost_micros: i64,
    pub messages_sent: i64,
    pub last_active: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct GoalMetrics {
    pub id: Uuid,
    pub title: String,
    pub status: String,
    pub department_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub progress_at: Option<DateTime<Utc>>,
    pub progress_reports: i64,
}

#[derive(Debug, Clone, Default, Serialize, sqlx::FromRow)]
pub struct ProposalCounts {
    pub open: i64,
    pub changes_requested: i64,
    pub applied: i64,
    pub rejected: i64,
    pub failed: i64,
}

/// Sessions of department agents in the window, with the department and
/// agent they belong to.
const ORG_SESSIONS: &str = "SELECT s.id, s.status, s.created_at, s.ended_at,
        (s.context->>'department_id') AS dept, (s.context->>'org_agent_id') AS agent
    FROM sessions s
    WHERE s.context ? 'department_id' AND COALESCE(s.ended_at, now()) >= $1";

impl Store {
    pub async fn org_metrics(&self, days: u32) -> Result<Metrics> {
        let days = days.clamp(1, 365);
        let since: DateTime<Utc> =
            sqlx::query_scalar("SELECT date_trunc('day', now()) - make_interval(days => $1 - 1)")
                .bind(days as i32)
                .fetch_one(self.pool())
                .await?;

        let daily: Vec<DayMetrics> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "WITH days AS (
                 SELECT generate_series($1::timestamptz, date_trunc('day', now()), interval '1 day') AS day
             ), s AS ({ORG_SESSIONS})
             SELECT d.day,
                 (SELECT count(*) FROM org_messages m
                   WHERE m.created_at >= d.day AND m.created_at < d.day + interval '1 day') AS messages,
                 (SELECT count(*) FROM org_messages m WHERE m.scope = 'inter_department'
                   AND m.created_at >= d.day AND m.created_at < d.day + interval '1 day') AS inter_department,
                 (SELECT count(*) FROM s
                   WHERE s.created_at >= d.day AND s.created_at < d.day + interval '1 day') AS sessions,
                 (SELECT count(*) FROM s WHERE s.status = 'failed'
                   AND s.ended_at >= d.day AND s.ended_at < d.day + interval '1 day') AS failed_sessions,
                 (SELECT count(*) FROM model_calls c JOIN s ON s.id = c.session_id
                   WHERE c.started_at >= d.day AND c.started_at < d.day + interval '1 day') AS model_calls,
                 (SELECT COALESCE(sum(COALESCE(c.input_tokens, 0) + COALESCE(c.output_tokens, 0)), 0)::bigint
                    FROM model_calls c JOIN s ON s.id = c.session_id
                   WHERE c.started_at >= d.day AND c.started_at < d.day + interval '1 day') AS tokens,
                 (SELECT COALESCE(sum(c.cost_micros), 0)::bigint
                    FROM model_calls c JOIN s ON s.id = c.session_id
                   WHERE c.started_at >= d.day AND c.started_at < d.day + interval '1 day') AS cost_micros,
                 (SELECT count(*) FROM org_activity a WHERE a.kind = 'denied'
                   AND a.at >= d.day AND a.at < d.day + interval '1 day') AS denied,
                 (SELECT count(*) FROM org_activity a WHERE a.kind = 'approval'
                   AND a.at >= d.day AND a.at < d.day + interval '1 day') AS approvals,
                 (SELECT count(*) FROM org_activity a WHERE a.kind = 'progress'
                   AND a.at >= d.day AND a.at < d.day + interval '1 day') AS progress_reports
             FROM days d ORDER BY d.day"
        )))
        .bind(since)
        .fetch_all(self.pool())
        .await?;

        let departments: Vec<DepartmentMetrics> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "WITH s AS ({ORG_SESSIONS})
             SELECT d.id, d.name, d.state,
                 (SELECT count(*) FROM org_agents a
                   WHERE a.department_id = d.id AND a.kind = 'worker') AS workers,
                 (SELECT count(*) FROM org_agents a WHERE a.department_id = d.id
                   AND a.desired <> 'stopped' AND a.status IS DISTINCT FROM 'asleep') AS active_agents,
                 (SELECT count(*) FROM org_agents a WHERE a.department_id = d.id
                   AND a.desired <> 'stopped' AND a.status = 'asleep') AS asleep_agents,
                 (SELECT count(*) FROM s WHERE s.dept = d.id::text AND s.created_at >= $1) AS sessions,
                 (SELECT count(*) FROM s WHERE s.dept = d.id::text AND s.status = 'failed'
                   AND s.ended_at >= $1) AS failed_sessions,
                 (SELECT COALESCE(sum(extract(epoch FROM COALESCE(s.ended_at, now())
                          - greatest(s.created_at, $1))), 0)::float8 / 3600
                    FROM s WHERE s.dept = d.id::text) AS agent_hours,
                 (SELECT count(*) FROM model_calls c JOIN s ON s.id = c.session_id
                   WHERE s.dept = d.id::text AND c.started_at >= $1) AS model_calls,
                 (SELECT COALESCE(sum(COALESCE(c.input_tokens, 0) + COALESCE(c.output_tokens, 0)), 0)::bigint
                    FROM model_calls c JOIN s ON s.id = c.session_id
                   WHERE s.dept = d.id::text AND c.started_at >= $1) AS tokens,
                 (SELECT COALESCE(sum(c.cost_micros), 0)::bigint
                    FROM model_calls c JOIN s ON s.id = c.session_id
                   WHERE s.dept = d.id::text AND c.started_at >= $1) AS cost_micros,
                 (SELECT count(*) FROM org_messages m WHERE m.from_department = d.id
                   AND m.from_agent IS NOT NULL AND m.created_at >= $1) AS messages_sent,
                 (SELECT count(*) FROM org_inbox i JOIN org_messages m ON m.id = i.message_id
                   WHERE i.department_id = d.id AND m.created_at >= $1) AS messages_received,
                 (SELECT avg(extract(epoch FROM COALESCE(i.delivered_at, now()) - m.created_at))::float8
                    FROM org_inbox i JOIN org_messages m ON m.id = i.message_id
                   WHERE i.department_id = d.id AND m.created_at >= $1) AS avg_wait_secs,
                 (SELECT count(*) FROM org_inbox i
                   WHERE i.department_id = d.id AND i.delivered_at IS NULL) AS waiting_now,
                 (SELECT count(*) FROM org_activity a WHERE a.department_id = d.id
                   AND a.kind = 'denied' AND a.at >= $1) AS denied,
                 (SELECT count(*) FROM org_activity a WHERE a.department_id = d.id
                   AND a.kind = 'approval' AND a.at >= $1) AS approvals,
                 (SELECT avg(a.value)::float8 / 1000 FROM org_activity a WHERE a.department_id = d.id
                   AND a.kind = 'approval' AND a.at >= $1) AS avg_approval_wait_secs,
                 (SELECT count(*) FROM org_activity a WHERE a.department_id = d.id
                   AND a.kind = 'progress' AND a.at >= $1) AS progress_reports,
                 (SELECT count(*) FROM org_activity a WHERE a.department_id = d.id
                   AND a.kind = 'data_query' AND a.at >= $1) AS data_queries,
                 greatest(
                   (SELECT max(m.created_at) FROM org_messages m
                     WHERE m.from_department = d.id AND m.from_agent IS NOT NULL),
                   (SELECT max(COALESCE(s.ended_at, now())) FROM s WHERE s.dept = d.id::text)
                 ) AS last_activity
             FROM departments d ORDER BY lower(d.name)"
        )))
        .bind(since)
        .fetch_all(self.pool())
        .await?;

        let agents: Vec<AgentMetrics> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "WITH s AS ({ORG_SESSIONS})
             SELECT a.id, a.department_id, a.name, a.kind, a.desired, a.status,
                 (SELECT count(*) FROM s WHERE s.agent = a.id::text AND s.created_at >= $1) AS sessions,
                 (SELECT count(*) FROM s WHERE s.agent = a.id::text AND s.status = 'failed'
                   AND s.ended_at >= $1) AS failed_sessions,
                 (SELECT COALESCE(sum(extract(epoch FROM COALESCE(s.ended_at, now())
                          - greatest(s.created_at, $1))), 0)::float8 / 3600
                    FROM s WHERE s.agent = a.id::text) AS agent_hours,
                 (SELECT count(*) FROM model_calls c JOIN s ON s.id = c.session_id
                   WHERE s.agent = a.id::text AND c.started_at >= $1) AS model_calls,
                 (SELECT COALESCE(sum(COALESCE(c.input_tokens, 0) + COALESCE(c.output_tokens, 0)), 0)::bigint
                    FROM model_calls c JOIN s ON s.id = c.session_id
                   WHERE s.agent = a.id::text AND c.started_at >= $1) AS tokens,
                 (SELECT COALESCE(sum(c.cost_micros), 0)::bigint
                    FROM model_calls c JOIN s ON s.id = c.session_id
                   WHERE s.agent = a.id::text AND c.started_at >= $1) AS cost_micros,
                 (SELECT count(*) FROM org_messages m WHERE m.from_agent = a.id
                   AND m.created_at >= $1) AS messages_sent,
                 greatest(
                   (SELECT max(m.created_at) FROM org_messages m WHERE m.from_agent = a.id),
                   (SELECT max(COALESCE(s.ended_at, now())) FROM s WHERE s.agent = a.id::text)
                 ) AS last_active
             FROM org_agents a ORDER BY a.department_id, a.kind DESC, a.name"
        )))
        .bind(since)
        .fetch_all(self.pool())
        .await?;

        let goals: Vec<GoalMetrics> = sqlx::query_as(
            "SELECT g.id, g.title, g.status, g.department_id, g.created_at, g.progress_at,
                 (SELECT count(*) FROM org_activity a WHERE a.kind = 'progress'
                   AND a.detail = g.id::text AND a.at >= $1) AS progress_reports
             FROM org_goals g ORDER BY g.created_at",
        )
        .bind(since)
        .fetch_all(self.pool())
        .await?;

        let proposals: ProposalCounts = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE status = 'open') AS open,
                 count(*) FILTER (WHERE status = 'changes_requested') AS changes_requested,
                 count(*) FILTER (WHERE status = 'applied') AS applied,
                 count(*) FILTER (WHERE status = 'rejected') AS rejected,
                 count(*) FILTER (WHERE status = 'failed') AS failed
             FROM org_proposals",
        )
        .fetch_one(self.pool())
        .await?;

        Ok(Metrics {
            days,
            since,
            daily,
            departments,
            agents,
            goals,
            proposals,
        })
    }
}
