//! Roles: *how* an agent works on a team.
//!
//! agentcore separates three concerns:
//!
//! * **Guardrails** (`agentcore-policy`): what the agent is *allowed* to do.
//!   Enforced on every action.
//! * **Roles** (this crate): how a developer, project manager or marketer on
//!   *this* team works — written instructions, the team's own convention files
//!   from the repository, which GitHub tools the agent gets, how work moves
//!   on the board, and how results are delivered.
//! * **Checks** (part of a role): verification that conventions were followed
//!   (commit message format, formatter, tests) before anything leaves the
//!   sandbox. Instructions are advice; checks are proof.
//!
//! Roles are TOML files, so a team reviews and versions its ways of working
//! like code.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum RoleError {
    #[error("failed to read role {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse role {path}: {source}")]
    Parse {
        path: String,
        source: Box<toml::de::Error>,
    },
    #[error("role `{role}`: {message}")]
    Invalid { role: String, message: String },
}

/// GitHub capabilities a role can grant. Each one unlocks a set of agent tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Capability {
    #[serde(rename = "issues:read")]
    IssuesRead,
    #[serde(rename = "issues:comment")]
    IssuesComment,
    #[serde(rename = "issues:write")]
    IssuesWrite,
    #[serde(rename = "milestones:read")]
    MilestonesRead,
    #[serde(rename = "milestones:write")]
    MilestonesWrite,
    #[serde(rename = "board:read")]
    BoardRead,
    #[serde(rename = "board:write")]
    BoardWrite,
    #[serde(rename = "discussions:read")]
    DiscussionsRead,
    #[serde(rename = "discussions:write")]
    DiscussionsWrite,
    /// Propose a pull request (title/body) for a human to deliver.
    #[serde(rename = "pull_requests:propose")]
    PullRequestsPropose,
}

