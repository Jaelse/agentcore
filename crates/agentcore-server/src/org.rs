//! Multi-agent organisations: departments of agents that work together and
//! talk to other departments only through their communicators.
//!
//! * the operator API (`/api/v1/org/...`),
//! * the `team_*` tools agents use to talk and share files,
//! * sending a message (routing + recipients),
//! * the first prompt of a department agent and starting its session.
//!
//! Where an agent runs and what it should be doing is decided in PostgreSQL;
//! [`crate::cluster`] makes each node converge to it.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use agentcore_core::org::{
    Party, RouteError, TOOL_FILES, TOOL_INSIGHTS, TOOL_SANDBOX, Target, route,
};
use agentcore_core::{
    AgentKind, DeliveredMessage, Department, DepartmentId, DepartmentState, Desired, OrgAgent,
    OrgAgentId, OrgMessage, OrgProfile, OrgSettings, Principal, SessionContext,
};
use agentcore_runtime::{CreateSession, Session, SessionOptions, ToolHandler};
use agentcore_store::{
    AgentInput, AgentScope, COMMUNICATOR_NAME, DepartmentInput, GoalInput, GoalUpdate,
    MessageFilter, NewMessage, ScheduleInput, ScheduleUpdate, Store, StoreError,
};
use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::{self, KeepAlive, Sse};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

use crate::AppState;
use crate::auth::Caller;
use crate::error::ApiError;

type ApiResult<T> = Result<T, ApiError>;

const MAX_MESSAGE_CHARS: usize = 16 * 1024;
const MAX_PATH: usize = 256;

// ---- a snapshot of the organisation -------------------------------------------

/// Departments and agents, loaded together (organisations are small).
pub struct Org {
    pub departments: Vec<Department>,
    pub agents: Vec<OrgAgent>,
    pub profile: OrgProfile,
    pub goals: Vec<agentcore_core::OrgGoal>,
}

impl Org {
    pub async fn load(store: &Store) -> Result<Self, StoreError> {
        Ok(Self {
            departments: store.list_departments().await?,
            agents: store.list_agents(None).await?,
            profile: store.org_profile().await?,
            goals: store.list_goals().await?,
        })
    }

    pub fn department(&self, id: DepartmentId) -> Option<&Department> {
        self.departments.iter().find(|d| d.id == id)
    }

    pub fn department_named(&self, name: &str) -> Option<&Department> {
        self.departments
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(name.trim()))
    }

    pub fn agent(&self, id: OrgAgentId) -> Option<&OrgAgent> {
        self.agents.iter().find(|a| a.id == id)
    }

    pub fn members(&self, department: DepartmentId) -> impl Iterator<Item = &OrgAgent> {
        self.agents
            .iter()
            .filter(move |a| a.department_id == department)
    }

    pub fn communicator(&self, department: DepartmentId) -> Option<&OrgAgent> {
        self.members(department)
            .find(|a| a.kind == AgentKind::Communicator)
    }

    /// `analyst (Research)`
    pub fn label(&self, agent: &OrgAgent) -> String {
        let dept = self
            .department(agent.department_id)
            .map_or("?", |d| d.name.as_str());
        format!("{} ({dept})", agent.name)
    }
}

// ---- sending messages ------------------------------------------------------------

pub enum Sender {
    Human(String),
    Agent(OrgAgentId),
}

#[derive(Debug, Clone, Copy)]
pub enum Recipient {
    Agent(OrgAgentId),
    Department(DepartmentId),
    AllDepartments,
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error(transparent)]
    Route(#[from] RouteError),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<SendError> for ApiError {
    fn from(err: SendError) -> Self {
        match err {
            SendError::Store(e) => e.into(),
            other => ApiError::new(StatusCode::BAD_REQUEST, other.to_string()),
        }
    }
}

fn party(agent: &OrgAgent) -> Party {
    Party::Agent {
        id: agent.id,
        department: agent.department_id,
        kind: agent.kind,
    }
}

fn target(agent: &OrgAgent) -> Target {
    Target::Agent {
        id: agent.id,
        department: agent.department_id,
        kind: agent.kind,
    }
}

/// Route a message, resolve its recipients, store it and notify every node.
pub async fn send(
    store: &Store,
    from: &Sender,
    to: Recipient,
    text: &str,
) -> Result<OrgMessage, SendError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(SendError::Invalid("the message is empty".into()));
    }
    if text.chars().count() > MAX_MESSAGE_CHARS {
        return Err(SendError::Invalid(format!(
            "messages are limited to {MAX_MESSAGE_CHARS} characters; put long content in a \
             department file and refer to it"
        )));
    }
    let org = Org::load(store).await?;
    let unknown = |what: String| SendError::Invalid(format!("{what} does not exist"));
    let (from_party, from_agent, from_name) = match from {
        Sender::Human(name) => (Party::Human, None, name.clone()),
        Sender::Agent(id) => {
            let agent = org
                .agent(*id)
                .ok_or_else(|| unknown(format!("agent {id}")))?;
            (party(agent), Some(agent), org.label(agent))
        }
    };
    let (to_target, to_kind, to_agent, to_department) = match to {
        Recipient::Agent(id) => {
            let agent = org
                .agent(id)
                .ok_or_else(|| unknown(format!("agent {id}")))?;
            (
                target(agent),
                "agent",
                Some(agent.id),
                Some(agent.department_id),
            )
        }
        Recipient::Department(id) => {
            org.department(id)
                .ok_or_else(|| unknown(format!("department {id}")))?;
            (Target::Department(id), "department", None, Some(id))
        }
        Recipient::AllDepartments => (Target::AllDepartments, "all_departments", None, None),
    };
    let scope = route(from_party, to_target)?;
    let inter = scope == agentcore_core::MessageScope::InterDepartment;

    let sender_id = from_agent.map(|a| a.id);
    let sender_dept = from_agent.map(|a| a.department_id);
    let recipients: Vec<&OrgAgent> = match to {
        Recipient::Agent(_) => to_agent.and_then(|id| org.agent(id)).into_iter().collect(),
        Recipient::Department(d) if inter => org.communicator(d).into_iter().collect(),
        Recipient::Department(d) => org.members(d).filter(|a| Some(a.id) != sender_id).collect(),
        Recipient::AllDepartments if from_agent.is_none() => org.agents.iter().collect(),
        Recipient::AllDepartments => org
            .departments
            .iter()
            .filter(|d| Some(d.id) != sender_dept)
            .filter_map(|d| org.communicator(d.id))
            .collect(),
    };
    if recipients.is_empty() {
        return Err(SendError::Invalid(
            "nobody would receive this message".into(),
        ));
    }
    let to_name = match to {
        Recipient::Agent(_) => recipients.first().map(|a| org.label(a)).unwrap_or_default(),
        Recipient::Department(d) => {
            let name = org.department(d).map_or("?", |d| d.name.as_str());
            if inter {
                name.to_string()
            } else {
                format!("everyone in {name}")
            }
        }
        Recipient::AllDepartments => "all departments".into(),
    };
    let message = store
        .insert_message(NewMessage {
            scope,
            from_agent: sender_id,
            from_department: sender_dept,
            from_name,
            to_kind,
            to_agent,
            to_department,
            to_name,
            text: text.to_string(),
            recipients: recipients.iter().map(|a| (a.id, a.department_id)).collect(),
        })
        .await?;
    let mut departments: Vec<DepartmentId> = recipients.iter().map(|a| a.department_id).collect();
    departments.extend(sender_dept);
    departments.sort();
    departments.dedup();
    notify(
        store,
        json!({
            "kind": "message",
            "id": message.id,
            "departments": departments,
            "agents": message.recipients,
        }),
    )
    .await;
    Ok(message)
}

