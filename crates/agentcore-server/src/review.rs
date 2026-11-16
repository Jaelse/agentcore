//! Reviewing a delivered pull request together with the agent.
//!
//! A person starts a review from a session whose pull request has a
//! preview. The agent then uses the preview in a headless browser that
//! agentcore runs (`browser_*` tools), shows each step to the reviewer
//! (`review_show`: a screenshot and a sentence in the conversation), and
//! tries whatever the reviewer asks. The reviewer approves or asks for
//! changes; after changes, the agent fixes them, delivers again, and once
//! the new preview is ready it shows the reviewer what changed.
//!
//! A review belongs to the pull request's branch (project + branch), not to
//! one session: a follow-up session on the same branch carries it on.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use agentcore_core::{
    Event, EventKind, Principal, ReviewDevice, ReviewInfo, ReviewOutcome, ReviewStep,
    ReviewVerdict, SessionId,
};
use agentcore_runtime::{Session, ToolHandler};
use agentcore_store::{PreviewAccessUpdate, Project};
use async_trait::async_trait;
use axum::Json;
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::AppState;
use crate::auth::Caller;
use crate::browser::{Access, Page, find_chromium, sandbox_allowed};
use crate::error::ApiError;
use crate::sessions::SessionRef;

type ApiResult<T> = Result<T, ApiError>;

const NO_REVIEW: &str = "No one is reviewing this pull request with you right now. The browser \
     and review tools only work while a person reviews with you (they start it from the session \
     page).";

// ---- open reviews -------------------------------------------------------------------

/// The reviews open on this node, by project and branch.
pub struct Reviews {
    open: Mutex<HashMap<(Uuid, String), Arc<Review>>>,
    browsers: Arc<Semaphore>,
}

impl Reviews {
    pub fn new(max_browsers: usize) -> Self {
        Self {
            open: Mutex::default(),
            browsers: Arc::new(Semaphore::new(max_browsers.max(1))),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(Uuid, String), Arc<Review>>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn get(&self, project: Uuid, branch: &str) -> Option<Arc<Review>> {
        self.lock().get(&(project, branch.to_string())).cloned()
    }

    fn insert(&self, review: Arc<Review>) -> Option<Arc<Review>> {
        self.lock()
            .insert((review.project, review.branch.clone()), review)
    }

    fn remove(&self, project: Uuid, branch: &str) -> Option<Arc<Review>> {
        self.lock().remove(&(project, branch.to_string()))
    }
}

struct OpenBrowser {
    page: Arc<Page>,
    used: Instant,
    _permit: OwnedSemaphorePermit,
}

pub(crate) struct Review {
    project: Uuid,
    branch: String,
    info: Mutex<ReviewInfo>,
    /// The session the reviewer follows (the latest one the review is in).
    session: Mutex<SessionId>,
    /// Sessions that recorded the start of this review (round).
    shown_in: Mutex<HashSet<SessionId>>,
    /// Changes were asked for: the next preview starts a new round.
    waiting_for_fix: AtomicBool,
    browser: tokio::sync::Mutex<Option<OpenBrowser>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Review {
    fn info(&self) -> ReviewInfo {
        lock(&self.info).clone()
    }

    /// Record the start of the review in `session` (once per round), so the
    /// reviewer sees it there.
    fn show_in(&self, session: &Session) {
        *lock(&self.session) = session.id();
        if lock(&self.shown_in).insert(session.id()) {
            let _ = session.record_review(EventKind::ReviewStarted {
                review: self.info(),
            });
        }
    }

    async fn close_browser(&self) {
        let open = self.browser.lock().await.take();
        if let Some(open) = open {
            open.page.close().await;
        }
    }

    /// The review's page, starting the browser if it is not running.
    async fn page(self: &Arc<Self>, state: &AppState, start: bool) -> Result<Arc<Page>, String> {
        let mut slot = self.browser.lock().await;
        if let Some(open) = slot.as_mut() {
            open.used = Instant::now();
            return Ok(open.page.clone());
        }
        if !start {
            return Err("The preview is not open: open it first with `browser_open`.".into());
        }
        let cfg = &state.config.review;
        let chromium = find_chromium(cfg.chromium.as_deref()).ok_or(
            "This agentcore server has no Chromium, so it cannot open the preview. \
             Tell the reviewer an admin needs to install Chromium on the server.",
        )?;
        let permit = state
            .reviews
            .browsers
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                "Too many reviews are running on this server right now; try again in a few minutes."
                    .to_string()
            })?;
        let info = self.info();
        let preview =
            Url::parse(&info.preview_url).map_err(|e| format!("bad preview address: {e}"))?;
        let secrets = state
            .store
            .preview_secrets(self.project)
            .await
            .map_err(|e| e.to_string())?;
        let access = Access {
            preview,
            extra_hosts: secrets.extra_hosts,
            bypass_secret: secrets.bypass_secret,
        };
        let page = Arc::new(
            Page::launch(
                &chromium,
                access,
                info.device,
                sandbox_allowed(cfg.chromium_sandbox),
            )
            .await?,
        );
        *slot = Some(OpenBrowser {
            page: page.clone(),
            used: Instant::now(),
            _permit: permit,
        });
        drop(slot);
        // Close the browser when it has not been used for a while.
        let idle = Duration::from_secs(cfg.idle_mins.max(1) * 60);
        let (weak, mine) = (Arc::downgrade(self), Arc::downgrade(&page));
        tokio::spawn(close_when_idle(weak, mine, idle));
        Ok(page)
    }
}

async fn close_when_idle(review: Weak<Review>, page: Weak<Page>, idle: Duration) {
    loop {
        tokio::time::sleep(Duration::from_secs(30).min(idle)).await;
        let (Some(review), Some(page)) = (review.upgrade(), page.upgrade()) else {
            return;
        };
        let mut slot = review.browser.lock().await;
        let Some(open) = slot.as_ref() else { return };
        if !Arc::ptr_eq(&open.page, &page) {
            return;
        }
        if open.used.elapsed() >= idle {
            let open = slot.take();
            drop(slot);
            if let Some(open) = open {
                open.page.close().await;
            }
            return;
        }
    }
}

// ---- the agent's tools --------------------------------------------------------------

fn def(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
    })
}

