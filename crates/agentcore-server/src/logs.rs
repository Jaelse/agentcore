//! Logs from everywhere in agentcore, collected into one file an admin
//! downloads, for example to hand to a model for analysis.
//!
//! * **server**: each node's operational (`tracing`) log. [`capture`] keeps the
//!   newest records of the node in memory, and warnings and errors are saved
//!   to PostgreSQL as well, so they are still there after the node went away.
//!   The export asks every live node for its records.
//! * **audit**: the hash-chained audit log of every session in the window
//!   (actions, verdicts, approvals, model calls, agent output).
//! * **admin**: configuration changes. **activity**: what department agents did.
//!
//! Formats: Markdown written to be read by a model (what agentcore is, what
//! each source means, the problems grouped, then the timeline, within a size
//! budget), JSON Lines, JSON, CSV and plain text. Secrets are redacted in all
//! of them, and every export is recorded in the change history.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use agentcore_core::{ActionOutcome, EventKind, OutputStream, SessionStatus, Verdict};
use agentcore_store::{NodeInfo, NodeLog, SessionRecord};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::types::Json as DbJson;
use tokio_util::sync::CancellationToken;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Subscriber, debug};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use uuid::Uuid;

use crate::AppState;
use crate::auth::Caller;
use crate::cluster::FORWARDED;
use crate::error::ApiError;

type ApiResult<T> = Result<T, ApiError>;

/// Records of this node's operational log kept in memory.
const BUFFER_RECORDS: usize = 20_000;
/// How long warnings and errors stay in PostgreSQL.
const RETENTION_DAYS: i64 = 14;
const PERSIST_EVERY: Duration = Duration::from_secs(5);
/// Most sessions whose audit logs one export reads.
const MAX_SESSIONS: i64 = 500;
const DEFAULT_LIMIT: usize = 100_000;
const MAX_LIMIT: usize = 1_000_000;
/// About 75k tokens: fits a model's context with room for the conversation.
const DEFAULT_MAX_CHARS: usize = 300_000;
/// Longest field value shown in the Markdown and text formats.
const MAX_FIELD_CHARS: usize = 400;
pub const REDACTED: &str = "[REDACTED]";
/// Target under which the runtime logs each audit event.
const AUDIT_ECHO: &str = "agentcore::event";

// ---- levels and sources -----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn of(level: &tracing::Level) -> Self {
        match *level {
            tracing::Level::ERROR => Self::Error,
            tracing::Level::WARN => Self::Warn,
            tracing::Level::INFO => Self::Info,
            _ => Self::Debug,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "error" => Self::Error,
            "warn" => Self::Warn,
            "info" => Self::Info,
            _ => Self::Debug,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Server,
    Audit,
    Admin,
    Activity,
}

impl Source {
    const ALL: [Self; 4] = [Self::Server, Self::Audit, Self::Admin, Self::Activity];

    fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Audit => "audit",
            Self::Admin => "admin",
            Self::Activity => "activity",
        }
    }
}

// ---- capturing this node's operational log ----------------------------------------

/// One record of a node's operational log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRecord {
    pub id: Uuid,
    /// Order in this node's buffer.
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub level: Level,
    pub target: String,
    pub message: String,
    /// The event's fields, and those of the spans it happened in.
    pub fields: Map<String, Value>,
}

struct Buffer {
    records: VecDeque<LogRecord>,
    next_seq: u64,
}

static BUFFER: LazyLock<Mutex<Buffer>> = LazyLock::new(|| {
    Mutex::new(Buffer {
        records: VecDeque::with_capacity(1024),
        next_seq: 1,
    })
});

/// Last `seq` saved to PostgreSQL.
static PERSISTED: AtomicU64 = AtomicU64::new(0);

fn push(mut record: LogRecord) {
    let mut buffer = BUFFER.lock().unwrap_or_else(|p| p.into_inner());
    record.seq = buffer.next_seq;
    buffer.next_seq += 1;
    if buffer.records.len() == BUFFER_RECORDS {
        buffer.records.pop_front();
    }
    buffer.records.push_back(record);
}

/// This node's buffered records in a time window.
pub fn recent(since: DateTime<Utc>, until: DateTime<Utc>) -> Vec<LogRecord> {
    let buffer = BUFFER.lock().unwrap_or_else(|p| p.into_inner());
    buffer
        .records
        .iter()
        .filter(|r| r.at >= since && r.at <= until)
        .cloned()
        .collect()
}

/// A `tracing` layer that keeps this node's log for the export. Install it
/// next to the formatting layer:
/// `tracing_subscriber::registry().with(filter).with(fmt).with(logs::capture())`.
pub fn capture() -> Capture {
    Capture
}

pub struct Capture;

#[derive(Default)]
struct Fields(Map<String, Value>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().into(), Value::String(format!("{value:?}")));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0
            .insert(field.name().into(), Value::String(value.into()));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.0
            .insert(field.name().into(), Value::String(value.to_string()));
    }
}

impl<S> Layer<S> for Capture
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(fields);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id)
            && let Some(fields) = span.extensions_mut().get_mut::<Fields>()
        {
            values.record(fields);
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(span_fields) = span.extensions().get::<Fields>() {
                    fields.0.extend(span_fields.0.clone());
                }
            }
        }
        event.record(&mut fields);
        let message = match fields.0.remove("message") {
            Some(Value::String(s)) => s,
            Some(other) => other.to_string(),
            None => String::new(),
        };
        let meta = event.metadata();
        push(LogRecord {
            id: Uuid::now_v7(),
            seq: 0,
            at: Utc::now(),
            level: Level::of(meta.level()),
            target: meta.target().into(),
            message,
            fields: fields.0,
        });
    }
}

// ---- keeping warnings and errors --------------------------------------------------

/// Save this node's new warnings and errors to PostgreSQL every few seconds,
/// and forget old ones.
pub fn spawn_persister(state: AppState, cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(PERSIST_EVERY);
        let mut pruned = std::time::Instant::now() - Duration::from_secs(3600);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                () = cancel.cancelled() => return,
            }
            persist(&state).await;
            if pruned.elapsed() >= Duration::from_secs(3600) {
                pruned = std::time::Instant::now();
                let before = Utc::now() - chrono::Duration::days(RETENTION_DAYS);
                if let Err(err) = state.store.prune_node_logs(before).await {
                    debug!(error = %err, "could not prune node logs");
                }
            }
        }
    });
}

