//! Team work: GitHub tools for agents, repository checkout, starting agents
//! on issues, delivery as pull requests, and the related API.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentcore_core::{IssueRef, SessionContext, SessionId, SessionStatus};
use agentcore_roles::{DeliveryKind, IssueContext, Role, Stage, WorkItem, tools_for};
use agentcore_runtime::{
    CreateSession, PreparedWorkspace, Session, SessionOptions, ToolHandler, WorkspaceSetup,
};
use agentcore_store::{BoardConfig, GitHubConfig, GitHubUpdate, Project, ProjectInput};
use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::AppState;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::github::{GhError, GitHub};
use crate::sessions::SessionRef;

type ApiResult<T> = Result<T, ApiError>;

/// Appended to anything an agent publishes (EU AI Act Art. 50).
const AI_FOOTER: &str = "\n\n<sub>🤖 Written by an AI agent working in agentcore.</sub>";

impl From<GhError> for ApiError {
    fn from(err: GhError) -> Self {
        let status = match &err {
            GhError::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::BAD_GATEWAY,
        };
        ApiError::new(status, err.to_string())
    }
}

impl AppState {
    /// GitHub client from the stored connection, if one is configured.
    pub async fn github(&self) -> ApiResult<Option<(GitHub, GitHubConfig, String)>> {
        Ok(self
            .store
            .github_credentials()
            .await?
            .map(|(config, token)| {
                (
                    GitHub::new(self.http.clone(), &config, token.clone()),
                    config,
                    token,
                )
            }))
    }

    async fn require_github(&self) -> ApiResult<(GitHub, GitHubConfig, String)> {
        self.github().await?.ok_or_else(|| {
            ApiError::new(
                StatusCode::PRECONDITION_FAILED,
                "GitHub is not connected: an admin can connect it under Settings",
            )
        })
    }

    async fn project(&self, id: Uuid) -> ApiResult<Project> {
        self.store
            .get_project(id)
            .await?
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("project {id} not found")))
    }

    fn role(&self, name: &str) -> ApiResult<Arc<Role>> {
        self.roles
            .get(name)
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, format!("unknown role `{name}`")))
    }

    fn mirror_path(&self, session: SessionId) -> PathBuf {
        self.config
            .storage
            .data_dir
            .join("repos")
            .join(format!("{session}.git"))
    }
}

// ---- GitHub tools for agents ---------------------------------------------------

fn def(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
    })
}

fn all_tool_definitions() -> Vec<Value> {
    let num = json!({ "type": "integer" });
    let text = json!({ "type": "string" });
    let strings = json!({ "type": "array", "items": { "type": "string" } });
    vec![
        def(
            "github_list_issues",
            "List issues of this project's repository.",
            json!({ "state": { "type": "string", "enum": ["open", "closed", "all"] }, "labels": { "type": "string", "description": "Comma-separated label names" }, "milestone": { "type": "string", "description": "Milestone number, `*` or `none`" }, "limit": num }),
            &[],
        ),
        def(
            "github_get_issue",
            "Read an issue with its comments.",
            json!({ "number": num }),
            &["number"],
        ),
        def(
            "github_comment_on_issue",
            "Comment on an issue. Visible to the whole team.",
            json!({ "number": num, "body": text }),
            &["number", "body"],
        ),
        def(
            "github_create_issue",
            "Create an issue.",
            json!({ "title": text, "body": text, "labels": strings, "milestone": num, "assignees": strings }),
            &["title"],
        ),
        def(
            "github_update_issue",
            "Update an issue: title, body, state (open/closed), labels, milestone, assignees. Omitted fields are unchanged.",
            json!({ "number": num, "title": text, "body": text, "state": { "type": "string", "enum": ["open", "closed"] }, "labels": strings, "milestone": { "type": ["integer", "null"] }, "assignees": strings }),
            &["number"],
        ),
        def(
            "github_list_milestones",
            "List milestones.",
            json!({ "state": { "type": "string", "enum": ["open", "closed", "all"] } }),
            &[],
        ),
        def(
            "github_create_milestone",
            "Create a milestone.",
            json!({ "title": text, "description": text, "due_on": { "type": "string", "description": "ISO 8601 date" } }),
            &["title"],
        ),
        def(
            "github_update_milestone",
            "Update a milestone.",
            json!({ "number": num, "title": text, "description": text, "due_on": text, "state": { "type": "string", "enum": ["open", "closed"] } }),
            &["number"],
        ),
        def(
            "github_list_board_items",
            "List cards on the project board (kanban), with their column and sprint.",
            json!({ "status": { "type": "string", "description": "Only cards in this column" }, "iteration": { "type": "string", "description": "`current` for the current sprint, or an iteration title" } }),
            &[],
        ),
        def(
            "github_set_board_status",
            "Move an issue's card to another column on the project board.",
            json!({ "issue_number": num, "status": text }),
            &["issue_number", "status"],
        ),
        def(
            "github_list_discussions",
            "List recent GitHub Discussions.",
            json!({ "limit": num }),
            &[],
        ),
        def(
            "github_get_discussion",
            "Read a discussion with its comments.",
            json!({ "number": num }),
            &["number"],
        ),
        def(
            "github_comment_on_discussion",
            "Reply in a discussion. Public.",
            json!({ "number": num, "body": text }),
            &["number", "body"],
        ),
        def(
            "propose_pull_request",
            "Propose the pull request for your committed work: title and description written the way this team writes pull requests. A human reviews and delivers it.",
            json!({ "title": text, "body": text }),
            &["title", "body"],
        ),
    ]
}

