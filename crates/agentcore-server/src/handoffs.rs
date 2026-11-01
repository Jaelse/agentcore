//! Handoffs: one department hands work (a specification) to another, which
//! takes it when it is available. The router carries a notice of it like any
//! message between departments, so the routing policies apply and the
//! receivers are woken by it as soon as they may run; until one of them takes
//! it, the handoff waits for the department. The receivers' answer goes back
//! to the sending department.

use agentcore_core::{Handoff, HandoffStatus, MessageStatus, OrgAgent};
use agentcore_store::{NewHandoff, Store};
use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::AppState;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::org::{Org, Recipient, SendError, Sender, notify, send};

type ApiResult<T> = Result<T, ApiError>;

const MAX_TITLE_CHARS: usize = 200;
/// Leaves room in the notice for the title and how to take it.
const MAX_SPEC_CHARS: usize = 15 * 1024;

/// The notice the receivers get: the whole specification, and what to do.
fn notice(id: Uuid, from: &str, title: &str, spec: &str) -> String {
    format!(
        "Handoff from {from}: {title}\n\n{spec}\n\n(handoff {id}: take it with \
         team_take_handoff when you start on it, and close it with team_finish_handoff when it \
         is done or you will not do it. Your answer goes back to {from}.)"
    )
}

pub fn handoff_json(org: &Org, h: &Handoff) -> Value {
    let name = |d| org.department(d).map(|d| d.name.clone());
    json!({
        "id": h.id,
        "from_department": h.from_department,
        "from_department_name": name(h.from_department),
        "from_name": h.from_name,
        "to_department": h.to_department,
        "to_department_name": name(h.to_department),
        "title": h.title,
        "spec": h.spec,
        "status": h.status,
        "message_id": h.message_id,
        "taken_by": h.taken_by,
        "taken_by_name": h.taken_by_name,
        "taken_at": h.taken_at,
        "result": h.result,
        "closed_at": h.closed_at,
        "created_at": h.created_at,
    })
}

async fn changed(store: &Store, h: &Handoff) {
    notify(
        store,
        json!({
            "kind": "handoffs",
            "id": h.id,
            "departments": [h.from_department, h.to_department],
        }),
    )
    .await;
}

/// An agent hands work to another department (by name).
pub async fn hand_off(
    store: &Store,
    from: &OrgAgent,
    department: &str,
    title: &str,
    spec: &str,
) -> Result<Handoff, SendError> {
    let (title, spec) = (title.trim(), spec.trim());
    if title.is_empty() || spec.is_empty() {
        return Err(SendError::Invalid(
            "a handoff needs a title and a specification".into(),
        ));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(SendError::Invalid(format!(
            "the title is limited to {MAX_TITLE_CHARS} characters"
        )));
    }
    if spec.chars().count() > MAX_SPEC_CHARS {
        return Err(SendError::Invalid(format!(
            "the specification is limited to {MAX_SPEC_CHARS} characters; put supporting \
             material in your department's files and summarise it"
        )));
    }
    let org = Org::load(store).await?;
    let from_dept = org
        .department(from.department_id)
        .ok_or_else(|| SendError::Invalid("your department no longer exists".into()))?;
    let to = org.department_named(department).ok_or_else(|| {
        let names: Vec<&str> = org
            .departments
            .iter()
            .filter(|d| d.id != from_dept.id)
            .map(|d| d.name.as_str())
            .collect();
        SendError::Invalid(format!(
            "unknown department `{department}`; departments: {}",
            names.join(", ")
        ))
    })?;
    if to.id == from_dept.id {
        return Err(SendError::Invalid(
            "that is your own department: hand work to a colleague with team_send_message".into(),
        ));
    }
    let id = Uuid::now_v7();
    let message = send(
        store,
        &Sender::Agent(from.id),
        Recipient::Department(to.id),
        &notice(id, &from_dept.name, title, spec),
    )
    .await?;
    let handoff = store
        .insert_handoff(NewHandoff {
            id,
            from_department: from_dept.id,
            from_agent: Some(from.id),
            from_name: org.label(from),
            to_department: to.id,
            title: title.to_string(),
            spec: spec.to_string(),
            status: if message.status == MessageStatus::Held {
                HandoffStatus::Held
            } else {
                HandoffStatus::Waiting
            },
            message_id: message.id,
        })
        .await?;
    changed(store, &handoff).await;
    Ok(handoff)
}

/// A person approved or refused the held notice `message`: its handoff (if
/// it carries one) follows.
pub async fn notice_decided(store: &Store, message: Uuid, delivered: bool, decision: &str) {
    match store.release_handoff(message, delivered, decision).await {
        Ok(Some(h)) => changed(store, &h).await,
        Ok(None) => {}
        Err(err) => tracing::warn!(%message, error = %err, "could not update the handoff"),
    }
}

