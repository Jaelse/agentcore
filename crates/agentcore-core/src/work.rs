//! Types for team work: repository workspaces, checks, changes and delivery.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Where a session's work comes from and where it goes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    /// `owner/name`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Base branch the work starts from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<IssueRef>,
    /// Local branch the agent works on (and that delivery pushes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_branch: Option<String>,
    /// Branch the work was delivered to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request_url: Option<String>,
    /// Organisation membership (department agents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub department_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub department_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_agent_id: Option<Uuid>,
    /// `worker`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_agent_kind: Option<String>,
    /// The department work item this session holds (its branch is
    /// `work_branch`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueRef {
    pub number: u64,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub optional: bool,
    pub skipped: bool,
    /// Output or explanation (truncated).
    pub detail: String,
}

/// A pull request the agent proposes; a human delivers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestProposal {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    /// `modified`, `added`, `deleted`, `renamed` or `untracked`.
    pub status: String,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub sha: String,
    pub author: String,
    pub subject: String,
}

/// What the agent changed relative to the base commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    pub base: String,
    pub head: Option<String>,
    pub commits: Vec<Commit>,
    pub files: Vec<ChangedFile>,
    /// Unified diff of committed and uncommitted changes against `base`.
    pub patch: String,
    pub patch_truncated: bool,
    pub uncommitted: bool,
    pub captured_at: DateTime<Utc>,
}

/// What GitHub reports about a delivered pull request's commit: the
/// repository's own checks (GitHub Actions, Vercel, Netlify, ...) and the
/// preview deployment, as agentcore watches them after delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestChecks {
    pub pull_request_url: String,
    pub commit: String,
    pub state: RemoteChecksState,
    pub checks: Vec<RemoteCheck>,
    /// Address of the preview deployment, once one is ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_url: Option<String>,
    /// The failures were sent to the agent to fix (attempt number).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_to_agent: Option<u32>,
    /// What agentcore concluded, for people (e.g. why a person is needed).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteChecksState {
    /// Checks are still running (or have not been reported yet).
    Pending,
    Passed,
    /// At least one check failed that a change to the code can fix.
    Failed,
    /// Only failures a person has to resolve (a blocked deployment,
    /// missing access, a cancelled run).
    NeedsPerson,
    /// Nothing was reported for the commit.
    None,
    /// agentcore stopped waiting before the checks finished.
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCheck {
    pub name: String,
    pub state: RemoteCheckState,
    /// What the check said about itself (one line).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Where people can look at it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCheckState {
    Pending,
    Passed,
    /// Failed in a way a change to the code can fix (a build or test error).
    Failed,
    /// Failed in a way only a person can resolve.
    NeedsPerson,
    /// Skipped, neutral or cancelled: does not count either way.
    Neutral,
}

/// A person reviewing a delivered pull request together with the agent: the
/// agent uses the preview in a browser run by agentcore and shows each step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewInfo {
    pub pull_request_url: String,
    pub preview_url: String,
    /// `phone` or `desktop`: the screen size the agent starts with.
    pub device: ReviewDevice,
    /// The person reviewing.
    pub reviewer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDevice {
    #[default]
    Phone,
    Desktop,
}

/// One step the agent showed the reviewer: what it did on the preview, what
/// it expected, and the screen afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewStep {
    pub title: String,
    /// What the agent did and what it expected, in plain words.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    pub outcome: ReviewOutcome,
    /// The page the screenshot shows.
    pub url: String,
    /// Id of the screenshot (`GET /sessions/{id}/screenshots/{shot}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOutcome {
    /// It worked the way the change is meant to.
    AsExpected,
    /// Something did not work or looked wrong.
    Problem,
    /// Just showing something (no expectation).
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approved,
    ChangesRequested,
}