/// GitHub tools bound to one project and filtered by the role's capabilities.
pub struct GitHubTools {
    gh: Option<GitHub>,
    owner: String,
    repo: String,
    board: Option<BoardConfig>,
    allowed: HashSet<&'static str>,
}

impl GitHubTools {
    fn new(gh: Option<GitHub>, project: &Project, role: &Role) -> Self {
        let allowed = role
            .capabilities
            .iter()
            .flat_map(|c| tools_for(*c).iter().copied())
            .collect();
        Self {
            gh,
            owner: project.repo_owner.clone(),
            repo: project.repo_name.clone(),
            board: project.board.clone(),
            allowed,
        }
    }
}

fn n(args: &Value, key: &str) -> Result<u64, String> {
    args[key]
        .as_u64()
        .ok_or_else(|| format!("`{key}` must be a number"))
}

fn text(args: &Value, key: &str) -> Result<String, String> {
    args[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| format!("`{key}` is required"))
}

/// Keep only the given keys that are present.
fn pick(args: &Value, keys: &[&str]) -> Value {
    let mut out = serde_json::Map::new();
    for k in keys {
        if let Some(v) = args.get(*k) {
            out.insert((*k).into(), v.clone());
        }
    }
    Value::Object(out)
}

#[async_trait]
impl ToolHandler for GitHubTools {
    fn definitions(&self) -> Vec<Value> {
        all_tool_definitions()
            .into_iter()
            .filter(|d| d["name"].as_str().is_some_and(|n| self.allowed.contains(n)))
            .filter(|d| {
                // Board tools only when the project has a board.
                self.board.is_some() || !d["name"].as_str().unwrap_or_default().contains("board")
            })
            .collect()
    }

