//! Outward channels and the outbox.

use agentcore_core::{Channel, ChannelKind, DepartmentId, OrgAgentId, OutboxItem, OutboxStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::types::Json;
use uuid::Uuid;

use crate::{Result, Store, StoreError, admin_event, key_hint};

/// Appended to every message unless a channel says otherwise.
pub const DEFAULT_DISCLOSURE: &str = "This message was written by an AI agent.";

fn invalid(msg: impl Into<String>) -> StoreError {
    StoreError::Invalid(msg.into())
}

fn secret_aad(id: Uuid) -> Vec<u8> {
    format!("channel:{id}").into_bytes()
}

/// Ciphertext and nonce of a channel's secret (both NULL when none is set).
type SealedSecret = (Option<Vec<u8>>, Option<Vec<u8>>);

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelInput {
    pub name: String,
    pub kind: ChannelKind,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub config: Value,
    /// SMTP password, Slack webhook URL, or the webhook's header value.
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub departments: Vec<DepartmentId>,
    #[serde(default = "yes")]
    pub requires_approval: bool,
    #[serde(default = "fifty")]
    pub max_per_day: u32,
    #[serde(default)]
    pub disclosure: Option<String>,
}

fn yes() -> bool {
    true
}

fn fifty() -> u32 {
    50
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChannelUpdate {
    pub description: Option<String>,
    pub config: Option<Value>,
    /// A new secret; an empty string removes it.
    pub secret: Option<String>,
    pub departments: Option<Vec<DepartmentId>>,
    pub requires_approval: Option<bool>,
    pub max_per_day: Option<u32>,
    pub disclosure: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(sqlx::FromRow)]
struct ChannelRow {
    id: Uuid,
    name: String,
    kind: String,
    description: String,
    config: Json<Value>,
    secret_hint: Option<String>,
    departments: Vec<Uuid>,
    requires_approval: bool,
    max_per_day: i32,
    disclosure: String,
    enabled: bool,
    updated_at: DateTime<Utc>,
    updated_by: String,
}

impl TryFrom<ChannelRow> for Channel {
    type Error = StoreError;

    fn try_from(r: ChannelRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            name: r.name,
            kind: r.kind.parse().map_err(StoreError::Invalid)?,
            description: r.description,
            config: r.config.0,
            secret_hint: r.secret_hint,
            departments: r.departments,
            requires_approval: r.requires_approval,
            max_per_day: r.max_per_day.max(0) as u32,
            disclosure: r.disclosure,
            enabled: r.enabled,
            updated_at: r.updated_at,
            updated_by: r.updated_by,
        })
    }
}

const CHANNEL_COLUMNS: &str = "id, name, kind, description, config, secret_hint, departments, \
    requires_approval, max_per_day, disclosure, enabled, updated_at, updated_by";

fn check_config(kind: ChannelKind, config: &Value) -> Result<()> {
    if !config.is_object() {
        return Err(invalid("`config` must be an object"));
    }
    match kind {
        ChannelKind::Email => {
            if config["host"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .is_empty()
            {
                return Err(invalid("an email channel needs the SMTP `host`"));
            }
            let from = config["from"].as_str().unwrap_or_default();
            if !from.contains('@') || from.contains(['\r', '\n']) {
                return Err(invalid("an email channel needs a `from` address"));
            }
            if let Some(tls) = config.get("tls").and_then(Value::as_str)
                && !["starttls", "tls", "none"].contains(&tls)
            {
                return Err(invalid("`tls` is starttls, tls or none"));
            }
            if let Some(port) = config.get("port")
                && !port.as_u64().is_some_and(|p| (1..=65535).contains(&p))
            {
                return Err(invalid("`port` is 1 to 65535"));
            }
        }
        ChannelKind::Webhook => {
            let url = config["url"].as_str().unwrap_or_default();
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err(invalid(
                    "a webhook channel needs a `url` (http:// or https://)",
                ));
            }
        }
        ChannelKind::Slack => {}
    }
    Ok(())
}

fn check_secret(kind: ChannelKind, secret: &str) -> Result<()> {
    if kind == ChannelKind::Slack
        && !(secret.starts_with("https://") || secret.starts_with("http://"))
    {
        return Err(invalid(
            "a Slack channel needs its incoming webhook URL as the secret",
        ));
    }
    Ok(())
}