/// Tell every node (including this one) that something changed.
pub async fn notify(store: &Store, payload: Value) {
    if let Err(err) = store.notify(&payload).await {
        tracing::warn!(error = %err, "could not notify the other nodes");
    }
}

pub fn delivered(message: &OrgMessage) -> DeliveredMessage {
    DeliveredMessage {
        message_id: message.id,
        from: message.from_name.clone(),
        to: message.to_name.clone(),
        scope: message.scope,
        text: message.text.clone(),
        sent_at: message.created_at,
    }
}

// ---- the agents' team tools -----------------------------------------------------

pub(crate) fn def(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
    })
}

/// `team_*` tools of one department agent. The department and identity come
/// from the agent's record, never from tool arguments.
pub struct TeamTools {
    store: Store,
    agent: OrgAgentId,
    kind: AgentKind,
    files: bool,
}

impl TeamTools {
    pub fn new(store: Store, agent: &OrgAgent, department: &Department) -> Self {
        Self {
            store,
            agent: agent.id,
            kind: agent.kind,
            files: agent.kind == AgentKind::Worker && department.grants(TOOL_FILES),
        }
    }

    async fn me(&self) -> Result<(Org, OrgAgent), String> {
        let org = Org::load(&self.store).await.map_err(|e| e.to_string())?;
        let me = org
            .agent(self.agent)
            .cloned()
            .ok_or("you are no longer a member of the organisation")?;
        Ok((org, me))
    }

    async fn colleagues(&self) -> Result<Value, String> {
        let (org, me) = self.me().await?;
        let dept = org
            .department(me.department_id)
            .ok_or("your department no longer exists")?;
        let colleagues: Vec<Value> = org
            .members(dept.id)
            .filter(|a| a.id != me.id)
            .map(|a| {
                json!({
                    "name": a.name,
                    "kind": a.kind,
                    "status": a.status.as_deref().filter(|_| a.desired != Desired::Stopped)
                        .unwrap_or("not running"),
                })
            })
            .collect();
        let others: Vec<&str> = org
            .departments
            .iter()
            .filter(|d| d.id != dept.id)
            .map(|d| d.name.as_str())
            .collect();
        Ok(json!({
            "you": me.name,
            "department": dept.name,
            "mission": dept.mission,
            "colleagues": colleagues,
            "other_departments": others,
            "how_to_reach_other_departments": match me.kind {
                AgentKind::Worker => "ask your communicator: team_send_message(to: \"communicator\")",
                AgentKind::Communicator => "team_send_to_department(department, text)",
            },
        }))
    }

    async fn send_message(&self, args: &Value) -> Result<Value, String> {
        let to = text_arg(args, "to")?;
        let text = text_arg(args, "text")?;
        let (org, me) = self.me().await?;
        let recipient = match to.trim().to_ascii_lowercase().as_str() {
            "everyone" | "all" => Recipient::Department(me.department_id),
            name => {
                let colleague = org
                    .members(me.department_id)
                    .find(|a| a.name == name)
                    .ok_or_else(|| {
                        format!(
                            "there is no `{name}` in your department; use a colleague's name, \
                             \"communicator\" or \"everyone\" (see team_list_colleagues)"
                        )
                    })?;
                Recipient::Agent(colleague.id)
            }
        };
        let sent = send(&self.store, &Sender::Agent(me.id), recipient, &text)
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({ "sent": true, "to": sent.to_name, "recipients": sent.recipients.len() }))
    }

    async fn send_to_department(&self, args: &Value) -> Result<Value, String> {
        let department = text_arg(args, "department")?;
        let text = text_arg(args, "text")?;
        let (org, me) = self.me().await?;
        let recipient = if department.trim().eq_ignore_ascii_case("all") {
            Recipient::AllDepartments
        } else {
            let dept = org.department_named(&department).ok_or_else(|| {
                let names: Vec<&str> = org.departments.iter().map(|d| d.name.as_str()).collect();
                format!(
                    "unknown department `{department}`; departments: {}",
                    names.join(", ")
                )
            })?;
            if dept.id == me.department_id {
                return Err(
                    "that is your own department: use team_send_message to talk to it".into(),
                );
            }
            Recipient::Department(dept.id)
        };
        let sent = send(&self.store, &Sender::Agent(me.id), recipient, &text)
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({ "sent": true, "to": sent.to_name, "recipients": sent.recipients.len() }))
    }

    async fn read_messages(&self) -> Result<Value, String> {
        let pending = self
            .store
            .pending_messages(self.agent)
            .await
            .map_err(|e| e.to_string())?;
        let ids: Vec<Uuid> = pending.iter().map(|m| m.id).collect();
        self.store
            .mark_delivered(self.agent, &ids)
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "messages": pending.iter().map(|m| json!({
                "from": m.from_name, "to": m.to_name, "sent_at": m.created_at, "text": m.text,
            })).collect::<Vec<_>>(),
        }))
    }

    async fn department(&self) -> Result<DepartmentId, String> {
        Ok(self
            .store
            .get_agent(self.agent)
            .await
            .map_err(|e| e.to_string())?
            .department_id)
    }
}

pub(crate) fn text_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| format!("missing string argument `{key}`"))
}

/// Normalise a department file path: relative, no `..`, no empty segments.
pub fn file_path(path: &str) -> Result<String, String> {
    let parts: Vec<&str> = path
        .trim()
        .trim_start_matches('/')
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.is_empty() || parts.contains(&"..") || path.contains('\0') {
        return Err(format!("invalid path `{path}`"));
    }
    let clean = parts.join("/");
    if clean.len() > MAX_PATH {
        return Err(format!("paths are limited to {MAX_PATH} characters"));
    }
    Ok(clean)
}

#[async_trait]
impl ToolHandler for TeamTools {
    fn definitions(&self) -> Vec<Value> {
        let text = json!({ "type": "string" });
        let mut tools = vec![
            def(
                "team_list_colleagues",
                "Who is in your department (with their status), your department's mission, and \
                 the names of the other departments.",
                json!({}),
                &[],
            ),
            def(
                "team_send_message",
                "Send a message inside your department. `to` is a colleague's name, \
                 \"communicator\" (your department's link to other departments) or \"everyone\" \
                 (the whole department).",
                json!({ "to": text, "text": text }),
                &["to", "text"],
            ),
            def(
                "team_read_messages",
                "Read your new messages (each message is returned once).",
                json!({}),
                &[],
            ),
        ];
        if self.kind == AgentKind::Worker {
            tools.push(def(
                "team_list_goals",
                "The organisation's goals that people set, with their latest progress.",
                json!({}),
                &[],
            ));
            tools.push(def(
                "team_report_progress",
                "Report progress on an active goal (replaces its latest progress note). Be \
                 factual: what changed, evidence, what is next. People decide when a goal is \
                 achieved.",
                json!({ "goal": { "type": "string", "description": "Goal id or title" }, "text": text }),
                &["goal", "text"],
            ));
        }
        if self.kind == AgentKind::Communicator {
            tools.push(def(
                "team_send_to_department",
                "Send a message to another department (its communicator receives it), or to \
                 every other department with department \"all\". Make it self-contained: say \
                 who asks and what is needed.",
                json!({ "department": text, "text": text }),
                &["department", "text"],
            ));
        }
        if self.files {
            tools.extend([
                def(
                    "team_list_files",
                    "List your department's shared files.",
                    json!({}),
                    &[],
                ),
                def(
                    "team_read_file",
                    "Read one of your department's shared files.",
                    json!({ "path": text }),
                    &["path"],
                ),
                def(
                    "team_write_file",
                    "Create or replace one of your department's shared files (up to 1 MiB). \
                     Every colleague in your department can read it.",
                    json!({ "path": text, "content": text }),
                    &["path", "content"],
                ),
            ]);
        }
        tools
    }

