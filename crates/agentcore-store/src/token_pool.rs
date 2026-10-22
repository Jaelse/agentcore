//! Pool of Claude subscription tokens: one token per running session.

use agentcore_core::SessionId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Result, Store, StoreError, admin_event, key_hint};

/// Statuses after which a session no longer needs its token.
const ENDED: &str = "('stopped', 'completed', 'failed')";

#[derive(Debug, Clone, Serialize)]
pub struct PoolToken {
    pub id: Uuid,
    pub label: String,
    pub hint: String,
    pub enabled: bool,
    /// The session using the token right now.
    pub in_use_by: Option<SessionId>,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewPoolToken {
    pub label: String,
    pub token: String,
}

/// Result of asking the pool for a session's token.
#[derive(Debug)]
pub enum Lease {
    /// The provider has no pool: use its own key.
    NoPool,
    /// The token for this session.
    Token(String),
    /// Every token is in use (or disabled): the user has to add more.
    Exhausted { total: i64 },
}

impl Store {
    /// Tokens in the provider's pool, with who uses them.
    pub async fn list_pool_tokens(&self, provider: &str) -> Result<Vec<PoolToken>> {
        let rows: Vec<(
            Uuid,
            String,
            String,
            bool,
            Option<Uuid>,
            DateTime<Utc>,
            String,
        )> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT t.id, t.label, t.hint, t.enabled,
                        CASE WHEN s.id IS NOT NULL AND s.status NOT IN {ENDED} THEN s.id END,
                        t.created_at, t.created_by
                 FROM claude_tokens t LEFT JOIN sessions s ON s.id = t.session_id
                 WHERE t.provider = $1 ORDER BY t.created_at, t.id"
        )))
        .bind(provider)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, label, hint, enabled, in_use_by, created_at, created_by)| PoolToken {
                    id,
                    label,
                    hint,
                    enabled,
                    in_use_by,
                    created_at,
                    created_by,
                },
            )
            .collect())
    }

    pub async fn add_pool_token(
        &self,
        provider: &str,
        new: NewPoolToken,
        actor: &str,
    ) -> Result<PoolToken> {
        let token = new.token.trim();
        let label = new.label.trim();
        if token.is_empty() {
            return Err(StoreError::Invalid("token must not be empty".into()));
        }
        if label.is_empty() || label.len() > 80 {
            return Err(StoreError::Invalid("label must be 1-80 characters".into()));
        }
        let kind: Option<(String,)> =
            sqlx::query_as("SELECT kind FROM model_providers WHERE name = $1")
                .bind(provider)
                .fetch_optional(&self.pool)
                .await?;
        match kind {
            None => return Err(StoreError::NotFound(format!("provider `{provider}`"))),
            Some((k,)) if k != "claude_subscription" => {
                return Err(StoreError::Invalid(
                    "only a Claude subscription provider has a token pool".into(),
                ));
            }
            Some(_) => {}
        }
        let id = Uuid::new_v4();
        let (nonce, ciphertext) = self
            .cipher
            .encrypt(token.as_bytes(), id.to_string().as_bytes())?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO claude_tokens (id, provider, label, ciphertext, nonce, hint, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(provider)
        .bind(label)
        .bind(&ciphertext)
        .bind(&nonce)
        .bind(key_hint(token))
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            "provider.token_add",
            provider,
            serde_json::json!({ "token": id, "label": label }),
        )
        .await?;
        tx.commit().await?;
        self.list_pool_tokens(provider)
            .await?
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| StoreError::NotFound("token".into()))
    }

    pub async fn set_pool_token_enabled(
        &self,
        provider: &str,
        id: Uuid,
        enabled: bool,
        actor: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let done =
            sqlx::query("UPDATE claude_tokens SET enabled = $3 WHERE id = $1 AND provider = $2")
                .bind(id)
                .bind(provider)
                .bind(enabled)
                .execute(&mut *tx)
                .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound("token".into()));
        }
        admin_event(
            &mut tx,
            actor,
            "provider.token_update",
            provider,
            serde_json::json!({ "token": id, "enabled": enabled }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn remove_pool_token(&self, provider: &str, id: Uuid, actor: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let done = sqlx::query("DELETE FROM claude_tokens WHERE id = $1 AND provider = $2")
            .bind(id)
            .bind(provider)
            .execute(&mut *tx)
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound("token".into()));
        }
        admin_event(
            &mut tx,
            actor,
            "provider.token_remove",
            provider,
            serde_json::json!({ "token": id }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The token for `session`: the one it already holds, else a free one
    /// (the longest unused first). Safe to call from several nodes at once.
    pub async fn lease_pool_token(&self, provider: &str, session: SessionId) -> Result<Lease> {
        let (total,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM claude_tokens WHERE provider = $1")
                .bind(provider)
                .fetch_one(&self.pool)
                .await?;
        if total == 0 {
            return Ok(Lease::NoPool);
        }
        let row: Option<(Uuid, Vec<u8>, Vec<u8>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE claude_tokens SET session_id = $2, leased_at = now()
             WHERE id = (
                 SELECT t.id FROM claude_tokens t
                 WHERE t.provider = $1 AND t.enabled
                   AND (t.session_id = $2 OR t.session_id IS NULL
                        OR NOT EXISTS (SELECT 1 FROM sessions s
                                       WHERE s.id = t.session_id AND s.status NOT IN {ENDED}))
                 ORDER BY (t.session_id = $2) DESC NULLS LAST, t.leased_at NULLS FIRST
                 LIMIT 1 FOR UPDATE SKIP LOCKED)
             RETURNING id, ciphertext, nonce"
        )))
        .bind(provider)
        .bind(session)
        .fetch_optional(&self.pool)
        .await?;
        let Some((id, ciphertext, nonce)) = row else {
            return Ok(Lease::Exhausted { total });
        };
        let token = self
            .cipher
            .decrypt(&nonce, &ciphertext, id.to_string().as_bytes())?;
        Ok(Lease::Token(
            String::from_utf8(token).map_err(|_| StoreError::Crypto)?,
        ))
    }
}
