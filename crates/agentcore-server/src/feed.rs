//! The home feed: what happened lately across the organisation.
//!
//! One time-ordered list merged from sessions, messages between departments
//! and people, proposals, the outbox, releases and goals. Nothing is stored
//! for it: every item is derived from records the other pages already show,
//! so it is readable by whoever can read those pages.
//!
//! Each source contributes at most [`PER_SOURCE`] of its newest records, so
//! paging back far enough on a very busy organisation runs out of history
//! before it runs out of events.

use std::collections::HashMap;

use agentcore_core::MessageScope;
use agentcore_core::{
    DepartmentId, OrgGoal, OrgMessage, OutboxItem, OutboxStatus, Proposal, ProposalStatus, Release,
    ReleaseStatus, SessionInfo, SessionStatus,
};
use agentcore_store::MessageFilter;
use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::AppState;
use crate::api::all_sessions;
use crate::auth::Caller;
use crate::error::ApiError;

type ApiResult<T> = Result<T, ApiError>;

/// Records read from each source.
const PER_SOURCE: i64 = 200;
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;
const KINDS: [&str; 6] = [
    "session", "message", "proposal", "outbox", "release", "goal",
];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FeedItem {
    /// Stable per event, for list keys.
    pub id: String,
    pub at: DateTime<Utc>,
    /// One of [`KINDS`].
    pub kind: &'static str,
    /// How the UI should colour it: `neutral`, `good`, `warn` (waiting for a
    /// person) or `bad`.
    pub tone: &'static str,
    pub title: String,
    pub detail: Option<String>,
    pub actor: Option<String>,
    pub department: Option<String>,
    pub project: Option<String>,
    /// The session to open, for session events.
    pub session: Option<String>,
}

impl FeedItem {
    fn new(id: String, at: DateTime<Utc>, kind: &'static str, title: String) -> Self {
        Self {
            id,
            at,
            kind,
            tone: "neutral",
            title,
            detail: None,
            actor: None,
            department: None,
            project: None,
            session: None,
        }
    }

    fn tone(mut self, tone: &'static str) -> Self {
        self.tone = tone;
        self
    }

    fn detail(mut self, text: &str) -> Self {
        self.detail = snippet(text, 240);
        self
    }

    fn actor(mut self, who: &str) -> Self {
        self.actor = Some(who.to_string()).filter(|w| !w.is_empty());
        self
    }

    fn department(
        mut self,
        names: &HashMap<DepartmentId, String>,
        id: Option<DepartmentId>,
    ) -> Self {
        self.department = id.and_then(|id| names.get(&id).cloned());
        self
    }
}

/// The first `max` characters of `text`, or `None` when there is nothing to show.
fn snippet(text: &str, max: usize) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    Some(out)
}

/// `human:alice` and `agent:<id>` read better without their prefix.
fn who(principal: &str) -> &str {
    principal
        .strip_prefix("human:")
        .or_else(|| principal.strip_prefix("agent:"))
        .unwrap_or(principal)
}

pub fn session_items(sessions: &[SessionInfo]) -> Vec<FeedItem> {
    let mut items = Vec::new();
    for s in sessions {
        let project = s.context.project_name.clone();
        let mut started = FeedItem::new(
            format!("session:{}:start", s.id),
            s.created_at,
            "session",
            format!("{} started a session", s.agent),
        )
        .detail(&s.task)
        .actor(who(&s.created_by.to_string()));
        started.project = project.clone();
        started.session = Some(s.id.to_string());
        items.push(started);

        if let (true, Some(ended)) = (s.status.is_terminal(), s.ended_at) {
            let (verb, tone) = match s.status {
                SessionStatus::Completed => ("finished", "good"),
                SessionStatus::Failed => ("failed", "bad"),
                _ => ("was stopped", "neutral"),
            };
            let mut end = FeedItem::new(
                format!("session:{}:end", s.id),
                ended,
                "session",
                format!("{} {verb}", s.agent),
            )
            .tone(tone)
            .detail(&s.task);
            end.project = project;
            end.session = Some(s.id.to_string());
            items.push(end);
        }
    }
    items
}

/// Messages inside one department are chatter; the feed shows what crosses
/// a department's border or comes from a person.
pub fn message_items(messages: &[OrgMessage]) -> Vec<FeedItem> {
    messages
        .iter()
        .filter(|m| m.scope != MessageScope::Internal)
        .map(|m| {
            FeedItem::new(
                format!("message:{}", m.id),
                m.created_at,
                "message",
                format!("{} → {}", m.from_name, m.to_name),
            )
            .detail(&m.text)
            .actor(&m.from_name)
        })
        .collect()
}

