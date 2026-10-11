//! Products, releases, rollouts, client reports and signing keys.

use agentcore_core::releases::{
    ChangeSize, CheckOutcome, GateResult, HealthGate, Risk, Stage, check_gates, check_plan,
    default_plan,
};
use agentcore_core::{
    Approval, Artifact, DepartmentId, OrgAgentId, Product, Release, ReleaseStatus,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::types::Json;
use uuid::Uuid;

use crate::{Result, Store, StoreError, admin_event, key_hint};

fn invalid(msg: impl Into<String>) -> StoreError {
    StoreError::Invalid(msg.into())
}

fn webhook_aad(id: Uuid) -> Vec<u8> {
    format!("release-webhook:{id}").into_bytes()
}

fn key_aad(key_id: &str) -> Vec<u8> {
    format!("release-signing-key:{key_id}").into_bytes()
}

/// Longest release notes.
pub const MAX_NOTES_CHARS: usize = 50_000;

// ---- products -----------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ProductInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub departments: Vec<DepartmentId>,
    #[serde(default)]
    pub repository: Option<String>,
    /// Default: canary 1%, 10%, 50%, everyone.
    #[serde(default)]
    pub plan: Option<Vec<Stage>>,
    #[serde(default)]
    pub gates: Vec<HealthGate>,
    #[serde(default = "one")]
    pub approvals_required: u32,
    #[serde(default)]
    pub artifact_hosts: Vec<String>,
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// Key for the webhook's HMAC-SHA256 signature.
    #[serde(default)]
    pub webhook_secret: Option<String>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProductUpdate {
    pub description: Option<String>,
    pub departments: Option<Vec<DepartmentId>>,
    /// An empty string removes it.
    pub repository: Option<String>,
    pub plan: Option<Vec<Stage>>,
    pub gates: Option<Vec<HealthGate>>,
    pub approvals_required: Option<u32>,
    pub artifact_hosts: Option<Vec<String>>,
    /// An empty string removes it.
    pub webhook_url: Option<String>,
    /// A new key; an empty string removes it.
    pub webhook_secret: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(sqlx::FromRow)]
struct ProductRow {
    id: Uuid,
    name: String,
    description: String,
    departments: Vec<Uuid>,
    repository: Option<String>,
    plan: Json<Vec<Stage>>,
    gates: Json<Vec<HealthGate>>,
    approvals_required: i32,
    artifact_hosts: Vec<String>,
    webhook_url: Option<String>,
    webhook_secret_hint: Option<String>,
    client_token_hint: Option<String>,
    frozen: Option<String>,
    frozen_by: Option<String>,
    enabled: bool,
    updated_at: DateTime<Utc>,
    updated_by: String,
}

impl From<ProductRow> for Product {
    fn from(r: ProductRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            description: r.description,
            departments: r.departments,
            repository: r.repository,
            plan: r.plan.0,
            gates: r.gates.0,
            approvals_required: r.approvals_required.max(1) as u32,
            artifact_hosts: r.artifact_hosts,
            webhook_url: r.webhook_url,
            webhook_secret_hint: r.webhook_secret_hint,
            client_token_hint: r.client_token_hint,
            frozen: r.frozen,
            frozen_by: r.frozen_by,
            enabled: r.enabled,
            updated_at: r.updated_at,
            updated_by: r.updated_by,
        }
    }
}

const PRODUCT_COLUMNS: &str = "id, name, description, departments, repository, plan, gates, \
    approvals_required, artifact_hosts, webhook_url, webhook_secret_hint, client_token_hint, \
    frozen, frozen_by, enabled, updated_at, updated_by";

fn check_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 63
        || name.starts_with(['-', '_'])
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(invalid(
            "a product name uses a-z, 0-9, - and _ (up to 63 characters)",
        ));
    }
    Ok(())
}

fn check_repository(repo: &str) -> Result<()> {
    let ok = repo.split_once('/').is_some_and(|(owner, name)| {
        let part = |s: &str| {
            !s.is_empty()
                && s.len() <= 100
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        };
        part(owner) && part(name)
    });
    if ok {
        Ok(())
    } else {
        Err(invalid("the repository is `owner/name`"))
    }
}

fn clean_hosts(hosts: &[String]) -> Result<Vec<String>> {
    if hosts.len() > 20 {
        return Err(invalid("at most 20 artifact hosts"));
    }
    hosts
        .iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .map(|h| {
            let bare = h.trim_start_matches('.');
            if bare.is_empty()
                || bare.len() > 253
                || !bare
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            {
                Err(invalid(format!(
                    "`{h}` is not a host name (downloads.example.com, or .example.com for its subdomains)"
                )))
            } else {
                Ok(h)
            }
        })
        .collect()
}

fn check_webhook(url: &str) -> Result<()> {
    if url.starts_with("https://") || url.starts_with("http://") {
        Ok(())
    } else {
        Err(invalid("the webhook url starts with http:// or https://"))
    }
}

fn check_approvals(n: u32) -> Result<()> {
    if (1..=5).contains(&n) {
        Ok(())
    } else {
        Err(invalid("a release needs 1 to 5 approvals"))
    }
}

// ---- releases -----------------------------------------------------------------------