fn tool_definitions() -> Vec<Value> {
    let reference = json!({ "type": "integer", "description": "The number shown next to it in the page listing, e.g. 3 for [3]." });
    let text = json!({ "type": "string" });
    vec![
        def(
            "browser_open",
            "Open a page of the pull request's preview in agentcore's browser (the reviewer's \
             screen size). Pass a path such as `/customers` or a full address on the preview; \
             leave it out for the preview's start page. Returns the page's controls (numbered) \
             and text, and a screenshot.",
            json!({ "url": text }),
            &[],
        ),
        def(
            "browser_look",
            "Look at the current page again: its controls (numbered), text and a screenshot.",
            json!({}),
            &[],
        ),
        def(
            "browser_click",
            "Click a control on the page (a button, link, checkbox, tab...).",
            json!({ "ref": reference }),
            &["ref"],
        ),
        def(
            "browser_type",
            "Replace the text in a field with `text`; with `submit`, press Enter afterwards.",
            json!({ "ref": reference, "text": text, "submit": { "type": "boolean" } }),
            &["ref", "text"],
        ),
        def(
            "browser_select",
            "Choose an option in a drop-down list, by its text or value.",
            json!({ "ref": reference, "option": text }),
            &["ref", "option"],
        ),
        def(
            "browser_press",
            "Press a key: Enter, Escape, Tab, Backspace, Space or an arrow key (ArrowDown, ...).",
            json!({ "key": text }),
            &["key"],
        ),
        def(
            "browser_scroll",
            "Scroll the page down or up by most of a screen.",
            json!({ "direction": { "type": "string", "enum": ["down", "up"] } }),
            &["direction"],
        ),
        def(
            "browser_back",
            "Go back to the previous page.",
            json!({}),
            &[],
        ),
        def(
            "browser_device",
            "Switch the screen size between a phone and a desktop.",
            json!({ "device": { "type": "string", "enum": ["phone", "desktop"] } }),
            &["device"],
        ),
        def(
            "browser_sign_in",
            "Fill in the reviewer login saved for this project (you never see the password): \
             the user name into one field and the password into another, then submit.",
            json!({ "username_ref": reference, "password_ref": reference, "submit": { "type": "boolean", "default": true } }),
            &["username_ref", "password_ref"],
        ),
        def(
            "review_show",
            "Show the reviewer a step: the current screen as a screenshot in the conversation, \
             with a short title, what you did and what you expected (`note`), and whether it \
             worked (`outcome`). This is what the reviewer sees, so call it after every step of \
             your test plan and for anything they asked to see.",
            json!({
                "title": text,
                "note": text,
                "outcome": { "type": "string", "enum": ["as_expected", "problem", "info"] }
            }),
            &["title", "outcome"],
        ),
    ]
}