/// Save the warnings and errors logged since the last call.
pub async fn persist(state: &AppState) {
    let last = PERSISTED.load(Ordering::SeqCst);
    let (logs, newest) = {
        let buffer = BUFFER.lock().unwrap_or_else(|p| p.into_inner());
        let newest = buffer.next_seq - 1;
        let logs: Vec<NodeLog> = buffer
            .records
            .iter()
            .filter(|r| r.seq > last && r.level >= Level::Warn)
            .map(|r| NodeLog {
                id: r.id,
                at: r.at,
                node: state.node.to_string(),
                level: r.level.as_str().into(),
                target: r.target.clone(),
                message: r.message.clone(),
                fields: DbJson(Value::Object(r.fields.clone())),
            })
            .collect();
        (logs, newest)
    };
    match state.store.save_node_logs(&logs).await {
        Ok(()) => PERSISTED.store(newest, Ordering::SeqCst),
        // Logged at debug: a warning would be saved next time, and fail again.
        Err(err) => debug!(error = %err, "could not save node logs"),
    }
}

// ---- the export -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// Markdown for a model to read.
    #[default]
    Md,
    Jsonl,
    Json,
    Csv,
    Txt,
}

impl Format {
    fn content_type(self) -> &'static str {
        match self {
            Self::Md => "text/markdown; charset=utf-8",
            Self::Jsonl => "application/x-ndjson",
            Self::Json => "application/json",
            Self::Csv => "text/csv; charset=utf-8",
            Self::Txt => "text/plain; charset=utf-8",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Md => "md",
            Self::Jsonl => "jsonl",
            Self::Json => "json",
            Self::Csv => "csv",
            Self::Txt => "txt",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ExportQuery {
    #[serde(default)]
    pub format: Format,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    /// Comma-separated sources; all when absent.
    pub sources: Option<String>,
    /// Lowest level included.
    pub level: Option<Level>,
    /// Only what concerns this session.
    pub session: Option<Uuid>,
    /// Most records; the newest are kept.
    pub limit: Option<usize>,
    /// Size budget of the Markdown format, in characters.
    pub max_chars: Option<usize>,
}

/// What to collect.
#[derive(Debug, Clone, Serialize)]
pub struct Filter {
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub sources: Vec<Source>,
    pub level: Level,
    pub session: Option<Uuid>,
    pub limit: usize,
}

impl Filter {
    fn from_query(q: &ExportQuery) -> ApiResult<Self> {
        let until = q.until.unwrap_or_else(Utc::now);
        let since = q.since.unwrap_or(until - chrono::Duration::hours(24));
        if since > until {
            return Err(bad_request("`since` is after `until`"));
        }
        let sources = match q.sources.as_deref().map(str::trim) {
            None | Some("") => Source::ALL.to_vec(),
            Some(list) => {
                let mut out = Vec::new();
                for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    let source = Source::ALL
                        .into_iter()
                        .find(|s| s.as_str() == name)
                        .ok_or_else(|| {
                            bad_request(format!(
                                "unknown source `{name}` (server, audit, admin, activity)"
                            ))
                        })?;
                    if !out.contains(&source) {
                        out.push(source);
                    }
                }
                out
            }
        };
        Ok(Self {
            since,
            until,
            sources,
            level: q.level.unwrap_or(Level::Debug),
            session: q.session,
            limit: q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        })
    }

    fn wants(&self, source: Source) -> bool {
        self.sources.contains(&source)
    }
}

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message)
}

/// One log record, whatever its source.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub at: DateTime<Utc>,
    pub source: Source,
    pub level: Level,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Log target, audit event type, admin action or activity kind.
    pub kind: String,
    pub message: String,
    pub fields: Map<String, Value>,
}

