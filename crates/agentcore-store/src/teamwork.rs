//! GitHub connection, projects, and change snapshots.

use agentcore_core::{Changes, SessionId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::types::Json;
use uuid::Uuid;

use crate::{Result, Store, StoreError, admin_event, key_hint};

const GITHUB_AAD: &[u8] = b"integration:github";

/// Non-secret GitHub connection settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GitHubConfig {
    /// REST API base, e.g. `https://api.github.com` or `https://ghe.example.com/api/v3`.
    pub api_url: String,
    /// Web / git base, e.g. `https://github.com`.
    pub web_url: String,
    /// Author of the agent's commits.
    pub commit_name: String,
    pub commit_email: String,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            api_url: "https://api.github.com".into(),
            web_url: "https://github.com".into(),
            commit_name: "agentcore[bot]".into(),
            commit_email: "agentcore@users.noreply.github.com".into(),
        }
    }
}

/// GitHub connection as shown to admins (no token).
#[derive(Debug, Clone, Serialize)]
pub struct GitHubConnection {
    pub config: GitHubConfig,
    pub token_hint: String,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GitHubUpdate {
    /// New token; omitted or empty keeps the current one.
    #[serde(default)]
    pub token: Option<String>,
    #[serde(flatten)]
    pub config: GitHubConfig,
}

/// Board column names (GitHub Projects "Status" options) per stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BoardColumns {
    pub ready: String,
    pub in_progress: String,
    pub in_review: String,
    pub done: String,
}

impl Default for BoardColumns {
    fn default() -> Self {
        Self {
            ready: "Todo".into(),
            in_progress: "In Progress".into(),
            in_review: "In Review".into(),
            done: "Done".into(),
        }
    }
}

/// A GitHub Projects (v2) board linked to a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardConfig {
    /// User or organisation that owns the board.
    pub owner: String,
    pub number: i64,
    #[serde(default = "status_field")]
    pub status_field: String,
    /// Iteration ("sprint") field, if the board uses one.
    #[serde(default)]
    pub iteration_field: Option<String>,
    #[serde(default)]
    pub columns: BoardColumns,
}

