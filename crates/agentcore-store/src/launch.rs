//! People who signed up for news about a product, and where visits,
//! downloads and sign-ups came from (daily counts per source).

use agentcore_core::Product;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::{Result, Store, StoreError, admin_event};

/// Distinct sources counted per product; later ones count as `other`
/// (anyone can make up a `src`).
pub const MAX_SOURCES: i64 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchEvent {
    Visit,
    Download,
    Signup,
}

impl LaunchEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Visit => "visit",
            Self::Download => "download",
            Self::Signup => "signup",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Segment {
    Developer,
    Business,
}

impl Segment {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Developer => "developer",
            Self::Business => "business",
        }
    }
}

pub struct NewSubscriber<'a> {
    pub product: Uuid,
    pub email: &'a str,
    pub segment: Segment,
    pub company: Option<&'a str>,
    pub source: &'a str,
    /// The words the person agreed to.
    pub consent: &'a str,
    pub unsubscribe_token: &'a str,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Subscriber {
    pub id: Uuid,
    pub email: String,
    pub segment: String,
    pub company: Option<String>,
    pub source: String,
    pub consent: String,
    pub subscribed_at: DateTime<Utc>,
    pub unsubscribed_at: Option<DateTime<Utc>>,
}

/// Counts per source over a period.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct SourceStats {
    pub source: String,
    pub visits: i64,
    pub downloads: i64,
    pub signups: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LaunchStats {
    pub days: u32,
    pub sources: Vec<SourceStats>,
    pub visits: i64,
    pub downloads: i64,
    pub signups: i64,
    /// People subscribed now (by segment).
    pub subscribers: i64,
    pub developers: i64,
    pub business: i64,
    /// Visits, downloads and sign-ups per day (oldest first).
    pub daily: Vec<(NaiveDate, i64, i64, i64)>,
}

