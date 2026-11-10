//! Announcements and who acknowledged them.

use agentcore_core::{AnnouncementAck, DepartmentId, OrgAgentId, OrgAnnouncement};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::{Result, Store, StoreError, admin_event};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AnnouncementInput {
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// `None`: everyone in the organisation.
    #[serde(default)]
    pub department_id: Option<DepartmentId>,
    #[serde(default)]
    pub ask_ack: bool,
    #[serde(default)]
    pub valid_until: Option<DateTime<Utc>>,
}

const MAX_TITLE: usize = 200;
const MAX_BODY: usize = 8 * 1024;
const MAX_NOTE: usize = 1000;
/// The notice board keeps the latest ones; older ones are history.
const LIST_LIMIT: i64 = 100;

#[derive(sqlx::FromRow)]
struct AnnouncementRow {
    id: Uuid,
    title: String,
    body: String,
    department_id: Option<Uuid>,
    ask_ack: bool,
    valid_until: Option<DateTime<Utc>>,
    withdrawn_at: Option<DateTime<Utc>>,
    withdrawn_by: Option<String>,
    created_by: String,
    created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct AckRow {
    announcement_id: Uuid,
    agent_id: Uuid,
    note: String,
    acked_at: DateTime<Utc>,
}

fn invalid(msg: impl Into<String>) -> StoreError {
    StoreError::Invalid(msg.into())
}

impl Store {
    /// The latest announcements, newest first, with their acknowledgements.
    pub async fn list_announcements(&self) -> Result<Vec<OrgAnnouncement>> {
        let rows: Vec<AnnouncementRow> = sqlx::query_as(
            "SELECT id, title, body, department_id, ask_ack, valid_until, withdrawn_at,
                    withdrawn_by, created_by, created_at
             FROM org_announcements ORDER BY created_at DESC LIMIT $1",
        )
        .bind(LIST_LIMIT)
        .fetch_all(&self.pool)
        .await?;
        let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
        let acks: Vec<AckRow> = sqlx::query_as(
            "SELECT announcement_id, agent_id, note, acked_at FROM org_announcement_acks
             WHERE announcement_id = ANY($1) ORDER BY acked_at",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| OrgAnnouncement {
                acks: acks
                    .iter()
                    .filter(|a| a.announcement_id == r.id)
                    .map(|a| AnnouncementAck {
                        agent_id: a.agent_id,
                        note: a.note.clone(),
                        acked_at: a.acked_at,
                    })
                    .collect(),
                id: r.id,
                title: r.title,
                body: r.body,
                department_id: r.department_id,
                ask_ack: r.ask_ack,
                valid_until: r.valid_until,
                withdrawn_at: r.withdrawn_at,
                withdrawn_by: r.withdrawn_by,
                created_by: r.created_by,
                created_at: r.created_at,
            })
            .collect())
    }

    pub async fn get_announcement(&self, id: Uuid) -> Result<OrgAnnouncement> {
        // Organisations announce rarely; the list is small.
        self.list_announcements()
            .await?
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| StoreError::NotFound(format!("announcement {id}")))
    }

    pub async fn create_announcement(
        &self,
        input: AnnouncementInput,
        actor: &str,
    ) -> Result<OrgAnnouncement> {
        let title = input.title.trim();
        if title.is_empty() || title.chars().count() > MAX_TITLE {
            return Err(invalid(format!(
                "an announcement needs a title of up to {MAX_TITLE} characters"
            )));
        }
        let body = input.body.trim();
        if body.chars().count() > MAX_BODY {
            return Err(invalid(format!(
                "an announcement is at most {MAX_BODY} characters"
            )));
        }
        if input.valid_until.is_some_and(|u| u <= Utc::now()) {
            return Err(invalid("`valid until` must be in the future"));
        }
        let id = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO org_announcements
                 (id, title, body, department_id, ask_ack, valid_until, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(title)
        .bind(body)
        .bind(input.department_id)
        .bind(input.ask_ack)
        .bind(input.valid_until)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            "announcement.create",
            title,
            serde_json::json!({ "id": id, "department_id": input.department_id }),
        )
        .await?;
        tx.commit().await?;
        self.get_announcement(id).await
    }

    /// Takes it off the notice board (and out of the agents' instructions).
    pub async fn withdraw_announcement(&self, id: Uuid, actor: &str) -> Result<OrgAnnouncement> {
        let current = self.get_announcement(id).await?;
        if current.withdrawn_at.is_some() {
            return Ok(current);
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE org_announcements SET withdrawn_at = now(), withdrawn_by = $2 WHERE id = $1",
        )
        .bind(id)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        admin_event(
            &mut tx,
            actor,
            "announcement.withdraw",
            &current.title,
            serde_json::json!({ "id": id }),
        )
        .await?;
        tx.commit().await?;
        self.get_announcement(id).await
    }

    /// An agent confirms it read the announcement; acknowledging again replaces the note.
    pub async fn acknowledge_announcement(
        &self,
        id: Uuid,
        agent: OrgAgentId,
        note: &str,
    ) -> Result<()> {
        let note = note.trim();
        if note.chars().count() > MAX_NOTE {
            return Err(invalid(format!(
                "an acknowledgement note is at most {MAX_NOTE} characters"
            )));
        }
        sqlx::query(
            "INSERT INTO org_announcement_acks (announcement_id, agent_id, note)
             VALUES ($1, $2, $3)
             ON CONFLICT (announcement_id, agent_id)
             DO UPDATE SET note = EXCLUDED.note, acked_at = now()",
        )
        .bind(id)
        .bind(agent)
        .bind(note)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