    async fn call(&self, tool: &str, args: &Value) -> Result<Value, String> {
        if !self.allowed.contains(tool) {
            return Err(format!("`{tool}` is not available in this role"));
        }
        let gh = self.gh.as_ref().ok_or("GitHub is not connected")?;
        let (o, r) = (self.owner.as_str(), self.repo.as_str());
        let board = || {
            self.board
                .as_ref()
                .ok_or_else(|| "this project has no board".to_string())
        };
        let result = match tool {
            "github_list_issues" => gh.list_issues(o, r, args).await,
            "github_get_issue" => gh
                .get_issue(o, r, n(args, "number")?)
                .await
                .map(|i| json!(i)),
            "github_comment_on_issue" => {
                let body = text(args, "body")? + AI_FOOTER;
                gh.comment_on_issue(o, r, n(args, "number")?, &body)
                    .await
                    .map(|url| json!({ "url": url }))
            }
            "github_create_issue" => {
                let mut fields = pick(args, &["title", "body", "labels", "milestone", "assignees"]);
                text(args, "title")?;
                fields["body"] = json!(format!(
                    "{}{AI_FOOTER}",
                    args["body"].as_str().unwrap_or_default()
                ));
                gh.create_issue(o, r, fields).await
            }
            "github_update_issue" => {
                let fields = pick(
                    args,
                    &["title", "body", "state", "labels", "milestone", "assignees"],
                );
                gh.update_issue(o, r, n(args, "number")?, fields).await
            }
            "github_list_milestones" => {
                gh.list_milestones(o, r, args["state"].as_str().unwrap_or("open"))
                    .await
            }
            "github_create_milestone" => {
                text(args, "title")?;
                gh.create_milestone(o, r, pick(args, &["title", "description", "due_on"]))
                    .await
            }
            "github_update_milestone" => {
                gh.update_milestone(
                    o,
                    r,
                    n(args, "number")?,
                    pick(args, &["title", "description", "due_on", "state"]),
                )
                .await
            }
            "github_list_board_items" => {
                let cfg = board()?;
                gh.board(cfg).await.map(|b| {
                    let status = args["status"].as_str();
                    let iteration = match args["iteration"].as_str() {
                        Some("current") => b.current_iteration.clone(),
                        other => other.map(String::from),
                    };
                    let items: Vec<Value> = b
                        .items
                        .iter()
                        .filter(|i| status.is_none_or(|s| i.status.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(s))))
                        .filter(|i| iteration.as_ref().is_none_or(|it| i.iteration.as_ref() == Some(it)))
                        .map(|i| json!({
                            "number": i.number, "title": i.title, "type": i.content_type, "status": i.status,
                            "sprint": i.iteration, "labels": i.labels, "assignees": i.assignees,
                            "milestone": i.milestone, "repository": i.repository, "url": i.url,
                        }))
                        .collect();
                    json!({ "board": b.title, "columns": b.columns, "current_sprint": b.current_iteration, "items": items })
                })
            }
            "github_set_board_status" => {
                let cfg = board()?;
                let issue = n(args, "issue_number")?;
                let status = text(args, "status")?;
                match gh
                    .move_issue(cfg, &format!("{o}/{r}"), issue, &status)
                    .await
                {
                    Ok(true) => Ok(json!({ "moved": true, "issue": issue, "status": status })),
                    Ok(false) => return Err(format!("issue #{issue} is not on the board")),
                    Err(e) => Err(e),
                }
            }
            "github_list_discussions" => {
                gh.list_discussions(o, r, args["limit"].as_u64().unwrap_or(20))
                    .await
            }
            "github_get_discussion" => gh.get_discussion(o, r, n(args, "number")?).await,
            "github_comment_on_discussion" => {
                let body = text(args, "body")? + AI_FOOTER;
                gh.comment_on_discussion(o, r, n(args, "number")?, &body)
                    .await
                    .map(|url| json!({ "url": url }))
            }
            other => return Err(format!("unknown tool `{other}`")),
        };
        result.map_err(|e| e.to_string())
    }
}

// ---- git on the host ---------------------------------------------------------------

/// Run git with hooks and fsmonitor disabled and no credential helpers. With
/// `auth`, the token is passed as an HTTP header via environment variables,
/// so it is neither on the command line nor written to any config file.
async fn git(
    args: &[&str],
    cwd: Option<&Path>,
    auth: Option<(&GitHubConfig, &str)>,
) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "credential.helper=",
    ])
    .args(args)
    .env("GIT_TERMINAL_PROMPT", "0")
    .stdin(std::process::Stdio::null())
    .kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut secret = None;
    if let Some((cfg, token)) = auth {
        let basic =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        cmd.env("GIT_CONFIG_COUNT", "1")
            .env(
                "GIT_CONFIG_KEY_0",
                format!("http.{}/.extraheader", cfg.web_url.trim_end_matches('/')),
            )
            .env(
                "GIT_CONFIG_VALUE_0",
                format!("AUTHORIZATION: basic {basic}"),
            );
        secret = Some((token.to_string(), basic));
    }
    let out = tokio::time::timeout(std::time::Duration::from_secs(600), cmd.output())
        .await
        .map_err(|_| "git timed out".to_string())?
        .map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let mut err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if let Some((token, basic)) = secret {
        err = err.replace(&token, "***").replace(&basic, "***");
    }
    Err(format!("git {}: {err}", args.first().unwrap_or(&"")))
}