pub fn proposal_items(proposals: &[Proposal]) -> Vec<FeedItem> {
    let mut items = Vec::new();
    for p in proposals {
        let waiting = matches!(
            p.status,
            ProposalStatus::Open | ProposalStatus::ChangesRequested
        );
        items.push(
            FeedItem::new(
                format!("proposal:{}:new", p.id),
                p.created_at,
                "proposal",
                format!("{} proposed: {}", who(&p.proposed_by), p.title),
            )
            .tone(if waiting { "warn" } else { "neutral" })
            .detail(&p.problem)
            .actor(who(&p.proposed_by)),
        );
        let decided = match p.status {
            ProposalStatus::Applied => Some(("applied", "good")),
            ProposalStatus::Rejected => Some(("rejected", "neutral")),
            ProposalStatus::Failed => Some(("failed to apply", "bad")),
            _ => None,
        };
        if let (Some((verb, tone)), Some(at)) = (decided, p.decided_at) {
            let by = p.decided_by.as_deref().map(who).unwrap_or("someone");
            items.push(
                FeedItem::new(
                    format!("proposal:{}:decided", p.id),
                    at,
                    "proposal",
                    format!("{by} {verb}: {}", p.title),
                )
                .tone(tone)
                .actor(by),
            );
        }
    }
    items
}

pub fn outbox_items(
    drafts: &[OutboxItem],
    departments: &HashMap<DepartmentId, String>,
) -> Vec<FeedItem> {
    let mut items = Vec::new();
    for d in drafts {
        let what = if d.subject.trim().is_empty() {
            &d.body
        } else {
            &d.subject
        };
        items.push(
            FeedItem::new(
                format!("outbox:{}:draft", d.id),
                d.created_at,
                "outbox",
                format!("{} drafted a message", who(&d.drafted_by)),
            )
            .tone(if d.status == OutboxStatus::Pending {
                "warn"
            } else {
                "neutral"
            })
            .detail(what)
            .actor(who(&d.drafted_by))
            .department(departments, d.department_id),
        );
        let outcome = match d.status {
            OutboxStatus::Sent => d.sent_at.map(|at| (at, "was sent", "good")),
            OutboxStatus::Rejected => d.decided_at.map(|at| (at, "was rejected", "neutral")),
            OutboxStatus::Failed => Some((d.updated_at, "could not be sent", "bad")),
            _ => None,
        };
        if let Some((at, verb, tone)) = outcome {
            let mut item = FeedItem::new(
                format!("outbox:{}:{}", d.id, d.status.as_str()),
                at,
                "outbox",
                format!("A message {verb}"),
            )
            .tone(tone)
            .detail(what)
            .department(departments, d.department_id);
            if let Some(by) = &d.decided_by {
                item = item.actor(who(by));
            }
            if let (OutboxStatus::Failed, Some(err)) = (d.status, &d.error) {
                item = item.detail(err);
            }
            items.push(item);
        }
    }
    items
}

pub fn release_items(
    releases: &[Release],
    products: &HashMap<Uuid, String>,
    departments: &HashMap<DepartmentId, String>,
) -> Vec<FeedItem> {
    let mut items = Vec::new();
    for r in releases {
        let name = products
            .get(&r.product_id)
            .map_or("A product", String::as_str);
        let label = format!("{name} {}", r.version);
        let waiting = matches!(
            r.status,
            ReleaseStatus::Proposed | ReleaseStatus::ChangesRequested
        );
        let base = |suffix: &str, at, title: String| {
            FeedItem::new(format!("release:{}:{suffix}", r.id), at, "release", title)
                .department(departments, r.department_id)
        };
        items.push(
            base(
                "proposed",
                r.created_at,
                format!("{} proposed release {label}", who(&r.proposed_by)),
            )
            .tone(if waiting { "warn" } else { "neutral" })
            .detail(&r.notes)
            .actor(who(&r.proposed_by)),
        );
        if let Some(at) = r.started_at {
            items.push(base("started", at, format!("{label} started rolling out")));
        }
        if let Some(at) = r.released_at {
            items.push(base("released", at, format!("{label} is out for everyone")).tone("good"));
        }
        let ended = match r.status {
            ReleaseStatus::RolledBack => Some(("was rolled back", "bad")),
            ReleaseStatus::Rejected => Some(("was rejected", "neutral")),
            _ => None,
        };
        if let Some((verb, tone)) = ended {
            let mut item = base("ended", r.updated_at, format!("{label} {verb}")).tone(tone);
            if let Some(reason) = &r.reason {
                item = item.detail(reason);
            }
            if let Some(by) = &r.decided_by {
                item = item.actor(who(by));
            }
            items.push(item);
        }
    }
    items
}

