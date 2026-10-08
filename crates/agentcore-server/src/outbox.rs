//! Talking to the outside world: channels (email over SMTP, Slack, webhooks)
//! and the outbox.
//!
//! Agents never send anything themselves: they draft (`outbox_draft`), a
//! person reads, edits, sends back or rejects each draft, and the server
//! sends what was approved, with the channel's AI disclosure appended. A
//! channel can be set to send without approval (with a daily limit), for
//! example an internal Slack channel.

use std::sync::LazyLock;
use std::time::Duration;

use agentcore_core::{Channel, ChannelKind, DepartmentId, OrgAgentId, OutboxItem, OutboxStatus};
use agentcore_runtime::ToolHandler;
use agentcore_store::{ChannelInput, ChannelUpdate, NewOutboxItem, OutboxRevision, Store};
use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use lettre::message::{Mailbox, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::AppState;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::org::{self, Org, Recipient, Sender, def, notify, text_arg};

type ApiResult<T> = Result<T, ApiError>;

const DEFAULT_MAX_RECIPIENTS: u64 = 10;
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Client for Slack and webhooks: no redirects (the message and key go
/// only where the admin configured).
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(SEND_TIMEOUT)
        .build()
        .unwrap_or_default()
});

/// The text that goes out: the message and the channel's disclosure.
pub fn outgoing_text(channel: &Channel, body: &str) -> String {
    if channel.disclosure.trim().is_empty() {
        body.trim().to_string()
    } else {
        format!("{}\n\n— {}", body.trim(), channel.disclosure.trim())
    }
}