fn clone_url(cfg: &GitHubConfig, owner: &str, repo: &str) -> String {
    format!("{}/{owner}/{repo}.git", cfg.web_url.trim_end_matches('/'))
}

/// Clones the project's repository: first a bare mirror that only agentcore
/// touches (used later to push), then the agent's workspace from it.
struct GitRepoSetup {
    config: GitHubConfig,
    token: String,
    owner: String,
    repo: String,
    branch: String,
    work_branch: String,
    mirror: PathBuf,
}

#[async_trait]
impl WorkspaceSetup for GitRepoSetup {
    async fn prepare(&self, dir: &Path) -> Result<PreparedWorkspace, String> {
        let url = clone_url(&self.config, &self.owner, &self.repo);
        if let Some(parent) = self.mirror.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| e.to_string())?;
        }
        let _ = tokio::fs::remove_dir_all(&self.mirror).await;
        let mirror = self.mirror.to_string_lossy().into_owned();
        git(
            &[
                "clone",
                "--quiet",
                "--bare",
                "--depth",
                "200",
                "--single-branch",
                "--branch",
                &self.branch,
                &url,
                &mirror,
            ],
            None,
            Some((&self.config, &self.token)),
        )
        .await?;
        let workspace = dir.to_string_lossy().into_owned();
        git(
            &[
                "clone",
                "--quiet",
                "--no-hardlinks",
                "--branch",
                &self.branch,
                &format!("file://{mirror}"),
                &workspace,
            ],
            None,
            None,
        )
        .await?;
        git(&["remote", "set-url", "origin", &url], Some(dir), None).await?;
        git(
            &["config", "user.name", &self.config.commit_name],
            Some(dir),
            None,
        )
        .await?;
        git(
            &["config", "user.email", &self.config.commit_email],
            Some(dir),
            None,
        )
        .await?;
        git(
            &["checkout", "--quiet", "-b", &self.work_branch],
            Some(dir),
            None,
        )
        .await?;
        let base = git(&["rev-parse", "HEAD"], Some(dir), None).await?;
        Ok(PreparedWorkspace {
            repository: format!("{}/{}", self.owner, self.repo),
            branch: self.branch.clone(),
            base_commit: base,
        })
    }
}

fn stage_column(cfg: &BoardConfig, stage: Stage) -> &str {
    match stage {
        Stage::Ready => &cfg.columns.ready,
        Stage::InProgress => &cfg.columns.in_progress,
        Stage::InReview => &cfg.columns.in_review,
        Stage::Done => &cfg.columns.done,
    }
}

/// Move the issue on the board and/or comment, without failing the caller.
async fn announce(
    gh: &GitHub,
    project: &Project,
    role: &Role,
    issue: Option<u64>,
    stage: Option<Stage>,
    comment: Option<String>,
) {
    let Some(issue) = issue else { return };
    if let (Some(stage), Some(board)) = (stage, &project.board) {
        let column = stage_column(board, stage);
        match gh
            .move_issue(board, &project.repository(), issue, column)
            .await
        {
            Ok(true) => {}
            Ok(false) => tracing::info!(issue, "issue is not on the project board"),
            Err(err) => tracing::warn!(issue, error = %err, "could not move the board card"),
        }
    }
    if role.workflow.announce_on_issue
        && let Some(body) = comment
        && let Err(err) = gh
            .comment_on_issue(&project.repo_owner, &project.repo_name, issue, &body)
            .await
    {
        tracing::warn!(issue, error = %err, "could not comment on the issue");
    }
}

// ---- starting work -----------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct StartRequest {
    #[serde(default)]
    pub issue_number: Option<u64>,
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub policy: Option<String>,
}