/// Board columns, mapped to the project's real column names per project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Ready,
    InProgress,
    InReview,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Workflow {
    /// Move the issue's board card here when an agent starts on it.
    pub on_start: Option<Stage>,
    /// Move the card here when the work is delivered.
    pub on_deliver: Option<Stage>,
    /// Comment on the issue when an agent starts / delivers.
    pub announce_on_issue: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryKind {
    /// Push a branch and open (or update) a pull request.
    PullRequest,
    /// Work stays in agentcore (reports, drafts, board and issue updates).
    #[default]
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Delivery {
    pub kind: DeliveryKind,
    /// Branch name; `{issue}`, `{slug}` and `{session}` are substituted.
    pub branch: String,
    pub draft: bool,
}

impl Default for Delivery {
    fn default() -> Self {
        Self {
            kind: DeliveryKind::None,
            branch: "agent/{issue}-{slug}".into(),
            draft: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckKind {
    /// Every new commit's subject line must match `pattern` (regex).
    CommitMessage { pattern: String },
    /// No uncommitted changes may remain.
    CleanWorktree,
    /// A shell command run inside the sandbox must succeed. Skipped unless
    /// the file `when_exists` exists in the workspace (if given).
    Command {
        run: String,
        #[serde(default)]
        when_exists: Option<String>,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },
}

fn default_timeout() -> u64 {
    600
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    /// Optional checks are reported but do not block delivery.
    #[serde(default)]
    pub optional: bool,
    #[serde(flatten)]
    pub kind: CheckKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Guardrail policy applied to sessions in this role.
    #[serde(default)]
    pub policy: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Files in the repository that describe how the team works. Loaded into
    /// the agent's instructions when present (team conventions win over the
    /// generic instructions below).
    #[serde(default)]
    pub repo_docs: Vec<String>,
    #[serde(default = "default_doc_bytes")]
    pub max_repo_doc_bytes: usize,
    /// The playbook: how someone in this role works.
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub workflow: Workflow,
    #[serde(default)]
    pub delivery: Delivery,
    #[serde(default)]
    pub checks: Vec<Check>,
}

fn default_doc_bytes() -> usize {
    24 * 1024
}

impl Role {
    pub fn from_toml(src: &str, origin: &str) -> Result<Self, RoleError> {
        let role: Self = toml::from_str(src).map_err(|e| RoleError::Parse {
            path: origin.into(),
            source: Box::new(e),
        })?;
        role.validate()?;
        Ok(role)
    }

    fn validate(&self) -> Result<(), RoleError> {
        let invalid = |message: String| RoleError::Invalid {
            role: self.name.clone(),
            message,
        };
        if self.name.trim().is_empty() {
            return Err(invalid("name must not be empty".into()));
        }
        for check in &self.checks {
            if let CheckKind::CommitMessage { pattern } = &check.kind {
                regex::Regex::new(pattern)
                    .map_err(|e| invalid(format!("check `{}`: bad pattern: {e}", check.name)))?;
            }
        }
        if self.delivery.kind == DeliveryKind::PullRequest
            && !self.capabilities.contains(&Capability::PullRequestsPropose)
        {
            return Err(invalid(
                "delivery.kind = \"pull_request\" requires the `pull_requests:propose` capability"
                    .into(),
            ));
        }
        Ok(())
    }

    pub fn display_title(&self) -> &str {
        if self.title.is_empty() {
            &self.name
        } else {
            &self.title
        }
    }

    /// SHA-256 of the canonical role, recorded in the audit log.
    pub fn digest(&self) -> String {
        hex::encode(Sha256::digest(serde_json::to_vec(self).unwrap_or_default()))
    }

    pub fn has(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// Branch name for a delivery.
    pub fn branch_name(&self, issue: Option<u64>, title: &str, session: &str) -> String {
        let slug = slugify(title);
        let issue = issue
            .map(|n| n.to_string())
            .unwrap_or_else(|| "task".into());
        let short_session: String = session
            .chars()
            .filter(|c| *c != '-')
            .rev()
            .take(8)
            .collect();
        let branch = self
            .delivery
            .branch
            .replace("{issue}", &issue)
            .replace("{slug}", &slug)
            .replace("{session}", &short_session);
        sanitize_branch(&branch)
    }
}

pub fn slugify(text: &str) -> String {
    let mut slug = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() { "work".into() } else { slug }
}

fn sanitize_branch(branch: &str) -> String {
    let cleaned: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "/-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut parts: Vec<&str> = cleaned
        .split('/')
        .map(|p| p.trim_matches('.'))
        .filter(|p| !p.is_empty() && *p != "..")
        .collect();
    if parts.is_empty() {
        parts.push("agent");
    }
    parts.join("/").replace("..", ".")
}

/// A file from the repository that documents the team's conventions.
#[derive(Debug, Clone)]
pub struct RepoDoc {
    pub path: String,
    pub content: String,
    pub truncated: bool,
}

/// The work item the agent is asked to do.
#[derive(Debug, Clone, Default)]
pub struct WorkItem {
    /// e.g. "acme/api"
    pub repository: Option<String>,
    pub branch: Option<String>,
    pub issue: Option<IssueContext>,
    /// Free-text task from the operator (or the issue title when absent).
    pub task: String,
    /// Project-specific notes from agentcore's project settings.
    pub project_notes: String,
}

#[derive(Debug, Clone, Default)]
pub struct IssueContext {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub body: String,
    pub labels: Vec<String>,
    pub milestone: Option<String>,
    pub comments: Vec<(String, String)>,
}

/// Tool names offered per capability (must match the server's tool table).
pub fn tools_for(capability: Capability) -> &'static [&'static str] {
    match capability {
        Capability::IssuesRead => &["github_list_issues", "github_get_issue"],
        Capability::IssuesComment => &["github_comment_on_issue"],
        Capability::IssuesWrite => &["github_create_issue", "github_update_issue"],
        Capability::MilestonesRead => &["github_list_milestones"],
        Capability::MilestonesWrite => &["github_create_milestone", "github_update_milestone"],
        Capability::BoardRead => &["github_list_board_items"],
        Capability::BoardWrite => &["github_set_board_status"],
        Capability::DiscussionsRead => &["github_list_discussions", "github_get_discussion"],
        Capability::DiscussionsWrite => &["github_comment_on_discussion"],
        Capability::PullRequestsPropose => &["propose_pull_request"],
    }
}

/// Compose the first message the agent receives: role playbook, the team's
/// own convention files, project notes, the work item, and how to deliver.
pub fn compose_prompt(role: &Role, docs: &[RepoDoc], item: &WorkItem) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Your role: {}\n\n", role.display_title()));
    out.push_str(
        "You are an AI agent working as a member of this team, supervised through agentcore. \
         Work the way this team works: follow the team's own conventions below first, then \
         this playbook.\n\n",
    );
    if !role.instructions.trim().is_empty() {
        out.push_str(role.instructions.trim());
        out.push_str("\n\n");
    }
    if !docs.is_empty() {
        out.push_str("# The team's conventions (from the repository)\n\n");
        out.push_str(
            "These files are written by the team. When they conflict with the playbook \
             above, the team's files win.\n\n",
        );
        for doc in docs {
            out.push_str(&format!("## {}\n\n{}\n", doc.path, doc.content.trim()));
            if doc.truncated {
                out.push_str("\n[truncated]\n");
            }
            out.push('\n');
        }
    }
    if !item.project_notes.trim().is_empty() {
        out.push_str("# Project notes\n\n");
        out.push_str(item.project_notes.trim());
        out.push_str("\n\n");
    }
    let required: Vec<&Check> = role.checks.iter().filter(|c| !c.optional).collect();
    if !role.checks.is_empty() {
        out.push_str("# Checks your work must pass\n\n");
        out.push_str("agentcore verifies these before your work leaves the sandbox:\n\n");
        for check in &role.checks {
            let detail = match &check.kind {
                CheckKind::CommitMessage { pattern } => {
                    format!("every commit subject matches `{pattern}`")
                }
                CheckKind::CleanWorktree => "no uncommitted changes remain".into(),
                CheckKind::Command { run, .. } => format!("`{run}` succeeds"),
            };
            let tag = if check.optional { " (optional)" } else { "" };
            out.push_str(&format!("- {}{tag}: {detail}\n", check.name));
        }
        if !required.is_empty() {
            out.push_str("\nRun these yourself before you finish.\n");
        }
        out.push('\n');
    }
    out.push_str("# Your task\n\n");
    if let Some(repo) = &item.repository {
        let branch = item.branch.as_deref().unwrap_or("default branch");
        out.push_str(&format!(
            "Repository `{repo}` (branch `{branch}`) is checked out in /workspace.\n\n"
        ));
    }
    if let Some(issue) = &item.issue {
        out.push_str(&format!(
            "Issue #{}: {}\n{}\n",
            issue.number, issue.title, issue.url
        ));
        if !issue.labels.is_empty() {
            out.push_str(&format!("Labels: {}\n", issue.labels.join(", ")));
        }
        if let Some(m) = &issue.milestone {
            out.push_str(&format!("Milestone: {m}\n"));
        }
        out.push('\n');
        if !issue.body.trim().is_empty() {
            out.push_str(issue.body.trim());
            out.push_str("\n\n");
        }
        for (author, body) in issue.comments.iter().take(20) {
            out.push_str(&format!("Comment by @{author}:\n{}\n\n", body.trim()));
        }
    }
    if !item.task.trim().is_empty() {
        if item.issue.is_some() {
            out.push_str("Instructions from your supervisor:\n");
        }
        out.push_str(item.task.trim());
        out.push_str("\n\n");
    }
    out.push_str("# When you are done\n\n");
    match role.delivery.kind {
        DeliveryKind::PullRequest => out.push_str(
            "Commit your work on the current branch following the team's commit conventions. \
             Do not push: you have no credentials, by design. Then call the \
             `propose_pull_request` tool with a title and a description written the way \
             this team writes pull requests (use the repository's pull request template if \
             there is one). A human reviews your changes and delivers them.\n",
        ),
        DeliveryKind::None => out.push_str(
            "Summarise what you did and anything a human should review. Your output stays \
             in agentcore unless you used a tool to publish it.\n",
        ),
    }
    out.push_str(
        "If something is unclear, stop and ask: your supervisor can answer and you will \
         continue in the same session.\n",
    );
    out
}

/// All roles of a deployment.
#[derive(Debug, Clone, Default)]
pub struct RoleSet {
    roles: BTreeMap<String, std::sync::Arc<Role>>,
}

impl RoleSet {
    pub fn load_dir(dir: &Path) -> Result<Self, RoleError> {
        let io = |source| RoleError::Io {
            path: dir.display().to_string(),
            source,
        };
        let mut set = Self::default();
        if !dir.exists() {
            return Ok(set);
        }
        let mut paths: Vec<_> = std::fs::read_dir(dir)
            .map_err(io)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(io)?
            .into_iter()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
            .collect();
        paths.sort();
        let mut seen = HashSet::new();
        for path in paths {
            let src = std::fs::read_to_string(&path).map_err(|source| RoleError::Io {
                path: path.display().to_string(),
                source,
            })?;
            let role = Role::from_toml(&src, &path.display().to_string())?;
            if !seen.insert(role.name.clone()) {
                return Err(RoleError::Invalid {
                    role: role.name,
                    message: "defined more than once".into(),
                });
            }
            set.roles
                .insert(role.name.clone(), std::sync::Arc::new(role));
        }
        Ok(set)
    }

    pub fn insert(&mut self, role: Role) {
        self.roles
            .insert(role.name.clone(), std::sync::Arc::new(role));
    }

    pub fn get(&self, name: &str) -> Option<std::sync::Arc<Role>> {
        self.roles.get(name).cloned()
    }

    pub fn iter(&self) -> impl Iterator<Item = &std::sync::Arc<Role>> {
        self.roles.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundled() -> RoleSet {
        RoleSet::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../roles")).unwrap()
    }

    #[test]
    fn bundled_roles_load() {
        let roles = bundled();
        for name in ["developer", "project-manager", "marketing"] {
            assert!(roles.get(name).is_some(), "missing role {name}");
        }
        let dev = roles.get("developer").unwrap();
        assert_eq!(dev.delivery.kind, DeliveryKind::PullRequest);
        assert!(dev.has(Capability::PullRequestsPropose));
        assert!(!roles.get("marketing").unwrap().has(Capability::IssuesWrite));
    }

    #[test]
    fn conventional_commit_check_pattern() {
        let dev = bundled().get("developer").unwrap();
        let CheckKind::CommitMessage { pattern } = &dev
            .checks
            .iter()
            .find(|c| matches!(c.kind, CheckKind::CommitMessage { .. }))
            .unwrap()
            .kind
        else {
            unreachable!()
        };
        let re = regex::Regex::new(pattern).unwrap();
        assert!(re.is_match("feat(api): add pagination"));
        assert!(re.is_match("fix: handle empty input"));
        assert!(!re.is_match("Added stuff"));
    }

    #[test]
    fn branch_names_are_safe() {
        let dev = bundled().get("developer").unwrap();
        assert_eq!(
            dev.branch_name(Some(42), "Fix: login fails on Safari!", "0190-abcd"),
            "agent/42-fix-login-fails-on-safari"
        );
        let mut evil = (*dev).clone();
        evil.delivery.branch = "../../{slug}/..lock".into();
        let branch = evil.branch_name(None, "x", "s");
        assert!(!branch.contains(".."), "{branch}");
        assert!(!branch.starts_with('/'));
    }

    #[test]
    fn prompt_puts_team_conventions_and_issue_in() {
        let dev = bundled().get("developer").unwrap();
        let prompt = compose_prompt(
            &dev,
            &[RepoDoc {
                path: "CONTRIBUTING.md".into(),
                content: "Use tabs.".into(),
                truncated: false,
            }],
            &WorkItem {
                repository: Some("acme/api".into()),
                branch: Some("main".into()),
                issue: Some(IssueContext {
                    number: 7,
                    title: "Add pagination".into(),
                    url: "https://github.com/acme/api/issues/7".into(),
                    body: "Lists are too long.".into(),
                    ..Default::default()
                }),
                task: String::new(),
                project_notes: "Deploys on Fridays are frozen.".into(),
            },
        );
        for needle in [
            "## CONTRIBUTING.md",
            "Use tabs.",
            "Issue #7: Add pagination",
            "Deploys on Fridays",
            "propose_pull_request",
            "acme/api",
        ] {
            assert!(prompt.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn rejects_pull_request_delivery_without_capability() {
        let src = "name = \"x\"\n[delivery]\nkind = \"pull_request\"\n";
        assert!(Role::from_toml(src, "x").is_err());
    }
}
