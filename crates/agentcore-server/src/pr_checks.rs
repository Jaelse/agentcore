//! After delivery: watch the repository's own checks on the pull request
//! (GitHub Actions, Vercel and other preview deployments), show them and the
//! preview link on the session, and send failures the code can fix back to
//! the agent that made the change, with the build log, so it can fix them and
//! deliver again. Failures only a person can resolve (a blocked deployment,
//! missing access) are left to people.
//!
//! The sandbox has no network: agentcore reads the logs (with the GitHub
//! token, and a Vercel token when one is connected) and hands the agent only
//! the text.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use agentcore_core::{
    PullRequestChecks, RemoteCheck, RemoteCheckState, RemoteChecksState, SessionId, SessionStatus,
};
use agentcore_store::Project;
use serde_json::Value;

use crate::AppState;
use crate::github::GitHub;
use crate::sessions::SessionRef;

/// Lines of a build log handed to the agent per failed check.
const LOG_LINES: usize = 80;
/// Characters of detail per failed check, and in all.
const DETAIL_CHARS: usize = 4_000;
const MESSAGE_CHARS: usize = 14_000;

/// Watches in progress, per session: a new delivery replaces the watch of the
/// previous one, and attempts count across deliveries.
#[derive(Default)]
pub struct Watches {
    sessions: Mutex<HashMap<SessionId, Watch>>,
}

#[derive(Default)]
struct Watch {
    generation: u64,
    attempts: u32,
}

impl Watches {
    fn start(&self, session: SessionId) -> u64 {
        let mut map = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let w = map.entry(session).or_default();
        w.generation += 1;
        w.generation
    }

    fn current(&self, session: SessionId, generation: u64) -> bool {
        let map = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        map.get(&session)
            .is_some_and(|w| w.generation == generation)
    }

    /// Count an attempt unless the limit is reached: the attempt number.
    fn attempt(&self, session: SessionId, max: u32) -> Option<u32> {
        let mut map = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let w = map.entry(session).or_default();
        (w.attempts < max).then(|| {
            w.attempts += 1;
            w.attempts
        })
    }

    fn passed(&self, session: SessionId) {
        let mut map = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(w) = map.get_mut(&session) {
            w.attempts = 0;
        }
    }
}