    async fn call(&self, tool: &str, arguments: &Value) -> Result<Value, String> {
        let offered = self
            .definitions()
            .iter()
            .any(|d| d["name"].as_str() == Some(tool));
        if !offered {
            return Err(format!("tool `{tool}` is not available to you"));
        }
        match tool {
            "team_list_colleagues" => self.colleagues().await,
            "team_send_message" => self.send_message(arguments).await,
            "team_read_messages" => self.read_messages().await,
            "team_send_to_department" => self.send_to_department(arguments).await,
            "team_list_goals" => {
                let goals = self.store.list_goals().await.map_err(|e| e.to_string())?;
                Ok(json!({ "goals": goals.iter().map(|g| json!({
                    "id": g.id, "title": g.title, "description": g.description,
                    "status": g.status, "progress": g.progress, "progress_by": g.progress_by,
                    "progress_at": g.progress_at,
                })).collect::<Vec<_>>() }))
            }
            "team_report_progress" => {
                let key = text_arg(arguments, "goal")?;
                let report = text_arg(arguments, "text")?;
                let (org, me) = self.me().await?;
                let goal = org
                    .goals
                    .iter()
                    .filter(|g| g.status == agentcore_core::GoalStatus::Active)
                    .find(|g| {
                        g.id.to_string() == key.trim() || g.title.eq_ignore_ascii_case(key.trim())
                    })
                    .ok_or_else(|| format!("no active goal `{key}` (see team_list_goals)"))?;
                let by = org.label(&me);
                let updated = self
                    .store
                    .report_goal_progress(goal.id, &report, &by)
                    .await
                    .map_err(|e| e.to_string())?;
                let detail = goal.id.to_string();
                let _ = self
                    .store
                    .record_activity(agentcore_store::Activity {
                        department: Some(me.department_id),
                        agent: Some(me.id),
                        session: None,
                        kind: "progress",
                        value: None,
                        detail: Some(&detail),
                    })
                    .await;
                notify(&self.store, json!({ "kind": "goals" })).await;
                Ok(json!({ "goal": updated.title, "recorded": true }))
            }
            "team_list_files" => {
                let files = self
                    .store
                    .list_department_files(self.department().await?)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(json!({ "files": files }))
            }
            "team_read_file" => {
                let path = file_path(&text_arg(arguments, "path")?)?;
                match self
                    .store
                    .read_department_file(self.department().await?, &path)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    Some(content) => Ok(json!({
                        "path": path,
                        "content": String::from_utf8_lossy(&content),
                    })),
                    None => Err(format!("no file `{path}` in your department")),
                }
            }
            "team_write_file" => {
                let path = file_path(&text_arg(arguments, "path")?)?;
                let content = text_arg(arguments, "content")?;
                let me = self
                    .store
                    .get_agent(self.agent)
                    .await
                    .map_err(|e| e.to_string())?;
                self.store
                    .write_department_file(me.department_id, &path, content.as_bytes(), &me.name)
                    .await
                    .map_err(|e| e.to_string())?;
                notify(
                    &self.store,
                    json!({ "kind": "files", "departments": [me.department_id] }),
                )
                .await;
                Ok(json!({ "path": path, "bytes": content.len() }))
            }
            other => Err(format!("unknown tool `{other}`")),
        }
    }
}

// ---- prompts ------------------------------------------------------------------------

/// First prompt of a department agent.
/// The repository a department works on, as given to its workers.
pub struct RepoInfo {
    /// `owner/name`
    pub repository: String,
    pub base_branch: String,
    /// The worker's own branch, when it has a checkout.
    pub work_branch: Option<String>,
    /// The role delivers pull requests.
    pub delivers: bool,
    /// GitHub tools the worker has.
    pub tools: Vec<String>,
}

/// Extra context for an agent's first prompt.
#[derive(Default)]
pub struct StartContext<'a> {
    pub repo: Option<&'a RepoInfo>,
    /// Why this session continues an earlier one, if it does.
    pub continuing: Option<&'a str>,
    /// Messages that arrived while the agent was asleep.
    pub messages: &'a [OrgMessage],
    /// Business data the department may read.
    pub data_sources: &'a [agentcore_core::DataSource],
}

/// Where an agent keeps its working notes (department files).
pub fn notes_path(agent: &OrgAgent) -> String {
    format!("notes/{}.md", agent.name)
}