/// The browser and review tools of one session (role capability
/// `previews:review`). Set `session` once the session exists.
pub struct ReviewTools {
    state: AppState,
    project: Uuid,
    pub session: Arc<OnceLock<SessionId>>,
}

impl ReviewTools {
    pub fn new(state: &AppState, project: Uuid) -> Self {
        Self {
            state: state.clone(),
            project,
            session: Arc::default(),
        }
    }

    /// Page text, notes and a screenshot, for the agent.
    async fn page_result(&self, session: SessionId, page: &Page) -> Result<Value, String> {
        let snap = page.snapshot().await?;
        let notes = page.take_notes();
        let shot = page.screenshot().await?;
        let id = self
            .state
            .store
            .save_screenshot(session, "image/jpeg", &shot)
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({ "page": snap.render(&notes), "screenshot": id }))
    }
}

fn reference(args: &Value, key: &str) -> Result<u32, String> {
    args[key]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| format!("`{key}` must be the number of a control on the page"))
}

fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args[key]
        .as_str()
        .ok_or_else(|| format!("`{key}` is required"))
}

#[async_trait]
impl ToolHandler for ReviewTools {
    fn definitions(&self) -> Vec<Value> {
        tool_definitions()
    }

    async fn call(&self, tool: &str, args: &Value) -> Result<Value, String> {
        let id = *self.session.get().ok_or("the session is still starting")?;
        let session = self
            .state
            .manager
            .get(id)
            .map_err(|_| "the session is not running".to_string())?;
        let branch = session
            .context()
            .work_branch
            .ok_or("this session has no pull request branch")?;
        let review = self
            .state
            .reviews
            .get(self.project, &branch)
            .ok_or(NO_REVIEW)?;
        if !review.waiting_for_fix.load(Ordering::SeqCst) {
            review.show_in(&session);
        }
        let state = &self.state;
        match tool {
            "browser_open" => {
                let page = review.page(state, true).await?;
                let preview = Url::parse(&review.info().preview_url).map_err(|e| e.to_string())?;
                let target = match args["url"]
                    .as_str()
                    .map(str::trim)
                    .filter(|u| !u.is_empty())
                {
                    None => preview,
                    Some(u) if u.starts_with("http://") || u.starts_with("https://") => {
                        Url::parse(u).map_err(|e| format!("not an address: {e}"))?
                    }
                    Some(path) => preview.join(path).map_err(|e| e.to_string())?,
                };
                page.open(&target).await?;
                self.page_result(id, &page).await
            }
            "review_show" => {
                let page = review.page(state, false).await?;
                let title = string(args, "title")?.trim().to_string();
                if title.is_empty() {
                    return Err("`title` is required".into());
                }
                let outcome = match args["outcome"].as_str().unwrap_or("info") {
                    "as_expected" => ReviewOutcome::AsExpected,
                    "problem" => ReviewOutcome::Problem,
                    _ => ReviewOutcome::Info,
                };
                let shot = page.screenshot().await?;
                let screenshot = state
                    .store
                    .save_screenshot(id, "image/jpeg", &shot)
                    .await
                    .map_err(|e| e.to_string())?;
                let step = ReviewStep {
                    title,
                    note: args["note"].as_str().unwrap_or_default().trim().to_string(),
                    outcome,
                    url: page.current_url().await,
                    screenshot: Some(screenshot),
                };
                session
                    .record_review(EventKind::ReviewStepShown { step })
                    .map_err(|e| e.to_string())?;
                Ok(json!({ "shown": "The reviewer now sees this step in the conversation." }))
            }
            _ => {
                let page = review.page(state, false).await?;
                match tool {
                    "browser_look" => {}
                    "browser_click" => page.click(reference(args, "ref")?).await?,
                    "browser_type" => {
                        page.type_text(
                            reference(args, "ref")?,
                            string(args, "text")?,
                            args["submit"].as_bool().unwrap_or(false),
                        )
                        .await?
                    }
                    "browser_select" => {
                        page.select(reference(args, "ref")?, string(args, "option")?)
                            .await?
                    }
                    "browser_press" => page.press(string(args, "key")?).await?,
                    "browser_scroll" => page.scroll(args["direction"] != "up").await?,
                    "browser_back" => page.back().await?,
                    "browser_device" => {
                        let device = match string(args, "device")? {
                            "desktop" => ReviewDevice::Desktop,
                            _ => ReviewDevice::Phone,
                        };
                        page.set_device(device).await?;
                    }
                    "browser_sign_in" => {
                        let secrets = state
                            .store
                            .preview_secrets(self.project)
                            .await
                            .map_err(|e| e.to_string())?;
                        let (Some(password), false) =
                            (secrets.password, secrets.username.is_empty())
                        else {
                            return Err("No reviewer login is saved for this project. Ask the \
                                        reviewer to add one (project settings, Preview access) \
                                        or how they want you to sign in."
                                .into());
                        };
                        page.type_text(reference(args, "username_ref")?, &secrets.username, false)
                            .await?;
                        page.type_text(
                            reference(args, "password_ref")?,
                            &password,
                            args["submit"].as_bool().unwrap_or(true),
                        )
                        .await?;
                    }
                    other => return Err(format!("unknown tool `{other}`")),
                }
                self.page_result(id, &page).await
            }
        }
    }
}