/// Check recipients against the channel's rules.
pub fn check_recipients(channel: &Channel, recipients: &[String]) -> Result<(), String> {
    if channel.kind != ChannelKind::Email {
        return Ok(());
    }
    if recipients.is_empty() {
        return Err("an email needs at least one recipient (`to`)".into());
    }
    let max = channel.config["max_recipients"]
        .as_u64()
        .unwrap_or(DEFAULT_MAX_RECIPIENTS);
    if recipients.len() as u64 > max {
        return Err(format!("this channel sends to at most {max} recipients"));
    }
    let allowed: Vec<String> = channel.config["allowed_domains"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase())
                .filter(|d| !d.is_empty())
                .collect()
        })
        .unwrap_or_default();
    for r in recipients {
        let address: Mailbox = r
            .trim()
            .parse()
            .map_err(|_| format!("`{r}` is not an email address"))?;
        let domain = address.email.domain().to_ascii_lowercase();
        if !allowed.is_empty() && !allowed.contains(&domain) {
            return Err(format!(
                "this channel only sends to {}",
                allowed
                    .iter()
                    .map(|d| format!("@{d}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    Ok(())
}

/// Send one message through a channel.
pub async fn deliver(
    store: &Store,
    channel: &Channel,
    item: &OutboxItem,
    approved_by: &str,
) -> Result<(), String> {
    check_recipients(channel, &item.recipients)?;
    let secret = store
        .channel_secret(channel.id)
        .await
        .map_err(|e| e.to_string())?;
    let text = outgoing_text(channel, &item.body);
    match channel.kind {
        ChannelKind::Email => send_email(channel, secret.as_deref(), item, &text).await,
        ChannelKind::Slack => {
            let url = secret.ok_or("the channel has no Slack webhook URL")?;
            let text = if item.subject.trim().is_empty() {
                text
            } else {
                format!("*{}*\n{text}", item.subject.trim())
            };
            post(&url, None, &json!({ "text": text })).await
        }
        ChannelKind::Webhook => {
            let url = channel.config["url"].as_str().unwrap_or_default();
            let header = channel.config["header"]
                .as_str()
                .unwrap_or("Authorization")
                .to_string();
            let payload = json!({
                "message_id": item.id,
                "channel": channel.name,
                "recipients": item.recipients,
                "subject": item.subject,
                "text": text,
                "body": item.body,
                "disclosure": channel.disclosure,
                "drafted_by": item.drafted_by,
                "approved_by": approved_by,
                "ai_generated": true,
            });
            post(
                url,
                secret.as_deref().map(|s| (header.as_str(), s)),
                &payload,
            )
            .await
        }
    }
}

async fn post(url: &str, auth: Option<(&str, &str)>, payload: &Value) -> Result<(), String> {
    let mut request = HTTP.post(url).json(payload);
    if let Some((name, value)) = auth {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("could not reach the channel: {e}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!(
            "the channel answered {status}: {}",
            body.chars().take(300).collect::<String>()
        ))
    }
}

async fn send_email(
    channel: &Channel,
    password: Option<&str>,
    item: &OutboxItem,
    text: &str,
) -> Result<(), String> {
    let c = &channel.config;
    let host = c["host"].as_str().unwrap_or_default();
    let tls = c["tls"].as_str().unwrap_or("starttls");
    let port = c["port"].as_u64().map(|p| p as u16).unwrap_or(match tls {
        "tls" => 465,
        "none" => 25,
        _ => 587,
    });
    let from: Mailbox = c["from"]
        .as_str()
        .unwrap_or_default()
        .parse()
        .map_err(|_| "the channel's `from` address is not valid".to_string())?;
    let mut message = Message::builder().from(from).subject(item.subject.trim());
    if let Some(reply_to) = c["reply_to"].as_str()
        && let Ok(reply_to) = reply_to.parse::<Mailbox>()
    {
        message = message.reply_to(reply_to);
    }
    for r in &item.recipients {
        message = message.to(r
            .trim()
            .parse()
            .map_err(|_| format!("`{r}` is not an email address"))?);
    }
    let message = message
        .header(ContentType::TEXT_PLAIN)
        .body(text.to_string())
        .map_err(|e| format!("could not build the email: {e}"))?;
    let builder = match tls {
        "tls" => {
            AsyncSmtpTransport::<Tokio1Executor>::relay(host).map_err(|e| format!("SMTP: {e}"))?
        }
        "none" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host),
        _ => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
            .map_err(|e| format!("SMTP: {e}"))?,
    };
    let mut builder = builder.port(port).timeout(Some(SEND_TIMEOUT));
    if let (Some(user), Some(password)) = (c["username"].as_str(), password) {
        builder = builder.credentials(Credentials::new(user.to_string(), password.to_string()));
    }
    builder
        .build()
        .send(message)
        .await
        .map(|_| ())
        .map_err(|e| format!("SMTP: {e}"))
}

/// Send an approved message: daily limit, claim (exactly once), deliver,
/// record, tell the agent.
pub async fn send(state: &AppState, id: Uuid, revision: u32, by: &str) -> ApiResult<OutboxItem> {
    let store = &state.store;
    let item = store.get_outbox_item(id).await?;
    let channel = store.get_channel(item.channel_id).await?;
    let conflict = |m: String| ApiError::new(StatusCode::CONFLICT, m);
    if !channel.enabled {
        return Err(conflict(format!(
            "channel `{}` is turned off",
            channel.name
        )));
    }
    if store.sent_today(channel.id).await? >= i64::from(channel.max_per_day) {
        return Err(conflict(format!(
            "channel `{}` reached its limit of {} messages today",
            channel.name, channel.max_per_day
        )));
    }
    check_recipients(&channel, &item.recipients)
        .map_err(|m| ApiError::new(StatusCode::BAD_REQUEST, m))?;
    let item = store.claim_outbox_item(id, revision, by).await?;
    let result = deliver(store, &channel, &item, by).await;
    let item = store.finish_outbox_item(id, result.clone(), by).await?;
    let detail = channel.name.clone();
    let _ = store
        .record_activity(agentcore_store::Activity {
            department: item.department_id,
            agent: item.agent_id,
            session: None,
            kind: if result.is_ok() {
                "outbox_sent"
            } else {
                "outbox_failed"
            },
            value: None,
            detail: Some(&detail),
        })
        .await;
    notify(store, json!({ "kind": "outbox" })).await;
    if let Some(agent) = item.agent_id {
        let what = describe(&channel, &item);
        let text = match &result {
            Ok(()) => format!("Your {what} was sent (approved by {by})."),
            Err(e) => format!("Your {what} could not be sent: {e}. People can retry it."),
        };
        tell(state, agent, &text).await;
    }
    Ok(item)
}

fn describe(channel: &Channel, item: &OutboxItem) -> String {
    let what = match channel.kind {
        ChannelKind::Email => "email",
        ChannelKind::Slack => "Slack message",
        ChannelKind::Webhook => "message",
    };
    if item.subject.trim().is_empty() {
        format!("{what} on `{}` (id {})", channel.name, item.id)
    } else {
        format!(
            "{what} \"{}\" on `{}` (id {})",
            item.subject.trim(),
            channel.name,
            item.id
        )
    }
}

async fn tell(state: &AppState, agent: OrgAgentId, text: &str) {
    if let Err(err) = org::send(
        &state.store,
        &Sender::Human("outbox".into()),
        Recipient::Agent(agent),
        text,
    )
    .await
    {
        tracing::debug!(error = %err, "could not tell the drafting agent");
    }
}

// ---- agent tools -------------------------------------------------------------------

/// `outbox_*` tools of a worker whose department may use channels.
pub struct OutboxTools {
    state: AppState,
    agent: OrgAgentId,
    department: DepartmentId,
}

impl OutboxTools {
    pub fn new(state: AppState, agent: OrgAgentId, department: DepartmentId) -> Self {
        Self {
            state,
            agent,
            department,
        }
    }
}

fn recipients_arg(args: &Value) -> Result<Vec<String>, String> {
    match args.get("to") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(s
            .split([',', ';'])
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .collect()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(|s| s.trim().to_string())
                    .ok_or_else(|| "`to` is a list of addresses".to_string())
            })
            .collect(),
        _ => Err("`to` is a list of addresses".into()),
    }
}

fn channel_json(c: &Channel, sent_today: i64) -> Value {
    json!({
        "name": c.name,
        "kind": c.kind,
        "description": c.description,
        "needs_approval": c.requires_approval,
        "allowed_domains": c.config.get("allowed_domains"),
        "max_recipients": (c.kind == ChannelKind::Email)
            .then(|| c.config["max_recipients"].as_u64().unwrap_or(DEFAULT_MAX_RECIPIENTS)),
        "max_per_day": c.max_per_day,
        "sent_today": sent_today,
        "disclosure_appended": c.disclosure,
    })
}

#[async_trait]
impl ToolHandler for OutboxTools {
    fn definitions(&self) -> Vec<Value> {
        vec![
            def(
                "outbox_channels",
                "Channels your department may use to reach people outside the organisation \
                 (email, Slack, webhooks), with their rules.",
                json!({}),
                &[],
            ),
            def(
                "outbox_draft",
                "Draft a message for a channel. A person reads it and approves, edits, sends it \
                 back or rejects it before anything is sent (unless the channel sends without \
                 approval). Write the final text: accurate, no invented facts, prices or \
                 promises. `to`: recipients (email); `subject`: email subject or headline.",
                json!({
                    "channel": { "type": "string" },
                    "to": { "type": "array", "items": { "type": "string" } },
                    "subject": { "type": "string" },
                    "text": { "type": "string" },
                }),
                &["channel", "text"],
            ),
            def(
                "outbox_drafts",
                "Your drafts and what happened to them (pending, changes_requested with \
                 people's feedback, sent, rejected, failed).",
                json!({ "status": { "type": "string" } }),
                &[],
            ),
            def(
                "outbox_revise",
                "Revise one of your drafts, usually after people asked for changes: give the \
                 `draft` id, the fields that change, and a `note` on what changed.",
                json!({
                    "draft": { "type": "string" },
                    "to": { "type": "array", "items": { "type": "string" } },
                    "subject": { "type": "string" },
                    "text": { "type": "string" },
                    "note": { "type": "string" },
                }),
                &["draft", "note"],
            ),
        ]
    }

    async fn call(&self, tool: &str, arguments: &Value) -> Result<Value, String> {
        let store = &self.state.store;
        let e = |e: agentcore_store::StoreError| e.to_string();
        let channels = store.channels_for(self.department).await.map_err(e)?;
        match tool {
            "outbox_channels" => {
                let mut list = Vec::new();
                for c in &channels {
                    list.push(channel_json(c, store.sent_today(c.id).await.map_err(e)?));
                }
                Ok(json!({ "channels": list }))
            }
            "outbox_draft" => {
                let name = text_arg(arguments, "channel")?;
                let channel = channels
                    .iter()
                    .find(|c| c.name == name.trim())
                    .ok_or_else(|| {
                        format!("your department has no channel `{name}` (see outbox_channels)")
                    })?;
                let recipients = recipients_arg(arguments)?;
                check_recipients(channel, &recipients)?;
                let org = Org::load(store).await.map_err(e)?;
                let me = org
                    .agent(self.agent)
                    .ok_or("you are no longer a member of the organisation")?;
                let item = store
                    .create_outbox_item(NewOutboxItem {
                        channel_id: channel.id,
                        department_id: Some(self.department),
                        agent_id: Some(self.agent),
                        drafted_by: org.label(me),
                        recipients,
                        subject: arguments["subject"].as_str().unwrap_or_default().into(),
                        body: text_arg(arguments, "text")?,
                    })
                    .await
                    .map_err(e)?;
                notify(store, json!({ "kind": "outbox" })).await;
                if !channel.requires_approval {
                    let by = format!("auto (channel `{}` sends without approval)", channel.name);
                    return match send(&self.state, item.id, item.revision, &by).await {
                        Ok(sent) => Ok(json!({
                            "draft": sent.id, "status": sent.status, "error": sent.error,
                        })),
                        Err(err) => Ok(json!({
                            "draft": item.id,
                            "status": "pending",
                            "note": format!("not sent automatically ({}); it waits for a person", err.message()),
                        })),
                    };
                }
                Ok(json!({
                    "draft": item.id,
                    "status": item.status,
                    "note": "A person reviews it before it is sent; you will get a message when it is decided.",
                }))
            }
            "outbox_drafts" => {
                let status = match arguments["status"].as_str() {
                    Some(s) if !s.is_empty() => Some(s.parse::<OutboxStatus>()?),
                    _ => None,
                };
                let items = store
                    .list_outbox(status, Some(self.agent), 100)
                    .await
                    .map_err(e)?;
                Ok(json!({ "drafts": items.iter().map(|i| json!({
                    "id": i.id,
                    "channel": channels.iter().find(|c| c.id == i.channel_id).map(|c| &c.name),
                    "to": i.recipients, "subject": i.subject, "status": i.status,
                    "revision": i.revision, "feedback": i.feedback, "error": i.error,
                    "decided_by": i.decided_by, "sent_at": i.sent_at,
                })).collect::<Vec<_>>() }))
            }
            "outbox_revise" => {
                let id: Uuid = text_arg(arguments, "draft")?
                    .trim()
                    .parse()
                    .map_err(|_| "`draft` is a draft id".to_string())?;
                let current = store.get_outbox_item(id).await.map_err(e)?;
                if current.agent_id != Some(self.agent) {
                    return Err("you can only revise your own drafts".into());
                }
                let recipients = match arguments.get("to") {
                    Some(v) if !v.is_null() => Some(recipients_arg(arguments)?),
                    _ => None,
                };
                if let (Some(r), Some(c)) = (
                    &recipients,
                    channels.iter().find(|c| c.id == current.channel_id),
                ) {
                    check_recipients(c, r)?;
                }
                let org = Org::load(store).await.map_err(e)?;
                let by = org
                    .agent(self.agent)
                    .map_or_else(|| "agent".to_string(), |a| org.label(a));
                let item = store
                    .revise_outbox_item(
                        id,
                        OutboxRevision {
                            recipients,
                            subject: arguments["subject"].as_str().map(String::from),
                            body: arguments["text"].as_str().map(String::from),
                            note: text_arg(arguments, "note")?,
                        },
                        &by,
                    )
                    .await
                    .map_err(e)?;
                notify(store, json!({ "kind": "outbox" })).await;
                Ok(json!({ "draft": item.id, "revision": item.revision, "status": item.status }))
            }
            other => Err(format!("unknown tool `{other}`")),
        }
    }
}

// ---- API -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct OutboxQuery {
    #[serde(default)]
    pub status: Option<OutboxStatus>,
}