/// A handoff by id, or by title among `candidates`.
pub fn find<'a>(candidates: &'a [Handoff], key: &str) -> Result<&'a Handoff, String> {
    let key = key.trim();
    candidates
        .iter()
        .find(|h| h.id.to_string() == key)
        .or_else(|| {
            candidates
                .iter()
                .find(|h| h.title.eq_ignore_ascii_case(key))
        })
        .ok_or_else(|| format!("no handoff `{key}` (see team_list_handoffs)"))
}

/// A worker takes a handoff for its department.
pub async fn take(store: &Store, me: &OrgAgent, key: &str) -> Result<Value, String> {
    let open = store
        .list_handoffs(Some(me.department_id), true)
        .await
        .map_err(|e| e.to_string())?;
    let incoming: Vec<Handoff> = open
        .into_iter()
        .filter(|h| h.to_department == me.department_id)
        .collect();
    let h = find(&incoming, key)?;
    if h.status == HandoffStatus::Held {
        return Err("that handoff still waits for a person to approve it".into());
    }
    let before = h
        .taken_by_name
        .clone()
        .filter(|_| h.taken_by != Some(me.id));
    let taken = store
        .take_handoff(h.id, me.department_id, me.id, &me.name)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("that handoff is not open any more")?;
    changed(store, &taken).await;
    let org = Org::load(store).await.map_err(|e| e.to_string())?;
    let mut out = handoff_json(&org, &taken);
    if let Some(before) = before {
        out["note"] = json!(format!("you took it over from {before}"));
    }
    Ok(out)
}

/// The receivers close a handoff; their answer goes back to the sending
/// department through the router.
pub async fn finish(
    store: &Store,
    me: &OrgAgent,
    key: &str,
    outcome: &str,
    result: &str,
) -> Result<Value, String> {
    let status = match outcome.trim() {
        "done" => HandoffStatus::Done,
        "declined" => HandoffStatus::Declined,
        other => return Err(format!("`outcome` is `done` or `declined`, not `{other}`")),
    };
    let result = result.trim();
    if result.is_empty() {
        return Err("say what was done (or why not) in `result`".into());
    }
    let open = store
        .list_handoffs(Some(me.department_id), true)
        .await
        .map_err(|e| e.to_string())?;
    let incoming: Vec<Handoff> = open
        .into_iter()
        .filter(|h| h.to_department == me.department_id && h.status != HandoffStatus::Held)
        .collect();
    let h = find(&incoming, key)?;
    let closed = store
        .close_handoff(h.id, Some(me.department_id), status, result)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("that handoff is not open any more")?;
    changed(store, &closed).await;
    let org = Org::load(store).await.map_err(|e| e.to_string())?;
    let mut out = handoff_json(&org, &closed);
    out.as_object_mut().map(|o| o.remove("spec"));
    // Tell the sender, if its department is still there.
    if org.department(closed.from_department).is_some() {
        let what = if status == HandoffStatus::Done {
            "is done"
        } else {
            "was declined"
        };
        let text = format!(
            "Your handoff \"{}\" {what} by {}:\n\n{result}",
            closed.title,
            org.label(me)
        );
        match send(
            store,
            &Sender::Agent(me.id),
            Recipient::Department(closed.from_department),
            &text,
        )
        .await
        {
            Ok(m) if m.status == MessageStatus::Held => {
                out["reply"] = json!("waiting for a person to approve it (routing policy)");
            }
            Ok(_) => out["reply"] = json!("sent"),
            Err(err) => {
                out["reply"] = json!(format!(
                    "not delivered ({err}); the sending department can still see the result \
                     in team_list_handoffs"
                ));
            }
        }
    }
    Ok(out)
}

// ---- API ------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct HandoffsQuery {
    #[serde(default)]
    pub department: Option<Uuid>,
    #[serde(default)]
    pub open: bool,
}

pub async fn list(
    State(state): State<AppState>,
    _caller: Caller,
    Query(q): Query<HandoffsQuery>,
) -> ApiResult<Json<Value>> {
    let handoffs = state.store.list_handoffs(q.department, q.open).await?;
    let org = Org::load(&state.store).await?;
    Ok(Json(json!(
        handoffs
            .iter()
            .map(|h| handoff_json(&org, h))
            .collect::<Vec<_>>()
    )))
}

#[derive(Debug, Default, Deserialize)]
pub struct CancelInput {
    #[serde(default)]
    pub reason: String,
}

/// A person withdraws an open handoff.
pub async fn cancel(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    body: Option<Json<CancelInput>>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let reason = body.map(|Json(b)| b.reason).unwrap_or_default();
    let result = match reason.trim() {
        "" => format!("cancelled by {}", caller.name),
        r => format!("cancelled by {}: {r}", caller.name),
    };
    let h = state.store.get_handoff(id).await?;
    if h.status == HandoffStatus::Held
        && let Some(message) = h.message_id
    {
        // Its notice need not wait for approval any more.
        let _ = state.store.reject_message(message, &result).await;
    }
    let closed = state
        .store
        .close_handoff(id, None, HandoffStatus::Cancelled, &result)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "the handoff is not open any more",
            )
        })?;
    changed(&state.store, &closed).await;
    let org = Org::load(&state.store).await?;
    Ok(Json(handoff_json(&org, &closed)))
}