// ---- reaching the agent ---------------------------------------------------------------

/// A message from the reviewer to the agent (shown folded in the
/// conversation: its first line says what it is).
fn from_reviewer(name: &str, text: &str) -> agentcore_core::DeliveredMessage {
    agentcore_core::DeliveredMessage {
        message_id: Uuid::now_v7(),
        from: name.to_string(),
        to: "you".into(),
        scope: agentcore_core::MessageScope::Internal,
        text: text.to_string(),
        sent_at: chrono::Utc::now(),
    }
}

/// Where the agent got the message.
enum Reached {
    /// In this session (the reviewer follows it there).
    Session(SessionId),
    /// In the department agent's inbox: it starts a session for it.
    Inbox,
}

/// Give the agent working on `found`'s branch a message from `by`: in the
/// session if it still runs, in a department agent's inbox, or in a new
/// session on the same branch. With `show`, the session it lands in shows
/// the review as running.
async fn reach(
    state: &AppState,
    found: &SessionRef,
    project: &Project,
    review: &Arc<Review>,
    text: String,
    by: &Caller,
    show: bool,
) -> ApiResult<Reached> {
    let landed = |s: &Session| {
        if show {
            review.show_in(s);
        } else {
            *lock(&review.session) = s.id();
        }
    };
    let info = found.info();
    let ctx = info.context.clone();
    if let Some(agent) = ctx.org_agent_id {
        crate::org::send(
            &state.store,
            &crate::org::Sender::Human(by.name.clone()),
            crate::org::Recipient::Agent(agent),
            &text,
        )
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
        state.reconcile.notify_one();
        if let SessionRef::Live(s) = found
            && !s.status().is_terminal()
        {
            landed(s);
            return Ok(Reached::Session(s.id()));
        }
        return Ok(Reached::Inbox);
    }
    if let SessionRef::Live(s) = found
        && !s.status().is_terminal()
    {
        landed(s);
        if s.deliver_messages(vec![from_reviewer(&by.name, &text)])
            .is_err()
        {
            // Busy: it gets the message once it waits for input.
            let (state, id) = (state.clone(), s.id());
            tokio::spawn(async move {
                if let Err(err) = crate::pr_checks::tell(&state, id, &text).await {
                    tracing::warn!(session = %id, error = %err, "could not give the agent the review");
                }
            });
        }
        return Ok(Reached::Session(s.id()));
    }
    // The session has ended: a new one carries on the same branch.
    let branch = ctx
        .work_branch
        .clone()
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "the session has no branch"))?;
    let req = crate::teamwork::StartRequest {
        issue_number: ctx.issue.as_ref().map(|i| i.number),
        task: text,
        role: ctx.role.clone(),
        agent: Some(info.agent.clone()),
        branch: Some(branch),
        ..Default::default()
    };
    let session = crate::teamwork::start_session(state, project, req, by.principal(), None).await?;
    landed(&session);
    Ok(Reached::Session(session.id()))
}

