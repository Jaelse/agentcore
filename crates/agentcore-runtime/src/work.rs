//! Team-work plumbing used by sessions: preparing a repository workspace,
//! external tools (GitHub), and git operations inside the sandbox.

use std::path::Path;
use std::sync::Arc;

use agentcore_core::{ChangedFile, Changes, Commit, ModelEndpoint, SessionContext};
use agentcore_roles::{Role, WorkItem};
use async_trait::async_trait;
use serde_json::Value;

/// Prepares the workspace directory on the host before the sandbox starts,
/// typically by cloning a repository. Runs before any agent code exists in
/// the directory.
#[async_trait]
pub trait WorkspaceSetup: Send + Sync {
    async fn prepare(&self, workspace_dir: &Path) -> Result<PreparedWorkspace, String>;
}

#[derive(Debug, Clone)]
pub struct PreparedWorkspace {
    pub repository: String,
    pub branch: String,
    pub base_commit: String,
}

/// Tools implemented outside the sandbox (e.g. GitHub). Calls go through
/// policy, approvals and the audit log like every other action.
#[async_trait]
pub trait ToolHandler: Send + Sync {
    /// MCP tool definitions (`name`, `description`, `inputSchema`).
    fn definitions(&self) -> Vec<Value>;

    async fn call(&self, tool: &str, arguments: &Value) -> Result<Value, String>;
}

/// Everything beyond agent + task that shapes a session.
#[derive(Clone, Default)]
pub struct SessionOptions {
    /// Model providers reachable through the model gateway.
    pub models: Vec<ModelEndpoint>,
    pub role: Option<Arc<Role>>,
    pub workspace: Option<Arc<dyn WorkspaceSetup>>,
    pub tools: Option<Arc<dyn ToolHandler>>,
    /// The work item (issue, notes) used to compose the role prompt.
    pub work_item: Option<WorkItem>,
    pub context: SessionContext,
    /// Do not offer the built-in sandbox tools (`run_command`, `read_file`,
    /// ...): the agent only gets the external tools. Used for communicators
    /// and departments that do not grant sandbox access.
    pub hide_sandbox_tools: bool,
    /// How the agent is named in the audit trail (default: the agent spec
    /// name), e.g. `Research/analyst`.
    pub agent_label: Option<String>,
}

/// Git configuration forced on every git command agentcore runs, so nothing
/// in the repository's own config (hooks, fsmonitor) is executed.
pub(crate) const GIT: &str =
    "git -c core.hooksPath=/dev/null -c core.fsmonitor=false -c core.pager=cat";

/// Shell script that prints what changed since `$1` (base commit).
pub(crate) fn changes_script() -> String {
    format!(
        r#"set -u
B="$1"
echo "@@HEAD"; {GIT} rev-parse HEAD 2>/dev/null || true
echo "@@COMMITS"; {GIT} log --format='%H%x09%an%x09%s' "$B..HEAD" 2>/dev/null || true
echo "@@NUMSTAT"; {GIT} diff --numstat -M "$B" 2>/dev/null || true
echo "@@STATUS"; {GIT} status --porcelain=v1 2>/dev/null || true
echo "@@END""#
    )
}

pub(crate) fn patch_script() -> String {
    format!(
        r#"{GIT} diff -M "$1" 2>/dev/null
{GIT} ls-files --others --exclude-standard 2>/dev/null | while IFS= read -r f; do
  {GIT} diff --no-index -- /dev/null "$f" 2>/dev/null
done
true"#
    )
}

/// Parse the output of [`changes_script`].
pub(crate) fn parse_changes(base: &str, output: &str, patch: String, truncated: bool) -> Changes {
    let mut section = "";
    let mut head = None;
    let mut commits = Vec::new();
    let mut files: Vec<ChangedFile> = Vec::new();
    let mut uncommitted = false;
    for line in output.lines() {
        if let Some(name) = line.strip_prefix("@@") {
            section = match name {
                "HEAD" => "head",
                "COMMITS" => "commits",
                "NUMSTAT" => "numstat",
                "STATUS" => "status",
                _ => "",
            };
            continue;
        }
        match section {
            "head" if !line.trim().is_empty() => head = Some(line.trim().to_string()),
            "commits" => {
                let mut parts = line.splitn(3, '\t');
                if let (Some(sha), Some(author), Some(subject)) =
                    (parts.next(), parts.next(), parts.next())
                {
                    commits.push(Commit {
                        sha: sha.into(),
                        author: author.into(),
                        subject: subject.into(),
                    });
                }
            }
            "numstat" => {
                let mut parts = line.splitn(3, '\t');
                if let (Some(add), Some(del), Some(path)) =
                    (parts.next(), parts.next(), parts.next())
                {
                    files.push(ChangedFile {
                        path: path.to_string(),
                        status: "modified".into(),
                        additions: add.parse().ok(),
                        deletions: del.parse().ok(),
                    });
                }
            }
            "status" if line.len() > 3 => {
                uncommitted = true;
                let code = &line[..2];
                let path = line[3..].to_string();
                let status = match code.trim() {
                    "??" => "untracked",
                    s if s.contains('D') => "deleted",
                    s if s.contains('A') => "added",
                    s if s.contains('R') => "renamed",
                    _ => "modified",
                };
                match files.iter_mut().find(|f| f.path == path) {
                    Some(f) => f.status = status.into(),
                    None => files.push(ChangedFile {
                        path,
                        status: status.into(),
                        additions: None,
                        deletions: None,
                    }),
                }
            }
            _ => {}
        }
    }
    // Files only present in committed history are "added" if the patch says so.
    for file in &mut files {
        if file.status == "modified"
            && patch.contains(&format!("--- /dev/null\n+++ b/{}", file.path))
        {
            file.status = "added".into();
        }
    }
    Changes {
        base: base.to_string(),
        head,
        commits,
        files,
        patch,
        patch_truncated: truncated,
        uncommitted,
        captured_at: chrono::Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_changes() {
        let out = "@@HEAD\nabc123\n@@COMMITS\nabc123\tagentcore\tfeat: add x\n@@NUMSTAT\n3\t1\tsrc/lib.rs\n@@STATUS\n?? notes.md\n M src/main.rs\n@@END\n";
        let c = parse_changes("base", out, String::new(), false);
        assert_eq!(c.head.as_deref(), Some("abc123"));
        assert_eq!(c.commits[0].subject, "feat: add x");
        assert!(c.uncommitted);
        let paths: Vec<_> = c
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.status.as_str()))
            .collect();
        assert_eq!(
            paths,
            [
                ("src/lib.rs", "modified"),
                ("notes.md", "untracked"),
                ("src/main.rs", "modified")
            ]
        );
    }
}
