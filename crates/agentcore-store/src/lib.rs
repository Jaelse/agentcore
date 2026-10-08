//! PostgreSQL persistence.
//!
//! * `sessions`: index of every session (the audit file remains the
//!   authoritative, hash-chained record of what happened).
//! * `model_providers`: upstream LLM providers; API keys encrypted at rest.
//! * `model_calls`: every LLM request/response relayed by the model gateway.
//! * `admin_events`: who changed configuration.

mod agents;
mod crypto;
mod insights;
mod org;
mod rhythm;
mod spending;
mod teamwork;
#[doc(hidden)]
pub mod testing;

use std::path::PathBuf;

use agentcore_core::{
    ModelCallOutcome, ModelEndpoint, Principal, ProviderKind, SessionId, SessionInfo, SessionStatus,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::types::Json;
use uuid::Uuid;

pub use agents::InstalledAgent;
pub use crypto::{Cipher, MASTER_KEY_ENV};
pub use insights::{
    Activity, AgentMetrics, DataSourceInput, DataSourceUpdate, DayMetrics, DepartmentMetrics,
    GoalMetrics, MAX_TABLE_BYTES, Metrics, NewProposal, ProposalCounts, ProposalRevision,
};
pub use org::{
    AgentInput, AgentScope, COMMUNICATOR_NAME, DepartmentInput, FileInfo, MAX_FILE_BYTES,
    MessageFilter, NewMessage, ORG_CHANNEL, RESERVED_NAMES, valid_agent_name,
};
pub use rhythm::{GoalInput, GoalUpdate, ScheduleInput, ScheduleUpdate};
pub use spending::{BudgetInput, BudgetUpdate, PriceInput};
pub use teamwork::{
    BoardColumns, BoardConfig, GitHubConfig, GitHubConnection, GitHubUpdate, Project, ProjectInput,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("database migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("master key: {0}")]
    MasterKey(String),
    #[error("failed to decrypt a secret (wrong master key?)")]
    Crypto,
    #[error("{0}")]
    Invalid(String),
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0} already exists")]
    Conflict(String),
    /// A limit set by an admin would be exceeded.
    #[error("{0}")]
    Limit(String),
    /// No node can take the work right now.
    #[error("{0}")]
    Unavailable(String),
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

fn status_str(status: SessionStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn parse_status(s: &str) -> SessionStatus {
    serde_json::from_value(serde_json::Value::String(s.into())).unwrap_or(SessionStatus::Failed)
}

fn outcome_str(outcome: ModelCallOutcome) -> String {
    serde_json::to_value(outcome)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: Uuid,
    agent: String,
    task: String,
    policy: String,
    policy_digest: String,
    status: String,
    created_by: Json<Principal>,
    created_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    actions: i64,
    model_calls: i64,
    audit_path: String,
    context: Json<agentcore_core::SessionContext>,
    node: Option<String>,
}

/// A session as persisted.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub info: SessionInfo,
    pub policy_digest: String,
    pub audit_path: PathBuf,
    /// Node that runs (or ran) the session.
    pub node: Option<String>,
}

impl From<SessionRow> for SessionRecord {
    fn from(row: SessionRow) -> Self {
        Self {
            info: SessionInfo {
                id: row.id,
                agent: row.agent,
                task: row.task,
                policy: row.policy,
                status: parse_status(&row.status),
                created_by: row.created_by.0,
                created_at: row.created_at,
                ended_at: row.ended_at,
                pending_approvals: 0,
                actions: row.actions.max(0) as u64,
                model_calls: row.model_calls.max(0) as u64,
                context: row.context.0,
            },
            policy_digest: row.policy_digest,
            audit_path: row.audit_path.into(),
            node: row.node,
        }
    }
}

/// Provider as shown to operators. Never contains the API key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    /// Last four characters of the key, for recognition.
    pub api_key_hint: String,
    /// Model name globs the provider may be used with; empty = any.
    pub allowed_models: Vec<String>,
    pub enabled: bool,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

impl ProviderInfo {
    pub fn endpoint(&self) -> ModelEndpoint {
        ModelEndpoint {
            name: self.name.clone(),
            kind: self.kind,
        }
    }
}

#[derive(sqlx::FromRow)]
struct ProviderRow {
    name: String,
    kind: String,
    base_url: String,
    api_key_ciphertext: Vec<u8>,
    api_key_nonce: Vec<u8>,
    api_key_hint: String,
    allowed_models: Vec<String>,
    enabled: bool,
    updated_at: DateTime<Utc>,
    updated_by: String,
}

impl ProviderRow {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: self.name.clone(),
            kind: self.kind.parse().unwrap_or(ProviderKind::Openai),
            base_url: self.base_url.clone(),
            api_key_hint: self.api_key_hint.clone(),
            allowed_models: self.allowed_models.clone(),
            enabled: self.enabled,
            updated_at: self.updated_at,
            updated_by: self.updated_by.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewProvider {
    pub name: String,
    pub kind: ProviderKind,
    #[serde(default)]
    pub base_url: Option<String>,
    /// May be empty for kinds with a default key (OpenCode Zen: `public`).
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

/// Partial update; `None` leaves a field unchanged.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProviderUpdate {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub allowed_models: Option<Vec<String>>,
    pub enabled: Option<bool>,
}

/// A model call as recorded by the gateway.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ModelCallRecord {
    pub id: Uuid,
    pub session_id: SessionId,
    pub provider: String,
    pub model: Option<String>,
    pub method: String,
    pub path: String,
    pub http_status: Option<i32>,
    pub outcome: String,
    pub detail: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub duration_ms: i64,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    pub bodies_truncated: bool,
    pub request_sha256: String,
    pub response_sha256: Option<String>,
}

impl ModelCallRecord {
    pub fn set_outcome(&mut self, outcome: ModelCallOutcome) {
        self.outcome = outcome_str(outcome);
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AdminEvent {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub details: Json<serde_json::Value>,
}

fn validate_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(StoreError::Invalid(
            "base_url must start with https:// or http://".into(),
        ));
    }
    Ok(url.to_string())
}

pub(crate) fn key_hint(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(4)..].iter().collect();
    format!("…{tail}")
}

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
    cipher: Cipher,
}

