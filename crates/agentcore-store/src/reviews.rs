//! Reviewing a pull request with the agent: preview access per project
//! (encrypted) and the screenshots the review shows.

use agentcore_core::SessionId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Result, Store, StoreError, admin_event, key_hint};

fn aad(project: Uuid) -> Vec<u8> {
    format!("preview-access:{project}").into_bytes()
}

/// What people see about a project's preview access (never the secrets).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewAccess {
    /// Last characters of the Vercel bypass secret, if one is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bypass_hint: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub username: String,
    pub has_password: bool,
    #[serde(default)]
    pub extra_hosts: Vec<String>,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

/// A change to a project's preview access. A secret left out is kept; an
/// empty one is removed.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PreviewAccessUpdate {
    #[serde(default)]
    pub bypass_secret: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub extra_hosts: Option<Vec<String>>,
}

/// Everything agentcore's browser needs (server use only).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviewSecrets {
    pub bypass_secret: Option<String>,
    pub username: String,
    pub password: Option<String>,
    pub extra_hosts: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Sealed {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bypass: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

type AccessRow = (
    String,
    Vec<String>,
    Vec<u8>,
    Vec<u8>,
    String,
    bool,
    DateTime<Utc>,
    String,
);

/// A host name as people type it: no scheme, path or port; lower case.
fn clean_host(host: &str) -> Option<String> {
    let host = host.trim().to_ascii_lowercase();
    let host = host
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = host.split(['/', '?', '#']).next().unwrap_or_default();
    let ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '*'));
    ok.then(|| host.to_string())
}

impl Store {
    async fn access_row(&self, project: Uuid) -> Result<Option<AccessRow>> {
        Ok(sqlx::query_as(
            "SELECT username, extra_hosts, secret_ciphertext, secret_nonce, bypass_hint,
                    has_password, updated_at, updated_by
             FROM preview_access WHERE project_id = $1",
        )
        .bind(project)
        .fetch_optional(self.pool())
        .await?)
    }

    pub async fn preview_access(&self, project: Uuid) -> Result<Option<PreviewAccess>> {
        Ok(self.access_row(project).await?.map(
            |(username, extra_hosts, _, _, hint, has_password, updated_at, updated_by)| {
                PreviewAccess {
                    bypass_hint: (!hint.is_empty()).then_some(hint),
                    username,
                    has_password,
                    extra_hosts,
                    updated_at,
                    updated_by,
                }
            },
        ))
    }

    /// The decrypted preview access (server use only).
    pub async fn preview_secrets(&self, project: Uuid) -> Result<PreviewSecrets> {
        let Some((username, extra_hosts, ciphertext, nonce, ..)) = self.access_row(project).await?
        else {
            return Ok(PreviewSecrets::default());
        };
        let plain = self.cipher.decrypt(&nonce, &ciphertext, &aad(project))?;
        let sealed: Sealed = serde_json::from_slice(&plain).map_err(|_| StoreError::Crypto)?;
        Ok(PreviewSecrets {
            bypass_secret: sealed.bypass,
            username,
            password: sealed.password,
            extra_hosts,
        })
    }

    pub async fn set_preview_access(
        &self,
        project: Uuid,
        update: PreviewAccessUpdate,
        actor: &str,
    ) -> Result<PreviewAccess> {
        let current = self.preview_secrets(project).await?;
        let pick = |new: Option<String>, old: Option<String>| match new {
            None => old,
            Some(s) if s.trim().is_empty() => None,
            Some(s) => Some(s.trim().to_string()),
        };
        let sealed = Sealed {
            bypass: pick(update.bypass_secret.clone(), current.bypass_secret),
            password: pick(update.password.clone(), current.password),
        };
        let username = update
            .username
            .map(|u| u.trim().to_string())
            .unwrap_or(current.username);
        let extra_hosts = match update.extra_hosts {
            None => current.extra_hosts,
            Some(hosts) => {
                let mut out = Vec::new();
                for h in hosts.iter().filter(|h| !h.trim().is_empty()) {
                    let host = clean_host(h)
                        .ok_or_else(|| StoreError::Invalid(format!("`{h}` is not a host name")))?;
                    if !out.contains(&host) {
                        out.push(host);
                    }
                }
                out
            }
        };
        let plain = serde_json::to_vec(&sealed).map_err(|_| StoreError::Crypto)?;
        let (nonce, ciphertext) = self.cipher.encrypt(&plain, &aad(project))?;
        let hint = sealed.bypass.as_deref().map(key_hint).unwrap_or_default();
        let mut tx = self.pool().begin().await?;
        let found: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM projects WHERE id = $1")
            .bind(project)
            .fetch_optional(&mut *tx)
            .await?;
        if found.is_none() {
            return Err(StoreError::NotFound(format!("project {project}")));
        }
        sqlx::query(
            "INSERT INTO preview_access (project_id, username, extra_hosts, secret_ciphertext,
                 secret_nonce, bypass_hint, has_password, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (project_id) DO UPDATE SET username = EXCLUDED.username,
                 extra_hosts = EXCLUDED.extra_hosts, secret_ciphertext = EXCLUDED.secret_ciphertext,
                 secret_nonce = EXCLUDED.secret_nonce, bypass_hint = EXCLUDED.bypass_hint,
                 has_password = EXCLUDED.has_password, updated_at = now(),
                 updated_by = EXCLUDED.updated_by",
        )
        .bind(project)
        .bind(&username)
        .bind(&extra_hosts)
        .bind(&ciphertext)
        .bind(&nonce)
        .bind(&hint)
        .bind(sealed.password.is_some())
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        let changed = |s: &Option<String>| match s {
            None => "kept",
            Some(s) if s.trim().is_empty() => "removed",
            Some(_) => "set",
        };
        admin_event(
            &mut tx,
            actor,
            "project.preview_access",
            &project.to_string(),
            serde_json::json!({
                "bypass_secret": changed(&update.bypass_secret),
                "password": changed(&update.password),
                "username": username,
                "extra_hosts": extra_hosts,
            }),
        )
        .await?;
        tx.commit().await?;
        self.preview_access(project)
            .await?
            .ok_or_else(|| StoreError::NotFound("preview access".into()))
    }

    // ---- screenshots ---------------------------------------------------------

    pub async fn save_screenshot(
        &self,
        session_id: SessionId,
        mime: &str,
        image: &[u8],
    ) -> Result<Uuid> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO screenshots (id, session_id, mime, image) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(session_id)
        .bind(mime)
        .bind(image)
        .execute(self.pool())
        .await?;
        Ok(id)
    }

    /// A screenshot of this session: its media type and bytes.
    pub async fn screenshot(
        &self,
        session_id: SessionId,
        id: Uuid,
    ) -> Result<Option<(String, Vec<u8>)>> {
        Ok(
            sqlx::query_as("SELECT mime, image FROM screenshots WHERE id = $1 AND session_id = $2")
                .bind(id)
                .bind(session_id)
                .fetch_optional(self.pool())
                .await?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_cleaned() {
        assert_eq!(
            clean_host("https://Accounts.Example.com/sign-in").as_deref(),
            Some("accounts.example.com")
        );
        assert_eq!(
            clean_host("auth.local:8443").as_deref(),
            Some("auth.local:8443")
        );
        assert_eq!(clean_host("bad host"), None);
        assert_eq!(clean_host(" "), None);
    }
}