pub fn compose_prompt(
    org: &Org,
    agent: &OrgAgent,
    dept: &Department,
    start: &StartContext<'_>,
) -> String {
    let repo = start.repo;
    let others: Vec<&str> = org
        .departments
        .iter()
        .filter(|d| d.id != dept.id)
        .map(|d| d.name.as_str())
        .collect();
    let others = if others.is_empty() {
        "none yet".to_string()
    } else {
        others.join(", ")
    };
    let mission = if dept.mission.trim().is_empty() {
        "(no mission written yet: ask the people supervising you)"
    } else {
        dept.mission.trim()
    };
    let mut p = String::new();
    let company = org.profile.company_name.trim();
    let about = org.profile.company_about.trim();
    let company_section = match (company.is_empty(), about.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("## Your company\n\n{company}\n\n"),
        (true, false) => format!("## Your company\n\n{about}\n\n"),
        (false, false) => format!("## Your company\n\n{company}: {about}\n\n"),
    };
    match agent.kind {
        AgentKind::Worker => {
            let colleagues: Vec<String> = org
                .members(dept.id)
                .filter(|a| a.id != agent.id)
                .map(|a| match a.kind {
                    AgentKind::Communicator => {
                        format!(
                            "- `{COMMUNICATOR_NAME}`: passes messages to and from other departments"
                        )
                    }
                    AgentKind::Worker => format!("- `{}`", a.name),
                })
                .collect();
            p.push_str(&format!(
                "You are `{}`, an AI agent in the **{}** department of an organisation of AI \
                 agents. People supervise the organisation: they can read every message and \
                 everything you do.\n\n{company_section}## Your department's mission\n\n{mission}\n\n",
                agent.name, dept.name
            ));
            let own = agent.instructions.trim();
            p.push_str("## Your own instructions\n\n");
            p.push_str(if own.is_empty() {
                "Work on the department's mission together with your colleagues."
            } else {
                own
            });
            p.push_str(&format!(
                "\n\n## Your department\n\n{}\n\nOther departments: {others}. You cannot contact \
                 them directly: ask your department's communicator (`team_send_message` with \
                 `to: \"communicator\"`) and say which department you need and what for. \
                 Answers come back to you as messages.\n\n",
                colleagues.join("\n")
            ));
            if let Some(repo) = repo {
                p.push_str(&format!(
                    "## Your repository\n\nYour department works on the GitHub repository `{}` \
                     (base branch `{}`).\n",
                    repo.repository, repo.base_branch
                ));
                if let Some(branch) = &repo.work_branch {
                    p.push_str(&format!(
                        "It is checked out in your sandbox at /workspace, on your own branch \
                         `{branch}`. Commit your work there.\n"
                    ));
                }
                if repo.delivers {
                    p.push_str(
                        "Your work leaves the sandbox as a pull request that a person reviews: \
                         when a change is ready, make sure the checks pass, then call \
                         `propose_pull_request` with a title and description, and tell your \
                         colleagues. Keep each change focused; start the next one after it is \
                         delivered.\n",
                    );
                }
                if !repo.tools.is_empty() {
                    p.push_str(&format!(
                        "GitHub tools you have: {}. Use issues to plan and track work so people \
                         can follow it.\n",
                        repo.tools.join(", ")
                    ));
                }
                p.push('\n');
            }
            p.push_str(
                "## Working together\n\n\
                 - `team_send_message(to, text)`: a colleague's name, `communicator`, or `everyone`.\n\
                 - `team_read_messages()`: new messages; `team_list_colleagues()`: who is here now.\n",
            );
            if dept.grants(TOOL_FILES) {
                p.push_str(
                    "- `team_list_files`, `team_read_file`, `team_write_file`: your department's \
                     shared files. Put longer results there and tell colleagues the path.\n",
                );
            }
            if dept.grants(TOOL_SANDBOX) {
                p.push_str(
                    "- `run_command`, `read_file`, `write_file`, `list_files`: your own sandbox \
                     (only you can see it).\n",
                );
            }
            p.push_str(
                "\nWhen you have done what you can for now, end your turn with a short summary. \
                 You are woken up when a message arrives.\n",
            );
        }
        AgentKind::Communicator => {
            let members: Vec<String> = org
                .members(dept.id)
                .filter(|a| a.kind == AgentKind::Worker)
                .map(|a| format!("`{}`", a.name))
                .collect();
            p.push_str(&format!(
                "You are the communicator of the **{}** department in an organisation of AI \
                 agents. People supervise the organisation and read every message.\n\n\
                 Your only job is to pass messages between your department and other \
                 departments. You do not do the department's work, and you have no tools other \
                 than messaging.\n\n{company_section}\
                 ## Your department\n\nMission: {mission}\n\nMembers: {}\n\n\
                 Other departments: {others}\n\n\
                 ## How you work\n\n\
                 - When a colleague asks you to contact another department, send a clear, \
                 self-contained message with `team_send_to_department(department, text)`: who \
                 is asking, what is needed, by when. Use department `all` only for things every \
                 department needs to know.\n\
                 - When a message arrives from another department, pass it on with \
                 `team_send_message(to, text)` to the colleague it concerns, or to `everyone`, \
                 and say which department it came from. Answers go back the same way.\n\
                 - Pass messages on faithfully. Do not invent facts, make commitments for your \
                 department, or share anything your colleagues did not ask you to share.\n\
                 - When you have passed everything on, end your turn. You are woken up when the \
                 next message arrives.\n",
                dept.name,
                if members.is_empty() {
                    "none yet".to_string()
                } else {
                    members.join(", ")
                },
            ));
        }
    }
    let goals: Vec<&agentcore_core::OrgGoal> = org
        .goals
        .iter()
        .filter(|g| g.status == agentcore_core::GoalStatus::Active)
        .collect();
    if !goals.is_empty() {
        p.push_str("\n## The organisation's goals\n\nSet by the people who run the company:\n\n");
        for g in goals {
            let owner = g
                .department_id
                .and_then(|d| org.department(d))
                .map(|d| format!(" (owned by {})", d.name))
                .unwrap_or_default();
            p.push_str(&format!("- **{}**{owner}", g.title));
            if !g.description.trim().is_empty() {
                p.push_str(&format!(": {}", g.description.trim()));
            }
            if !g.progress.trim().is_empty() {
                p.push_str(&format!(" Latest progress: {}", g.progress.trim()));
            }
            p.push('\n');
        }
        if agent.kind == AgentKind::Worker {
            p.push_str(
                "\nWork towards them, and report progress with `team_report_progress` when \
                 something changes.\n",
            );
        }
    }
    if agent.kind == AgentKind::Worker && !start.data_sources.is_empty() {
        p.push_str(
            "\n## Business data\n\nRead-only data your department may use (`data_list_sources`, \
             `data_query`). Base decisions on it and say which source and query numbers come \
             from.\n\n",
        );
        for s in start.data_sources {
            p.push_str(&format!(
                "- **{}** ({}): {}\n",
                s.name,
                s.kind.as_str(),
                s.description
            ));
        }
    }
    if agent.kind == AgentKind::Worker && dept.grants(TOOL_INSIGHTS) {
        p.push_str(
            "\n## Improving the organisation\n\nYou can see how the whole organisation works \
             (`insights_metrics`, `insights_org`, `insights_messages`). Find what is inefficient \
             (wasted agent time or tokens, slow responses, failing agents, goals without \
             progress, needless approvals or denials) and propose improvements with \
             `insights_propose`: the problem, the evidence from the metrics, the solution and \
             the concrete changes. Nothing changes until a person applies it. When people ask \
             for changes, revise the proposal with `insights_revise`; when they reject one, do \
             not propose it again unless something changed. After a change was applied, check \
             in the metrics whether it helped.\n",
        );
    }
    if agent.kind == AgentKind::Worker && dept.grants(TOOL_FILES) {
        p.push_str(&format!(
            "\n## Your notes\n\nKeep your working notes in the department file `{}`: what you \
             are working on, decisions, and the next steps. Sessions have a time limit; when one \
             ends you continue in a fresh session, starting from these notes.\n",
            notes_path(agent)
        ));
    }
    if let Some(why) = start.continuing {
        p.push_str(&format!(
            "\n## You are continuing\n\n{why} This is a fresh session: your earlier sandbox is \
             gone{}. Start by reading your notes",
            if repo.and_then(|r| r.work_branch.as_ref()).is_some() {
                " and you have a fresh checkout (deliver work before a session ends)"
            } else {
                ""
            }
        ));
        p.push_str(
            if dept.grants(TOOL_FILES) && agent.kind == AgentKind::Worker {
                " (`team_read_file`) and the recent messages, then carry on.\n"
            } else {
                " and the recent messages (`team_read_messages`), then carry on.\n"
            },
        );
    }
    if !start.messages.is_empty() {
        p.push_str("\n## New messages\n\n");
        for m in start.messages {
            p.push_str(&delivered(m).render());
            p.push_str("\n\n");
        }
    }
    p
}