pub async fn start_session(
    state: &AppState,
    project: &Project,
    req: StartRequest,
    caller: &Caller,
) -> ApiResult<Arc<Session>> {
    let role = state.role(req.role.as_deref().unwrap_or(&project.role))?;
    let (gh, config, token) = state.require_github().await?;
    let issue = match req.issue_number {
        Some(n) => Some(
            gh.get_issue(&project.repo_owner, &project.repo_name, n)
                .await?,
        ),
        None => None,
    };
    let task = match (&issue, req.task.trim()) {
        (_, t) if !t.is_empty() => t.to_string(),
        (Some(i), _) => format!("Work on issue #{}: {}", i.number, i.title),
        (None, _) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "describe the task or pick an issue",
            ));
        }
    };
    let id_hint = Uuid::now_v7().to_string();
    let work_branch = role.branch_name(
        issue.as_ref().map(|i| i.number),
        issue.as_ref().map_or(&task, |i| &i.title),
        &id_hint,
    );
    let context = SessionContext {
        role: Some(role.name.clone()),
        project_id: Some(project.id),
        project_name: Some(project.name.clone()),
        repository: Some(project.repository()),
        base_branch: Some(project.default_branch.clone()),
        issue: issue.as_ref().map(|i| IssueRef {
            number: i.number,
            title: i.title.clone(),
            url: i.url.clone(),
        }),
        work_branch: Some(work_branch.clone()),
        ..Default::default()
    };
    let work_item = WorkItem {
        repository: Some(project.repository()),
        branch: Some(project.default_branch.clone()),
        issue: issue.as_ref().map(|i| IssueContext {
            number: i.number,
            title: i.title.clone(),
            url: i.url.clone(),
            body: i.body.clone(),
            labels: i.labels.clone(),
            milestone: i.milestone.clone(),
            comments: i
                .comments
                .iter()
                .map(|c| (c.author.clone(), c.body.clone()))
                .collect(),
        }),
        task: if req.task.trim().is_empty() {
            String::new()
        } else {
            req.task.trim().to_string()
        },
        project_notes: project.notes.clone(),
    };
    let models = state.store.enabled_endpoints().await?;
    let tools = GitHubTools::new(Some(gh.clone()), project, &role);
    // The mirror path is per session; the session id is only known after
    // creation, so the setup reads it lazily through a shared cell.
    let mirror_cell: Arc<std::sync::OnceLock<PathBuf>> = Arc::default();
    let setup = LazyMirrorSetup {
        cell: mirror_cell.clone(),
        inner: GitRepoSetup {
            config: config.clone(),
            token: token.clone(),
            owner: project.repo_owner.clone(),
            repo: project.repo_name.clone(),
            branch: project.default_branch.clone(),
            work_branch,
            mirror: PathBuf::new(),
        },
    };
    let session = state.manager.create(
        CreateSession {
            agent: req.agent.unwrap_or_else(|| project.agent.clone()),
            task,
            policy: req.policy,
        },
        caller.principal(),
        SessionOptions {
            models,
            role: Some(role.clone()),
            workspace: Some(Arc::new(setup)),
            tools: Some(Arc::new(tools)),
            work_item: Some(work_item),
            context,
            ..Default::default()
        },
    )?;
    let _ = mirror_cell.set(state.mirror_path(session.id()));
    state.persist(&session).await;
    state.follow(session.clone());

    let link = state
        .config
        .server
        .public_url
        .as_ref()
        .map(|u| {
            format!(
                " ([session]({}/#{}))",
                u.trim_end_matches('/'),
                session.id()
            )
        })
        .unwrap_or_default();
    let comment = format!(
        "🤖 An AI agent ({} role, agentcore) started working on this issue, supervised by {}{link}.",
        role.display_title(),
        caller.name
    );
    let project = project.clone();
    let issue_number = issue.map(|i| i.number);
    tokio::spawn(async move {
        announce(
            &gh,
            &project,
            &role,
            issue_number,
            role.workflow.on_start,
            Some(comment),
        )
        .await;
    });
    Ok(session)
}

/// Wraps [`GitRepoSetup`] so the mirror path can be set after the session id
/// exists (the workspace is prepared asynchronously, after creation).
struct LazyMirrorSetup {
    cell: Arc<std::sync::OnceLock<PathBuf>>,
    inner: GitRepoSetup,
}