impl Store {
    /// Connect and apply pending migrations.
    pub async fn connect(url: &str, cipher: Cipher) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .connect(url)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool, cipher })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // ---- sessions -----------------------------------------------------------

    pub async fn upsert_session(&self, record: &SessionRecord) -> Result<()> {
        let info = &record.info;
        sqlx::query(
            "INSERT INTO sessions (id, agent, task, policy, policy_digest, status, created_by,
                                   created_at, ended_at, actions, model_calls, audit_path, context, node)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
             ON CONFLICT (id) DO UPDATE SET status = EXCLUDED.status,
                 ended_at = EXCLUDED.ended_at, actions = EXCLUDED.actions,
                 model_calls = EXCLUDED.model_calls, context = EXCLUDED.context",
        )
        .bind(info.id)
        .bind(&info.agent)
        .bind(&info.task)
        .bind(&info.policy)
        .bind(&record.policy_digest)
        .bind(status_str(info.status))
        .bind(Json(&info.created_by))
        .bind(info.created_at)
        .bind(info.ended_at)
        .bind(info.actions as i64)
        .bind(info.model_calls as i64)
        .bind(record.audit_path.to_string_lossy().as_ref())
        .bind(Json(&info.context))
        .bind(&record.node)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_sessions(&self, limit: i64) -> Result<Vec<SessionRecord>> {
        let rows: Vec<SessionRow> = sqlx::query_as(
            "SELECT id, agent, task, policy, policy_digest, status, created_by, created_at,
                    ended_at, actions, model_calls, audit_path, context, node
             FROM sessions ORDER BY created_at DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_session(&self, id: SessionId) -> Result<Option<SessionRecord>> {
        let row: Option<SessionRow> = sqlx::query_as(
            "SELECT id, agent, task, policy, policy_digest, status, created_by, created_at,
                    ended_at, actions, model_calls, audit_path, context, node
             FROM sessions WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Mark sessions of this node that were live when it last stopped as
    /// failed and return them, so their audit logs can be closed. Sessions
    /// of other nodes are left alone (sessions without a node predate
    /// clustering and belong to whichever node starts first).
    pub async fn fail_interrupted_sessions(&self, node: &str) -> Result<Vec<SessionRecord>> {
        let rows: Vec<SessionRow> = sqlx::query_as(
            "UPDATE sessions SET status = 'failed', ended_at = now()
             WHERE status NOT IN ('stopped', 'completed', 'failed')
               AND (node = $1 OR node IS NULL)
             RETURNING id, agent, task, policy, policy_digest, status, created_by, created_at,
                       ended_at, actions, model_calls, audit_path, context, node",
        )
        .bind(node)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    // ---- model providers --------------------------------------------------

    const PROVIDER_COLUMNS: &str = "name, kind, base_url, api_key_ciphertext, api_key_nonce,
        api_key_hint, allowed_models, enabled, updated_at, updated_by";

    pub async fn list_providers(&self) -> Result<Vec<ProviderInfo>> {
        let rows: Vec<ProviderRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {} FROM model_providers ORDER BY name",
            Self::PROVIDER_COLUMNS
        )))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(ProviderRow::info).collect())
    }

    /// Enabled providers, as handed to new sessions.
    pub async fn enabled_endpoints(&self) -> Result<Vec<ModelEndpoint>> {
        Ok(self
            .list_providers()
            .await?
            .into_iter()
            .filter(|p| p.enabled)
            .map(|p| p.endpoint())
            .collect())
    }

    /// Provider plus its decrypted API key, for the gateway only.
    pub async fn provider_credentials(&self, name: &str) -> Result<Option<(ProviderInfo, String)>> {
        let row: Option<ProviderRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {} FROM model_providers WHERE name = $1",
            Self::PROVIDER_COLUMNS
        )))
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let key = self.cipher.decrypt(
            &row.api_key_nonce,
            &row.api_key_ciphertext,
            row.name.as_bytes(),
        )?;
        let key = String::from_utf8(key).map_err(|_| StoreError::Crypto)?;
        Ok(Some((row.info(), key)))
    }

    pub async fn create_provider(&self, new: NewProvider, actor: &str) -> Result<ProviderInfo> {
        let name = new.name.trim().to_ascii_lowercase();
        let valid_name = !name.is_empty()
            && name.len() <= 63
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            && name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        if !valid_name {
            return Err(StoreError::Invalid(
                "name must be 1-63 characters of a-z, 0-9, - or _".into(),
            ));
        }
        let api_key = match new.api_key.trim() {
            "" => new
                .kind
                .default_api_key()
                .ok_or_else(|| StoreError::Invalid("api_key must not be empty".into()))?,
            key => key,
        };
        let base_url = validate_url(
            new.base_url
                .as_deref()
                .filter(|u| !u.trim().is_empty())
                .unwrap_or(new.kind.default_base_url()),
        )?;
        let (nonce, ciphertext) = self.cipher.encrypt(api_key.as_bytes(), name.as_bytes())?;
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO model_providers (name, kind, base_url, api_key_ciphertext, api_key_nonce,
                                          api_key_hint, allowed_models, enabled, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(&name)
        .bind(new.kind.as_str())
        .bind(&base_url)
        .bind(&ciphertext)
        .bind(&nonce)
        .bind(key_hint(api_key))
        .bind(&new.allowed_models)
        .bind(new.enabled)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            return Err(StoreError::Conflict(format!("provider `{name}`")));
        }
        admin_event(
            &mut tx,
            actor,
            "provider.create",
            &name,
            serde_json::json!({
                "kind": new.kind, "base_url": base_url,
                "allowed_models": new.allowed_models, "enabled": new.enabled,
            }),
        )
        .await?;
        tx.commit().await?;
        self.provider_info(&name).await
    }

    pub async fn update_provider(
        &self,
        name: &str,
        update: ProviderUpdate,
        actor: &str,
    ) -> Result<ProviderInfo> {
        let mut tx = self.pool.begin().await?;
        let exists: Option<(String,)> =
            sqlx::query_as("SELECT name FROM model_providers WHERE name = $1 FOR UPDATE")
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?;
        if exists.is_none() {
            return Err(StoreError::NotFound(format!("provider `{name}`")));
        }
        let mut changed = serde_json::Map::new();
        if let Some(url) = &update.base_url {
            let url = validate_url(url)?;
            sqlx::query("UPDATE model_providers SET base_url = $2 WHERE name = $1")
                .bind(name)
                .bind(&url)
                .execute(&mut *tx)
                .await?;
            changed.insert("base_url".into(), url.into());
        }
        if let Some(key) = update
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        {
            let (nonce, ciphertext) = self.cipher.encrypt(key.as_bytes(), name.as_bytes())?;
            sqlx::query(
                "UPDATE model_providers
                 SET api_key_ciphertext = $2, api_key_nonce = $3, api_key_hint = $4
                 WHERE name = $1",
            )
            .bind(name)
            .bind(&ciphertext)
            .bind(&nonce)
            .bind(key_hint(key))
            .execute(&mut *tx)
            .await?;
            changed.insert("api_key".into(), "rotated".into());
        }
        if let Some(models) = &update.allowed_models {
            sqlx::query("UPDATE model_providers SET allowed_models = $2 WHERE name = $1")
                .bind(name)
                .bind(models)
                .execute(&mut *tx)
                .await?;
            changed.insert("allowed_models".into(), serde_json::json!(models));
        }
        if let Some(enabled) = update.enabled {
            sqlx::query("UPDATE model_providers SET enabled = $2 WHERE name = $1")
                .bind(name)
                .bind(enabled)
                .execute(&mut *tx)
                .await?;
            changed.insert("enabled".into(), enabled.into());
        }
        sqlx::query(
            "UPDATE model_providers SET updated_at = now(), updated_by = $2 WHERE name = $1",
        )
        .bind(name)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        admin_event(&mut tx, actor, "provider.update", name, changed.into()).await?;
        tx.commit().await?;
        self.provider_info(name).await
    }

    pub async fn delete_provider(&self, name: &str, actor: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("DELETE FROM model_providers WHERE name = $1")
            .bind(name)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("provider `{name}`")));
        }
        admin_event(
            &mut tx,
            actor,
            "provider.delete",
            name,
            serde_json::json!({}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn provider_info(&self, name: &str) -> Result<ProviderInfo> {
        self.list_providers()
            .await?
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| StoreError::NotFound(format!("provider `{name}`")))
    }

    // ---- model calls ----------------------------------------------------------

    pub async fn insert_model_call(&self, call: &ModelCallRecord) -> Result<()> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO model_calls (id, session_id, provider, model, method, path, http_status,
                     outcome, detail, input_tokens, output_tokens, started_at, duration_ms,
                     request_body, response_body, bodies_truncated, request_sha256, response_sha256,
                     cost_micros)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                         $17, $18, {})",
            // Priced now: later price changes do not rewrite history.
            spending::price_sql("$3", "$4", "$10::bigint", "$11::bigint")
        )))
        .bind(call.id)
        .bind(call.session_id)
        .bind(&call.provider)
        .bind(&call.model)
        .bind(&call.method)
        .bind(&call.path)
        .bind(call.http_status)
        .bind(&call.outcome)
        .bind(&call.detail)
        .bind(call.input_tokens)
        .bind(call.output_tokens)
        .bind(call.started_at)
        .bind(call.duration_ms)
        .bind(&call.request_body)
        .bind(&call.response_body)
        .bind(call.bodies_truncated)
        .bind(&call.request_sha256)
        .bind(&call.response_sha256)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_model_call(
        &self,
        session_id: SessionId,
        id: Uuid,
    ) -> Result<Option<ModelCallRecord>> {
        Ok(
            sqlx::query_as("SELECT * FROM model_calls WHERE session_id = $1 AND id = $2")
                .bind(session_id)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn admin_events(&self, limit: i64) -> Result<Vec<AdminEvent>> {
        Ok(
            sqlx::query_as("SELECT * FROM admin_events ORDER BY id DESC LIMIT $1")
                .bind(limit)
                .fetch_all(&self.pool)
                .await?,
        )
    }
}

pub(crate) async fn admin_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: &str,
    action: &str,
    target: &str,
    details: serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO admin_events (actor, action, target, details) VALUES ($1, $2, $3, $4)",
    )
    .bind(actor)
    .bind(action)
    .bind(target)
    .bind(Json(details))
    .execute(&mut **tx)
    .await?;
    Ok(())
}