fn status_field() -> String {
    "Status".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub repo_owner: String,
    pub repo_name: String,
    pub default_branch: String,
    pub agent: String,
    pub role: String,
    pub board: Option<BoardConfig>,
    pub notes: String,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

impl Project {
    pub fn repository(&self) -> String {
        format!("{}/{}", self.repo_owner, self.repo_name)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectInput {
    pub name: String,
    /// `owner/name`
    pub repository: String,
    #[serde(default = "main")]
    pub default_branch: String,
    pub agent: String,
    pub role: String,
    #[serde(default)]
    pub board: Option<BoardConfig>,
    #[serde(default)]
    pub notes: String,
}

fn main() -> String {
    "main".into()
}

#[derive(sqlx::FromRow)]
struct ProjectRow {
    id: Uuid,
    name: String,
    repo_owner: String,
    repo_name: String,
    default_branch: String,
    agent: String,
    role: String,
    board: Option<Json<BoardConfig>>,
    notes: String,
    updated_at: DateTime<Utc>,
    updated_by: String,
}

impl From<ProjectRow> for Project {
    fn from(r: ProjectRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            repo_owner: r.repo_owner,
            repo_name: r.repo_name,
            default_branch: r.default_branch,
            agent: r.agent,
            role: r.role,
            board: r.board.map(|b| b.0),
            notes: r.notes,
            updated_at: r.updated_at,
            updated_by: r.updated_by,
        }
    }
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && s != "."
        && s != ".."
}

fn split_repository(repo: &str) -> Result<(String, String)> {
    let repo = repo.trim().trim_end_matches(".git");
    let repo = repo
        .strip_prefix("https://github.com/")
        .unwrap_or(repo)
        .trim_matches('/');
    match repo.split_once('/') {
        Some((owner, name)) if valid_segment(owner) && valid_segment(name) => {
            Ok((owner.to_string(), name.to_string()))
        }
        _ => Err(StoreError::Invalid(
            "repository must look like `owner/name`".into(),
        )),
    }
}

fn validate_branch(branch: &str) -> Result<()> {
    let ok = !branch.is_empty()
        && !branch.starts_with('-')
        && !branch.contains("..")
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/-_.".contains(c));
    if ok {
        Ok(())
    } else {
        Err(StoreError::Invalid(format!(
            "invalid branch name `{branch}`"
        )))
    }
}

impl Store {
    // ---- GitHub connection ----------------------------------------------------

    pub async fn github_connection(&self) -> Result<Option<GitHubConnection>> {
        let row: Option<(Json<GitHubConfig>, String, DateTime<Utc>, String)> = sqlx::query_as(
            "SELECT config, secret_hint, updated_at, updated_by FROM integrations WHERE kind = 'github'",
        )
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(
            |(config, token_hint, updated_at, updated_by)| GitHubConnection {
                config: config.0,
                token_hint,
                updated_at,
                updated_by,
            },
        ))
    }

    /// Connection settings plus the decrypted token (server use only).
    pub async fn github_credentials(&self) -> Result<Option<(GitHubConfig, String)>> {
        let row: Option<(Json<GitHubConfig>, Vec<u8>, Vec<u8>)> = sqlx::query_as(
            "SELECT config, secret_ciphertext, secret_nonce FROM integrations WHERE kind = 'github'",
        )
        .fetch_optional(self.pool())
        .await?;
        let Some((config, ciphertext, nonce)) = row else {
            return Ok(None);
        };
        let token = self.cipher.decrypt(&nonce, &ciphertext, GITHUB_AAD)?;
        let token = String::from_utf8(token).map_err(|_| StoreError::Crypto)?;
        Ok(Some((config.0, token)))
    }

    pub async fn set_github(&self, update: GitHubUpdate, actor: &str) -> Result<GitHubConnection> {
        let config = update.config;
        for url in [&config.api_url, &config.web_url] {
            if !(url.starts_with("https://")
                || url.starts_with("http://")
                || url.starts_with("file://"))
            {
                return Err(StoreError::Invalid(format!("`{url}` is not a URL")));
            }
        }
        let token = update
            .token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty());
        let mut tx = self.pool().begin().await?;
        match token {
            Some(token) => {
                let (nonce, ciphertext) = self.cipher.encrypt(token.as_bytes(), GITHUB_AAD)?;
                sqlx::query(
                    "INSERT INTO integrations (kind, config, secret_ciphertext, secret_nonce, secret_hint, updated_by)
                     VALUES ('github', $1, $2, $3, $4, $5)
                     ON CONFLICT (kind) DO UPDATE SET config = EXCLUDED.config,
                         secret_ciphertext = EXCLUDED.secret_ciphertext,
                         secret_nonce = EXCLUDED.secret_nonce, secret_hint = EXCLUDED.secret_hint,
                         updated_at = now(), updated_by = EXCLUDED.updated_by",
                )
                .bind(Json(&config))
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
                     WHERE kind = 'github'",
                )
                .bind(Json(&config))
                .bind(actor)
                .execute(&mut *tx)
                .await?;
                if updated.rows_affected() == 0 {
                    return Err(StoreError::Invalid(
                        "a token is required to connect GitHub".into(),
                    ));
                }
            }
        }
        let mut details = serde_json::to_value(&config).unwrap_or_default();
        if token.is_some() {
            details["token"] = "set".into();
        }
        admin_event(&mut tx, actor, "github.update", "github", details).await?;
        tx.commit().await?;
        self.github_connection()
            .await?
            .ok_or_else(|| StoreError::NotFound("github connection".into()))
    }

    pub async fn delete_github(&self, actor: &str) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("DELETE FROM integrations WHERE kind = 'github'")
            .execute(&mut *tx)
            .await?;
        admin_event(
            &mut tx,
            actor,
            "github.delete",
            "github",
            serde_json::json!({}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    // ---- projects ------------------------------------------------------------------

    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        let rows: Vec<ProjectRow> = sqlx::query_as("SELECT * FROM projects ORDER BY name")
            .fetch_all(self.pool())
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_project(&self, id: Uuid) -> Result<Option<Project>> {
        let row: Option<ProjectRow> = sqlx::query_as("SELECT * FROM projects WHERE id = $1")
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
    }

    /// Create (`id = None`) or replace a project.
    pub async fn save_project(
        &self,
        id: Option<Uuid>,
        input: ProjectInput,
        actor: &str,
    ) -> Result<Project> {
        let name = input.name.trim().to_string();
        if name.is_empty() {
            return Err(StoreError::Invalid("name must not be empty".into()));
        }
        let (owner, repo) = split_repository(&input.repository)?;
        validate_branch(input.default_branch.trim())?;
        if let Some(board) = &input.board
            && (board.number <= 0 || !valid_segment(&board.owner))
        {
            return Err(StoreError::Invalid(
                "board needs an owner and a positive number".into(),
            ));
        }
        let mut tx = self.pool().begin().await?;
        let id = match id {
            Some(id) => {
                let updated = sqlx::query(
                    "UPDATE projects SET name=$2, repo_owner=$3, repo_name=$4, default_branch=$5, agent=$6,
                         role=$7, board=$8, notes=$9, updated_at=now(), updated_by=$10 WHERE id=$1",
                )
                .bind(id)
                .bind(&name)
                .bind(&owner)
                .bind(&repo)
                .bind(input.default_branch.trim())
                .bind(&input.agent)
                .bind(&input.role)
                .bind(input.board.as_ref().map(Json))
                .bind(&input.notes)
                .bind(actor)
                .execute(&mut *tx)
                .await
                .map_err(unique_name)?;
                if updated.rows_affected() == 0 {
                    return Err(StoreError::NotFound(format!("project {id}")));
                }
                admin_event(
                    &mut tx,
                    actor,
                    "project.update",
                    &name,
                    serde_json::json!({"id": id}),
                )
                .await?;
                id
            }
            None => {
                let id = Uuid::now_v7();
                sqlx::query(
                    "INSERT INTO projects (id, name, repo_owner, repo_name, default_branch, agent, role, board, notes, updated_by)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
                )
                .bind(id)
                .bind(&name)
                .bind(&owner)
                .bind(&repo)
                .bind(input.default_branch.trim())
                .bind(&input.agent)
                .bind(&input.role)
                .bind(input.board.as_ref().map(Json))
                .bind(&input.notes)
                .bind(actor)
                .execute(&mut *tx)
                .await
                .map_err(unique_name)?;
                admin_event(
                    &mut tx,
                    actor,
                    "project.create",
                    &name,
                    serde_json::json!({"id": id, "repository": format!("{owner}/{repo}")}),
                )
                .await?;
                id
            }
        };
        tx.commit().await?;
        self.get_project(id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("project {id}")))
    }

    pub async fn delete_project(&self, id: Uuid, actor: &str) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        let deleted = sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("project {id}")));
        }
        admin_event(
            &mut tx,
            actor,
            "project.delete",
            &id.to_string(),
            serde_json::json!({}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    // ---- change snapshots ------------------------------------------------------

    pub async fn save_changes(&self, session_id: SessionId, changes: &Changes) -> Result<()> {
        sqlx::query(
            "INSERT INTO session_changes (session_id, changes) VALUES ($1, $2)
             ON CONFLICT (session_id) DO UPDATE SET changes = EXCLUDED.changes, updated_at = now()",
        )
        .bind(session_id)
        .bind(Json(changes))
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn get_changes(&self, session_id: SessionId) -> Result<Option<Changes>> {
        let row: Option<(Json<Changes>,)> =
            sqlx::query_as("SELECT changes FROM session_changes WHERE session_id = $1")
                .bind(session_id)
                .fetch_optional(self.pool())
                .await?;
        Ok(row.map(|r| r.0.0))
    }
}

fn unique_name(err: sqlx::Error) -> StoreError {
    match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            StoreError::Conflict("a project with this name".into())
        }
        _ => err.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_parsing() {
        assert_eq!(
            split_repository("acme/api").unwrap(),
            ("acme".into(), "api".into())
        );
        assert_eq!(
            split_repository("https://github.com/acme/api.git").unwrap(),
            ("acme".into(), "api".into())
        );
        assert!(split_repository("acme").is_err());
        assert!(split_repository("acme/../x").is_err());
        assert!(split_repository("ac me/api").is_err());
        assert!(validate_branch("--upload-pack=x").is_err());
        assert!(validate_branch("release/1.2").is_ok());
    }
}