/// What a check is, beyond what people see: where its log is.
#[derive(Debug, Clone, PartialEq)]
enum Source {
    /// A commit status (Vercel, Netlify, ...).
    Status { target_url: Option<String> },
    /// A check run; `actions` for GitHub Actions jobs (the id is the job).
    Run {
        id: u64,
        actions: bool,
        output: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct Found {
    check: RemoteCheck,
    source: Source,
}

/// Start watching the checks on `commit` of the pull request `pr_url`, after
/// a delivery of `session`.
pub(crate) fn watch(
    state: &AppState,
    session: SessionId,
    project: &Project,
    commit: String,
    pr_url: String,
) {
    if !state.config.pull_request_checks.enabled {
        return;
    }
    let generation = state.pr_watches.start(session);
    let (state, project) = (state.clone(), project.clone());
    tokio::spawn(async move {
        run(&state, session, generation, &project, commit, pr_url).await;
    });
}

async fn run(
    state: &AppState,
    session: SessionId,
    generation: u64,
    project: &Project,
    commit: String,
    pr_url: String,
) {
    let cfg = state.config.pull_request_checks.clone();
    let poll = Duration::from_secs(cfg.poll_secs.max(1));
    let max_wait = Duration::from_secs(cfg.max_wait_mins * 60);
    let grace = Duration::from_secs(cfg.no_checks_after_secs);
    let started = Instant::now();
    let (owner, repo) = (project.repo_owner.as_str(), project.repo_name.as_str());
    let mut last: Option<PullRequestChecks> = None;
    let mut confirmed = false;
    let mut errors = 0;
    loop {
        tokio::time::sleep(poll).await;
        if !state.pr_watches.current(session, generation) {
            return; // delivered again: a newer watch took over
        }
        let Ok(Some((gh, _, _))) = state.github().await else {
            return;
        };
        let report = match look(&gh, owner, repo, &commit).await {
            Ok((found, preview_url)) => {
                errors = 0;
                let mut state = overall(&found);
                if state == RemoteChecksState::None && started.elapsed() < grace {
                    state = RemoteChecksState::Pending;
                }
                (found, preview_url, state)
            }
            Err(err) => {
                errors += 1;
                tracing::warn!(%session, error = %err, "could not read the pull request's checks");
                if errors >= 10 {
                    return;
                }
                continue;
            }
        };
        let (found, preview_url, mut overall_state) = report;
        let timed_out = started.elapsed() >= max_wait;
        if timed_out && overall_state == RemoteChecksState::Pending {
            overall_state = RemoteChecksState::TimedOut;
        }
        let mut report = PullRequestChecks {
            pull_request_url: pr_url.clone(),
            commit: commit.clone(),
            state: overall_state,
            checks: found.iter().map(|f| f.check.clone()).collect(),
            preview_url,
            sent_to_agent: None,
            note: String::new(),
        };
        let settled = overall_state != RemoteChecksState::Pending;
        // A check can register late: wait for the same result twice.
        if !settled || (!confirmed && !timed_out) {
            confirmed = settled;
            if !settled && last.as_ref() != Some(&report) {
                record(state, session, report.clone()).await;
                last = Some(report);
            }
            continue;
        }
        match overall_state {
            RemoteChecksState::Passed => state.pr_watches.passed(session),
            RemoteChecksState::Failed => {
                // The report first, then the message it announces, so the
                // conversation reads in order.
                let Some((attempt, text)) =
                    hand_back(state, session, &gh, owner, repo, &found, &mut report).await
                else {
                    record(state, session, report).await;
                    return;
                };
                record(state, session, report.clone()).await;
                if let Err(err) = tell(state, session, &text).await {
                    report.sent_to_agent = None;
                    report.note = format!(
                        "Could not tell the agent (attempt {attempt}): {err}. A person needs to look."
                    );
                    record(state, session, report).await;
                }
                return;
            }
            RemoteChecksState::NeedsPerson => {
                report.note = format!(
                    "Not something the agent can fix in code: {}. A person needs to look \
                     (for example a deployment the hosting provider blocked, or missing access).",
                    names(&found, RemoteCheckState::NeedsPerson)
                );
            }
            RemoteChecksState::TimedOut => {
                report.note = format!(
                    "Stopped waiting after {} minutes; the checks were still running.",
                    cfg.max_wait_mins
                );
            }
            RemoteChecksState::None => {
                report.note = "The repository reported no checks for this commit.".into();
            }
            RemoteChecksState::Pending => {}
        }
        record(state, session, report).await;
        return;
    }
}

/// The message about the fixable failures for the agent, with its attempt
/// number (the report says it is sent); `None` when the agent has had its
/// attempts (the report says a person is needed).
async fn hand_back(
    state: &AppState,
    session: SessionId,
    gh: &GitHub,
    owner: &str,
    repo: &str,
    found: &[Found],
    report: &mut PullRequestChecks,
) -> Option<(u32, String)> {
    let max = state.config.pull_request_checks.max_fix_attempts;
    let Some(attempt) = state.pr_watches.attempt(session, max) else {
        report.note =
            format!("Still failing after the agent tried {max} times: a person needs to look.");
        return None;
    };
    let vercel = state.store.vercel_token().await.ok().flatten();
    let mut details = Vec::new();
    for f in found
        .iter()
        .filter(|f| f.check.state == RemoteCheckState::Failed)
    {
        let detail = failure_detail(state, gh, owner, repo, f, vercel.as_deref()).await;
        details.push((f.check.clone(), detail));
    }
    let others: Vec<&RemoteCheck> = found
        .iter()
        .filter(|f| f.check.state == RemoteCheckState::NeedsPerson)
        .map(|f| &f.check)
        .collect();
    let text = message(report, &details, &others, attempt, max);
    report.sent_to_agent = Some(attempt);
    report.note = format!("Sent the failures to the agent to fix (attempt {attempt} of {max}).");
    Some((attempt, text))
}

/// Record the report on the session, if it is still there.
async fn record(state: &AppState, session: SessionId, report: PullRequestChecks) {
    match state.lookup(session).await {
        Ok(SessionRef::Live(s)) if !s.status().is_terminal() => {
            if let Err(err) = s.record_pull_request_checks(report) {
                tracing::warn!(%session, error = %err, "could not record the pull request's checks");
            }
        }
        _ => {
            tracing::info!(%session, state = ?report.state, "pull request checks of an ended session")
        }
    }
}

/// Hand `text` to the agent: department agents through their mailbox (it
/// waits for their next turn, even in a later session); others in their
/// session as soon as it waits for input.
async fn tell(state: &AppState, session: SessionId, text: &str) -> Result<(), String> {
    let live = match state.lookup(session).await {
        Ok(SessionRef::Live(s)) => Some(s),
        _ => None,
    };
    let org_agent = live.as_ref().and_then(|s| s.context().org_agent_id);
    if let Some(agent) = org_agent {
        crate::org::send(
            &state.store,
            &crate::org::Sender::Human("agentcore".into()),
            crate::org::Recipient::Agent(agent),
            text,
        )
        .await
        .map_err(|e| e.to_string())?;
        state.reconcile.notify_one();
        return Ok(());
    }
    let Some(s) = live else {
        return Err("the session has ended".into());
    };
    // Wait for the agent to finish what it is doing (up to an hour).
    for _ in 0..720 {
        let status = s.status();
        if status.is_terminal() {
            return Err("the session has ended".into());
        }
        if status == SessionStatus::AwaitingInput && !s.is_paused() {
            let note = agentcore_core::DeliveredMessage {
                message_id: uuid::Uuid::now_v7(),
                from: "agentcore".into(),
                to: "you".into(),
                scope: agentcore_core::MessageScope::Internal,
                text: text.into(),
                sent_at: chrono::Utc::now(),
            };
            if s.deliver_messages(vec![note]).is_ok() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    Err("the agent stayed busy for an hour".into())
}

/// The commit's checks and its preview deployment, as GitHub reports them.
async fn look(
    gh: &GitHub,
    owner: &str,
    repo: &str,
    sha: &str,
) -> Result<(Vec<Found>, Option<String>), crate::github::GhError> {
    let statuses = gh.commit_statuses(owner, repo, sha).await?;
    let runs = gh.check_runs(owner, repo, sha).await?;
    let preview = gh.preview_url(owner, repo, sha).await?;
    Ok((found(&statuses, &runs), preview))
}

fn found(statuses: &[Value], runs: &[Value]) -> Vec<Found> {
    let mut out = Vec::new();
    for s in statuses {
        let name = s["context"].as_str().unwrap_or("status").to_string();
        let description = s["description"].as_str().unwrap_or_default().to_string();
        let target_url = s["target_url"].as_str().map(String::from);
        out.push(Found {
            check: RemoteCheck {
                state: status_state(s["state"].as_str().unwrap_or_default(), &description),
                name,
                description,
                url: target_url.clone(),
            },
            source: Source::Status { target_url },
        });
    }
    // The latest run of each name (re-runs keep the old ones).
    let mut latest: HashMap<&str, &Value> = HashMap::new();
    for r in runs {
        let name = r["name"].as_str().unwrap_or("check");
        let newer = latest
            .get(name)
            .is_none_or(|old| r["id"].as_u64() > old["id"].as_u64());
        if newer {
            latest.insert(name, r);
        }
    }
    let mut runs: Vec<&Value> = latest.into_values().collect();
    runs.sort_by_key(|r| r["id"].as_u64());
    for r in runs {
        let output = &r["output"];
        let description = output["title"]
            .as_str()
            .filter(|t| !t.is_empty())
            .or(r["conclusion"].as_str())
            .or(r["status"].as_str())
            .unwrap_or_default()
            .to_string();
        let text = [output["summary"].as_str(), output["text"].as_str()]
            .into_iter()
            .flatten()
            .filter(|t| !t.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        out.push(Found {
            check: RemoteCheck {
                name: r["name"].as_str().unwrap_or("check").to_string(),
                state: run_state(
                    r["status"].as_str().unwrap_or_default(),
                    r["conclusion"].as_str(),
                ),
                description,
                url: r["details_url"]
                    .as_str()
                    .or(r["html_url"].as_str())
                    .map(String::from),
            },
            source: Source::Run {
                id: r["id"].as_u64().unwrap_or_default(),
                actions: r["app"]["slug"] == "github-actions",
                output: text,
            },
        });
    }
    out
}

/// A commit status: failures the hosting provider decided (blocked,
/// cancelled, no access, quota) need a person; others are the code's.
fn status_state(state: &str, description: &str) -> RemoteCheckState {
    match state {
        "success" => RemoteCheckState::Passed,
        "pending" => RemoteCheckState::Pending,
        "failure" | "error" => {
            let d = description.to_ascii_lowercase();
            let person = [
                "blocked",
                "canceled",
                "cancelled",
                "not authorized",
                "unauthorized",
                "permission",
                "access",
                "quota",
                "limit",
                "billing",
                "suspended",
            ];
            if person.iter().any(|p| d.contains(p)) {
                RemoteCheckState::NeedsPerson
            } else {
                RemoteCheckState::Failed
            }
        }
        _ => RemoteCheckState::Neutral,
    }
}

fn run_state(status: &str, conclusion: Option<&str>) -> RemoteCheckState {
    if status != "completed" {
        return RemoteCheckState::Pending;
    }
    match conclusion {
        Some("success") => RemoteCheckState::Passed,
        Some("failure" | "timed_out" | "startup_failure") => RemoteCheckState::Failed,
        Some("action_required") => RemoteCheckState::NeedsPerson,
        _ => RemoteCheckState::Neutral, // neutral, skipped, cancelled, stale
    }
}

fn overall(found: &[Found]) -> RemoteChecksState {
    let any = |s: RemoteCheckState| found.iter().any(|f| f.check.state == s);
    if found.is_empty() {
        RemoteChecksState::None
    } else if any(RemoteCheckState::Pending) {
        RemoteChecksState::Pending
    } else if any(RemoteCheckState::Failed) {
        RemoteChecksState::Failed
    } else if any(RemoteCheckState::NeedsPerson) {
        RemoteChecksState::NeedsPerson
    } else {
        RemoteChecksState::Passed
    }
}

fn names(found: &[Found], state: RemoteCheckState) -> String {
    found
        .iter()
        .filter(|f| f.check.state == state)
        .map(|f| {
            if f.check.description.is_empty() {
                f.check.name.clone()
            } else {
                format!("{} ({})", f.check.name, f.check.description)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// What went wrong in a failed check, as far as agentcore can read it.
async fn failure_detail(
    state: &AppState,
    gh: &GitHub,
    owner: &str,
    repo: &str,
    f: &Found,
    vercel: Option<&str>,
) -> String {
    match &f.source {
        Source::Run {
            id,
            actions: true,
            output,
        } => {
            let mut parts = Vec::new();
            if let Ok(annotations) = gh.check_run_annotations(owner, repo, *id).await {
                let lines: Vec<String> = annotations
                    .iter()
                    .filter(|a| a["annotation_level"] == "failure")
                    .take(20)
                    .map(|a| {
                        format!(
                            "{}:{}: {}",
                            a["path"].as_str().unwrap_or("?"),
                            a["start_line"].as_u64().unwrap_or(0),
                            a["message"].as_str().unwrap_or_default().trim()
                        )
                    })
                    .collect();
                if !lines.is_empty() {
                    parts.push(lines.join("\n"));
                }
            }
            match gh.job_log(owner, repo, *id).await {
                Ok(log) => parts.push(log_tail(&log, LOG_LINES)),
                Err(err) => {
                    tracing::debug!(error = %err, "no job log");
                    if !output.is_empty() {
                        parts.push(output.clone());
                    }
                }
            }
            parts.join("\n\n")
        }
        Source::Run { output, .. } => output.clone(),
        Source::Status { target_url } => {
            let Some((team, deployment)) = target_url.as_deref().and_then(vercel_deployment) else {
                return String::new();
            };
            let Some(token) = vercel else {
                return "(agentcore cannot read Vercel build logs: Vercel is not connected under \
                        Settings → Integrations.)"
                    .into();
            };
            match vercel_log(state, token, &team, &deployment).await {
                Ok(log) => log,
                Err(err) => format!("(could not read the Vercel build log: {err})"),
            }
        }
    }
}

/// `(team slug, deployment id)` from a Vercel status link such as
/// `https://vercel.com/<team>/<project>/<id>`.
fn vercel_deployment(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("https://vercel.com/")?;
    let parts: Vec<&str> = rest.split(['/', '?', '#']).collect();
    let (team, id) = (*parts.first()?, *parts.get(2)?);
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    };
    if !ok(team) || !ok(id) {
        return None;
    }
    let id = if id.starts_with("dpl_") {
        id.to_string()
    } else {
        format!("dpl_{id}")
    };
    Some((team.to_string(), id))
}

async fn vercel_log(
    state: &AppState,
    token: &str,
    team: &str,
    deployment: &str,
) -> Result<String, String> {
    let base = state
        .config
        .pull_request_checks
        .vercel_api_url
        .trim_end_matches('/');
    let resp = state
        .http
        .get(format!(
            "{base}/v3/deployments/{deployment}/events?builds=1&slug={}",
            crate::github::urlencode(team)
        ))
        .bearer_auth(token)
        .header("user-agent", "agentcore")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = body["error"]["message"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| format!("Vercel answered {status}"));
        return Err(msg);
    }
    Ok(log_tail(&vercel_text(&body), LOG_LINES))
}

/// The text lines of Vercel's build events.
fn vercel_text(events: &Value) -> String {
    events
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["payload"]["text"].as_str().or(e["text"].as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The last `n` lines, without timestamps and terminal colours.
fn log_tail(log: &str, n: usize) -> String {
    let lines: Vec<String> = log
        .lines()
        .map(|l| strip_ansi(strip_timestamp(l)))
        .filter(|l| !l.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// `2026-11-09T07:36:21.3594710Z text` → `text`.
fn strip_timestamp(line: &str) -> &str {
    match line.split_once(' ') {
        Some((ts, rest))
            if ts.len() >= 20
                && ts.as_bytes()[4] == b'-'
                && ts.as_bytes()[10] == b'T'
                && ts.ends_with('Z') =>
        {
            rest
        }
        _ => line,
    }
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn clip(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    // Keep the end: build errors are at the bottom.
    let skip = count - max;
    format!("…{}", text.chars().skip(skip).collect::<String>())
}

/// The message to the agent about its failed checks.
fn message(
    report: &PullRequestChecks,
    failed: &[(RemoteCheck, String)],
    others: &[&RemoteCheck],
    attempt: u32,
    max: u32,
) -> String {
    let mut text = format!(
        "The checks on your pull request {} failed (commit {}), so it cannot be merged yet:\n",
        report.pull_request_url,
        &report.commit[..report.commit.len().min(12)],
    );
    let mut budget = MESSAGE_CHARS;
    for (check, detail) in failed {
        text.push_str(&format!("\n## {}", check.name));
        if !check.description.is_empty() {
            text.push_str(&format!(": {}", check.description));
        }
        text.push('\n');
        let detail = clip(detail.trim(), DETAIL_CHARS.min(budget));
        budget = budget.saturating_sub(detail.chars().count());
        if detail.is_empty() {
            text.push_str(
                "(agentcore could not read its log; reproduce it in your sandbox by running \
                 the project's build and tests)\n",
            );
        } else {
            text.push_str(&format!("```\n{detail}\n```\n"));
        }
    }
    if !others.is_empty() {
        text.push_str(&format!(
            "\nNot yours to fix (a person is told): {}.\n",
            others
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    text.push_str(&format!(
        "\nFix the cause in the code, run the same build or tests in your sandbox to make sure, \
         commit, and call `propose_pull_request` again: it updates the same pull request. \
         This is attempt {attempt} of {max}; if the cause is outside the code (configuration, \
         secrets, the hosting provider), do not guess: tell the people supervising you."
    ));
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn statuses_and_runs_are_classified() {
        let statuses = vec![
            json!({ "context": "Vercel", "state": "failure",
                    "description": "Deployment has failed — run this Vercel CLI command: npx vercel inspect dpl_X --logs",
                    "target_url": "https://vercel.com/team-a/app/4YLGLWJ6BT17" }),
            json!({ "context": "Vercel – other", "state": "failure", "description": "Deployment was blocked" }),
        ];
        let runs = vec![
            json!({ "id": 1, "name": "build", "status": "completed", "conclusion": "failure",
                    "app": { "slug": "github-actions" }, "details_url": "https://github.com/a/b/actions/runs/1/job/1",
                    "output": { "title": null, "summary": null, "text": null } }),
            json!({ "id": 3, "name": "build", "status": "completed", "conclusion": "success",
                    "app": { "slug": "github-actions" }, "output": {} }),
            json!({ "id": 2, "name": "lint", "status": "in_progress", "conclusion": null, "app": { "slug": "x" }, "output": {} }),
            json!({ "id": 4, "name": "preview comments", "status": "completed", "conclusion": "skipped", "app": { "slug": "vercel" }, "output": {} }),
        ];
        let f = found(&statuses, &runs);
        let states: Vec<(&str, RemoteCheckState)> = f
            .iter()
            .map(|f| (f.check.name.as_str(), f.check.state))
            .collect();
        assert_eq!(
            states,
            vec![
                ("Vercel", RemoteCheckState::Failed),
                ("Vercel – other", RemoteCheckState::NeedsPerson),
                ("lint", RemoteCheckState::Pending),
                ("build", RemoteCheckState::Passed), // the re-run (id 3) counts
                ("preview comments", RemoteCheckState::Neutral),
            ]
        );
        assert_eq!(overall(&f), RemoteChecksState::Pending);
        let done: Vec<Found> = f
            .into_iter()
            .filter(|f| f.check.state != RemoteCheckState::Pending)
            .collect();
        assert_eq!(overall(&done), RemoteChecksState::Failed);
        let person: Vec<Found> = done
            .iter()
            .filter(|f| f.check.state != RemoteCheckState::Failed)
            .cloned()
            .collect();
        assert_eq!(overall(&person), RemoteChecksState::NeedsPerson);
        assert_eq!(overall(&[]), RemoteChecksState::None);
        assert_eq!(
            run_state("completed", Some("action_required")),
            RemoteCheckState::NeedsPerson
        );
    }

    #[test]
    fn vercel_links_name_the_deployment() {
        assert_eq!(
            vercel_deployment(
                "https://vercel.com/jaelse-ronalds-projects/aizumey/4YLGLWJ6BT17pSeQar1m9jwbNX6f"
            ),
            Some((
                "jaelse-ronalds-projects".into(),
                "dpl_4YLGLWJ6BT17pSeQar1m9jwbNX6f".into()
            ))
        );
        assert_eq!(vercel_deployment("https://vercel.com/team"), None);
        assert_eq!(vercel_deployment("https://example.com/a/b/c"), None);
        assert_eq!(vercel_deployment("https://vercel.com/a/b/../x"), None);
    }

    #[test]
    fn logs_keep_the_end_without_noise() {
        let log = "2026-11-09T07:36:21.3594710Z \u{1b}[31mFailed to compile.\u{1b}[0m\n\n\
                   2026-11-09T07:36:21.3600000Z ./src/x.tsx\nModule not found: Can't resolve './ui/table'";
        assert_eq!(
            log_tail(log, 2),
            "./src/x.tsx\nModule not found: Can't resolve './ui/table'"
        );
        assert_eq!(log_tail(log, 10).lines().next(), Some("Failed to compile."));
        let events = json!([
            { "type": "stdout", "payload": { "text": "Running build" } },
            { "type": "stderr", "text": "Error: boom" },
            { "type": "delimiter" },
        ]);
        assert_eq!(vercel_text(&events), "Running build\nError: boom");
        assert_eq!(clip("abcdef", 3), "…def");
    }

    #[test]
    fn the_message_says_what_failed_and_what_to_do() {
        let report = PullRequestChecks {
            pull_request_url: "https://github.com/a/b/pull/37".into(),
            commit: "709bc6a6a2ef9c0ab7d1bb459f39b30efe90daca".into(),
            state: RemoteChecksState::Failed,
            checks: vec![],
            preview_url: None,
            sent_to_agent: None,
            note: String::new(),
        };
        let vercel = RemoteCheck {
            name: "Vercel".into(),
            state: RemoteCheckState::Failed,
            description: "Deployment has failed".into(),
            url: None,
        };
        let blocked = RemoteCheck {
            name: "Netlify".into(),
            state: RemoteCheckState::NeedsPerson,
            description: "blocked".into(),
            url: None,
        };
        let text = message(
            &report,
            &[
                (
                    vercel.clone(),
                    "Module not found: Can't resolve './ui/table'".into(),
                ),
                (vercel, String::new()),
            ],
            &[&blocked],
            2,
            3,
        );
        assert!(
            text.contains("pull/37 failed (commit 709bc6a6a2ef)"),
            "{text}"
        );
        assert!(
            text.contains("## Vercel: Deployment has failed\n```\nModule not found"),
            "{text}"
        );
        assert!(text.contains("could not read its log"), "{text}");
        assert!(
            text.contains("Not yours to fix (a person is told): Netlify."),
            "{text}"
        );
        assert!(text.contains("`propose_pull_request` again"), "{text}");
        assert!(text.contains("attempt 2 of 3"), "{text}");
    }

    #[test]
    fn attempts_are_limited_and_reset_by_a_pass() {
        let w = Watches::default();
        let s = uuid::Uuid::now_v7();
        let g1 = w.start(s);
        assert!(w.current(s, g1));
        let g2 = w.start(s);
        assert!(!w.current(s, g1) && w.current(s, g2));
        assert_eq!(w.attempt(s, 2), Some(1));
        assert_eq!(w.attempt(s, 2), Some(2));
        assert_eq!(w.attempt(s, 2), None);
        w.passed(s);
        assert_eq!(w.attempt(s, 2), Some(1));
    }
}