impl Entry {
    fn mentions(&self, id: &str) -> bool {
        self.message.contains(id) || Value::Object(self.fields.clone()).to_string().contains(id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub id: Uuid,
    pub agent: String,
    pub status: String,
    pub node: Option<String>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub actions: u64,
    pub model_calls: u64,
    pub task: String,
}

impl From<&SessionRecord> for SessionSummary {
    fn from(r: &SessionRecord) -> Self {
        Self {
            id: r.info.id,
            agent: r.info.agent.clone(),
            status: status_str(r.info.status),
            node: r.node.clone(),
            created_by: r.info.created_by.to_string(),
            created_at: r.info.created_at,
            ended_at: r.info.ended_at,
            actions: r.info.actions,
            model_calls: r.info.model_calls,
            task: truncate(&redact(&r.info.task), 300),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Meta {
    pub generated_at: DateTime<Utc>,
    pub generated_by: String,
    /// The node that put the export together.
    pub generated_on: String,
    pub version: &'static str,
    pub filter: Filter,
    /// Records in the export.
    pub records: usize,
    /// Records left out because of `limit` (the oldest).
    pub truncated: usize,
    pub by_source: BTreeMap<Source, usize>,
    pub by_level: BTreeMap<Level, usize>,
    /// What could not be collected, and other things the reader should know.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Bundle {
    pub meta: Meta,
    pub nodes: Vec<NodeInfo>,
    pub sessions: Vec<SessionSummary>,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
pub struct NodeQuery {
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
}

/// This node's buffered operational log (asked for by the node putting an
/// export together).
pub async fn node_records(
    caller: Caller,
    Query(q): Query<NodeQuery>,
) -> ApiResult<Json<Vec<LogRecord>>> {
    caller.require_admin()?;
    Ok(Json(recent(q.since, q.until)))
}

/// Everything in one file. Admins only.
pub async fn export(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Query(q): Query<ExportQuery>,
) -> ApiResult<Response> {
    caller.require_admin()?;
    let filter = Filter::from_query(&q)?;
    let bundle = collect(&state, &caller, headers.get(header::AUTHORIZATION), filter).await?;
    let format = q.format;
    let body = match format {
        Format::Md => render_markdown(&bundle, q.max_chars.unwrap_or(DEFAULT_MAX_CHARS)),
        Format::Jsonl => render_jsonl(&bundle),
        Format::Json => serde_json::to_string_pretty(&bundle).unwrap_or_default(),
        Format::Csv => render_csv(&bundle),
        Format::Txt => render_text(&bundle),
    };
    state
        .store
        .record_log_export(
            &caller.name,
            json!({
                "format": format,
                "since": bundle.meta.filter.since,
                "until": bundle.meta.filter.until,
                "sources": bundle.meta.filter.sources,
                "level": bundle.meta.filter.level,
                "session": bundle.meta.filter.session,
                "records": bundle.meta.records,
            }),
        )
        .await?;
    let name = format!(
        "agentcore-logs-{}.{}",
        bundle.meta.generated_at.format("%Y%m%d-%H%M%S"),
        format.extension()
    );
    Ok((
        [
            (header::CONTENT_TYPE, format.content_type().to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    )
        .into_response())
}

/// Collect, filter, redact.
pub async fn collect(
    state: &AppState,
    caller: &Caller,
    authorization: Option<&header::HeaderValue>,
    filter: Filter,
) -> ApiResult<Bundle> {
    let mut notes = Vec::new();
    let mut entries = Vec::new();
    let nodes = state
        .store
        .nodes(state.config.cluster.node_timeout_secs)
        .await?;
    let records = state
        .store
        .sessions_between(filter.since, filter.until, filter.session, MAX_SESSIONS)
        .await?;
    if records.len() as i64 == MAX_SESSIONS {
        notes.push(format!(
            "More than {MAX_SESSIONS} sessions ran in this window; only the newest \
             {MAX_SESSIONS} are included. Narrow the window to see the others."
        ));
    }
    if filter.wants(Source::Server) {
        entries.extend(server_entries(state, authorization, &nodes, &filter, &mut notes).await?);
    }
    if filter.wants(Source::Audit) {
        entries.extend(audit_entries(&records, &filter, &mut notes).await);
    }
    if filter.wants(Source::Admin) {
        for e in state
            .store
            .admin_events_between(filter.since, filter.until, filter.limit as i64)
            .await?
        {
            entries.push(Entry {
                at: e.at,
                source: Source::Admin,
                level: Level::Info,
                node: None,
                session: None,
                agent: None,
                kind: e.action.clone(),
                message: format!("{} {} {}", e.actor, e.action, e.target),
                fields: object(e.details.0),
            });
        }
    }
    if filter.wants(Source::Activity) {
        for a in state
            .store
            .activity_between(
                filter.since,
                filter.until,
                filter.session,
                filter.limit as i64,
            )
            .await?
        {
            let mut fields = Map::new();
            for (key, id) in [("department", a.department_id), ("agent_id", a.agent_id)] {
                if let Some(id) = id {
                    fields.insert(key.into(), id.to_string().into());
                }
            }
            if let Some(value) = a.value {
                fields.insert("value".into(), value.into());
            }
            let kind = a.kind.to_lowercase();
            entries.push(Entry {
                at: a.at,
                source: Source::Activity,
                level: if kind.contains("fail") || kind.contains("error") {
                    Level::Warn
                } else {
                    Level::Info
                },
                node: None,
                session: a.session_id,
                agent: None,
                kind: a.kind.clone(),
                message: a.detail.clone().unwrap_or_else(|| a.kind.clone()),
                fields,
            });
        }
    }

    let session = filter.session.map(|id| id.to_string());
    entries.retain(|e| {
        e.level >= filter.level
            && e.at >= filter.since
            && e.at <= filter.until
            && match (&session, e.session) {
                (None, _) => true,
                (Some(_), Some(s)) => Some(s) == filter.session,
                (Some(id), None) => e.mentions(id),
            }
    });
    entries.sort_by_key(|e| e.at);
    let truncated = entries.len().saturating_sub(filter.limit);
    if truncated > 0 {
        entries.drain(..truncated);
        notes.push(format!(
            "{truncated} older records were left out (limit {}).",
            filter.limit
        ));
    }
    for entry in &mut entries {
        entry.message = redact(&entry.message);
        for (key, value) in entry.fields.iter_mut() {
            redact_value(value, Some(key));
        }
    }
    let mut by_source = BTreeMap::new();
    let mut by_level = BTreeMap::new();
    for e in &entries {
        *by_source.entry(e.source).or_default() += 1;
        *by_level.entry(e.level).or_default() += 1;
    }
    Ok(Bundle {
        meta: Meta {
            generated_at: Utc::now(),
            generated_by: caller.name.clone(),
            generated_on: state.node.to_string(),
            version: env!("CARGO_PKG_VERSION"),
            filter,
            records: entries.len(),
            truncated,
            by_source,
            by_level,
            notes,
        },
        nodes,
        sessions: records.iter().map(SessionSummary::from).collect(),
        entries,
    })
}

fn server_entry(node: &str, r: LogRecord) -> Entry {
    let session = ["session", "session_id"]
        .iter()
        .find_map(|k| r.fields.get(*k)?.as_str()?.parse().ok());
    Entry {
        at: r.at,
        source: Source::Server,
        level: r.level,
        node: Some(node.to_string()),
        session,
        agent: r
            .fields
            .get("agent")
            .and_then(Value::as_str)
            .map(String::from),
        kind: r.target,
        message: r.message,
        fields: r.fields,
    }
}

/// This node's buffer, the other live nodes' buffers, and the warnings and
/// errors saved by every node (including the ones that are gone).
async fn server_entries(
    state: &AppState,
    authorization: Option<&header::HeaderValue>,
    nodes: &[NodeInfo],
    filter: &Filter,
    notes: &mut Vec<String>,
) -> ApiResult<Vec<Entry>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    // The runtime also logs every audit event (without a message); with the
    // audit logs in the export, those only repeat them.
    let repeat = |target: &str| filter.wants(Source::Audit) && target == AUDIT_ECHO;
    for r in recent(filter.since, filter.until) {
        seen.insert(r.id);
        if !repeat(&r.target) {
            out.push(server_entry(&state.node, r));
        }
    }
    let others = nodes
        .iter()
        .filter(|n| n.name != *state.node && n.alive)
        .map(|n| async move {
            let mut request = state
                .cluster_http
                .get(format!(
                    "{}/api/v1/logs/node",
                    n.internal_url.trim_end_matches('/')
                ))
                .query(&[
                    ("since", rfc3339(filter.since)),
                    ("until", rfc3339(filter.until)),
                ])
                .header(&FORWARDED, &*state.node)
                .timeout(Duration::from_secs(15));
            if let Some(auth) = authorization {
                request = request.header(header::AUTHORIZATION, auth);
            }
            let result = async {
                let response = request.send().await?.error_for_status()?;
                response.json::<Vec<LogRecord>>().await
            }
            .await;
            (n.name.clone(), result)
        });
    for (node, result) in futures::future::join_all(others).await {
        match result {
            Ok(records) => {
                for r in records {
                    if seen.insert(r.id) && !repeat(&r.target) {
                        out.push(server_entry(&node, r));
                    }
                }
            }
            Err(err) => notes.push(format!(
                "Node {node} did not send its log ({err}); only the warnings and errors it \
                 saved to the database are included for it."
            )),
        }
    }
    for log in state
        .store
        .node_logs(filter.since, filter.until, filter.limit as i64)
        .await?
    {
        if seen.insert(log.id) {
            out.push(Entry {
                at: log.at,
                source: Source::Server,
                level: Level::parse(&log.level),
                node: Some(log.node.clone()),
                session: ["session", "session_id"]
                    .iter()
                    .find_map(|k| log.fields.0.get(*k)?.as_str()?.parse().ok()),
                agent: None,
                kind: log.target,
                message: log.message,
                fields: object(log.fields.0),
            });
        }
    }
    let live: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
    let gone: HashSet<&str> = out
        .iter()
        .filter_map(|e| e.node.as_deref())
        .filter(|n| !live.contains(n))
        .collect();
    if !gone.is_empty() {
        let mut gone: Vec<_> = gone.into_iter().collect();
        gone.sort_unstable();
        notes.push(format!(
            "Nodes that are gone: {}. Only their warnings and errors are included.",
            gone.join(", ")
        ));
    }
    Ok(out)
}

async fn audit_entries(
    records: &[SessionRecord],
    filter: &Filter,
    notes: &mut Vec<String>,
) -> Vec<Entry> {
    let reads = records.iter().map(|r| {
        let path = r.audit_path.clone();
        async move {
            let read = tokio::task::spawn_blocking(move || agentcore_audit::read_events(&path))
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
            (r, read)
        }
    });
    let mut out = Vec::new();
    for (record, read) in futures::future::join_all(reads).await {
        let session = record.info.id;
        match read {
            Ok(events) => {
                for verified in events {
                    let event = verified.event;
                    if event.timestamp < filter.since || event.timestamp > filter.until {
                        continue;
                    }
                    let (level, message) = describe(&event.kind);
                    let mut fields = object(serde_json::to_value(&event.kind).unwrap_or_default());
                    fields.remove("event");
                    if matches!(event.kind, EventKind::Output { .. }) {
                        fields.remove("line");
                    }
                    fields.insert("seq".into(), event.seq.into());
                    out.push(Entry {
                        at: event.timestamp,
                        source: Source::Audit,
                        level,
                        node: match &event.kind {
                            EventKind::SessionRecovered { node, .. } => Some(node.clone()),
                            _ => None,
                        },
                        session: Some(session),
                        agent: Some(record.info.agent.clone()),
                        kind: event.kind.name().into(),
                        message,
                        fields,
                    });
                }
            }
            Err(err) => {
                notes.push(format!(
                    "The audit log of session {session} could not be read: {err}"
                ));
                out.push(Entry {
                    at: record.info.ended_at.unwrap_or(record.info.created_at),
                    source: Source::Audit,
                    level: Level::Error,
                    node: record.node.clone(),
                    session: Some(session),
                    agent: Some(record.info.agent.clone()),
                    kind: "audit_unreadable".into(),
                    message: format!("audit log could not be read or verified: {err}"),
                    fields: Map::new(),
                });
            }
        }
    }
    out
}

/// Level and one-line description of an audit event.
fn describe(kind: &EventKind) -> (Level, String) {
    use Level::{Debug, Error, Info, Warn};
    match kind {
        EventKind::SessionCreated {
            agent,
            policy,
            created_by,
            ..
        } => (
            Info,
            format!("session created: agent {agent}, policy {policy}, by {created_by}"),
        ),
        EventKind::SandboxStarted { backend, .. } => (Info, format!("sandbox started ({backend})")),
        EventKind::WorkspacePrepared {
            repository,
            branch,
            base_commit,
        } => (
            Info,
            format!("workspace prepared: {repository} branch {branch} at {base_commit}"),
        ),
        EventKind::RoleApplied { role, .. } => (Info, format!("role {role} applied")),
        EventKind::AgentStarted { program, args, .. } => {
            (Info, format!("agent started: {program} {}", args.join(" ")))
        }
        EventKind::TurnStarted { turn } => (Info, format!("turn {turn} started")),
        EventKind::TurnEnded { turn, exit_code } => (
            if exit_code.is_some_and(|c| c != 0) {
                Warn
            } else {
                Info
            },
            format!("turn {turn} ended (exit code {})", code(*exit_code)),
        ),
        EventKind::UserMessage { by, text } => (Info, format!("message from {by}: {text}")),
        EventKind::MessagesDelivered { messages } => (
            Info,
            format!("{} message(s) delivered to the agent", messages.len()),
        ),
        EventKind::ChecksCompleted {
            passed, results, ..
        } => {
            if *passed {
                (Info, format!("checks passed ({})", results.len()))
            } else {
                (Warn, format!("checks failed ({} run)", results.len()))
            }
        }
        EventKind::PullRequestProposed { proposal } => {
            (Info, format!("pull request proposed: {}", proposal.title))
        }
        EventKind::Delivered {
            branch,
            commit,
            pull_request_url,
            ..
        } => (
            Info,
            format!(
                "delivered branch {branch} at {commit}{}",
                pull_request_url
                    .as_deref()
                    .map(|u| format!(", {u}"))
                    .unwrap_or_default()
            ),
        ),
        EventKind::Output { stream, line } => (
            match stream {
                OutputStream::Stdout => Debug,
                OutputStream::Stderr => Info,
            },
            line.clone(),
        ),
        EventKind::ActionRequested { action, .. } => {
            (Info, format!("action requested: {}", action.summary()))
        }
        EventKind::PolicyEvaluated { verdict, .. } => match verdict {
            Verdict::Allow { rule } => (
                Info,
                format!("policy allowed{}", rule_suffix(rule.as_deref())),
            ),
            Verdict::Deny { rule, reason } => (
                Warn,
                format!("policy denied{}: {reason}", rule_suffix(rule.as_deref())),
            ),
            Verdict::RequireApproval { rule, reason } => (
                Info,
                format!(
                    "policy requires approval{}: {reason}",
                    rule_suffix(rule.as_deref())
                ),
            ),
        },
        EventKind::ApprovalRequested { action, reason, .. } => (
            Info,
            format!("approval requested for {}: {reason}", action.summary()),
        ),
        EventKind::ApprovalResolved {
            approved,
            by,
            comment,
            ..
        } => (
            if *approved { Info } else { Warn },
            format!(
                "{} by {by}{}",
                if *approved { "approved" } else { "rejected" },
                comment
                    .as_deref()
                    .map(|c| format!(": {c}"))
                    .unwrap_or_default()
            ),
        ),
        EventKind::ActionCompleted { outcome, .. } => match outcome {
            ActionOutcome::Succeeded { .. } => (Info, "action succeeded".into()),
            ActionOutcome::Failed { error } => (Warn, format!("action failed: {error}")),
            ActionOutcome::Denied { reason } => (Warn, format!("action denied: {reason}")),
        },
        EventKind::ModelCall {
            provider,
            model,
            http_status,
            outcome,
            input_tokens,
            output_tokens,
            duration_ms,
            detail,
            ..
        } => {
            let outcome = serde_json::to_value(outcome)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default();
            let level = match outcome.as_str() {
                "upstream_error" => Error,
                "completed" if http_status.is_none_or(|s| s < 400) => Info,
                _ => Warn,
            };
            (
                level,
                format!(
                    "model call {provider}/{}: {outcome}, HTTP {}, {duration_ms} ms, \
                     {} in / {} out tokens{}",
                    model.as_deref().unwrap_or("?"),
                    http_status.map_or("-".into(), |s| s.to_string()),
                    input_tokens.map_or("?".into(), |t| t.to_string()),
                    output_tokens.map_or("?".into(), |t| t.to_string()),
                    detail
                        .as_deref()
                        .map(|d| format!(": {d}"))
                        .unwrap_or_default()
                ),
            )
        }
        EventKind::StatusChanged { status } => (Info, format!("status: {}", status_str(*status))),
        EventKind::StopRequested { by, reason } => {
            (Warn, format!("stop requested by {by}: {reason}"))
        }
        EventKind::Paused { by } => (Info, format!("paused by {by}")),
        EventKind::TerminalInput { by, turn } => {
            (Info, format!("{by} typed into the terminal (turn {turn})"))
        }
        EventKind::Resumed { by } => (Info, format!("resumed by {by}")),
        EventKind::SessionRecovered { node, reason } => {
            (Warn, format!("carried on by node {node}: {reason}"))
        }
        EventKind::RecordingClosed { bytes, .. } => {
            (Info, format!("terminal recording closed ({bytes} bytes)"))
        }
        EventKind::SessionEnded {
            status,
            exit_code,
            reason,
        } => (
            match status {
                SessionStatus::Failed => Error,
                SessionStatus::Stopped => Warn,
                _ => Info,
            },
            format!(
                "session ended: {} (exit code {}){}",
                status_str(*status),
                code(*exit_code),
                reason
                    .as_deref()
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default()
            ),
        ),
    }
}

fn code(exit_code: Option<i32>) -> String {
    exit_code.map_or("-".into(), |c| c.to_string())
}

fn rule_suffix(rule: Option<&str>) -> String {
    rule.map(|r| format!(" (rule {r})")).unwrap_or_default()
}

fn status_str(status: SessionStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        Value::Null => Map::new(),
        other => Map::from_iter([("value".to_string(), other)]),
    }
}

fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

// ---- redaction --------------------------------------------------------------------

/// Prefixes of well-known secret formats (API keys, tokens).
const SECRET_PREFIXES: &[&str] = &[
    "sk-ant-",
    "sk-proj-",
    "sk-",
    "sk_live_",
    "rk_live_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xapp-",
    "AKIA",
    "AIza",
];

/// Names whose values are secrets (`api_key=…`, `"authorization": "…"`).
fn sensitive(name: &str) -> bool {
    let name = name.to_ascii_lowercase().replace('-', "_");
    const EXACT: &[&str] = &[
        "token",
        "key",
        "apikey",
        "secret",
        "password",
        "passwd",
        "authorization",
        "cookie",
        "set_cookie",
        "credentials",
    ];
    const ENDINGS: &[&str] = &[
        "_token",
        "_key",
        "_apikey",
        "_secret",
        "_password",
        "_credentials",
    ];
    EXACT.contains(&name.as_str()) || ENDINGS.iter().any(|e| name.ends_with(e))
}

fn token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn run(bytes: &[u8], from: usize, pred: impl Fn(u8) -> bool) -> usize {
    bytes[from..].iter().take_while(|b| pred(**b)).count()
}

/// Replace secrets in free text: known key formats, bearer tokens,
/// `name=value` pairs with a sensitive name, and passwords in URLs.
pub fn redact(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'outer: while i < bytes.len() {
        let boundary = i == 0 || !ident_char(bytes[i - 1]);
        if boundary {
            for prefix in SECRET_PREFIXES {
                if bytes[i..].starts_with(prefix.as_bytes()) {
                    let n = run(bytes, i + prefix.len(), token_char);
                    if n >= 12 {
                        out.push_str(prefix);
                        out.push_str(REDACTED);
                        i += prefix.len() + n;
                        continue 'outer;
                    }
                }
            }
            if bytes.len() >= i + 7 && bytes[i..i + 7].eq_ignore_ascii_case(b"bearer ") {
                let n = run(bytes, i + 7, |b| {
                    token_char(b) || matches!(b, b'.' | b'/' | b'+' | b'=' | b'~')
                });
                if n >= 8 {
                    out.push_str(&text[i..i + 7]);
                    out.push_str(REDACTED);
                    i += 7 + n;
                    continue 'outer;
                }
            }
            // name=value, name: value, "name": "value"
            let n = run(bytes, i, ident_char);
            if n > 0 && sensitive(&text[i..i + n]) {
                let mut j = i + n;
                if bytes.get(j) == Some(&b'"') {
                    j += 1;
                }
                if matches!(bytes.get(j), Some(b'=' | b':')) {
                    j += 1;
                    j += run(bytes, j, |b| b == b' ');
                    if bytes.get(j) == Some(&b'"') {
                        j += 1;
                    }
                    let end = |b: u8| {
                        !b.is_ascii_whitespace()
                            && !matches!(b, b'"' | b'\'' | b',' | b';' | b'&' | b')' | b']' | b'}')
                    };
                    let mut value = run(bytes, j, end);
                    // `Authorization: Bearer <token>`: the scheme and the token.
                    let scheme = &text[j..j + value];
                    if (scheme.eq_ignore_ascii_case("bearer")
                        || scheme.eq_ignore_ascii_case("basic"))
                        && bytes.get(j + value) == Some(&b' ')
                    {
                        value += 1 + run(bytes, j + value + 1, end);
                    }
                    if value > 0 && !text[j..].starts_with(REDACTED) {
                        out.push_str(&text[i..j]);
                        out.push_str(REDACTED);
                        i = j + value;
                        continue 'outer;
                    }
                }
            }
        }
        // scheme://user:password@host
        if bytes[i..].starts_with(b"://") {
            let start = i + 3;
            let n = run(bytes, start, |b| {
                !b.is_ascii_whitespace() && !matches!(b, b'/' | b'@' | b'"' | b'\'')
            });
            if bytes.get(start + n) == Some(&b'@')
                && let Some(colon) = text[start..start + n].find(':')
            {
                out.push_str(&text[i..start + colon + 1]);
                out.push_str(REDACTED);
                i = start + n;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap_or_default();
        out.push(ch);
        i += ch.len_utf8().max(1);
    }
    out
}

/// Redact a JSON value: strings under sensitive keys entirely, other strings
/// with [`redact`].
pub fn redact_value(value: &mut Value, key: Option<&str>) {
    match value {
        Value::String(s) => {
            if key.is_some_and(sensitive) && !s.is_empty() {
                *s = REDACTED.into();
            } else {
                *s = redact(s);
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(item, key);
            }
        }
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                redact_value(v, Some(k));
            }
        }
        _ => {}
    }
}

// ---- formats ----------------------------------------------------------------------

/// Last 8 characters of a session id: ids are UUIDv7, whose first ones are
/// the time and the same for sessions started together.
fn short(id: Uuid) -> String {
    id.simple().to_string()[24..].to_string()
}

/// Fields as `key=value`, leaving out what the line already shows.
fn fields_inline(e: &Entry) -> String {
    let session = e.session.map(|s| s.to_string());
    let mut out = String::new();
    for (key, value) in &e.fields {
        let shown = match key.as_str() {
            "session" | "session_id" | "id" => value.as_str() == session.as_deref(),
            "agent" => value.as_str() == e.agent.as_deref(),
            _ => false,
        };
        if shown {
            continue;
        }
        let value = match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let value = truncate(&value.replace('\n', "\\n"), MAX_FIELD_CHARS);
        let _ = write!(out, " {key}={value}");
    }
    out
}

/// `2026-11-08T10:00:01.123Z ERROR audit   node-a s:0194d2c1 session_ended: …`
fn line(e: &Entry) -> String {
    let mut out = format!(
        "{} {:<5} {:<8}",
        rfc3339(e.at),
        e.level.as_str().to_uppercase(),
        e.source.as_str()
    );
    if let Some(node) = &e.node {
        let _ = write!(out, " node={node}");
    }
    if let Some(session) = e.session {
        let _ = write!(out, " s={}", short(session));
    }
    if let Some(agent) = &e.agent {
        let _ = write!(out, " agent={agent}");
    }
    let message = e.message.replace('\n', "\\n");
    let _ = write!(out, " {}: {message}", e.kind);
    let fields = fields_inline(e);
    if !fields.is_empty() {
        let _ = write!(out, " |{fields}");
    }
    out
}

pub fn render_jsonl(bundle: &Bundle) -> String {
    let mut out = String::new();
    let header = json!({
        "type": "meta",
        "meta": bundle.meta,
        "nodes": bundle.nodes,
        "sessions": bundle.sessions,
    });
    out.push_str(&header.to_string());
    out.push('\n');
    for e in &bundle.entries {
        let mut value = serde_json::to_value(e).unwrap_or_default();
        if let Value::Object(map) = &mut value {
            map.insert("type".into(), "log".into());
        }
        out.push_str(&value.to_string());
        out.push('\n');
    }
    out
}

fn csv_cell(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

pub fn render_csv(bundle: &Bundle) -> String {
    let mut out = String::from("time,source,level,node,session,agent,kind,message,fields\n");
    for e in &bundle.entries {
        let row = [
            rfc3339(e.at),
            e.source.as_str().into(),
            e.level.as_str().into(),
            e.node.clone().unwrap_or_default(),
            e.session.map(|s| s.to_string()).unwrap_or_default(),
            e.agent.clone().unwrap_or_default(),
            e.kind.clone(),
            e.message.clone(),
            if e.fields.is_empty() {
                String::new()
            } else {
                Value::Object(e.fields.clone()).to_string()
            },
        ];
        let cells: Vec<String> = row.iter().map(|c| csv_cell(c)).collect();
        out.push_str(&cells.join(","));
        out.push('\n');
    }
    out
}

pub fn render_text(bundle: &Bundle) -> String {
    let m = &bundle.meta;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# agentcore {} logs, {} to {} (UTC), exported by {} on {} at {}",
        m.version,
        rfc3339(m.filter.since),
        rfc3339(m.filter.until),
        m.generated_by,
        m.generated_on,
        rfc3339(m.generated_at)
    );
    let _ = writeln!(
        out,
        "# {} records; sources: {}; level >= {}",
        m.records,
        sources_list(&m.filter.sources),
        m.filter.level.as_str()
    );
    for note in &m.notes {
        let _ = writeln!(out, "# note: {note}");
    }
    for s in &bundle.sessions {
        let _ = writeln!(
            out,
            "# session s={} {} agent={} status={} node={} started={}",
            short(s.id),
            s.id,
            s.agent,
            s.status,
            s.node.as_deref().unwrap_or("-"),
            rfc3339(s.created_at)
        );
    }
    for e in &bundle.entries {
        out.push_str(&line(e));
        out.push('\n');
    }
    out
}

fn sources_list(sources: &[Source]) -> String {
    sources
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn md_cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// A problem pattern: the message with numbers and ids taken out.
fn pattern(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut last_hash = false;
    for word in message.split_inclusive(|c: char| c.is_whitespace() || c == '/' || c == '=') {
        let (body, sep) = match word.char_indices().last() {
            Some((i, c)) if c.is_whitespace() || c == '/' || c == '=' => word.split_at(i),
            _ => (word, ""),
        };
        let digits = body.chars().filter(char::is_ascii_digit).count();
        if digits > 0 && digits * 3 >= body.len() {
            if !last_hash {
                out.push('#');
            }
            last_hash = true;
        } else {
            out.push_str(body);
            last_hash = false;
        }
        out.push_str(sep);
        if !sep.is_empty() {
            last_hash = false;
        }
    }
    truncate(&out, 200)
}

/// Level, source, kind and message pattern.
type ProblemKey = (Level, Source, String, String);
/// Count, first and last time, an example message.
type Problem = (usize, DateTime<Utc>, DateTime<Utc>, String);

pub fn render_markdown(bundle: &Bundle, max_chars: usize) -> String {
    let m = &bundle.meta;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# agentcore logs: {} to {} (UTC)\n",
        rfc3339(m.filter.since),
        rfc3339(m.filter.until)
    );
    out.push_str(
        "This is a log export from **agentcore**, written to be read by a person or a model \
         analysing what happened. It is self-contained: the sections below explain what \
         agentcore is, where each record comes from, and how to read the timeline.\n\n",
    );
    out.push_str("## About agentcore and this export\n\n");
    out.push_str(
        "agentcore is a Rust server that runs AI coding and business agents as supervised \
         coworkers. Each agent runs in a **session**: a sandbox (a container, or a local \
         process in development) with a workspace. Every side effect the agent wants (run a \
         command, read or write a file, reach the network, call a tool) goes through a \
         **policy** that allows, denies or asks a human to approve it. Agents reach language \
         models only through agentcore's **model gateway**. Agents are organised into \
         **departments**; several server **nodes** can share one PostgreSQL database and \
         data volume, and when a node goes away its sessions are carried on by another.\n\n",
    );
    out.push_str("Sources:\n\n");
    out.push_str(
        "- `server`: the operational log of the agentcore server processes (one per node). \
         `kind` is the Rust module that logged it. Full detail for live nodes; for nodes that \
         are gone, only warnings and errors.\n\
         - `audit`: the tamper-evident audit log of each session: the agent's actions, policy \
         verdicts, approvals, model calls, status changes and the agent's own terminal output \
         (`output`; stdout is level debug, stderr info). `kind` is the event type.\n\
         - `admin`: configuration changes made by people (`kind` is the action).\n\
         - `activity`: what department agents did, as counted for the dashboards.\n\n",
    );
    out.push_str(
        "Timeline lines read: `time level source [node=…] [s=<session>] [agent=…] kind: \
         message | key=value …`. Times are UTC. `s=` is the session: the last 8 \
         characters of its id (see the Sessions table). Secrets are replaced with \
         `[REDACTED]`; long field values are shortened with `…`.\n\n",
    );

    out.push_str("## Export\n\n| | |\n|---|---|\n");
    let _ = writeln!(out, "| agentcore version | {} |", m.version);
    let _ = writeln!(
        out,
        "| Exported | {} by {} on node {} |",
        rfc3339(m.generated_at),
        md_cell(&m.generated_by),
        md_cell(&m.generated_on)
    );
    let _ = writeln!(
        out,
        "| Window | {} to {} |",
        rfc3339(m.filter.since),
        rfc3339(m.filter.until)
    );
    let _ = writeln!(out, "| Sources | {} |", sources_list(&m.filter.sources));
    let _ = writeln!(out, "| Lowest level | {} |", m.filter.level.as_str());
    if let Some(session) = m.filter.session {
        let _ = writeln!(out, "| Only session | {session} |");
    }
    let by_level: Vec<String> = m
        .by_level
        .iter()
        .rev()
        .map(|(l, n)| format!("{n} {}", l.as_str()))
        .collect();
    let by_source: Vec<String> = m
        .by_source
        .iter()
        .map(|(s, n)| format!("{n} {}", s.as_str()))
        .collect();
    let _ = writeln!(
        out,
        "| Records | {} ({}; {}) |",
        m.records,
        by_level.join(", "),
        by_source.join(", ")
    );
    out.push('\n');
    if !m.notes.is_empty() {
        out.push_str("Notes:\n\n");
        for note in &m.notes {
            let _ = writeln!(out, "- {}", note.replace('\n', " "));
        }
        out.push('\n');
    }

    out.push_str("## Nodes\n\n");
    if bundle.nodes.is_empty() {
        out.push_str("No node is registered right now.\n\n");
    } else {
        out.push_str("| Node | Version | Started | Last seen | Alive | Capacity |\n|---|---|---|---|---|---|\n");
        for n in &bundle.nodes {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} |",
                md_cell(&n.name),
                md_cell(&n.version),
                rfc3339(n.started_at),
                rfc3339(n.last_seen),
                if n.alive { "yes" } else { "no" },
                n.capacity
            );
        }
        out.push('\n');
    }

    out.push_str("## Sessions\n\n");
    if bundle.sessions.is_empty() {
        out.push_str("No session ran in this window.\n\n");
    } else {
        out.push_str(
            "| s= | Session id | Agent | Status | Node | Started | Ended | Actions | Model calls | Task |\n\
             |---|---|---|---|---|---|---|---|---|---|\n",
        );
        for s in &bundle.sessions {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                short(s.id),
                s.id,
                md_cell(&s.agent),
                s.status,
                md_cell(s.node.as_deref().unwrap_or("-")),
                rfc3339(s.created_at),
                s.ended_at.map_or("-".into(), rfc3339),
                s.actions,
                s.model_calls,
                md_cell(&truncate(&s.task, 120))
            );
        }
        out.push('\n');
    }

    out.push_str("## Problems\n\nErrors and warnings, grouped by message pattern (numbers and ids replaced by `#`), most frequent first.\n\n");
    let mut groups: HashMap<ProblemKey, Problem> = HashMap::new();
    for e in bundle.entries.iter().filter(|e| e.level >= Level::Warn) {
        let key = (e.level, e.source, e.kind.clone(), pattern(&e.message));
        let group = groups
            .entry(key)
            .or_insert_with(|| (0, e.at, e.at, e.message.clone()));
        group.0 += 1;
        group.2 = e.at;
    }
    if groups.is_empty() {
        out.push_str("None.\n\n");
    } else {
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_by(|a, b| {
            b.0.0
                .cmp(&a.0.0)
                .then(b.1.0.cmp(&a.1.0))
                .then(a.1.1.cmp(&b.1.1))
        });
        out.push_str("| Count | Level | Source | Kind | Example message | First | Last |\n|---|---|---|---|---|---|---|\n");
        let total = groups.len();
        for ((level, source, kind, _), (count, first, last, example)) in groups.iter().take(60) {
            let _ = writeln!(
                out,
                "| {count} | {} | {} | {} | {} | {} | {} |",
                level.as_str(),
                source.as_str(),
                md_cell(kind),
                md_cell(&truncate(example, 300)),
                rfc3339(*first),
                rfc3339(*last)
            );
        }
        if total > 60 {
            let _ = writeln!(out, "\n{} more patterns are in the timeline.", total - 60);
        }
        out.push('\n');
    }

    // The timeline gets what is left of the budget: every error and warning
    // first, then the newest of the rest.
    let lines: Vec<String> = bundle.entries.iter().map(line).collect();
    let budget = max_chars.saturating_sub(out.len() + 600);
    let mut order: Vec<usize> = (0..lines.len()).collect();
    order.sort_by(|&a, &b| {
        let (ea, eb) = (&bundle.entries[a], &bundle.entries[b]);
        let rank = |e: &Entry| (e.level >= Level::Warn, e.level);
        rank(eb).cmp(&rank(ea)).then(eb.at.cmp(&ea.at))
    });
    let mut used = 0;
    let mut keep = vec![false; lines.len()];
    for i in order {
        let cost = lines[i].len() + 1;
        if used + cost > budget {
            continue;
        }
        used += cost;
        keep[i] = true;
    }
    let kept = keep.iter().filter(|k| **k).count();
    out.push_str("## Timeline\n\n");
    if kept < lines.len() {
        let _ = writeln!(
            out,
            "{kept} of {} records are shown, to stay within {max_chars} characters: errors and \
             warnings first, then the newest of the rest. `… n records left out …` marks the \
             gaps. Download a JSON Lines or CSV export for all of them.\n",
            lines.len()
        );
    }
    out.push_str("```text\n");
    let mut skipped = 0;
    for (i, text) in lines.iter().enumerate() {
        if keep[i] {
            if skipped > 0 {
                let _ = writeln!(out, "… {skipped} records left out …");
                skipped = 0;
            }
            out.push_str(&text.replace("```", "'''"));
            out.push('\n');
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        let _ = writeln!(out, "… {skipped} records left out …");
    }
    out.push_str("```\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn redacts_secrets_in_text() {
        let cases = [
            (
                "key sk-ant-api03-abcdefghijklmnop used",
                "key sk-ant-[REDACTED] used",
            ),
            (
                "Authorization: Bearer eyJhbGciOi.payload.sig",
                "Authorization: [REDACTED]",
            ),
            (
                "curl -H 'authorization: Bearer abc'",
                "curl -H 'authorization: [REDACTED]'",
            ),
            (
                "sent Bearer abcdefgh12345678 upstream",
                "sent Bearer [REDACTED] upstream",
            ),
            (
                "https://x-access-token:ghs_abcdefghijklmnop1234@github.com/o/r",
                "https://x-access-token:[REDACTED]@github.com/o/r",
            ),
            (
                "https://bob:hunter2@example.com/x",
                "https://bob:[REDACTED]@example.com/x",
            ),
            (
                "GET /x?api_key=abc123&page=2",
                "GET /x?api_key=[REDACTED]&page=2",
            ),
            (
                r#"{"x-api-key":"sk-real-key","model":"m"}"#,
                r#"{"x-api-key":"[REDACTED]","model":"m"}"#,
            ),
            (
                "AGENTCORE_GATEWAY_TOKEN=0123456789abcdef env",
                "AGENTCORE_GATEWAY_TOKEN=[REDACTED] env",
            ),
            ("ghp_shortone", "ghp_shortone"),
        ];
        for (input, expected) in cases {
            assert_eq!(redact(input), expected, "{input}");
        }
        // Words that merely contain a prefix or a sensitive name are kept.
        for keep in [
            "task-1234567890abcdef is done",
            "monkey=banana",
            "input_tokens=25 output_tokens=15 max_tokens=10",
            "tokens: 25",
            "keyboard: on",
            "naïve résumé ✓",
        ] {
            assert_eq!(redact(keep), keep);
        }
    }

    #[test]
    fn redacts_sensitive_fields() {
        let mut value = json!({
            "headers": { "Authorization": "Bearer abcdefgh12345678", "accept": "*/*" },
            "api_key": "plain",
            "input_tokens": 12,
            "note": "uses sk-proj-abcdefghijklmnopqrstu",
        });
        redact_value(&mut value, None);
        assert_eq!(value["headers"]["Authorization"], REDACTED);
        assert_eq!(value["headers"]["accept"], "*/*");
        assert_eq!(value["api_key"], REDACTED);
        assert_eq!(value["input_tokens"], 12);
        assert_eq!(value["note"], "uses sk-proj-[REDACTED]");
    }

    #[test]
    fn patterns_group_messages_that_differ_in_numbers() {
        assert_eq!(
            pattern("heartbeat timed out after 5012 ms"),
            pattern("heartbeat timed out after 77 ms")
        );
        assert_eq!(
            pattern("session 0194d2c1-aaaa-7bbb-8ccc-0123456789ab failed"),
            pattern("session 0194d2c1-ffff-7bbb-8ccc-999999999999 failed")
        );
        assert_ne!(pattern("could not reach node-a"), pattern("disk full"));
    }

    #[test]
    fn capture_keeps_events_with_their_span_fields() {
        let subscriber = tracing_subscriber::registry().with(capture());
        let marker = Uuid::new_v4().to_string();
        let before = Utc::now();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("request", method = "GET", uri = "/api/v1/x");
            let _enter = span.enter();
            tracing::warn!(marker = %marker, attempt = 3, "upstream failed");
        });
        let record = recent(before, Utc::now())
            .into_iter()
            .find(|r| r.fields.get("marker").and_then(Value::as_str) == Some(&marker))
            .expect("captured");
        assert_eq!(record.level, Level::Warn);
        assert_eq!(record.message, "upstream failed");
        assert_eq!(record.fields["attempt"], 3);
        assert_eq!(record.fields["method"], "GET");
        assert_eq!(record.fields["uri"], "/api/v1/x");
    }

    fn entry(level: Level, message: &str, secs: i64) -> Entry {
        Entry {
            at: DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap(),
            source: Source::Server,
            level,
            node: Some("node-a".into()),
            session: None,
            agent: None,
            kind: "agentcore_server::cluster".into(),
            message: message.into(),
            fields: Map::from_iter([("n".to_string(), json!(secs))]),
        }
    }

    fn bundle(entries: Vec<Entry>) -> Bundle {
        let at = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        Bundle {
            meta: Meta {
                generated_at: at,
                generated_by: "root".into(),
                generated_on: "node-a".into(),
                version: "test",
                filter: Filter {
                    since: at,
                    until: at,
                    sources: Source::ALL.to_vec(),
                    level: Level::Debug,
                    session: None,
                    limit: 10,
                },
                records: entries.len(),
                truncated: 0,
                by_source: BTreeMap::new(),
                by_level: BTreeMap::new(),
                notes: vec![],
            },
            nodes: vec![],
            sessions: vec![],
            entries,
        }
    }

    #[test]
    fn markdown_keeps_problems_within_its_budget() {
        let mut entries: Vec<Entry> = (0..2000)
            .map(|i| {
                entry(
                    Level::Info,
                    &format!("routine work item {i} with some padding"),
                    i,
                )
            })
            .collect();
        entries.push(entry(Level::Error, "lost the database", 5));
        entries.sort_by_key(|e| e.at);
        let md = render_markdown(&bundle(entries), 20_000);
        assert!(md.len() < 22_000, "{}", md.len());
        assert!(md.contains("## Problems"));
        assert!(md.contains("| 1 | error | server |"));
        assert!(
            md.contains("ERROR server   node=node-a agentcore_server::cluster: lost the database")
        );
        assert!(md.contains("records left out"));
        // The newest routine records are the ones kept.
        assert!(md.contains("routine work item 1999 "));
        assert!(!md.contains("routine work item 100 "));
    }

    #[test]
    fn csv_and_jsonl_carry_every_record() {
        let entries = vec![
            entry(Level::Info, "plain", 1),
            entry(Level::Warn, "has, a comma and \"quotes\"", 2),
        ];
        let b = bundle(entries);
        let csv = render_csv(&b);
        assert_eq!(csv.lines().count(), 3);
        assert!(csv.contains(r#""has, a comma and ""quotes""""#));
        let jsonl = render_jsonl(&b);
        let lines: Vec<Value> = jsonl
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["type"], "meta");
        assert_eq!(lines[2]["level"], "warn");
        assert_eq!(lines[2]["type"], "log");
    }
}