pub async fn list(
    State(state): State<AppState>,
    _caller: Caller,
    Query(q): Query<OutboxQuery>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(
        state.store.list_outbox(q.status, None, 500).await?
    )))
}

/// A person edits a draft before approving it (a new revision).
pub async fn edit(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(revision): Json<OutboxRevision>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    if let Some(recipients) = &revision.recipients {
        let item = state.store.get_outbox_item(id).await?;
        let channel = state.store.get_channel(item.channel_id).await?;
        check_recipients(&channel, recipients)
            .map_err(|m| ApiError::new(StatusCode::BAD_REQUEST, m))?;
    }
    let item = state
        .store
        .revise_outbox_item(id, revision, &caller.name)
        .await?;
    notify(&state.store, json!({ "kind": "outbox" })).await;
    Ok(Json(json!(item)))
}

#[derive(Debug, Deserialize)]
pub struct SendInput {
    /// The revision the person read.
    pub revision: u32,
}

/// Approve and send.
pub async fn approve(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(input): Json<SendInput>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    Ok(Json(json!(
        send(&state, id, input.revision, &caller.name).await?
    )))
}

#[derive(Debug, Deserialize)]
pub struct FeedbackInput {
    #[serde(default)]
    pub text: String,
}

pub async fn request_changes(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(input): Json<FeedbackInput>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let item = state
        .store
        .request_outbox_changes(id, &input.text, &caller.name)
        .await?;
    notify(&state.store, json!({ "kind": "outbox" })).await;
    if let Some(agent) = item.agent_id {
        let channel = state.store.get_channel(item.channel_id).await?;
        tell(
            &state,
            agent,
            &format!(
                "{} asked for changes to your {}: {}\nRevise it with outbox_revise.",
                caller.name,
                describe(&channel, &item),
                input.text.trim()
            ),
        )
        .await;
    }
    Ok(Json(json!(item)))
}