/// What a release consists of (proposed, then revised).
#[derive(Debug, Clone)]
pub struct ReleaseContent {
    pub commit: Option<String>,
    pub notes: String,
    pub artifacts: Vec<Artifact>,
    pub sbom: Option<Value>,
    pub checks: Vec<CheckOutcome>,
    pub changes: ChangeSize,
    pub risk: Risk,
    pub plan: Vec<Stage>,
    pub approvals_required: u32,
}

pub struct NewRelease {
    pub product_id: Uuid,
    /// Already checked and normalised.
    pub version: String,
    pub content: ReleaseContent,
    pub department_id: Option<DepartmentId>,
    pub agent_id: Option<OrgAgentId>,
    pub proposed_by: String,
}

fn check_content(c: &ReleaseContent) -> Result<()> {
    if c.notes.trim().is_empty() {
        return Err(invalid(
            "a release has notes: what changed, for the people who use it",
        ));
    }
    if c.notes.chars().count() > MAX_NOTES_CHARS {
        return Err(invalid(format!(
            "release notes are limited to {MAX_NOTES_CHARS} characters"
        )));
    }
    if let Some(commit) = &c.commit
        && !(7..=64).contains(&commit.len())
    {
        return Err(invalid("`commit` is a git commit hash"));
    }
    if c.checks.len() > 100 {
        return Err(invalid("at most 100 checks"));
    }
    check_plan(&c.plan).map_err(invalid)?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct ReleaseRow {
    id: Uuid,
    product_id: Uuid,
    version: String,
    commit_sha: Option<String>,
    notes: String,
    artifacts: Json<Vec<Artifact>>,
    sbom: Option<Json<Value>>,
    checks: Json<Vec<CheckOutcome>>,
    changes: Json<ChangeSize>,
    risk: Json<Risk>,
    plan: Json<Vec<Stage>>,
    approvals_required: i32,
    approvals: Json<Vec<Approval>>,
    status: String,
    stage: i32,
    stage_started_at: Option<DateTime<Utc>>,
    gates: Json<Vec<GateResult>>,
    gates_checked_at: Option<DateTime<Utc>>,
    department_id: Option<Uuid>,
    agent_id: Option<Uuid>,
    proposed_by: String,
    revision: i32,
    history: Json<Vec<Value>>,
    feedback: Option<String>,
    reason: Option<String>,
    decided_by: Option<String>,
    started_at: Option<DateTime<Utc>>,
    released_at: Option<DateTime<Utc>>,
    attestation: Option<Json<Value>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<ReleaseRow> for Release {
    type Error = StoreError;

    fn try_from(r: ReleaseRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            product_id: r.product_id,
            version: r.version,
            commit: r.commit_sha,
            notes: r.notes,
            artifacts: r.artifacts.0,
            sbom: r.sbom.map(|j| j.0),
            checks: r.checks.0,
            changes: r.changes.0,
            risk: r.risk.0,
            plan: r.plan.0,
            approvals_required: r.approvals_required.max(1) as u32,
            approvals: r.approvals.0,
            status: r.status.parse().map_err(StoreError::Invalid)?,
            stage: r.stage.max(0) as u32,
            stage_started_at: r.stage_started_at,
            gates: r.gates.0,
            gates_checked_at: r.gates_checked_at,
            department_id: r.department_id,
            agent_id: r.agent_id,
            proposed_by: r.proposed_by,
            revision: r.revision.max(1) as u32,
            history: r.history.0,
            feedback: r.feedback,
            reason: r.reason,
            decided_by: r.decided_by,
            started_at: r.started_at,
            released_at: r.released_at,
            attestation: r.attestation.map(|j| j.0),
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

const RELEASE_COLUMNS: &str = "id, product_id, version, commit_sha, notes, artifacts, sbom, \
    checks, changes, risk, plan, approvals_required, approvals, status, stage, stage_started_at, \
    gates, gates_checked_at, department_id, agent_id, proposed_by, revision, history, feedback, \
    reason, decided_by, started_at, released_at, attestation, created_at, updated_at";

fn entry(action: &str, by: &str, note: &str) -> Json<Value> {
    Json(json!({ "action": action, "by": by, "note": note, "at": Utc::now() }))
}

/// What happened to a release, for reports and signals.
#[derive(Debug, Clone, Default, Serialize, sqlx::FromRow)]
pub struct ReleaseCounts {
    pub proposed: i64,
    pub oldest_proposed: Option<DateTime<Utc>>,
    pub rolling_out: i64,
    pub paused: i64,
    pub released: i64,
    pub rolled_back: i64,
}

/// What a unit reports about a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportEvent {
    Installed,
    InstallFailed,
    Crash,
    /// Running fine (counts the unit without changing anything else).
    Healthy,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, sqlx::FromRow)]
pub struct ReportStats {
    /// Units that reported anything.
    pub units: i64,
    pub installed: i64,
    pub install_failed: i64,
    /// Units with at least one crash.
    pub crashed: i64,
}

impl ReportStats {
    /// Share of reporting units without a crash.
    pub fn crash_free(&self) -> Option<f64> {
        (self.units > 0).then(|| (self.units - self.crashed) as f64 / self.units as f64)
    }

    /// Share of install attempts that worked.
    pub fn install_success(&self) -> Option<f64> {
        let attempts = self.installed + self.install_failed;
        (attempts > 0).then(|| self.installed as f64 / attempts as f64)
    }
}

/// A published signing key.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct SigningKey {
    pub key_id: String,
    #[serde(skip)]
    pub public_key: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub retired_at: Option<DateTime<Utc>>,
}

impl Store {
    // ---- products -------------------------------------------------------------

    pub async fn list_products(&self) -> Result<Vec<Product>> {
        let rows: Vec<ProductRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {PRODUCT_COLUMNS} FROM release_products ORDER BY name"
        )))
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_product(&self, id: Uuid) -> Result<Product> {
        let row: Option<ProductRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {PRODUCT_COLUMNS} FROM release_products WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row
            .ok_or_else(|| StoreError::NotFound(format!("product {id}")))?
            .into())
    }

    /// The product an update client may read: by name and client token hash.
    pub async fn product_for_client(&self, name: &str, token_sha256: &str) -> Result<Product> {
        let row: Option<ProductRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {PRODUCT_COLUMNS} FROM release_products
             WHERE name = $1 AND enabled AND client_token_sha256 = $2"
        )))
        .bind(name)
        .bind(token_sha256)
        .fetch_optional(self.pool())
        .await?;
        Ok(row
            .ok_or_else(|| StoreError::NotFound(format!("product `{name}` for this token")))?
            .into())
    }

    /// Enabled products a department may propose releases for.
    pub async fn products_for(&self, department: DepartmentId) -> Result<Vec<Product>> {
        let rows: Vec<ProductRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {PRODUCT_COLUMNS} FROM release_products
             WHERE enabled AND $1 = ANY(departments) ORDER BY name"
        )))
        .bind(department)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Create a product; `client_token` is the hash and hint of the token
    /// update clients use (the server makes the token).
    pub async fn create_product(
        &self,
        input: ProductInput,
        client_token: (&str, &str),
        actor: &str,
    ) -> Result<Product> {
        let name = input.name.trim();
        check_name(name)?;
        let plan = input.plan.unwrap_or_else(default_plan);
        check_plan(&plan).map_err(invalid)?;
        check_gates(&input.gates).map_err(invalid)?;
        check_approvals(input.approvals_required)?;
        let hosts = clean_hosts(&input.artifact_hosts)?;
        let repository = input
            .repository
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty());
        if let Some(r) = &repository {
            check_repository(r)?;
        }
        let webhook_url = input
            .webhook_url
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty());
        if let Some(u) = &webhook_url {
            check_webhook(u)?;
        }
        let id = Uuid::now_v7();
        let secret = input.webhook_secret.filter(|s| !s.trim().is_empty());
        let sealed = match &secret {
            Some(s) => Some(self.cipher.encrypt(s.as_bytes(), &webhook_aad(id))?),
            None => None,
        };
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "INSERT INTO release_products (id, name, description, departments, repository, plan,
                 gates, approvals_required, artifact_hosts, webhook_url, webhook_secret_ciphertext,
                 webhook_secret_nonce, webhook_secret_hint, client_token_sha256, client_token_hint,
                 updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
        )
        .bind(id)
        .bind(name)
        .bind(input.description.trim())
        .bind(&input.departments)
        .bind(&repository)
        .bind(Json(&plan))
        .bind(Json(&input.gates))
        .bind(input.approvals_required as i32)
        .bind(&hosts)
        .bind(&webhook_url)
        .bind(sealed.as_ref().map(|(_, c)| c))
        .bind(sealed.as_ref().map(|(n, _)| n))
        .bind(secret.as_deref().map(key_hint))
        .bind(client_token.0)
        .bind(client_token.1)
        .bind(actor)
        .execute(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                StoreError::Conflict(format!("product `{name}`"))
            }
            _ => StoreError::Db(e),
        })?;
        admin_event(
            &mut tx,
            actor,
            "product.create",
            name,
            json!({
                "id": id, "departments": input.departments, "plan": plan,
                "gates": input.gates.len(), "approvals_required": input.approvals_required,
                "artifact_hosts": hosts,
            }),
        )
        .await?;
        tx.commit().await?;
        self.get_product(id).await
    }

    pub async fn update_product(
        &self,
        id: Uuid,
        update: ProductUpdate,
        actor: &str,
    ) -> Result<Product> {
        let current = self.get_product(id).await?;
        let plan = update.plan.clone().unwrap_or(current.plan.clone());
        check_plan(&plan).map_err(invalid)?;
        let gates = update.gates.clone().unwrap_or(current.gates.clone());
        check_gates(&gates).map_err(invalid)?;
        let approvals = update
            .approvals_required
            .unwrap_or(current.approvals_required);
        check_approvals(approvals)?;
        let hosts = match &update.artifact_hosts {
            Some(h) => clean_hosts(h)?,
            None => current.artifact_hosts.clone(),
        };
        let repository = match update.repository.as_deref().map(str::trim) {
            Some("") => None,
            Some(r) => {
                check_repository(r)?;
                Some(r.to_string())
            }
            None => current.repository.clone(),
        };
        let webhook_url = match update.webhook_url.as_deref().map(str::trim) {
            Some("") => None,
            Some(u) => {
                check_webhook(u)?;
                Some(u.to_string())
            }
            None => current.webhook_url.clone(),
        };
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "UPDATE release_products SET description = $2, departments = $3, repository = $4,
                 plan = $5, gates = $6, approvals_required = $7, artifact_hosts = $8,
                 webhook_url = $9, enabled = $10, updated_at = now(), updated_by = $11
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
        .bind(update.departments.as_ref().unwrap_or(&current.departments))
        .bind(&repository)
        .bind(Json(&plan))
        .bind(Json(&gates))
        .bind(approvals as i32)
        .bind(&hosts)
        .bind(&webhook_url)
        .bind(update.enabled.unwrap_or(current.enabled))
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        match update.webhook_secret.as_deref().map(str::trim) {
            Some("") => {
                sqlx::query(
                    "UPDATE release_products SET webhook_secret_ciphertext = NULL,
                         webhook_secret_nonce = NULL, webhook_secret_hint = NULL WHERE id = $1",
                )
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
            Some(secret) => {
                let (nonce, ciphertext) =
                    self.cipher.encrypt(secret.as_bytes(), &webhook_aad(id))?;
                sqlx::query(
                    "UPDATE release_products SET webhook_secret_ciphertext = $2,
                         webhook_secret_nonce = $3, webhook_secret_hint = $4 WHERE id = $1",
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
            "product.update",
            &current.name,
            json!({
                "id": id, "plan": update.plan, "gates": update.gates.map(|g| g.len()),
                "approvals_required": update.approvals_required,
                "artifact_hosts": update.artifact_hosts, "departments": update.departments,
                "webhook_secret_changed": update.webhook_secret.is_some(),
                "enabled": update.enabled,
            }),
        )
        .await?;
        tx.commit().await?;
        self.get_product(id).await
    }

    /// Delete a product and its release history (not while a rollout runs).
    pub async fn delete_product(&self, id: Uuid, actor: &str) -> Result<()> {
        let current = self.get_product(id).await?;
        if self.active_release(id).await?.is_some() {
            return Err(StoreError::Conflict(
                "a rollout of this product (finish or roll it back first)".into(),
            ));
        }
        let mut tx = self.pool().begin().await?;
        sqlx::query("DELETE FROM release_products WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        admin_event(
            &mut tx,
            actor,
            "product.delete",
            &current.name,
            json!({ "id": id }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Replace the client token (hash and hint).
    pub async fn set_client_token(
        &self,
        id: Uuid,
        sha256: &str,
        hint: &str,
        actor: &str,
    ) -> Result<Product> {
        let current = self.get_product(id).await?;
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "UPDATE release_products SET client_token_sha256 = $2, client_token_hint = $3,
                 updated_at = now(), updated_by = $4 WHERE id = $1",
        )
        .bind(id)
        .bind(sha256)
        .bind(hint)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            "product.client_token",
            &current.name,
            json!({ "id": id }),
        )
        .await?;
        tx.commit().await?;
        self.get_product(id).await
    }

    /// Start (`Some(reason)`) or end (`None`) a change freeze.
    pub async fn set_freeze(&self, id: Uuid, reason: Option<&str>, actor: &str) -> Result<Product> {
        let current = self.get_product(id).await?;
        let reason = reason.map(str::trim);
        if reason == Some("") {
            return Err(invalid("say why releases are frozen"));
        }
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "UPDATE release_products SET frozen = $2, frozen_by = $3, updated_at = now(),
                 updated_by = $4 WHERE id = $1",
        )
        .bind(id)
        .bind(reason)
        .bind(reason.map(|_| actor))
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            if reason.is_some() {
                "product.freeze"
            } else {
                "product.unfreeze"
            },
            &current.name,
            json!({ "id": id, "reason": reason }),
        )
        .await?;
        tx.commit().await?;
        self.get_product(id).await
    }

    /// The decrypted webhook key (server use only).
    pub async fn webhook_secret(&self, id: Uuid) -> Result<Option<String>> {
        let row: Option<crate::outbox::SealedSecret> = sqlx::query_as(
            "SELECT webhook_secret_ciphertext, webhook_secret_nonce FROM release_products
             WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        match row {
            Some((Some(ciphertext), Some(nonce))) => {
                let secret = self.cipher.decrypt(&nonce, &ciphertext, &webhook_aad(id))?;
                Ok(Some(
                    String::from_utf8(secret).map_err(|_| StoreError::Crypto)?,
                ))
            }
            _ => Ok(None),
        }
    }

    // ---- releases -------------------------------------------------------------

    /// Propose a release. Its version must be above every version of the
    /// product used so far (rolled-back versions count; rejected ones do not).
    pub async fn create_release(&self, new: NewRelease) -> Result<Release> {
        check_content(&new.content)?;
        let version: agentcore_core::Version = new.version.parse().map_err(invalid)?;
        let mut tx = self.pool().begin().await?;
        // Proposals of one product are serialised by its row.
        let exists: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM release_products WHERE id = $1 FOR UPDATE")
                .bind(new.product_id)
                .fetch_optional(&mut *tx)
                .await?;
        if exists.is_none() {
            return Err(StoreError::NotFound(format!("product {}", new.product_id)));
        }
        let used: Vec<String> = sqlx::query_scalar(
            "SELECT version FROM releases WHERE product_id = $1 AND status <> 'rejected'",
        )
        .bind(new.product_id)
        .fetch_all(&mut *tx)
        .await?;
        if let Some(highest) = used
            .iter()
            .filter_map(|v| v.parse::<agentcore_core::Version>().ok())
            .max()
            && version <= highest
        {
            return Err(invalid(format!(
                "version {version} is not above {highest}, the highest version used so far \
                 (versions are never reused)"
            )));
        }
        let id = Uuid::now_v7();
        let c = &new.content;
        sqlx::query(
            "INSERT INTO releases (id, product_id, version, commit_sha, notes, artifacts, sbom,
                 checks, changes, risk, plan, approvals_required, department_id, agent_id,
                 proposed_by, history)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                     jsonb_build_array($16::jsonb))",
        )
        .bind(id)
        .bind(new.product_id)
        .bind(version.to_string())
        .bind(&c.commit)
        .bind(c.notes.trim())
        .bind(Json(&c.artifacts))
        .bind(c.sbom.as_ref().map(Json))
        .bind(Json(&c.checks))
        .bind(Json(&c.changes))
        .bind(Json(&c.risk))
        .bind(Json(&c.plan))
        .bind(c.approvals_required as i32)
        .bind(new.department_id)
        .bind(new.agent_id)
        .bind(&new.proposed_by)
        .bind(entry(
            "proposed",
            &new.proposed_by,
            &format!("risk {} ({})", c.risk.score, c.risk.level.as_str()),
        ))
        .execute(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                StoreError::Conflict(format!("version {version}"))
            }
            _ => StoreError::Db(e),
        })?;
        tx.commit().await?;
        self.get_release(id).await
    }

    pub async fn get_release(&self, id: Uuid) -> Result<Release> {
        let row: Option<ReleaseRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {RELEASE_COLUMNS} FROM releases WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.ok_or_else(|| StoreError::NotFound(format!("release {id}")))?
            .try_into()
    }

    /// Newest first.
    pub async fn list_releases(
        &self,
        product: Option<Uuid>,
        status: Option<ReleaseStatus>,
        agent: Option<OrgAgentId>,
        limit: i64,
    ) -> Result<Vec<Release>> {
        let rows: Vec<ReleaseRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {RELEASE_COLUMNS} FROM releases
             WHERE ($1::uuid IS NULL OR product_id = $1) AND ($2::text IS NULL OR status = $2)
               AND ($3::uuid IS NULL OR agent_id = $3)
             ORDER BY created_at DESC LIMIT $4"
        )))
        .bind(product)
        .bind(status.map(ReleaseStatus::as_str))
        .bind(agent)
        .bind(limit.clamp(1, 1000))
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// The release of a product that is rolling out or paused.
    pub async fn active_release(&self, product: Uuid) -> Result<Option<Release>> {
        let row: Option<ReleaseRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {RELEASE_COLUMNS} FROM releases
             WHERE product_id = $1 AND status IN ('rolling_out', 'paused')"
        )))
        .bind(product)
        .fetch_optional(self.pool())
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// The release everyone outside a rollout runs: the last one released.
    pub async fn baseline_release(&self, product: Uuid) -> Result<Option<Release>> {
        let row: Option<ReleaseRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {RELEASE_COLUMNS} FROM releases
             WHERE product_id = $1 AND status = 'released'
             ORDER BY released_at DESC LIMIT 1"
        )))
        .bind(product)
        .fetch_optional(self.pool())
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// A shipped (or shipping) release by version.
    pub async fn release_by_version(&self, product: Uuid, version: &str) -> Result<Release> {
        let row: Option<ReleaseRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {RELEASE_COLUMNS} FROM releases
             WHERE product_id = $1 AND version = $2 AND status <> 'rejected'"
        )))
        .bind(product)
        .bind(version)
        .fetch_optional(self.pool())
        .await?;
        row.ok_or_else(|| StoreError::NotFound(format!("release {version}")))?
            .try_into()
    }

    /// Releases that are rolling out (for the heartbeat).
    pub async fn rolling_releases(&self) -> Result<Vec<Release>> {
        self.list_releases(None, Some(ReleaseStatus::RollingOut), None, 1000)
            .await
    }

    /// A new revision (while undecided): approvals start over.
    pub async fn revise_release(
        &self,
        id: Uuid,
        seen_revision: u32,
        content: ReleaseContent,
        note: &str,
        by: &str,
    ) -> Result<Release> {
        check_content(&content)?;
        let updated = sqlx::query(
            "UPDATE releases SET commit_sha = $3, notes = $4, artifacts = $5, sbom = $6,
                 checks = $7, changes = $8, risk = $9, plan = $10, approvals_required = $11,
                 approvals = '[]', status = 'proposed', feedback = NULL,
                 revision = revision + 1, history = history || jsonb_build_array($12::jsonb),
                 updated_at = now()
             WHERE id = $1 AND revision = $2 AND status IN ('proposed', 'changes_requested')",
        )
        .bind(id)
        .bind(seen_revision as i32)
        .bind(&content.commit)
        .bind(content.notes.trim())
        .bind(Json(&content.artifacts))
        .bind(content.sbom.as_ref().map(Json))
        .bind(Json(&content.checks))
        .bind(Json(&content.changes))
        .bind(Json(&content.risk))
        .bind(Json(&content.plan))
        .bind(content.approvals_required as i32)
        .bind(entry(
            "revised",
            by,
            &format!("{} (risk {})", note.trim(), content.risk.score),
        ))
        .execute(self.pool())
        .await?;
        if updated.rows_affected() == 0 {
            return self.release_conflict(id).await;
        }
        self.get_release(id).await
    }

    /// Record verified artifacts (digests the server checked).
    pub async fn set_release_artifacts(
        &self,
        id: Uuid,
        revision: u32,
        artifacts: &[Artifact],
    ) -> Result<()> {
        sqlx::query(
            "UPDATE releases SET artifacts = $3, updated_at = now()
             WHERE id = $1 AND revision = $2 AND status = 'proposed'",
        )
        .bind(id)
        .bind(revision as i32)
        .bind(Json(artifacts))
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// A person approves the revision they read. Distinct people; nobody
    /// approves what they proposed themselves.
    pub async fn approve_release(&self, id: Uuid, revision: u32, by: &str) -> Result<Release> {
        let mut tx = self.pool().begin().await?;
        let row: Option<ReleaseRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {RELEASE_COLUMNS} FROM releases WHERE id = $1 FOR UPDATE"
        )))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let current: Release = row
            .ok_or_else(|| StoreError::NotFound(format!("release {id}")))?
            .try_into()?;
        match current.status {
            ReleaseStatus::Proposed => {}
            ReleaseStatus::ChangesRequested => {
                return Err(StoreError::Conflict(
                    "a request for changes on this release (it is revised first)".into(),
                ));
            }
            other => {
                return Err(StoreError::Conflict(format!(
                    "a decision on this release ({})",
                    other.as_str()
                )));
            }
        }
        if current.revision != revision {
            return Err(StoreError::Conflict(format!(
                "revision {} of this release (you saw revision {revision})",
                current.revision
            )));
        }
        if current.proposed_by == by {
            return Err(invalid(
                "nobody approves a release they proposed (four eyes)",
            ));
        }
        if current.approvals.iter().any(|a| a.by == by) {
            return Err(StoreError::Conflict(format!("{by}'s approval")));
        }
        let approval = Approval {
            by: by.to_string(),
            at: Utc::now(),
            revision,
        };
        sqlx::query(
            "UPDATE releases SET approvals = approvals || jsonb_build_array($2::jsonb),
                 history = history || jsonb_build_array($3::jsonb), updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(Json(&approval))
        .bind(entry("approved", by, &format!("revision {revision}")))
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            by,
            "release.approve",
            &current.version,
            json!({ "id": id, "revision": revision }),
        )
        .await?;
        tx.commit().await?;
        self.get_release(id).await
    }

    /// Start the rollout once enough people approved (exactly one caller
    /// wins). Refused while the product is frozen or another rollout runs.
    pub async fn start_release(
        &self,
        id: Uuid,
        revision: u32,
        attestation: &Value,
        by: &str,
    ) -> Result<Release> {
        let mut tx = self.pool().begin().await?;
        let updated = sqlx::query(
            "UPDATE releases r SET status = 'rolling_out', stage = 0, stage_started_at = now(),
                 started_at = now(), attestation = $3, decided_by = $4, gates = '[]',
                 history = history || jsonb_build_array($5::jsonb), updated_at = now()
             WHERE id = $1 AND revision = $2 AND status = 'proposed'
               AND jsonb_array_length(approvals) >= approvals_required
               AND (SELECT frozen FROM release_products p WHERE p.id = r.product_id) IS NULL",
        )
        .bind(id)
        .bind(revision as i32)
        .bind(Json(attestation))
        .bind(by)
        .bind(entry("started", by, "rollout started"))
        .execute(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => StoreError::Conflict(
                "another rollout of this product (finish or roll it back first)".into(),
            ),
            _ => StoreError::Db(e),
        })?;
        if updated.rows_affected() == 0 {
            drop(tx);
            let current = self.get_release(id).await?;
            let product = self.get_product(current.product_id).await?;
            if let Some(reason) = product.frozen {
                return Err(StoreError::Conflict(format!(
                    "a change freeze on `{}`: {reason}",
                    product.name
                )));
            }
            if current.status == ReleaseStatus::Proposed
                && (current.approvals.len() as u32) < current.approvals_required
            {
                return Err(StoreError::Conflict(format!(
                    "a release with {} of {} approvals",
                    current.approvals.len(),
                    current.approvals_required
                )));
            }
            return self.release_conflict(id).await;
        }
        let version: String = sqlx::query_scalar("SELECT version FROM releases WHERE id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        admin_event(&mut tx, by, "release.start", &version, json!({ "id": id })).await?;
        tx.commit().await?;
        self.get_release(id).await
    }

    pub async fn request_release_changes(
        &self,
        id: Uuid,
        feedback: &str,
        by: &str,
    ) -> Result<Release> {
        let feedback = feedback.trim();
        if feedback.is_empty() {
            return Err(invalid("say what should change"));
        }
        let updated = sqlx::query(
            "UPDATE releases SET status = 'changes_requested', feedback = $2, approvals = '[]',
                 history = history || jsonb_build_array($3::jsonb), updated_at = now()
             WHERE id = $1 AND status IN ('proposed', 'changes_requested')",
        )
        .bind(id)
        .bind(feedback)
        .bind(entry("changes_requested", by, feedback))
        .execute(self.pool())
        .await?;
        if updated.rows_affected() == 0 {
            return self.release_conflict(id).await;
        }
        self.get_release(id).await
    }

    pub async fn reject_release(&self, id: Uuid, reason: &str, by: &str) -> Result<Release> {
        let mut tx = self.pool().begin().await?;
        let version: Option<String> = sqlx::query_scalar(
            "UPDATE releases SET status = 'rejected', reason = NULLIF($2, ''), decided_by = $3,
                 history = history || jsonb_build_array($4::jsonb), updated_at = now()
             WHERE id = $1 AND status IN ('proposed', 'changes_requested') RETURNING version",
        )
        .bind(id)
        .bind(reason.trim())
        .bind(by)
        .bind(entry("rejected", by, reason.trim()))
        .fetch_optional(&mut *tx)
        .await?;
        let Some(version) = version else {
            drop(tx);
            return self.release_conflict(id).await;
        };
        admin_event(
            &mut tx,
            by,
            "release.reject",
            &version,
            json!({ "id": id, "reason": reason.trim() }),
        )
        .await?;
        tx.commit().await?;
        self.get_release(id).await
    }

    /// Move to the next stage (one caller wins per stage).
    pub async fn advance_release(&self, id: Uuid, from_stage: u32, by: &str) -> Result<bool> {
        let mut tx = self.pool().begin().await?;
        let version: Option<String> = sqlx::query_scalar(
            "UPDATE releases SET stage = stage + 1, stage_started_at = now(), gates = '[]',
                 gates_checked_at = NULL, history = history || jsonb_build_array($3::jsonb),
                 updated_at = now()
             WHERE id = $1 AND status = 'rolling_out' AND stage = $2
               AND stage + 1 < jsonb_array_length(plan)
             RETURNING version",
        )
        .bind(id)
        .bind(from_stage as i32)
        .bind(entry(
            "advanced",
            by,
            &format!("stage {} → {}", from_stage + 1, from_stage + 2),
        ))
        .fetch_optional(&mut *tx)
        .await?;
        let Some(version) = version else {
            return Ok(false);
        };
        admin_event(
            &mut tx,
            by,
            "release.advance",
            &version,
            json!({ "id": id, "stage": from_stage + 1 }),
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The last stage soaked: everyone has it (one caller wins).
    pub async fn complete_release(&self, id: Uuid, from_stage: u32, by: &str) -> Result<bool> {
        let mut tx = self.pool().begin().await?;
        let version: Option<String> = sqlx::query_scalar(
            "UPDATE releases SET status = 'released', released_at = now(),
                 history = history || jsonb_build_array($3::jsonb), updated_at = now()
             WHERE id = $1 AND status = 'rolling_out' AND stage = $2
               AND stage + 1 = jsonb_array_length(plan)
             RETURNING version",
        )
        .bind(id)
        .bind(from_stage as i32)
        .bind(entry("released", by, "everyone has it"))
        .fetch_optional(&mut *tx)
        .await?;
        let Some(version) = version else {
            return Ok(false);
        };
        admin_event(
            &mut tx,
            by,
            "release.complete",
            &version,
            json!({ "id": id }),
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn pause_release(&self, id: Uuid, reason: &str, by: &str) -> Result<Release> {
        self.transition(
            id,
            "UPDATE releases SET status = 'paused', reason = $2, decided_by = $3,
                 history = history || jsonb_build_array($4::jsonb), updated_at = now()
             WHERE id = $1 AND status = 'rolling_out' RETURNING version",
            reason,
            by,
            "paused",
        )
        .await
    }

    /// Continue; the current stage soaks again from now.
    pub async fn resume_release(&self, id: Uuid, by: &str) -> Result<Release> {
        let product = self.get_release(id).await?.product_id;
        if let Some(reason) = self.get_product(product).await?.frozen {
            return Err(StoreError::Conflict(format!("a change freeze: {reason}")));
        }
        self.transition(
            id,
            "UPDATE releases SET status = 'rolling_out', reason = NULLIF($2, ''), stage_started_at = now(),
                 decided_by = $3, history = history || jsonb_build_array($4::jsonb),
                 updated_at = now()
             WHERE id = $1 AND status = 'paused' RETURNING version",
            "",
            by,
            "resumed",
        )
        .await
    }

    /// Take a release back: units are offered the last released version.
    pub async fn roll_back_release(&self, id: Uuid, reason: &str, by: &str) -> Result<Release> {
        if reason.trim().is_empty() {
            return Err(invalid("say why it is rolled back"));
        }
        self.transition(
            id,
            "UPDATE releases SET status = 'rolled_back', reason = $2, decided_by = $3,
                 history = history || jsonb_build_array($4::jsonb), updated_at = now()
             WHERE id = $1 AND status IN ('rolling_out', 'paused', 'released')
             RETURNING version",
            reason,
            by,
            "rolled_back",
        )
        .await
    }

    async fn transition(
        &self,
        id: Uuid,
        sql: &'static str,
        reason: &str,
        by: &str,
        action: &str,
    ) -> Result<Release> {
        let mut tx = self.pool().begin().await?;
        let version: Option<String> = sqlx::query_scalar(sql)
            .bind(id)
            .bind(reason.trim())
            .bind(by)
            .bind(entry(action, by, reason.trim()))
            .fetch_optional(&mut *tx)
            .await?;
        let Some(version) = version else {
            drop(tx);
            return self.release_conflict(id).await;
        };
        admin_event(
            &mut tx,
            by,
            &format!("release.{action}"),
            &version,
            json!({ "id": id, "reason": reason.trim() }),
        )
        .await?;
        tx.commit().await?;
        self.get_release(id).await
    }

    pub async fn record_gates(&self, id: Uuid, results: &[GateResult]) -> Result<()> {
        sqlx::query("UPDATE releases SET gates = $2, gates_checked_at = now() WHERE id = $1")
            .bind(id)
            .bind(Json(results))
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn release_counts(&self, since: DateTime<Utc>) -> Result<ReleaseCounts> {
        Ok(sqlx::query_as(
            "SELECT count(*) FILTER (WHERE status = 'proposed') AS proposed,
                 min(created_at) FILTER (WHERE status = 'proposed') AS oldest_proposed,
                 count(*) FILTER (WHERE status = 'rolling_out') AS rolling_out,
                 count(*) FILTER (WHERE status = 'paused') AS paused,
                 count(*) FILTER (WHERE status = 'released' AND released_at >= $1) AS released,
                 count(*) FILTER (WHERE status = 'rolled_back' AND updated_at >= $1) AS rolled_back
             FROM releases",
        )
        .bind(since)
        .fetch_one(self.pool())
        .await?)
    }

    async fn release_conflict(&self, id: Uuid) -> Result<Release> {
        let release = self.get_release(id).await?;
        Err(StoreError::Conflict(format!(
            "a decision on this release ({})",
            release.status.as_str()
        )))
    }

    // ---- client reports -------------------------------------------------------

    pub async fn record_report(
        &self,
        release: Uuid,
        unit_hash: &str,
        event: ReportEvent,
    ) -> Result<()> {
        let (installed, failed, crashed) = match event {
            ReportEvent::Installed => (true, false, false),
            ReportEvent::InstallFailed => (false, true, false),
            ReportEvent::Crash => (false, false, true),
            ReportEvent::Healthy => (false, false, false),
        };
        sqlx::query(
            "INSERT INTO release_reports (release_id, unit_hash, installed, install_failed,
                 crashed, crashes)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (release_id, unit_hash) DO UPDATE SET
                 installed = release_reports.installed OR EXCLUDED.installed,
                 install_failed = release_reports.install_failed OR EXCLUDED.install_failed,
                 crashed = release_reports.crashed OR EXCLUDED.crashed,
                 crashes = release_reports.crashes + EXCLUDED.crashes,
                 last_seen = now()",
        )
        .bind(release)
        .bind(unit_hash)
        .bind(installed)
        .bind(failed)
        .bind(crashed)
        .bind(i32::from(crashed))
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn report_stats(&self, release: Uuid) -> Result<ReportStats> {
        Ok(sqlx::query_as(
            "SELECT count(*) AS units, count(*) FILTER (WHERE installed) AS installed,
                 count(*) FILTER (WHERE install_failed AND NOT installed) AS install_failed,
                 count(*) FILTER (WHERE crashed) AS crashed
             FROM release_reports WHERE release_id = $1",
        )
        .bind(release)
        .fetch_one(self.pool())
        .await?)
    }

    // ---- signing keys ---------------------------------------------------------

    /// Every key, newest first (retired ones still verify old signatures).
    pub async fn signing_keys(&self) -> Result<Vec<SigningKey>> {
        Ok(sqlx::query_as(
            "SELECT key_id, public_key, created_at, retired_at FROM release_keys
             ORDER BY created_at DESC",
        )
        .fetch_all(self.pool())
        .await?)
    }

    /// The active key's id and decrypted PKCS#8 document.
    pub async fn active_signing_key(&self) -> Result<Option<(String, Vec<u8>)>> {
        let row: Option<(String, Vec<u8>, Vec<u8>)> = sqlx::query_as(
            "SELECT key_id, pkcs8_ciphertext, pkcs8_nonce FROM release_keys
             WHERE retired_at IS NULL",
        )
        .fetch_optional(self.pool())
        .await?;
        match row {
            Some((key_id, ciphertext, nonce)) => {
                let pkcs8 = self
                    .cipher
                    .decrypt(&nonce, &ciphertext, &key_aad(&key_id))?;
                Ok(Some((key_id, pkcs8)))
            }
            None => Ok(None),
        }
    }

    /// Make a new key the active one (the previous one is retired). With
    /// `only_if_none`, nothing happens when a key is already active (two
    /// nodes making the first key at once).
    pub async fn add_signing_key(
        &self,
        key_id: &str,
        public_key: &[u8],
        pkcs8: &[u8],
        only_if_none: bool,
        actor: &str,
    ) -> Result<()> {
        let (nonce, ciphertext) = self.cipher.encrypt(pkcs8, &key_aad(key_id))?;
        let mut tx = self.pool().begin().await?;
        if only_if_none {
            let active: i64 =
                sqlx::query_scalar("SELECT count(*) FROM release_keys WHERE retired_at IS NULL")
                    .fetch_one(&mut *tx)
                    .await?;
            if active > 0 {
                return Ok(());
            }
        }
        sqlx::query("UPDATE release_keys SET retired_at = now() WHERE retired_at IS NULL")
            .execute(&mut *tx)
            .await?;
        let inserted = sqlx::query(
            "INSERT INTO release_keys (id, key_id, public_key, pkcs8_ciphertext, pkcs8_nonce,
                 created_by)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Uuid::now_v7())
        .bind(key_id)
        .bind(public_key)
        .bind(&ciphertext)
        .bind(&nonce)
        .bind(actor)
        .execute(&mut *tx)
        .await;
        match inserted {
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() && only_if_none => {
                return Ok(());
            }
            other => {
                other?;
            }
        }
        admin_event(
            &mut tx,
            actor,
            "release.signing_key",
            key_id,
            json!({ "key_id": key_id }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