fn reached_json(reached: Reached) -> Value {
    match reached {
        Reached::Session(id) => json!({ "session_id": id }),
        Reached::Inbox => json!({
            "session_id": null,
            "note": "The agent's session has ended: the review is in its inbox, and it starts a \
                     new session for it shortly. Open it from the department page."
        }),
    }
}

// ---- starting and finishing ---------------------------------------------------------

/// The latest pull request and preview of a session (from its checks).
fn latest_preview(events: &[Event]) -> Option<(String, String)> {
    events.iter().rev().find_map(|e| match &e.kind {
        EventKind::PullRequestChecked { report } => report
            .preview_url
            .clone()
            .map(|p| (report.pull_request_url.clone(), p)),
        _ => None,
    })
}

fn start_message(info: &ReviewInfo) -> String {
    let device = match info.device {
        ReviewDevice::Phone => "a phone",
        ReviewDevice::Desktop => "a desktop computer",
    };
    format!(
        "{reviewer} wants to review your pull request with you and see how it works: {pr}\n\
         Its preview is at {preview}. You have a browser on it (the browser_* tools), sized like \
         {device}.\n\n\
         1. Read the pull request's changes and its issue, and write the reviewer a short test \
            plan: the 3 to 8 things a user would do to see the change work, including what \
            could go wrong.\n\
         2. Go through the plan on the preview. After each step call `review_show` with what \
            you did, what you expected and whether it happened. Those screenshots are what the \
            reviewer sees, so show every step. If you need to sign in, use `browser_sign_in` \
            when the project has a reviewer login, otherwise ask the reviewer.\n\
         3. Then answer the reviewer's questions and try what they ask on the preview, showing \
            each result with `review_show`.\n\n\
         Do not change the code during the review: note what the reviewer wants changed. They \
         finish the review with Approve or Request changes; if they ask for changes you will be \
         told what to do.",
        reviewer = info.reviewer,
        pr = info.pull_request_url,
        preview = info.preview_url,
    )
}

#[derive(Debug, Default, Deserialize)]
pub struct StartReview {
    #[serde(default)]
    pub device: ReviewDevice,
}