#[async_trait]
impl WorkspaceSetup for LazyMirrorSetup {
    async fn prepare(&self, dir: &Path) -> Result<PreparedWorkspace, String> {
        // Wait (briefly) until the creator stored the path.
        for _ in 0..100 {
            if let Some(mirror) = self.cell.get() {
                let setup = GitRepoSetup {
                    mirror: mirror.clone(),
                    ..self.inner.clone_shallow()
                };
                return setup.prepare(dir).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Err("internal error: mirror path not set".into())
    }
}

impl GitRepoSetup {
    fn clone_shallow(&self) -> Self {
        Self {
            config: self.config.clone(),
            token: self.token.clone(),
            owner: self.owner.clone(),
            repo: self.repo.clone(),
            branch: self.branch.clone(),
            work_branch: self.work_branch.clone(),
            mirror: self.mirror.clone(),
        }
    }
}

// ---- delivery ------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct DeliverRequest {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub draft: Option<bool>,
}

async fn deliver(
    state: &AppState,
    session: &Session,
    caller: &Caller,
    req: DeliverRequest,
) -> ApiResult<Value> {
    let ctx = session.context();
    let project_id = ctx.project_id.ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "only project sessions can be delivered",
        )
    })?;
    let project = state.project(project_id).await?;
    let role = session
        .role()
        .cloned()
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "session has no role"))?;
    if role.delivery.kind != DeliveryKind::PullRequest {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("the {} role does not deliver pull requests", role.name),
        ));
    }
    let branch = ctx
        .work_branch
        .clone()
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "session has no work branch"))?;
    let proposal = session.proposal();
    let title = req
        .title
        .filter(|t| !t.trim().is_empty())
        .or_else(|| proposal.as_ref().map(|p| p.title.clone()))
        .ok_or_else(|| {
            ApiError::new(StatusCode::BAD_REQUEST, "a pull request title is required")
        })?;
    let body = req
        .body
        .or_else(|| proposal.map(|p| p.body))
        .unwrap_or_default();

    // 1. The team's checks.
    let (passed, results) = session.run_checks(caller.principal()).await?;
    if !passed {
        return Err(ApiError::with_body(
            StatusCode::CONFLICT,
            json!({ "error": "checks failed: ask the agent to fix them, then deliver again", "checks": results }),
        ));
    }
    let (gh, config, token) = state.require_github().await?;

    // 2. Commits leave the sandbox as a bundle and land in agentcore's mirror.
    let (bundle, head) = session.export_bundle().await?;
    let mirror = state.mirror_path(session.id());
    let bundle_path = mirror.with_extension("bundle");
    tokio::fs::write(&bundle_path, &bundle)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mirror_s = mirror.to_string_lossy().into_owned();
    let refspec = format!("+HEAD:refs/heads/{branch}");
    let fetched = git(
        &[
            "--git-dir",
            &mirror_s,
            "fetch",
            "--quiet",
            "--no-tags",
            &bundle_path.to_string_lossy(),
            &refspec,
        ],
        None,
        None,
    )
    .await;
    let _ = tokio::fs::remove_file(&bundle_path).await;
    fetched.map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let fetched_head = git(
        &[
            "--git-dir",
            &mirror_s,
            "rev-parse",
            &format!("refs/heads/{branch}"),
        ],
        None,
        None,
    )
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if fetched_head != head {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "bundle head mismatch",
        ));
    }

    // 3. Push the agent's branch (and only it).
    let url = clone_url(&config, &project.repo_owner, &project.repo_name);
    git(
        &[
            "--git-dir",
            &mirror_s,
            "push",
            "--quiet",
            &url,
            &format!("+refs/heads/{branch}:refs/heads/{branch}"),
        ],
        None,
        Some((&config, &token)),
    )
    .await
    .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e))?;

    // 4. Pull request with issue link and AI disclosure.
    let mut full_body = body.trim().to_string();
    if let Some(issue) = &ctx.issue
        && !full_body.contains(&format!("#{}", issue.number))
    {
        full_body.push_str(&format!("\n\nCloses #{}", issue.number));
    }
    full_body.push_str(&format!(
        "\n\n---\n🤖 Prepared by an AI agent in agentcore (role: {}, session `{}`) and delivered by {} after review.",
        role.display_title(),
        session.id(),
        caller.name
    ));
    let base = ctx
        .base_branch
        .clone()
        .unwrap_or_else(|| project.default_branch.clone());
    let (pr_number, pr_url) = gh
        .upsert_pull_request(&crate::github::PullRequest {
            owner: &project.repo_owner,
            repo: &project.repo_name,
            branch: &branch,
            base: &base,
            title: title.trim(),
            body: &full_body,
            draft: req.draft.unwrap_or(role.delivery.draft),
        })
        .await?;

    // 5. Board and issue.
    announce(
        &gh,
        &project,
        &role,
        ctx.issue.as_ref().map(|i| i.number),
        role.workflow.on_deliver,
        Some(format!("🤖 Pull request #{pr_number} is ready for review.")),
    )
    .await;
    session.record_delivery(
        caller.principal(),
        branch.clone(),
        head.clone(),
        Some(pr_url.clone()),
    )?;
    state.persist(session).await;
    Ok(
        json!({ "branch": branch, "commit": head, "pull_request": { "number": pr_number, "url": pr_url }, "checks": results }),
    )
}