pub async fn reject(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(input): Json<FeedbackInput>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let item = state
        .store
        .reject_outbox_item(id, &input.text, &caller.name)
        .await?;
    notify(&state.store, json!({ "kind": "outbox" })).await;
    if let Some(agent) = item.agent_id {
        let channel = state.store.get_channel(item.channel_id).await?;
        let why = if input.text.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", input.text.trim())
        };
        tell(
            &state,
            agent,
            &format!(
                "{} rejected your {}{why}. It will not be sent.",
                caller.name,
                describe(&channel, &item)
            ),
        )
        .await;
    }
    Ok(Json(json!(item)))
}

pub async fn list_channels(
    State(state): State<AppState>,
    _caller: Caller,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(state.store.list_channels().await?)))
}

pub async fn create_channel(
    State(state): State<AppState>,
    caller: Caller,
    Json(input): Json<ChannelInput>,
) -> ApiResult<impl IntoResponse> {
    caller.require_admin()?;
    let channel = state.store.create_channel(input, &caller.name).await?;
    notify(&state.store, json!({ "kind": "channels" })).await;
    Ok((StatusCode::CREATED, Json(json!(channel))))
}

pub async fn update_channel(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(update): Json<ChannelUpdate>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let channel = state.store.update_channel(id, update, &caller.name).await?;
    notify(&state.store, json!({ "kind": "channels" })).await;
    Ok(Json(json!(channel)))
}