/// `POST /sessions/{id}/review`: review the session's pull request with
/// the agent, on its latest preview.
pub async fn start(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<SessionId>,
    body: Option<Json<StartReview>>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let cfg = &state.config.review;
    if !cfg.enabled {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "reviews with the agent are turned off on this server",
        ));
    }
    if find_chromium(cfg.chromium.as_deref()).is_none() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this server has no Chromium, so the agent cannot open the preview: an admin needs \
             to install it (see the guide)",
        ));
    }
    let found = state.lookup(id).await?;
    let ctx = found.info().context;
    let (Some(project_id), Some(branch)) = (ctx.project_id, ctx.work_branch.clone()) else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "only sessions that deliver a pull request can be reviewed",
        ));
    };
    let (pr, preview) = latest_preview(&found.events().await?).ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "this pull request has no preview yet: reviews start once its preview is ready",
        )
    })?;
    let project = state.project(project_id).await?;
    let info = ReviewInfo {
        pull_request_url: pr,
        preview_url: preview,
        device: body.map(|b| b.0.device).unwrap_or_default(),
        reviewer: caller.name.clone(),
    };
    let review = Arc::new(Review {
        project: project_id,
        branch,
        info: Mutex::new(info.clone()),
        session: Mutex::new(id),
        shown_in: Mutex::default(),
        waiting_for_fix: AtomicBool::new(false),
        browser: tokio::sync::Mutex::default(),
    });
    if let Some(old) = state.reviews.insert(review.clone()) {
        old.close_browser().await;
    }
    let reached = reach(
        &state,
        &found,
        &project,
        &review,
        start_message(&info),
        &caller,
        true,
    )
    .await;
    match reached {
        Ok(r) => Ok(Json(reached_json(r))),
        Err(err) => {
            state.reviews.remove(review.project, &review.branch);
            Err(err)
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct FinishReview {
    pub verdict: ReviewVerdict,
    #[serde(default)]
    pub comment: String,
}

/// What the agent showed in this review, for the pull request comment.
fn steps_summary(events: &[Event]) -> String {
    let start = events
        .iter()
        .rposition(|e| matches!(e.kind, EventKind::ReviewStarted { .. }))
        .unwrap_or(0);
    events[start..]
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ReviewStepShown { step } => Some(format!(
                "- {} {}",
                match step.outcome {
                    ReviewOutcome::AsExpected => "✅",
                    ReviewOutcome::Problem => "⚠️",
                    ReviewOutcome::Info => "ℹ️",
                },
                step.title
            )),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn pull_request_number(url: &str) -> Option<u64> {
    url.trim_end_matches('/').rsplit('/').next()?.parse().ok()
}

/// `POST /sessions/{id}/review/finish`: approve, or ask for changes (the
/// agent makes them and shows the new preview).
pub async fn finish(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<SessionId>,
    Json(req): Json<FinishReview>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let found = state.lookup(id).await?;
    let ctx = found.info().context;
    let review = ctx
        .project_id
        .zip(ctx.work_branch.as_deref())
        .and_then(|(p, b)| state.reviews.get(p, b))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::CONFLICT,
                "no review is open for this pull request (it may have ended when the server restarted)",
            )
        })?;
    let comment = req.comment.trim().to_string();
    if req.verdict == ReviewVerdict::ChangesRequested && comment.is_empty() {
        let asked = found.events().await?.iter().any(|e| {
            matches!(
                &e.kind,
                EventKind::UserMessage {
                    by: Principal::Human(_),
                    ..
                }
            )
        });
        if !asked {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "say what should change",
            ));
        }
    }
    let info = review.info();
    let project = state.project(review.project).await?;
    review.close_browser().await;

    // On the pull request, for the team.
    let steps = steps_summary(&found.events().await?);
    let headline = match req.verdict {
        ReviewVerdict::Approved => format!(
            "✅ **{}** reviewed this on the preview with the agent and **approved** it.",
            caller.name
        ),
        ReviewVerdict::ChangesRequested => format!(
            "✏️ **{}** reviewed this on the preview with the agent and **asked for changes**.",
            caller.name
        ),
    };
    let mut body = headline;
    if !comment.is_empty() {
        body.push_str(&format!("\n\n> {}", comment.replace('\n', "\n> ")));
    }
    if !steps.is_empty() {
        body.push_str(&format!("\n\nWhat the agent showed:\n{steps}"));
    }
    body.push_str(&format!(
        "\n\n<sub>Posted by agentcore for {} ({}).</sub>",
        caller.name, info.preview_url
    ));
    if let (Some(number), Ok((gh, _, _))) = (
        pull_request_number(&info.pull_request_url),
        state.require_github().await,
    ) && let Err(err) = gh
        .comment_on_issue(&project.repo_owner, &project.repo_name, number, &body)
        .await
    {
        tracing::warn!(error = %err, "could not comment on the pull request");
    }

    let record = |s: &Session| {
        let _ = s.record_review(EventKind::ReviewFinished {
            verdict: req.verdict,
            by: caller.principal(),
            comment: comment.clone(),
        });
    };
    if req.verdict == ReviewVerdict::Approved {
        state.reviews.remove(review.project, &review.branch);
        if let SessionRef::Live(s) = &found {
            record(s);
            let _ = s.deliver_messages(vec![from_reviewer(
                &caller.name,
                &format!(
                    "{} approved the pull request after the review. Thank you; nothing more \
                     to do for the review.",
                    caller.name
                ),
            )]);
        }
        return Ok(Json(json!({ "session_id": id })));
    }

    review.waiting_for_fix.store(true, Ordering::SeqCst);
    // The next round starts once the new preview is ready.
    lock(&review.shown_in).clear();
    let said = if comment.is_empty() {
        String::new()
    } else {
        format!("\n\nThey wrote:\n{comment}")
    };
    let text = format!(
        "{} asked for changes after the review.{said}\n\n\
         Make every change they asked for during this review, and fix each step you showed as \
         a problem. Then commit and call `propose_pull_request` again: it updates the same pull \
         request. Once the new preview is ready, agentcore tells you, and you show the reviewer \
         what changed with `review_show`.",
        caller.name
    );
    if let SessionRef::Live(s) = &found {
        record(s);
    }
    let reached = reach(&state, &found, &project, &review, text, &caller, false).await?;
    Ok(Json(reached_json(reached)))
}