// ---- API: roles, GitHub connection, projects ----------------------------------------

pub async fn list_roles(State(state): State<AppState>, _caller: Caller) -> Json<Value> {
    let roles: Vec<_> = state
        .roles
        .iter()
        .map(|r| {
            let mut v = json!(r.as_ref());
            v["digest"] = json!(r.digest());
            v["tools"] = json!(
                r.capabilities
                    .iter()
                    .flat_map(|c| tools_for(*c).iter().copied())
                    .collect::<Vec<_>>()
            );
            v
        })
        .collect();
    Json(json!(roles))
}

pub async fn get_github(State(state): State<AppState>, caller: Caller) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    Ok(Json(json!(state.store.github_connection().await?)))
}

pub async fn put_github(
    State(state): State<AppState>,
    caller: Caller,
    Json(update): Json<GitHubUpdate>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    Ok(Json(json!(
        state.store.set_github(update, &caller.name).await?
    )))
}

pub async fn delete_github(State(state): State<AppState>, caller: Caller) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    state.store.delete_github(&caller.name).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn test_github(State(state): State<AppState>, caller: Caller) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let (gh, _, _) = state.require_github().await?;
    Ok(Json(json!({ "login": gh.viewer().await? })))
}

pub async fn list_projects(
    State(state): State<AppState>,
    _caller: Caller,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(state.store.list_projects().await?)))
}

fn validate_project(state: &AppState, input: &ProjectInput) -> ApiResult<()> {
    state.role(&input.role)?;
    if !state.manager.agents().any(|a| a.name == input.agent) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("unknown agent `{}`", input.agent),
        ));
    }
    Ok(())
}

pub async fn create_project(
    State(state): State<AppState>,
    caller: Caller,
    Json(input): Json<ProjectInput>,
) -> ApiResult<impl IntoResponse> {
    caller.require_admin()?;
    validate_project(&state, &input)?;
    Ok((
        StatusCode::CREATED,
        Json(json!(
            state.store.save_project(None, input, &caller.name).await?
        )),
    ))
}

pub async fn update_project(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<Uuid>,
    Json(input): Json<ProjectInput>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    validate_project(&state, &input)?;
    Ok(Json(json!(
        state
            .store
            .save_project(Some(id), input, &caller.name)
            .await?
    )))
}

pub async fn delete_project(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<Uuid>,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    state.store.delete_project(id, &caller.name).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn project_board(
    State(state): State<AppState>,
    _caller: Caller,
    UrlPath(id): UrlPath<Uuid>,
) -> ApiResult<Json<Value>> {
    let project = state.project(id).await?;
    let board = project
        .board
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "this project has no board"))?;
    let (gh, _, _) = state.require_github().await?;
    let board_data = gh.board(board).await?;
    Ok(Json(json!({ "board": board_data, "config": board })))
}

#[derive(Debug, Deserialize)]
pub struct IssuesQuery {
    #[serde(default)]
    pub state: Option<String>,
}