impl Store {
    /// Count a visit, download or sign-up for today.
    pub async fn record_launch_event(
        &self,
        product: Uuid,
        source: &str,
        kind: LaunchEvent,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO launch_events (product_id, day, source, kind, count)
             SELECT $1, (now() AT TIME ZONE 'UTC')::date,
                    CASE WHEN EXISTS (SELECT 1 FROM launch_events WHERE product_id = $1 AND source = $2)
                           OR (SELECT count(DISTINCT source) FROM launch_events WHERE product_id = $1) < $4
                         THEN $2 ELSE 'other' END,
                    $3, 1
             ON CONFLICT (product_id, day, source, kind)
             DO UPDATE SET count = launch_events.count + 1",
        )
        .bind(product)
        .bind(source)
        .bind(kind.as_str())
        .bind(MAX_SOURCES)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn launch_stats(&self, product: Uuid, days: u32) -> Result<LaunchStats> {
        let days = days.clamp(1, 365);
        let since = Utc::now().date_naive() - chrono::Duration::days(i64::from(days) - 1);
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT source, kind, sum(count)::bigint FROM launch_events
             WHERE product_id = $1 AND day >= $2 GROUP BY source, kind",
        )
        .bind(product)
        .bind(since)
        .fetch_all(self.pool())
        .await?;
        let mut stats = LaunchStats {
            days,
            ..Default::default()
        };
        for (source, kind, n) in rows {
            let entry = match stats.sources.iter_mut().find(|s| s.source == source) {
                Some(e) => e,
                None => {
                    stats.sources.push(SourceStats {
                        source,
                        ..Default::default()
                    });
                    stats.sources.last_mut().expect("just pushed")
                }
            };
            match kind.as_str() {
                "visit" => entry.visits += n,
                "download" => entry.downloads += n,
                _ => entry.signups += n,
            }
        }
        stats.sources.sort_by(|a, b| {
            (b.signups, b.downloads, b.visits).cmp(&(a.signups, a.downloads, a.visits))
        });
        stats.visits = stats.sources.iter().map(|s| s.visits).sum();
        stats.downloads = stats.sources.iter().map(|s| s.downloads).sum();
        stats.signups = stats.sources.iter().map(|s| s.signups).sum();
        stats.daily = sqlx::query_as(
            "SELECT day, sum(count) FILTER (WHERE kind = 'visit')::bigint,
                    COALESCE(sum(count) FILTER (WHERE kind = 'download'), 0)::bigint,
                    COALESCE(sum(count) FILTER (WHERE kind = 'signup'), 0)::bigint
             FROM launch_events WHERE product_id = $1 AND day >= $2
             GROUP BY day ORDER BY day",
        )
        .bind(product)
        .bind(since)
        .fetch_all(self.pool())
        .await?
        .into_iter()
        .map(|(d, v, dl, s): (NaiveDate, Option<i64>, i64, i64)| (d, v.unwrap_or(0), dl, s))
        .collect();
        let (all, dev, biz): (i64, i64, i64) = sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE segment = 'developer'),
                    count(*) FILTER (WHERE segment = 'business')
             FROM launch_subscribers WHERE product_id = $1 AND unsubscribed_at IS NULL",
        )
        .bind(product)
        .fetch_one(self.pool())
        .await?;
        stats.subscribers = all;
        stats.developers = dev;
        stats.business = biz;
        Ok(stats)
    }

    /// Add someone who asked for news. `false`: the address was already
    /// subscribed (nothing changes; a person who unsubscribed and signs up
    /// again is subscribed again, with the new consent).
    pub async fn add_subscriber(&self, new: NewSubscriber<'_>) -> Result<bool> {
        let inserted = sqlx::query(
            "INSERT INTO launch_subscribers (id, product_id, email, segment, company, source,
                 consent, unsubscribe_token)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (product_id, lower(email)) DO UPDATE SET
                 segment = EXCLUDED.segment, company = EXCLUDED.company,
                 source = EXCLUDED.source, consent = EXCLUDED.consent,
                 subscribed_at = now(), unsubscribed_at = NULL
             WHERE launch_subscribers.unsubscribed_at IS NOT NULL",
        )
        .bind(Uuid::now_v7())
        .bind(new.product)
        .bind(new.email)
        .bind(new.segment.as_str())
        .bind(new.company)
        .bind(new.source)
        .bind(new.consent)
        .bind(new.unsubscribe_token)
        .execute(self.pool())
        .await?;
        Ok(inserted.rows_affected() > 0)
    }

    /// The unsubscribe token of an address (for the confirmation of a
    /// returning subscriber, which keeps its first token).
    pub async fn unsubscribe_token(&self, product: Uuid, email: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT unsubscribe_token FROM launch_subscribers
             WHERE product_id = $1 AND lower(email) = lower($2)",
        )
        .bind(product)
        .bind(email)
        .fetch_optional(self.pool())
        .await?)
    }

    /// Unsubscribe by token; the address, when someone was subscribed.
    pub async fn unsubscribe(&self, product: Uuid, token: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "UPDATE launch_subscribers SET unsubscribed_at = now()
             WHERE product_id = $1 AND unsubscribe_token = $2 AND unsubscribed_at IS NULL
             RETURNING email",
        )
        .bind(product)
        .bind(token)
        .fetch_optional(self.pool())
        .await?)
    }

    /// Subscribed people (admin export); recorded in the admin log.
    pub async fn export_subscribers(
        &self,
        product: &Product,
        actor: &str,
    ) -> Result<Vec<Subscriber>> {
        let rows: Vec<Subscriber> = sqlx::query_as(
            "SELECT id, email, segment, company, source, consent, subscribed_at, unsubscribed_at
             FROM launch_subscribers WHERE product_id = $1 AND unsubscribed_at IS NULL
             ORDER BY subscribed_at",
        )
        .bind(product.id)
        .fetch_all(self.pool())
        .await?;
        let mut tx = self.pool().begin().await?;
        admin_event(
            &mut tx,
            actor,
            "product.subscribers_export",
            &product.name,
            json!({ "id": product.id, "rows": rows.len() }),
        )
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Enabled products a department announces.
    pub async fn products_announced_by(
        &self,
        department: agentcore_core::DepartmentId,
    ) -> Result<Vec<Product>> {
        Ok(self
            .list_products()
            .await?
            .into_iter()
            .filter(|p| p.enabled && p.announce_departments.contains(&department))
            .collect())
    }

    /// A product by name (for public pages).
    pub async fn product_named(&self, name: &str) -> Result<Product> {
        self.list_products()
            .await?
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| StoreError::NotFound(format!("product `{name}`")))
    }
}