// ---- outbox -------------------------------------------------------------------

pub struct NewOutboxItem {
    pub channel_id: Uuid,
    pub department_id: Option<DepartmentId>,
    pub agent_id: Option<OrgAgentId>,
    pub drafted_by: String,
    pub recipients: Vec<String>,
    pub subject: String,
    pub body: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OutboxRevision {
    pub recipients: Option<Vec<String>>,
    pub subject: Option<String>,
    pub body: Option<String>,
    /// What changed and why.
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, sqlx::FromRow)]
pub struct OutboxCounts {
    pub pending: i64,
    pub changes_requested: i64,
    pub sent: i64,
    pub rejected: i64,
    pub failed: i64,
    pub oldest_pending: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct OutboxRow {
    id: Uuid,
    channel_id: Uuid,
    department_id: Option<Uuid>,
    agent_id: Option<Uuid>,
    drafted_by: String,
    recipients: Vec<String>,
    subject: String,
    body: String,
    status: String,
    revision: i32,
    history: Json<Vec<Value>>,
    feedback: Option<String>,
    decided_by: Option<String>,
    decided_at: Option<DateTime<Utc>>,
    sent_at: Option<DateTime<Utc>>,
    error: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<OutboxRow> for OutboxItem {
    type Error = StoreError;

    fn try_from(r: OutboxRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            channel_id: r.channel_id,
            department_id: r.department_id,
            agent_id: r.agent_id,
            drafted_by: r.drafted_by,
            recipients: r.recipients,
            subject: r.subject,
            body: r.body,
            status: r.status.parse().map_err(StoreError::Invalid)?,
            revision: r.revision.max(1) as u32,
            history: r.history.0,
            feedback: r.feedback,
            decided_by: r.decided_by,
            decided_at: r.decided_at,
            sent_at: r.sent_at,
            error: r.error,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

const OUTBOX_COLUMNS: &str = "id, channel_id, department_id, agent_id, drafted_by, recipients, \
    subject, body, status, revision, history, feedback, decided_by, decided_at, sent_at, error, \
    created_at, updated_at";

/// Longest message.
pub const MAX_BODY_CHARS: usize = 20_000;

fn check_message(subject: &str, body: &str, recipients: &[String]) -> Result<()> {
    if body.trim().is_empty() {
        return Err(invalid("the message is empty"));
    }
    if body.chars().count() > MAX_BODY_CHARS {
        return Err(invalid(format!(
            "messages are limited to {MAX_BODY_CHARS} characters"
        )));
    }
    if subject.contains(['\r', '\n']) || subject.chars().count() > 300 {
        return Err(invalid("the subject is one line of up to 300 characters"));
    }
    if recipients.len() > 50
        || recipients
            .iter()
            .any(|r| r.trim().is_empty() || r.contains(['\r', '\n', ',', ';']) || r.len() > 320)
    {
        return Err(invalid("recipients are a list of single addresses"));
    }
    Ok(())
}

impl Store {
    // ---- channels ---------------------------------------------------------------

    pub async fn list_channels(&self) -> Result<Vec<Channel>> {
        let rows: Vec<ChannelRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {CHANNEL_COLUMNS} FROM org_channels ORDER BY name"
        )))
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn get_channel(&self, id: Uuid) -> Result<Channel> {
        let row: Option<ChannelRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {CHANNEL_COLUMNS} FROM org_channels WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.ok_or_else(|| StoreError::NotFound(format!("channel {id}")))?
            .try_into()
    }

    /// Enabled channels a department may use.
    pub async fn channels_for(&self, department: DepartmentId) -> Result<Vec<Channel>> {
        let rows: Vec<ChannelRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {CHANNEL_COLUMNS} FROM org_channels
             WHERE enabled AND $1 = ANY(departments) ORDER BY name"
        )))
        .bind(department)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn create_channel(&self, input: ChannelInput, actor: &str) -> Result<Channel> {
        let name = input.name.trim();
        if name.is_empty()
            || name.len() > 63
            || name.starts_with(['-', '_'])
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(invalid(
                "a channel name uses a-z, 0-9, - and _ (up to 63 characters)",
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
            (None, ChannelKind::Slack) => {
                return Err(invalid("a Slack channel needs its incoming webhook URL"));
            }
            _ => {}
        }
        let id = Uuid::now_v7();
        let sealed = match &secret {
            Some(s) => Some(self.cipher.encrypt(s.as_bytes(), &secret_aad(id))?),
            None => None,
        };
        let disclosure = input
            .disclosure
            .unwrap_or_else(|| DEFAULT_DISCLOSURE.to_string());
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "INSERT INTO org_channels (id, name, kind, description, config, secret_ciphertext,
                 secret_nonce, secret_hint, departments, requires_approval, max_per_day,
                 disclosure, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(id)
        .bind(name)
        .bind(input.kind.as_str())
        .bind(input.description.trim())
        .bind(Json(&config))
        .bind(sealed.as_ref().map(|(_, c)| c))
        .bind(sealed.as_ref().map(|(n, _)| n))
        .bind(secret.as_deref().map(key_hint))
        .bind(&input.departments)
        .bind(input.requires_approval)
        .bind(input.max_per_day.min(100_000) as i32)
        .bind(disclosure.trim())
        .bind(actor)
        .execute(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                StoreError::Conflict(format!("channel `{name}`"))
            }
            _ => StoreError::Db(e),
        })?;
        admin_event(
            &mut tx,
            actor,
            "channel.create",
            name,
            json!({
                "id": id, "kind": input.kind.as_str(), "departments": input.departments,
                "requires_approval": input.requires_approval,
            }),
        )
        .await?;
        tx.commit().await?;
        self.get_channel(id).await
    }

    pub async fn update_channel(
        &self,
        id: Uuid,
        update: ChannelUpdate,
        actor: &str,
    ) -> Result<Channel> {
        let current = self.get_channel(id).await?;
        let config = update.config.clone().unwrap_or(current.config.clone());
        check_config(current.kind, &config)?;
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "UPDATE org_channels SET description = $2, config = $3, departments = $4,
                 requires_approval = $5, max_per_day = $6, disclosure = $7, enabled = $8,
                 updated_at = now(), updated_by = $9
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
        .bind(
            update
                .requires_approval
                .unwrap_or(current.requires_approval),
        )
        .bind(
            update
                .max_per_day
                .unwrap_or(current.max_per_day)
                .min(100_000) as i32,
        )
        .bind(
            update
                .disclosure
                .as_deref()
                .map(str::trim)
                .unwrap_or(&current.disclosure),
        )
        .bind(update.enabled.unwrap_or(current.enabled))
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        match update.secret.as_deref().map(str::trim) {
            Some("") if current.kind == ChannelKind::Slack => {
                return Err(invalid("a Slack channel needs its incoming webhook URL"));
            }
            Some("") => {
                sqlx::query(
                    "UPDATE org_channels SET secret_ciphertext = NULL, secret_nonce = NULL,
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
                    "UPDATE org_channels SET secret_ciphertext = $2, secret_nonce = $3,
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
            "channel.update",
            &current.name,
            json!({
                "id": id, "secret_changed": update.secret.is_some(),
                "requires_approval": update.requires_approval,
            }),
        )
        .await?;
        tx.commit().await?;
        self.get_channel(id).await
    }

    pub async fn delete_channel(&self, id: Uuid, actor: &str) -> Result<()> {
        let current = self.get_channel(id).await?;
        let mut tx = self.pool().begin().await?;
        sqlx::query("DELETE FROM org_channels WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        admin_event(
            &mut tx,
            actor,
            "channel.delete",
            &current.name,
            json!({ "id": id }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The decrypted secret (server use only).
    pub async fn channel_secret(&self, id: Uuid) -> Result<Option<String>> {
        let row: Option<SealedSecret> = sqlx::query_as(
            "SELECT secret_ciphertext, secret_nonce FROM org_channels WHERE id = $1",
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

    /// Messages a channel sent since the start of today (UTC).
    pub async fn sent_today(&self, channel: Uuid) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM org_outbox WHERE channel_id = $1 AND status IN ('sent', 'sending')
               AND COALESCE(sent_at, decided_at) >= date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'",
        )
        .bind(channel)
        .fetch_one(self.pool())
        .await?)
    }

    // ---- outbox -----------------------------------------------------------------

    pub async fn create_outbox_item(&self, new: NewOutboxItem) -> Result<OutboxItem> {
        check_message(&new.subject, &new.body, &new.recipients)?;
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO org_outbox (id, channel_id, department_id, agent_id, drafted_by,
                 recipients, subject, body)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(new.channel_id)
        .bind(new.department_id)
        .bind(new.agent_id)
        .bind(&new.drafted_by)
        .bind(&new.recipients)
        .bind(new.subject.trim())
        .bind(new.body.trim())
        .execute(self.pool())
        .await?;
        self.get_outbox_item(id).await
    }

    pub async fn get_outbox_item(&self, id: Uuid) -> Result<OutboxItem> {
        let row: Option<OutboxRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {OUTBOX_COLUMNS} FROM org_outbox WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.ok_or_else(|| StoreError::NotFound(format!("outbox message {id}")))?
            .try_into()
    }

    /// Newest first; `agent` limits to one agent's drafts.
    pub async fn list_outbox(
        &self,
        status: Option<OutboxStatus>,
        agent: Option<OrgAgentId>,
        limit: i64,
    ) -> Result<Vec<OutboxItem>> {
        let rows: Vec<OutboxRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {OUTBOX_COLUMNS} FROM org_outbox
             WHERE ($1::text IS NULL OR status = $1) AND ($2::uuid IS NULL OR agent_id = $2)
             ORDER BY created_at DESC LIMIT $3"
        )))
        .bind(status.map(OutboxStatus::as_str))
        .bind(agent)
        .bind(limit.clamp(1, 1000))
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// A new revision by the agent or a person (only while undecided or
    /// after a failed send).
    pub async fn revise_outbox_item(
        &self,
        id: Uuid,
        revision: OutboxRevision,
        by: &str,
    ) -> Result<OutboxItem> {
        let current = self.get_outbox_item(id).await?;
        let recipients = revision.recipients.unwrap_or(current.recipients.clone());
        let subject = revision.subject.unwrap_or(current.subject.clone());
        let body = revision.body.unwrap_or(current.body.clone());
        check_message(&subject, &body, &recipients)?;
        let previous = json!({
            "revision": current.revision,
            "recipients": current.recipients,
            "subject": current.subject,
            "body": current.body,
            "feedback": current.feedback,
            "replaced_by": by,
            "note": revision.note,
            "at": Utc::now(),
        });
        let updated = sqlx::query(
            "UPDATE org_outbox SET recipients = $2, subject = $3, body = $4, status = 'pending',
                 feedback = NULL, error = NULL, revision = revision + 1,
                 history = history || jsonb_build_array($5::jsonb), updated_at = now()
             WHERE id = $1 AND revision = $6
               AND status IN ('pending', 'changes_requested', 'failed')",
        )
        .bind(id)
        .bind(&recipients)
        .bind(subject.trim())
        .bind(body.trim())
        .bind(Json(previous))
        .bind(current.revision as i32)
        .execute(self.pool())
        .await?;
        if updated.rows_affected() == 0 {
            return self.outbox_conflict(id).await;
        }
        self.get_outbox_item(id).await
    }

    pub async fn request_outbox_changes(
        &self,
        id: Uuid,
        feedback: &str,
        by: &str,
    ) -> Result<OutboxItem> {
        let feedback = feedback.trim();
        if feedback.is_empty() {
            return Err(invalid("say what should change"));
        }
        let entry = json!({ "feedback": feedback, "by": by, "at": Utc::now() });
        let updated = sqlx::query(
            "UPDATE org_outbox SET status = 'changes_requested', feedback = $2,
                 history = history || jsonb_build_array($3::jsonb), updated_at = now()
             WHERE id = $1 AND status IN ('pending', 'changes_requested', 'failed')",
        )
        .bind(id)
        .bind(feedback)
        .bind(Json(entry))
        .execute(self.pool())
        .await?;
        if updated.rows_affected() == 0 {
            return self.outbox_conflict(id).await;
        }
        self.get_outbox_item(id).await
    }

    pub async fn reject_outbox_item(&self, id: Uuid, reason: &str, by: &str) -> Result<OutboxItem> {
        let updated = sqlx::query(
            "UPDATE org_outbox SET status = 'rejected', feedback = NULLIF($2, ''),
                 decided_by = $3, decided_at = now(), updated_at = now()
             WHERE id = $1 AND status IN ('pending', 'changes_requested', 'failed')",
        )
        .bind(id)
        .bind(reason.trim())
        .bind(by)
        .execute(self.pool())
        .await?;
        if updated.rows_affected() == 0 {
            return self.outbox_conflict(id).await;
        }
        self.get_outbox_item(id).await
    }

    /// Take a message for sending (exactly one caller wins); `revision` is
    /// the one the person approved.
    pub async fn claim_outbox_item(&self, id: Uuid, revision: u32, by: &str) -> Result<OutboxItem> {
        let updated = sqlx::query(
            "UPDATE org_outbox SET status = 'sending', decided_by = $3, decided_at = now(),
                 error = NULL, updated_at = now()
             WHERE id = $1 AND revision = $2
               AND status IN ('pending', 'changes_requested', 'failed')",
        )
        .bind(id)
        .bind(revision as i32)
        .bind(by)
        .execute(self.pool())
        .await?;
        if updated.rows_affected() == 0 {
            let current = self.get_outbox_item(id).await?;
            if matches!(
                current.status,
                OutboxStatus::Pending | OutboxStatus::ChangesRequested | OutboxStatus::Failed
            ) {
                return Err(StoreError::Conflict(format!(
                    "revision {} of this message (you saw revision {revision})",
                    current.revision
                )));
            }
            return self.outbox_conflict(id).await;
        }
        self.get_outbox_item(id).await
    }

    /// Record how sending went.
    pub async fn finish_outbox_item(
        &self,
        id: Uuid,
        result: std::result::Result<(), String>,
        actor: &str,
    ) -> Result<OutboxItem> {
        let mut tx = self.pool().begin().await?;
        match &result {
            Ok(()) => {
                sqlx::query(
                    "UPDATE org_outbox SET status = 'sent', sent_at = now(), updated_at = now()
                     WHERE id = $1",
                )
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
            Err(error) => {
                sqlx::query(
                    "UPDATE org_outbox SET status = 'failed', error = $2, updated_at = now()
                     WHERE id = $1",
                )
                .bind(id)
                .bind(error)
                .execute(&mut *tx)
                .await?;
            }
        }
        admin_event(
            &mut tx,
            actor,
            if result.is_ok() {
                "outbox.sent"
            } else {
                "outbox.failed"
            },
            &id.to_string(),
            json!({ "error": result.as_ref().err() }),
        )
        .await?;
        tx.commit().await?;
        self.get_outbox_item(id).await
    }

    /// Put a claimed message back (it was not sent).
    pub async fn unclaim_outbox_item(&self, id: Uuid, reason: &str) -> Result<OutboxItem> {
        sqlx::query(
            "UPDATE org_outbox SET status = 'pending', decided_by = NULL, decided_at = NULL,
                 error = $2, updated_at = now()
             WHERE id = $1 AND status = 'sending'",
        )
        .bind(id)
        .bind(reason)
        .execute(self.pool())
        .await?;
        self.get_outbox_item(id).await
    }

    pub async fn outbox_counts(&self, since: DateTime<Utc>) -> Result<OutboxCounts> {
        Ok(sqlx::query_as(
            "SELECT count(*) FILTER (WHERE status = 'pending') AS pending,
                 count(*) FILTER (WHERE status = 'changes_requested') AS changes_requested,
                 count(*) FILTER (WHERE status = 'sent' AND sent_at >= $1) AS sent,
                 count(*) FILTER (WHERE status = 'rejected' AND decided_at >= $1) AS rejected,
                 count(*) FILTER (WHERE status = 'failed') AS failed,
                 min(created_at) FILTER (WHERE status = 'pending') AS oldest_pending
             FROM org_outbox",
        )
        .bind(since)
        .fetch_one(self.pool())
        .await?)
    }

    async fn outbox_conflict(&self, id: Uuid) -> Result<OutboxItem> {
        let item = self.get_outbox_item(id).await?;
        Err(StoreError::Conflict(format!(
            "a decision on this message ({})",
            item.status.as_str()
        )))
    }
}