/// The delivered branch has a new preview (its checks passed): when the
/// reviewer asked for changes, the agent shows them what changed.
pub(crate) async fn preview_ready(
    state: &AppState,
    session: SessionId,
    project: Uuid,
    branch: &str,
    preview: &str,
) {
    let Some(review) = state.reviews.get(project, branch) else {
        return;
    };
    lock(&review.info).preview_url = preview.to_string();
    if !review.waiting_for_fix.swap(false, Ordering::SeqCst) {
        return;
    }
    if let Ok(s) = state.manager.get(session) {
        review.show_in(&s);
    }
    let info = review.info();
    let text = format!(
        "The new preview with your changes is ready: {preview}\n\
         Show {} what changed: open it with `browser_open`, go through the steps they asked \
         you to change and the ones that had a problem, and show each with `review_show`. \
         Then they approve or ask for more.",
        info.reviewer
    );
    if let Err(err) = crate::pr_checks::tell(state, session, &text).await {
        tracing::warn!(session = %session, error = %err, "could not tell the agent the new preview is ready");
    }
}

// ---- screenshots and preview access ---------------------------------------------------

/// `GET /sessions/{id}/screenshots/{shot}`
pub async fn screenshot(
    State(state): State<AppState>,
    _caller: Caller,
    UrlPath((id, shot)): UrlPath<(SessionId, Uuid)>,
) -> ApiResult<Response> {
    let (mime, bytes) = state
        .store
        .screenshot(id, shot)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such screenshot"))?;
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "private, max-age=86400, immutable")
        .body(Body::from(bytes))
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// `GET /projects/{id}/preview-access`
pub async fn get_access(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<Uuid>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    Ok(Json(json!(state.store.preview_access(id).await?)))
}

/// `PUT /projects/{id}/preview-access` (admins)
pub async fn put_access(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<Uuid>,
    Json(update): Json<PreviewAccessUpdate>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    Ok(Json(json!(
        state
            .store
            .set_preview_access(id, update, &caller.name)
            .await?
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_match_the_capability() {
        let names: Vec<String> = tool_definitions()
            .iter()
            .map(|d| d["name"].as_str().unwrap().to_string())
            .collect();
        let mut expected: Vec<String> = agentcore_roles::REVIEW_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut got = names.clone();
        got.sort();
        expected.sort();
        assert_eq!(got, expected);
    }

    #[test]
    fn pull_request_numbers() {
        assert_eq!(
            pull_request_number("https://github.com/acme/demo/pull/37"),
            Some(37)
        );
        assert_eq!(pull_request_number("https://github.com/acme/demo"), None);
    }

    #[test]
    fn the_start_message_names_the_preview_and_the_tools() {
        let text = start_message(&ReviewInfo {
            pull_request_url: "https://github.com/acme/demo/pull/3".into(),
            preview_url: "https://demo-git-x.vercel.app".into(),
            device: ReviewDevice::Phone,
            reviewer: "Ana".into(),
        });
        assert!(text.contains("Ana wants to review"));
        assert!(text.contains("https://demo-git-x.vercel.app"));
        assert!(text.contains("`review_show`"));
        assert!(text.contains("a phone"));
    }
}
