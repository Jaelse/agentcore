//! The Cloudflare connection: an API token (encrypted) and the account it
//! works in. Agents never see the token; agentcore calls Cloudflare for them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::types::Json;

use crate::{Result, Store, StoreError, admin_event, key_hint};

const CLOUDFLARE_AAD: &[u8] = b"integration:cloudflare";

/// Non-secret Cloudflare settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudflareConfig {
    /// The account resources are created in (32 hex characters).
    pub account_id: String,
    /// The account's name, as Cloudflare reported it when connecting.
    pub account_name: String,
}

/// The Cloudflare connection as shown to admins (no token).
#[derive(Debug, Clone, Serialize)]
pub struct CloudflareConnection {
    pub account_id: String,
    pub account_name: String,
    pub token_hint: String,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

/// Whether `id` looks like a Cloudflare account id.
pub fn valid_account_id(id: &str) -> bool {
    id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())
}

impl Store {
    pub async fn cloudflare_connection(&self) -> Result<Option<CloudflareConnection>> {
        let row: Option<(Json<CloudflareConfig>, String, DateTime<Utc>, String)> = sqlx::query_as(
            "SELECT config, secret_hint, updated_at, updated_by FROM integrations WHERE kind = 'cloudflare'",
        )
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(
            |(config, token_hint, updated_at, updated_by)| CloudflareConnection {
                account_id: config.0.account_id,
                account_name: config.0.account_name,
                token_hint,
                updated_at,
                updated_by,
            },
        ))
    }

    /// The account settings and the decrypted token (server use only).
    pub async fn cloudflare_credentials(&self) -> Result<Option<(CloudflareConfig, String)>> {
        let row: Option<(Json<CloudflareConfig>, Vec<u8>, Vec<u8>)> = sqlx::query_as(
            "SELECT config, secret_ciphertext, secret_nonce FROM integrations WHERE kind = 'cloudflare'",
        )
        .fetch_optional(self.pool())
        .await?;
        let Some((config, ciphertext, nonce)) = row else {
            return Ok(None);
        };
        let token = self.cipher.decrypt(&nonce, &ciphertext, CLOUDFLARE_AAD)?;
        let token = String::from_utf8(token).map_err(|_| StoreError::Crypto)?;
        Ok(Some((config.0, token)))
    }

    /// Connect Cloudflare, or change the account. `token: None` keeps the
    /// stored token (only allowed when one is stored).
    pub async fn set_cloudflare(
        &self,
        config: &CloudflareConfig,
        token: Option<&str>,
        actor: &str,
    ) -> Result<CloudflareConnection> {
        if !valid_account_id(&config.account_id) {
            return Err(StoreError::Invalid(
                "the account id is the 32-character id shown in the Cloudflare dashboard's URL"
                    .into(),
            ));
        }
        let token = token.map(str::trim).filter(|t| !t.is_empty());
        let mut tx = self.pool().begin().await?;
        match token {
            Some(token) => {
                let (nonce, ciphertext) = self.cipher.encrypt(token.as_bytes(), CLOUDFLARE_AAD)?;
                sqlx::query(
                    "INSERT INTO integrations (kind, config, secret_ciphertext, secret_nonce, secret_hint, updated_by)
                     VALUES ('cloudflare', $1, $2, $3, $4, $5)
                     ON CONFLICT (kind) DO UPDATE SET config = EXCLUDED.config,
                         secret_ciphertext = EXCLUDED.secret_ciphertext,
                         secret_nonce = EXCLUDED.secret_nonce, secret_hint = EXCLUDED.secret_hint,
                         updated_at = now(), updated_by = EXCLUDED.updated_by",
                )
                .bind(Json(config))
                .bind(&ciphertext)
                .bind(&nonce)
                .bind(key_hint(token))
                .bind(actor)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                let updated = sqlx::query(
                    "UPDATE integrations SET config = $1, updated_at = now(), updated_by = $2
                     WHERE kind = 'cloudflare'",
                )
                .bind(Json(config))
                .bind(actor)
                .execute(&mut *tx)
                .await?;
                if updated.rows_affected() == 0 {
                    return Err(StoreError::Invalid(
                        "an API token is required to connect Cloudflare".into(),
                    ));
                }
            }
        }
        admin_event(
            &mut tx,
            actor,
            "cloudflare.update",
            "cloudflare",
            serde_json::json!({
                "account_id": config.account_id,
                "token": if token.is_some() { "set" } else { "unchanged" },
            }),
        )
        .await?;
        tx.commit().await?;
        self.cloudflare_connection()
            .await?
            .ok_or_else(|| StoreError::NotFound("cloudflare connection".into()))
    }

    pub async fn delete_cloudflare(&self, actor: &str) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("DELETE FROM integrations WHERE kind = 'cloudflare'")
            .execute(&mut *tx)
            .await?;
        admin_event(
            &mut tx,
            actor,
            "cloudflare.delete",
            "cloudflare",
            serde_json::json!({}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_ids_are_32_hex_characters() {
        assert!(valid_account_id("0123456789abcdef0123456789ABCDEF"));
        assert!(!valid_account_id("0123456789abcdef"));
        assert!(!valid_account_id("0123456789abcdef0123456789abcdeg"));
        assert!(!valid_account_id("../../user/tokens/verify/xxxxxxxx"));
    }
}