/// Start the session of a department agent on this node.
pub async fn start_agent_session(
    state: &AppState,
    org: &Org,
    agent: &OrgAgent,
    dept: &Department,
    models: Vec<agentcore_core::ModelEndpoint>,
    continuing: Option<&str>,
) -> Result<Arc<Session>, ApiError> {
    let policy = match agent.kind {
        AgentKind::Worker => dept.policy.clone(),
        AgentKind::Communicator => state.config.org.communicator_policy.clone(),
    };
    let label = format!("{}/{}", dept.name, agent.name);
    let mut context = SessionContext {
        department_id: Some(dept.id),
        department_name: Some(dept.name.clone()),
        org_agent_id: Some(agent.id),
        org_agent_kind: Some(agent.kind.as_str().into()),
        ..Default::default()
    };
    let mut options = SessionOptions {
        models,
        hide_sandbox_tools: agent.kind == AgentKind::Communicator || !dept.grants(TOOL_SANDBOX),
        agent_label: Some(label),
        ..Default::default()
    };
    let mut tools: Vec<Arc<dyn ToolHandler>> =
        vec![Arc::new(TeamTools::new(state.store.clone(), agent, dept))];
    let data_sources = match agent.kind {
        AgentKind::Worker => state.store.data_sources_for(dept.id).await?,
        AgentKind::Communicator => Vec::new(),
    };
    if agent.kind == AgentKind::Worker {
        // Business data granted to the department (checked on every call).
        if !data_sources.is_empty() {
            tools.push(Arc::new(crate::insights::DataTools::new(
                state.store.clone(),
                agent.id,
                dept.id,
            )));
        }
        // The retrospective: metrics, structure and proposals.
        if dept.grants(TOOL_INSIGHTS) {
            tools.push(Arc::new(crate::insights::InsightsTools::new(
                state.clone(),
                agent.id,
            )));
        }
    }
    let mut repo = None;
    let mut mirror_cell = None;

    // Workers of a department linked to a project get the repository: a
    // checkout on their own branch (with sandbox access), GitHub tools for
    // their role, the role's playbook and the team's conventions, and
    // delivery as a pull request.
    if agent.kind == AgentKind::Worker
        && let Some(project_id) = dept.project_id
    {
        let project = state.project(project_id).await?;
        let role = state.role(dept.role.as_deref().unwrap_or(&project.role))?;
        let (gh, config, token) = state.require_github().await?;
        let github = crate::teamwork::GitHubTools::new(Some(gh), &project, &role);
        let github_tools: Vec<String> = github
            .definitions()
            .iter()
            .filter_map(|d| d["name"].as_str().map(String::from))
            .collect();
        tools.push(Arc::new(github));
        context.role = Some(role.name.clone());
        context.project_id = Some(project.id);
        context.project_name = Some(project.name.clone());
        context.repository = Some(project.repository());
        context.base_branch = Some(project.default_branch.clone());
        let mut work_branch = None;
        if dept.grants(TOOL_SANDBOX) {
            let hint = Uuid::now_v7().simple().to_string();
            let branch = role.branch_name(
                None,
                &format!("{} {} {}", dept.name, agent.name, &hint[hint.len() - 6..]),
                &hint,
            );
            let (setup, cell) =
                crate::teamwork::repository_setup(&project, &config, &token, branch.clone());
            options.workspace = Some(setup);
            options.work_item = Some(agentcore_roles::WorkItem {
                repository: Some(project.repository()),
                branch: Some(project.default_branch.clone()),
                issue: None,
                task: String::new(),
                project_notes: project.notes.clone(),
            });
            context.work_branch = Some(branch.clone());
            work_branch = Some(branch);
            mirror_cell = Some(cell);
        } else {
            options.work_item = Some(agentcore_roles::WorkItem {
                project_notes: project.notes.clone(),
                ..Default::default()
            });
        }
        repo = Some(RepoInfo {
            repository: project.repository(),
            base_branch: project.default_branch.clone(),
            delivers: work_branch.is_some()
                && role.delivery.kind == agentcore_roles::DeliveryKind::PullRequest,
            work_branch,
            tools: github_tools,
        });
        options.role = Some(role);
    }
    options.tools = Some(Arc::new(Toolset(tools)));
    options.context = context;
    // Messages that arrived while the agent was not running start it off.
    let pending = state.store.pending_messages(agent.id).await?;
    let start = StartContext {
        repo: repo.as_ref(),
        continuing,
        messages: &pending,
        data_sources: &data_sources,
    };
    let session = state.manager.create(
        CreateSession {
            agent: agent.agent.clone(),
            task: compose_prompt(org, agent, dept, &start),
            policy: Some(policy),
        },
        principal(&agent.changed_by),
        options,
    )?;
    if let Some(cell) = mirror_cell {
        let _ = cell.set(state.mirror_path(session.id()));
    }
    if !pending.is_empty() {
        let ids: Vec<Uuid> = pending.iter().map(|m| m.id).collect();
        state.store.mark_delivered(agent.id, &ids).await?;
    }
    Ok(session)
}

/// Several tool handlers as one: definitions are concatenated; a call goes
/// to the handler that defines the tool.
pub struct Toolset(pub Vec<Arc<dyn ToolHandler>>);

#[async_trait]
impl ToolHandler for Toolset {
    fn definitions(&self) -> Vec<Value> {
        self.0.iter().flat_map(|h| h.definitions()).collect()
    }

    async fn call(&self, tool: &str, arguments: &Value) -> Result<Value, String> {
        for handler in &self.0 {
            if handler
                .definitions()
                .iter()
                .any(|d| d["name"].as_str() == Some(tool))
            {
                return handler.call(tool, arguments).await;
            }
        }
        Err(format!("unknown tool `{tool}`"))
    }
}

/// The principal behind a recorded name (`agentcore` = the system).
pub fn principal(name: &str) -> Principal {
    if name == "agentcore" || name.is_empty() {
        Principal::System
    } else {
        Principal::human(name)
    }
}

// ---- API ------------------------------------------------------------------------------

fn department_json(dept: &Department, agents: &[&OrgAgent]) -> Value {
    let mut value = json!(dept);
    value["agents"] = json!(agents);
    value
}

pub async fn overview(State(state): State<AppState>, _caller: Caller) -> ApiResult<Json<Value>> {
    let org = Org::load(&state.store).await?;
    let departments: Vec<Value> = org
        .departments
        .iter()
        .map(|d| department_json(d, &org.members(d.id).collect::<Vec<_>>()))
        .collect();
    Ok(Json(json!({
        "node": &*state.node,
        "profile": org.profile,
        "settings": state.store.org_settings().await?,
        "departments": departments,
        "goals": org.goals,
        "check_ins": state.store.list_schedules(None).await?,
        "data_sources": state.store.list_data_sources().await?,
        "proposals_pending": state
            .store
            .list_proposals(None)
            .await?
            .iter()
            .filter(|p| p.status.is_pending())
            .count(),
        "nodes": state.store.list_nodes(state.config.cluster.node_timeout_secs).await?,
        "communicator_policy": state.config.org.communicator_policy,
    })))
}

// ---- goals and check-ins ----------------------------------------------------------

pub async fn list_goals(State(state): State<AppState>, _caller: Caller) -> ApiResult<Json<Value>> {
    Ok(Json(json!(state.store.list_goals().await?)))
}

pub async fn create_goal(
    State(state): State<AppState>,
    caller: Caller,
    Json(input): Json<GoalInput>,
) -> ApiResult<impl IntoResponse> {
    caller.require_operator()?;
    let goal = state.store.create_goal(input, &caller.name).await?;
    notify(&state.store, json!({ "kind": "goals" })).await;
    Ok((StatusCode::CREATED, Json(json!(goal))))
}

pub async fn update_goal(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(update): Json<GoalUpdate>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let goal = state.store.update_goal(id, update, &caller.name).await?;
    notify(&state.store, json!({ "kind": "goals" })).await;
    Ok(Json(json!(goal)))
}

pub async fn delete_goal(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    caller.require_operator()?;
    state.store.delete_goal(id, &caller.name).await?;
    notify(&state.store, json!({ "kind": "goals" })).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct ProgressInput {
    pub text: String,
}

/// A person records progress on a goal (agents use `team_report_progress`).
pub async fn goal_progress(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(input): Json<ProgressInput>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let goal = state
        .store
        .report_goal_progress(id, &input.text, &caller.name)
        .await?;
    notify(&state.store, json!({ "kind": "goals" })).await;
    Ok(Json(json!(goal)))
}

pub async fn list_check_ins(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<DepartmentId>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(state.store.list_schedules(Some(id)).await?)))
}

pub async fn create_check_in(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<DepartmentId>,
    Json(input): Json<ScheduleInput>,
) -> ApiResult<impl IntoResponse> {
    caller.require_operator()?;
    let check_in = state.store.create_schedule(id, input, &caller.name).await?;
    notify(
        &state.store,
        json!({ "kind": "checkins", "departments": [id] }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(json!(check_in))))
}

