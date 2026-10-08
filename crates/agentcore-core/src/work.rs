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
    /// `worker` or `communicator`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_agent_kind: Option<String>,
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