pub async fn project_issues(
    State(state): State<AppState>,
    _caller: Caller,
    UrlPath(id): UrlPath<Uuid>,
    Query(q): Query<IssuesQuery>,
) -> ApiResult<Json<Value>> {
    let project = state.project(id).await?;
    let (gh, _, _) = state.require_github().await?;
    let args = json!({ "state": q.state.unwrap_or_else(|| "open".into()), "limit": 100 });
    Ok(Json(
        gh.list_issues(&project.repo_owner, &project.repo_name, &args)
            .await?,
    ))
}

pub async fn start_project_session(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<Uuid>,
    Json(req): Json<StartRequest>,
) -> ApiResult<impl IntoResponse> {
    caller.require_operator()?;
    let project = state.project(id).await?;
    let session = start_session(&state, &project, req, &caller).await?;
    Ok((StatusCode::CREATED, Json(json!(session.info()))))
}

// ---- API: conversation, changes, checks, delivery --------------------------------

#[derive(Debug, Deserialize)]
pub struct MessageRequest {
    pub text: String,
}

pub async fn send_message(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<SessionId>,
    Json(req): Json<MessageRequest>,
) -> ApiResult<StatusCode> {
    caller.require_operator()?;
    if req.text.trim().is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "message must not be empty",
        ));
    }
    state
        .lookup(id)
        .await?
        .live()?
        .send_message(caller.principal(), req.text.trim().to_string())?;
    Ok(StatusCode::ACCEPTED)
}

pub async fn finish_session(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<SessionId>,
) -> ApiResult<StatusCode> {
    caller.require_operator()?;
    state.lookup(id).await?.live()?.finish(caller.principal())?;
    Ok(StatusCode::ACCEPTED)
}

pub async fn changes(
    State(state): State<AppState>,
    _caller: Caller,
    UrlPath(id): UrlPath<SessionId>,
) -> ApiResult<Json<Value>> {
    let changes = match state.lookup(id).await? {
        SessionRef::Live(s) if s.status() == SessionStatus::AwaitingInput => s.changes().await?,
        SessionRef::Live(s) => s.last_changes(),
        SessionRef::Archived(_) => state.store.get_changes(id).await?,
    };
    Ok(Json(json!(changes)))
}

pub async fn run_checks(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<SessionId>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let session = state.lookup(id).await?.live()?;
    let (passed, results) = session.run_checks(caller.principal()).await?;
    Ok(Json(json!({ "passed": passed, "checks": results })))
}

pub async fn deliver_session(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<SessionId>,
    body: Option<Json<DeliverRequest>>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let session = state.lookup(id).await?.live()?;
    {
        let mut busy = state.delivering.lock().await;
        if !busy.insert(id) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "a delivery is already running for this session",
            ));
        }
    }
    let result = deliver(
        &state,
        &session,
        &caller,
        body.map(|b| b.0).unwrap_or_default(),
    )
    .await;
    state.delivering.lock().await.remove(&id);
    Ok(Json(result?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_follow_role_capabilities() {
        let roles = agentcore_roles::RoleSet::load_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../roles"),
        )
        .unwrap();
        let project = Project {
            id: Uuid::nil(),
            name: "p".into(),
            repo_owner: "acme".into(),
            repo_name: "api".into(),
            default_branch: "main".into(),
            agent: "opencode".into(),
            role: "developer".into(),
            board: None,
            notes: String::new(),
            updated_at: chrono::Utc::now(),
            updated_by: "x".into(),
        };
        let names = |role: &str| -> Vec<String> {
            GitHubTools::new(None, &project, &roles.get(role).unwrap())
                .definitions()
                .iter()
                .map(|d| d["name"].as_str().unwrap().to_string())
                .collect()
        };
        let dev = names("developer");
        assert!(dev.contains(&"propose_pull_request".to_string()));
        assert!(!dev.contains(&"github_update_issue".to_string()));
        assert!(
            !dev.iter().any(|n| n.contains("board")),
            "no board configured"
        );
        let pm = names("project-manager");
        assert!(pm.contains(&"github_create_milestone".to_string()));
        assert!(!pm.contains(&"propose_pull_request".to_string()));
        let marketing = names("marketing");
        assert!(marketing.contains(&"github_comment_on_discussion".to_string()));
        assert!(!marketing.contains(&"github_create_issue".to_string()));
    }
}