pub async fn update_check_in(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(update): Json<ScheduleUpdate>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let check_in = state
        .store
        .update_schedule(id, update, &caller.name)
        .await?;
    notify(
        &state.store,
        json!({ "kind": "checkins", "departments": [check_in.department_id] }),
    )
    .await;
    Ok(Json(json!(check_in)))
}

pub async fn delete_check_in(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    caller.require_operator()?;
    let check_in = state.store.get_schedule(id).await?;
    state.store.delete_schedule(id, &caller.name).await?;
    notify(
        &state.store,
        json!({ "kind": "checkins", "departments": [check_in.department_id] }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Send a check-in now (its schedule continues from now).
pub async fn run_check_in(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let check_in = state.store.get_schedule(id).await?;
    if !check_in.enabled {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this check-in is turned off; turn it on first",
        ));
    }
    state.store.run_schedule_now(id).await?;
    crate::cluster::run_check_ins(&state)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!(state.store.get_schedule(id).await?)))
}

pub async fn get_settings(
    State(state): State<AppState>,
    _caller: Caller,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(state.store.org_settings().await?)))
}

pub async fn put_settings(
    State(state): State<AppState>,
    caller: Caller,
    Json(settings): Json<OrgSettings>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let saved = state.store.set_org_settings(settings, &caller.name).await?;
    notify(&state.store, json!({ "kind": "settings" })).await;
    Ok(Json(json!(saved)))
}

pub(crate) async fn check_department(
    state: &AppState,
    input: &mut DepartmentInput,
) -> ApiResult<()> {
    if let Some(project_id) = input.project_id {
        let project = state.project(project_id).await?;
        let role = input
            .role
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .unwrap_or(&project.role);
        state.role(role)?;
    } else if input.role.as_deref().is_some_and(|r| !r.trim().is_empty()) {
        state.role(input.role.as_deref().unwrap_or_default().trim())?;
    }
    if input.policy.trim().is_empty() {
        input.policy = if state.manager.policies().get("department").is_some() {
            "department".into()
        } else {
            state.config.policies.default.clone()
        };
    }
    let bad = |m: String| ApiError::new(StatusCode::BAD_REQUEST, m);
    if state.manager.policies().get(input.policy.trim()).is_none() {
        return Err(bad(format!("unknown policy `{}`", input.policy)));
    }
    if state
        .manager
        .policies()
        .get(&state.config.org.communicator_policy)
        .is_none()
    {
        return Err(bad(format!(
            "the communicator policy `{}` does not exist",
            state.config.org.communicator_policy
        )));
    }
    check_agent_spec(state, &input.communicator_agent)
}

pub(crate) fn check_agent_spec(state: &AppState, name: &str) -> ApiResult<()> {
    if state.manager.agent(name.trim()).is_some() {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("unknown agent `{name}` (configure it under [[agents]])"),
        ))
    }
}

pub async fn create_department(
    State(state): State<AppState>,
    caller: Caller,
    Json(mut input): Json<DepartmentInput>,
) -> ApiResult<impl IntoResponse> {
    caller.require_admin()?;
    check_department(&state, &mut input).await?;
    let dept = state.store.create_department(input, &caller.name).await?;
    notify(
        &state.store,
        json!({ "kind": "department", "departments": [dept.id] }),
    )
    .await;
    let agents = state.store.list_agents(Some(dept.id)).await?;
    Ok((
        StatusCode::CREATED,
        Json(department_json(&dept, &agents.iter().collect::<Vec<_>>())),
    ))
}

pub async fn update_department(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<DepartmentId>,
    Json(mut input): Json<DepartmentInput>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    check_department(&state, &mut input).await?;
    let dept = state
        .store
        .update_department(id, input, &caller.name)
        .await?;
    notify(
        &state.store,
        json!({ "kind": "department", "departments": [id] }),
    )
    .await;
    Ok(Json(json!(dept)))
}

pub async fn delete_department(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<DepartmentId>,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    state.store.delete_department(id, &caller.name).await?;
    notify(
        &state.store,
        json!({ "kind": "department", "departments": [id] }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn add_agent(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<DepartmentId>,
    Json(input): Json<AgentInput>,
) -> ApiResult<impl IntoResponse> {
    caller.require_operator()?;
    check_agent_spec(&state, &input.agent)?;
    let agent = state.store.add_agent(id, input, &caller.name).await?;
    notify(
        &state.store,
        json!({ "kind": "agents", "departments": [id] }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(json!(agent))))
}

#[derive(Debug, Deserialize)]
pub struct AgentUpdate {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
}

pub async fn update_agent(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<OrgAgentId>,
    Json(update): Json<AgentUpdate>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    if let Some(agent) = &update.agent {
        check_agent_spec(&state, agent)?;
    }
    let agent = state
        .store
        .update_agent(id, update.agent, update.instructions, &caller.name)
        .await?;
    notify(
        &state.store,
        json!({ "kind": "agents", "departments": [agent.department_id] }),
    )
    .await;
    Ok(Json(json!(agent)))
}

pub async fn delete_agent(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<OrgAgentId>,
) -> ApiResult<StatusCode> {
    caller.require_operator()?;
    let agent = state.store.get_agent(id).await?;
    state.store.delete_agent(id, &caller.name).await?;
    notify(
        &state.store,
        json!({ "kind": "agents", "departments": [agent.department_id] }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    Start,
    Pause,
    Resume,
    Stop,
}

/// Apply a control to the agents in scope; returns the changed agents.
pub(crate) async fn control_agents(
    state: &AppState,
    scope: AgentScope,
    control: Control,
    by: &str,
) -> ApiResult<(Vec<OrgAgent>, Vec<Value>)> {
    let store = &state.store;
    let mut failed = Vec::new();
    let changed = match control {
        Control::Start => {
            let candidates: Vec<OrgAgent> = match scope {
                AgentScope::Agent(id) => vec![store.get_agent(id).await?],
                AgentScope::Department(d) => store.list_agents(Some(d)).await?,
                AgentScope::All => store.list_agents(None).await?,
            };
            let mut started = Vec::new();
            for agent in candidates {
                match store
                    .start_agent(agent.id, state.config.cluster.node_timeout_secs, by)
                    .await
                {
                    Ok(Some(a)) => started.push(a),
                    Ok(None) => {}
                    Err(err @ StoreError::Unavailable(_))
                        if matches!(scope, AgentScope::Agent(_)) =>
                    {
                        return Err(err.into());
                    }
                    Err(err) => {
                        failed.push(json!({ "agent": agent.name, "error": err.to_string() }))
                    }
                }
            }
            started
        }
        Control::Pause => {
            store
                .set_desired(scope, &[Desired::Running], Desired::Paused, None, by)
                .await?
        }
        Control::Resume => {
            store
                .set_desired(scope, &[Desired::Paused], Desired::Running, None, by)
                .await?
        }
        Control::Stop => {
            store
                .set_desired(
                    scope,
                    &[Desired::Running, Desired::Paused],
                    Desired::Stopped,
                    Some(&format!("stopped by {by}")),
                    by,
                )
                .await?
        }
    };
    let mut departments: Vec<DepartmentId> = changed.iter().map(|a| a.department_id).collect();
    departments.sort();
    departments.dedup();
    notify(
        store,
        json!({ "kind": "agents", "departments": departments }),
    )
    .await;
    Ok((changed, failed))
}

pub async fn control_agent(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, control)): Path<(OrgAgentId, Control)>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    control_agents(&state, AgentScope::Agent(id), control, &caller.name).await?;
    Ok(Json(json!(state.store.get_agent(id).await?)))
}

pub async fn control_department(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, control)): Path<(DepartmentId, Control)>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    state.store.get_department(id).await?;
    let (changed, failed) = match control {
        // A paused department pauses all its agents, including ones started
        // later, until it is resumed.
        Control::Pause | Control::Resume => {
            let to = if control == Control::Pause {
                DepartmentState::Paused
            } else {
                DepartmentState::Active
            };
            state
                .store
                .set_department_state(Some(id), to, &caller.name)
                .await?;
            notify(
                &state.store,
                json!({ "kind": "department", "departments": [id] }),
            )
            .await;
            if control == Control::Resume {
                // Agents paused one by one are resumed too.
                control_agents(&state, AgentScope::Department(id), control, &caller.name).await?
            } else {
                (Vec::new(), Vec::new())
            }
        }
        _ => control_agents(&state, AgentScope::Department(id), control, &caller.name).await?,
    };
    Ok(Json(json!({
        "department": state.store.get_department(id).await?,
        "changed": changed.len(),
        "failed": failed,
    })))
}

/// Pause or resume every department.
pub async fn pause_all(State(state): State<AppState>, caller: Caller) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let n = state
        .store
        .set_department_state(None, DepartmentState::Paused, &caller.name)
        .await?;
    notify(&state.store, json!({ "kind": "department" })).await;
    Ok(Json(json!({ "paused_departments": n })))
}