pub async fn delete_channel(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    state.store.delete_channel(id, &caller.name).await?;
    notify(&state.store, json!({ "kind": "channels" })).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct TestInput {
    #[serde(default)]
    pub to: Vec<String>,
}

/// Send a test message right away (admin), to check the settings.
pub async fn test_channel(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(input): Json<TestInput>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let channel = state.store.get_channel(id).await?;
    let now = chrono::Utc::now();
    let item = OutboxItem {
        id: Uuid::now_v7(),
        channel_id: id,
        department_id: None,
        agent_id: None,
        drafted_by: caller.name.clone(),
        recipients: input.to,
        subject: "Test from agentcore".into(),
        body: format!(
            "This is a test of the `{}` channel, sent by {}.",
            channel.name, caller.name
        ),
        status: OutboxStatus::Sending,
        revision: 1,
        history: Vec::new(),
        feedback: None,
        decided_by: Some(caller.name.clone()),
        decided_at: Some(now),
        sent_at: None,
        error: None,
        created_at: now,
        updated_at: now,
    };
    Ok(Json(
        match deliver(&state.store, &channel, &item, &caller.name).await {
            Ok(()) => json!({ "ok": true }),
            Err(error) => json!({ "ok": false, "error": error }),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(kind: ChannelKind, config: Value) -> Channel {
        Channel {
            id: Uuid::nil(),
            name: "c".into(),
            kind,
            description: String::new(),
            config,
            secret_hint: None,
            departments: vec![],
            requires_approval: true,
            max_per_day: 5,
            disclosure: "Written by an AI agent.".into(),
            enabled: true,
            updated_at: chrono::Utc::now(),
            updated_by: "a".into(),
        }
    }

    #[test]
    fn email_recipients_follow_the_rules() {
        let c = channel(
            ChannelKind::Email,
            json!({ "allowed_domains": ["Example.com", "@acme.io"], "max_recipients": 2 }),
        );
        let ok =
            |r: &[&str]| check_recipients(&c, &r.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(ok(&["Ana <ana@example.com>"]).is_ok());
        assert!(ok(&["bo@ACME.io"]).is_ok());
        assert!(ok(&["eve@evil.com"]).unwrap_err().contains("@example.com"));
        assert!(ok(&["a@example.com", "b@example.com", "c@example.com"]).is_err());
        assert!(ok(&["not an address"]).is_err());
        assert!(ok(&[]).is_err());
        let slack = channel(ChannelKind::Slack, json!({}));
        assert!(check_recipients(&slack, &[]).is_ok());
    }

    #[test]
    fn disclosure_is_appended() {
        let c = channel(ChannelKind::Slack, json!({}));
        assert_eq!(
            outgoing_text(&c, " Hi \n"),
            "Hi\n\n— Written by an AI agent."
        );
        let mut quiet = c.clone();
        quiet.disclosure = String::new();
        assert_eq!(outgoing_text(&quiet, "Hi"), "Hi");
    }

    #[test]
    fn recipients_from_a_list_or_a_string() {
        assert_eq!(
            recipients_arg(&json!({ "to": "a@x.com, b@x.com; c@x.com" })).unwrap(),
            ["a@x.com", "b@x.com", "c@x.com"]
        );
        assert!(recipients_arg(&json!({ "to": [1] })).is_err());
        assert!(recipients_arg(&json!({})).unwrap().is_empty());
    }
}