pub fn goal_items(goals: &[OrgGoal], departments: &HashMap<DepartmentId, String>) -> Vec<FeedItem> {
    let mut items = Vec::new();
    for g in goals {
        items.push(
            FeedItem::new(
                format!("goal:{}:new", g.id),
                g.created_at,
                "goal",
                format!("{} set a goal: {}", who(&g.created_by), g.title),
            )
            .detail(&g.description)
            .actor(who(&g.created_by))
            .department(departments, g.department_id),
        );
        if let Some(at) = g.progress_at.filter(|_| !g.progress.trim().is_empty()) {
            let by = g.progress_by.as_deref().map(who).unwrap_or("Someone");
            items.push(
                FeedItem::new(
                    format!("goal:{}:progress", g.id),
                    at,
                    "goal",
                    format!("{by} reported progress on {}", g.title),
                )
                .detail(&g.progress)
                .actor(by)
                .department(departments, g.department_id),
            );
        }
    }
    items
}

#[derive(Debug, Deserialize)]
pub struct FeedQuery {
    /// How many items to return (default 50, at most 200).
    pub limit: Option<usize>,
    /// Only items older than this, to page back.
    pub before: Option<DateTime<Utc>>,
    /// Comma-separated kinds to keep (default: all).
    pub kind: Option<String>,
}

/// `GET /feed`: `{ items, has_more }`, newest first.
pub async fn feed(
    State(state): State<AppState>,
    _caller: Caller,
    Query(q): Query<FeedQuery>,
) -> ApiResult<Json<Value>> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let kinds: Option<Vec<&str>> = q.kind.as_deref().map(|k| {
        k.split(',')
            .map(str::trim)
            .filter(|k| KINDS.contains(k))
            .collect()
    });
    let wants = |kind: &str| kinds.as_ref().is_none_or(|k| k.contains(&kind));

    let departments: HashMap<_, _> = state
        .store
        .list_departments()
        .await?
        .into_iter()
        .map(|d| (d.id, d.name))
        .collect();
    let mut items = Vec::new();
    if wants("session") {
        items.extend(session_items(&all_sessions(&state).await?));
    }
    if wants("message") {
        let messages = state
            .store
            .list_messages(MessageFilter {
                department: None,
                agent: None,
                after: None,
                limit: PER_SOURCE,
            })
            .await?;
        items.extend(message_items(&messages));
    }
    if wants("proposal") {
        let mut proposals = state.store.list_proposals(None).await?;
        proposals.sort_by_key(|p| std::cmp::Reverse(p.created_at));
        proposals.truncate(PER_SOURCE as usize);
        items.extend(proposal_items(&proposals));
    }
    if wants("outbox") {
        let drafts = state.store.list_outbox(None, None, PER_SOURCE).await?;
        items.extend(outbox_items(&drafts, &departments));
    }
    if wants("release") {
        let products: HashMap<_, _> = state
            .store
            .list_products()
            .await?
            .into_iter()
            .map(|p| (p.id, p.name))
            .collect();
        let releases = state
            .store
            .list_releases(None, None, None, PER_SOURCE)
            .await?;
        items.extend(release_items(&releases, &products, &departments));
    }
    if wants("goal") {
        items.extend(goal_items(&state.store.list_goals().await?, &departments));
    }

    items.retain(|i| q.before.is_none_or(|before| i.at < before));
    items.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.id.cmp(&b.id)));
    let has_more = items.len() > limit;
    items.truncate(limit);
    Ok(Json(json!({ "items": items, "has_more": has_more })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_are_trimmed_and_cut() {
        assert_eq!(snippet("  \n ", 10), None);
        assert_eq!(snippet(" hi ", 10).as_deref(), Some("hi"));
        assert_eq!(snippet("abcdef", 3).as_deref(), Some("abc…"));
        assert_eq!(snippet("äöü", 3).as_deref(), Some("äöü"));
    }

    #[test]
    fn principals_lose_their_prefix() {
        assert_eq!(who("human:alice"), "alice");
        assert_eq!(who("agent:7"), "7");
        assert_eq!(who("system"), "system");
    }

    #[test]
    fn empty_actors_are_left_out() {
        let item = FeedItem::new("x".into(), Utc::now(), "goal", "t".into()).actor("");
        assert_eq!(item.actor, None);
    }
}