pub async fn resume_all(State(state): State<AppState>, caller: Caller) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let n = state
        .store
        .set_department_state(None, DepartmentState::Active, &caller.name)
        .await?;
    control_agents(&state, AgentScope::All, Control::Resume, &caller.name).await?;
    notify(&state.store, json!({ "kind": "department" })).await;
    Ok(Json(json!({ "resumed_departments": n })))
}

/// Part of the emergency stop: every department agent on every node.
pub async fn stop_all_agents(state: &AppState, by: &str, reason: &str) -> ApiResult<usize> {
    let stopped = state
        .store
        .set_desired(
            AgentScope::All,
            &[Desired::Running, Desired::Paused],
            Desired::Stopped,
            Some(reason),
            by,
        )
        .await?;
    notify(
        &state.store,
        json!({ "kind": "stop_all", "origin": &*state.node, "by": by, "reason": reason }),
    )
    .await;
    Ok(stopped.len())
}

#[derive(Debug, Default, Deserialize)]
pub struct MessagesQuery {
    #[serde(default)]
    pub department: Option<DepartmentId>,
    #[serde(default)]
    pub agent: Option<OrgAgentId>,
    #[serde(default)]
    pub after: Option<Uuid>,
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn list_messages(
    State(state): State<AppState>,
    _caller: Caller,
    Query(q): Query<MessagesQuery>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(
        state
            .store
            .list_messages(MessageFilter {
                department: q.department,
                agent: q.agent,
                after: q.after,
                limit: q.limit.unwrap_or(200),
            })
            .await?
    )))
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum MessageTo {
    Agent { agent: OrgAgentId },
    Department { department: DepartmentId },
    Keyword(String),
}

#[derive(Debug, Deserialize)]
pub struct PostMessage {
    pub to: MessageTo,
    pub text: String,
}

pub async fn post_message(
    State(state): State<AppState>,
    caller: Caller,
    Json(req): Json<PostMessage>,
) -> ApiResult<impl IntoResponse> {
    caller.require_operator()?;
    let to = match req.to {
        MessageTo::Agent { agent } => Recipient::Agent(agent),
        MessageTo::Department { department } => Recipient::Department(department),
        MessageTo::Keyword(k) if k == "all_departments" => Recipient::AllDepartments,
        MessageTo::Keyword(k) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("unknown recipient `{k}`"),
            ));
        }
    };
    let message = send(
        &state.store,
        &Sender::Human(caller.name.clone()),
        to,
        &req.text,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!(message))))
}

pub async fn list_files(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<DepartmentId>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(state.store.list_department_files(id).await?)))
}

pub async fn get_file(
    State(state): State<AppState>,
    _caller: Caller,
    Path((id, path)): Path<(DepartmentId, String)>,
) -> ApiResult<Json<Value>> {
    let path = file_path(&path).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    match state.store.read_department_file(id, &path).await? {
        Some(content) => Ok(Json(json!({
            "path": path,
            "content": String::from_utf8_lossy(&content),
        }))),
        None => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("no file `{path}`"),
        )),
    }
}

#[derive(Debug, Deserialize)]
pub struct FileBody {
    pub content: String,
}

pub async fn put_file(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, path)): Path<(DepartmentId, String)>,
    Json(body): Json<FileBody>,
) -> ApiResult<StatusCode> {
    caller.require_operator()?;
    let path = file_path(&path).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    state
        .store
        .write_department_file(id, &path, body.content.as_bytes(), &caller.name)
        .await?;
    notify(
        &state.store,
        json!({ "kind": "files", "departments": [id] }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Change notifications from every node. Clients refetch what changed.
pub async fn stream(
    State(state): State<AppState>,
    _caller: Caller,
) -> Sse<impl Stream<Item = Result<sse::Event, Infallible>>> {
    let events = BroadcastStream::new(state.org_events.subscribe()).map(|item| {
        Ok(match item {
            Ok(value) => sse::Event::default()
                .event("org")
                .json_data(value)
                .unwrap_or_else(|_| sse::Event::default().event("error")),
            // A slow client missed some: it refetches everything.
            Err(_) => sse::Event::default().event("lagged").data("{}"),
        })
    });
    Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Departments by id, for the reconciler.
pub fn by_id(departments: Vec<Department>) -> HashMap<DepartmentId, Department> {
    departments.into_iter().map(|d| (d.id, d)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_paths_are_normalised() {
        assert_eq!(file_path("/notes//plan.md").unwrap(), "notes/plan.md");
        assert_eq!(file_path("./a/./b").unwrap(), "a/b");
        assert!(file_path("../etc/passwd").is_err());
        assert!(file_path("a/../../b").is_err());
        assert!(file_path("  ").is_err());
        assert!(file_path(&"x".repeat(300)).is_err());
    }
}

// ---- building the organisation from templates ------------------------------------

pub async fn templates(State(state): State<AppState>, _caller: Caller) -> Json<Value> {
    let categories: Vec<Value> = crate::templates::CATEGORIES
        .iter()
        .map(|(id, title, description)| json!({ "id": id, "title": title, "description": description }))
        .collect();
    Json(json!({
        "categories": categories,
        "departments": state.templates.departments,
        "blueprints": state.templates.blueprints,
    }))
}

pub async fn suggestions(State(state): State<AppState>, _caller: Caller) -> ApiResult<Json<Value>> {
    let org = Org::load(&state.store).await?;
    let limits = state.store.org_settings().await?;
    Ok(Json(json!(crate::templates::suggest(
        &state.templates,
        &org.profile,
        &org.departments,
        &org.agents,
        limits,
    ))))
}

pub async fn get_profile(State(state): State<AppState>, _caller: Caller) -> ApiResult<Json<Value>> {
    Ok(Json(json!(state.store.org_profile().await?)))
}

fn check_blueprint(state: &AppState, profile: &OrgProfile) -> ApiResult<()> {
    match profile.blueprint.as_deref() {
        Some(id) if state.templates.blueprint(id).is_none() => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("unknown blueprint `{id}`"),
        )),
        _ => Ok(()),
    }
}

pub async fn put_profile(
    State(state): State<AppState>,
    caller: Caller,
    Json(profile): Json<OrgProfile>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    check_blueprint(&state, &profile)?;
    let saved = state.store.set_org_profile(&profile, &caller.name).await?;
    notify(&state.store, json!({ "kind": "settings" })).await;
    Ok(Json(json!(saved)))
}

#[derive(Debug, Deserialize)]
pub struct BuildRequest {
    /// Company and growth path; saved before anything is created.
    #[serde(default)]
    pub profile: Option<OrgProfile>,
    /// Department templates to create (existing departments are skipped).
    #[serde(default)]
    pub departments: Vec<String>,
    #[serde(default)]
    pub size: crate::templates::Size,
    /// Configured agent every new member runs (default: the first one).
    #[serde(default)]
    pub agent: Option<String>,
    /// Agent the communicators run (default: `agent`).
    #[serde(default)]
    pub communicator_agent: Option<String>,
    /// Start the new departments right away.
    #[serde(default)]
    pub start: bool,
    /// Project (GitHub repository) to link the new departments to: those
    /// whose template has a role on a repository (engineering, product,
    /// marketing, ...).
    #[serde(default)]
    pub project_id: Option<Uuid>,
    /// A first goal for the organisation (its title).
    #[serde(default)]
    pub goal: Option<String>,
    /// Keep the organisation working on its own: a daily check-in for each
    /// new department and a weekly review in Strategy.
    #[serde(default)]
    pub check_ins: bool,
    /// Raise the limits if the plan needs it (admins only).
    #[serde(default)]
    pub raise_limits: bool,
    /// Only return the plan.
    #[serde(default)]
    pub dry_run: bool,
}

/// Create departments (with their agents) from templates, in one step.
/// Check-ins that keep a new department working towards the goals: a daily
/// one for its lead, and a weekly review for Strategy.
pub fn default_check_ins(template: &str, lead: OrgAgentId) -> Vec<ScheduleInput> {
    if template == "retrospective" {
        return vec![ScheduleInput {
            name: "daily retrospective".into(),
            message: "Daily retrospective. Read the metrics and signals, find what costs the \
                      organisation the most time or money for the least result, and propose \
                      improvements (or revise the ones people sent back)."
                .into(),
            every_minutes: 24 * 60,
            agent_id: Some(lead),
            first_run_at: None,
        }];
    }
    let mut check_ins = vec![ScheduleInput {
        name: "daily".into(),
        message: "Daily check-in. Read the goals, your notes and new messages. Decide the most \
                  valuable next step towards the goals for your department, do it (or ask a \
                  colleague to), update your notes, and report progress on any goal that moved."
            .into(),
        every_minutes: 24 * 60,
        agent_id: Some(lead),
        first_run_at: None,
    }];
    if template == "strategy" {
        check_ins.push(ScheduleInput {
            name: "weekly review".into(),
            message: "Weekly review. Ask every department (through the communicator) what they \
                      did, what is blocked and what is next. Update the goals' progress, write \
                      the weekly summary, and list the decisions people need to make."
                .into(),
            every_minutes: 7 * 24 * 60,
            agent_id: Some(lead),
            first_run_at: None,
        });
    }
    check_ins
}

pub async fn build(
    State(state): State<AppState>,
    caller: Caller,
    Json(req): Json<BuildRequest>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let agent = match req.agent.clone() {
        Some(a) => a,
        None => state
            .manager
            .agents()
            .into_iter()
            .map(|a| a.name)
            .min()
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "no agents are configured"))?,
    };
    check_agent_spec(&state, &agent)?;
    let communicator = req
        .communicator_agent
        .clone()
        .unwrap_or_else(|| agent.clone());
    check_agent_spec(&state, &communicator)?;
    let mut profile = state.store.org_profile().await?;
    if let Some(p) = &req.profile {
        check_blueprint(&state, p)?;
        profile = p.clone();
    }
    if let Some(project) = req.project_id {
        state.project(project).await?;
    }
    let existing = state.store.list_departments().await?;
    let limits = state.store.org_settings().await?;
    let plan = crate::templates::plan(
        &state.templates,
        &req.departments,
        req.size,
        &profile,
        &existing,
        limits,
    )
    .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    if req.dry_run {
        return Ok(Json(json!({ "plan": plan })));
    }
    if !plan.fits && !req.raise_limits {
        return Err(ApiError::with_body(
            StatusCode::CONFLICT,
            json!({
                "error": format!(
                    "this needs room for {} departments and {} agents per department; the \
                     limits are {} and {}",
                    plan.needs.max_departments,
                    plan.needs.max_agents_per_department,
                    limits.max_departments,
                    limits.max_agents_per_department,
                ),
                "plan": plan,
            }),
        ));
    }
    if req.profile.is_some() {
        state.store.set_org_profile(&profile, &caller.name).await?;
    }
    if !plan.fits {
        state
            .store
            .set_org_settings(plan.needs, &caller.name)
            .await?;
    }

    let mut created = Vec::new();
    let mut errors = Vec::new();
    for planned in plan.departments.iter().filter(|d| !d.exists) {
        let mut input = DepartmentInput {
            name: planned.name.clone(),
            description: planned.description.clone(),
            mission: planned.mission.clone(),
            policy: planned.policy.clone().unwrap_or_default(),
            tools: planned.tools.clone(),
            communicator_agent: communicator.clone(),
            template: Some(planned.template.clone()),
            project_id: req.project_id.filter(|_| planned.role.is_some()),
            role: planned.role.clone().filter(|_| req.project_id.is_some()),
        };
        check_department(&state, &mut input).await?;
        let dept = match state.store.create_department(input, &caller.name).await {
            Ok(d) => d,
            Err(err) => {
                errors.push(json!({ "department": planned.name, "error": err.to_string() }));
                continue;
            }
        };
        let mut lead = None;
        for member in &planned.agents {
            match state
                .store
                .add_agent(
                    dept.id,
                    AgentInput {
                        name: member.name.clone(),
                        agent: agent.clone(),
                        instructions: member.instructions.clone(),
                    },
                    &caller.name,
                )
                .await
            {
                Ok(a) => {
                    lead.get_or_insert(a.id);
                }
                Err(err) => errors.push(json!({
                    "department": planned.name, "agent": member.name, "error": err.to_string(),
                })),
            }
        }
        if req.check_ins
            && let Some(lead) = lead
        {
            for input in default_check_ins(&planned.template, lead) {
                if let Err(err) = state
                    .store
                    .create_schedule(dept.id, input, &caller.name)
                    .await
                {
                    errors.push(json!({ "department": planned.name, "error": err.to_string() }));
                }
            }
        }
        created.push(dept);
    }
    let mut goal = None;
    if let Some(title) = req.goal.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        let input = GoalInput {
            title: title.to_string(),
            ..Default::default()
        };
        match state.store.create_goal(input, &caller.name).await {
            Ok(g) => goal = Some(g),
            Err(err) => errors.push(json!({ "goal": title, "error": err.to_string() })),
        }
    }
    let mut start_failed = Vec::new();
    if req.start {
        for dept in &created {
            let (_, failed) = control_agents(
                &state,
                AgentScope::Department(dept.id),
                Control::Start,
                &caller.name,
            )
            .await?;
            start_failed.extend(failed);
        }
    }
    notify(&state.store, json!({ "kind": "department" })).await;
    Ok(Json(json!({
        "plan": plan,
        "created": created,
        "errors": errors,
        "start_failed": start_failed,
        "goal": goal,
        "profile": profile,
        "settings": state.store.org_settings().await?,
    })))
}
