//! Host-process backend. **Provides no isolation**: the agent and its tool
//! calls run as the agentcore user on the host. Policy checks still apply to
//! gateway actions, but nothing stops the agent process itself from touching
//! the host. Use it only for local development and tests.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use agentcore_core::LaunchPlan;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::{
    ExecOutput, ExecRequest, Result, Sandbox, SandboxError, SandboxProvider, SandboxRequest,
    WORKSPACE,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessConfig {
    /// Must be set to `true` to acknowledge this backend is not a sandbox.
    pub allow_insecure: bool,
    /// Value of `PATH` for agent processes (defaults to the host `PATH`).
    pub path: Option<String>,
}

pub struct ProcessProvider {
    config: ProcessConfig,
}

impl ProcessProvider {
    pub fn new(config: ProcessConfig) -> Result<Self> {
        if !config.allow_insecure {
            return Err(SandboxError::Config(
                "the `process` backend provides no isolation; set \
                 sandbox.process.allow_insecure = true to use it for development"
                    .into(),
            ));
        }
        Ok(Self { config })
    }
}

#[async_trait]
impl SandboxProvider for ProcessProvider {
    fn name(&self) -> &'static str {
        "process"
    }

    async fn create(&self, request: &SandboxRequest) -> Result<Arc<dyn Sandbox>> {
        tokio::fs::create_dir_all(&request.workspace_dir)
            .await
            .map_err(SandboxError::io("create workspace"))?;
        let root = tokio::fs::canonicalize(&request.workspace_dir)
            .await
            .map_err(SandboxError::io("resolve workspace"))?;
        tracing::warn!(session = %request.session_id, "using the insecure `process` sandbox backend");
        Ok(Arc::new(ProcessSandbox {
            root,
            path: self
                .config
                .path
                .clone()
                .or_else(|| std::env::var("PATH").ok())
                .unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into()),
            killed: AtomicBool::new(false),
            groups: Mutex::new(Vec::new()),
        }))
    }
}

pub struct ProcessSandbox {
    root: PathBuf,
    path: String,
    killed: AtomicBool,
    /// Process group ids of everything we started.
    groups: Mutex<Vec<u32>>,
}

impl ProcessSandbox {
    fn check_alive(&self) -> Result<()> {
        if self.killed.load(Ordering::SeqCst) {
            Err(SandboxError::Killed)
        } else {
            Ok(())
        }
    }

    /// Map an absolute sandbox path (`/workspace/...`) onto the host,
    /// refusing anything that escapes the workspace, including via symlinks.
    fn host_path(&self, path: &str) -> Result<PathBuf> {
        let outside = || SandboxError::OutsideWorkspace(path.to_string());
        let rel = Path::new(path)
            .strip_prefix(WORKSPACE)
            .map_err(|_| outside())?;
        if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(outside());
        }
        let candidate = self.root.join(rel);
        // Resolve the deepest existing ancestor and make sure it's still inside.
        let mut existing = candidate.as_path();
        while !existing.exists() {
            existing = existing.parent().ok_or_else(outside)?;
        }
        let resolved = existing
            .canonicalize()
            .map_err(SandboxError::io("resolve path"))?;
        if !resolved.starts_with(&self.root) {
            return Err(outside());
        }
        Ok(candidate)
    }

    fn command(&self, program: &str, args: &[String], cwd: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", &self.root)
            .env("AGENTCORE_WORKSPACE", &self.root)
            // Stop git from discovering a repository *above* the workspace
            // (e.g. the agentcore checkout itself when data_dir is inside it).
            .env(
                "GIT_CEILING_DIRECTORIES",
                self.root.parent().unwrap_or(&self.root),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        cmd
    }

    fn track(&self, child: &tokio::process::Child) {
        if let Some(pid) = child.id() {
            self.groups
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(pid);
        }
    }
}

#[cfg(unix)]
fn kill_group(pgid: u32) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    let _ = killpg(Pid::from_raw(pgid as i32), Signal::SIGKILL);
}

#[cfg(not(unix))]
fn kill_group(_pgid: u32) {}

#[async_trait]
impl Sandbox for ProcessSandbox {
    fn backend(&self) -> &'static str {
        "process"
    }

    fn describe(&self) -> serde_json::Value {
        serde_json::json!({ "workspace": self.root, "isolation": "none" })
    }

    async fn spawn(&self, plan: &LaunchPlan) -> Result<tokio::process::Child> {
        self.check_alive()?;
        let mut cmd = self.command(&plan.program, &plan.args, &self.root);
        cmd.envs(&plan.env);
        let child = cmd
            .spawn()
            .map_err(SandboxError::io(format!("spawn `{}`", plan.program)))?;
        self.track(&child);
        Ok(child)
    }

    async fn exec(&self, request: ExecRequest) -> Result<ExecOutput> {
        self.check_alive()?;
        let cwd = self.host_path(&request.cwd)?;
        let mut cmd = self.command(&request.command, &request.args, &cwd);
        cmd.envs(&request.env);
        let child = cmd
            .spawn()
            .map_err(SandboxError::io(format!("spawn `{}`", request.command)))?;
        self.track(&child);
        let pgid = child.id();
        crate::io::collect(
            child,
            request.timeout,
            request.max_output_bytes,
            move || {
                if let Some(pgid) = pgid {
                    kill_group(pgid);
                }
            },
        )
        .await
        .map_err(SandboxError::io("exec"))
    }

    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<(Vec<u8>, bool)> {
        self.check_alive()?;
        let host = self.host_path(path)?;
        let file = tokio::fs::File::open(&host)
            .await
            .map_err(SandboxError::io(format!("open {path}")))?;
        crate::io::read_limited(file, max_bytes)
            .await
            .map_err(SandboxError::io(format!("read {path}")))
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
        self.check_alive()?;
        let host = self.host_path(path)?;
        if let Some(parent) = host.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(SandboxError::io(format!("mkdir for {path}")))?;
        }
        // Re-check after creating directories, then refuse to follow a symlink
        // at the final component.
        let host = self.host_path(path)?;
        if let Ok(meta) = tokio::fs::symlink_metadata(&host).await
            && meta.file_type().is_symlink()
        {
            return Err(SandboxError::OutsideWorkspace(path.to_string()));
        }
        tokio::fs::write(&host, contents)
            .await
            .map_err(SandboxError::io(format!("write {path}")))
    }

    async fn kill(&self) -> Result<()> {
        self.killed.store(true, Ordering::SeqCst);
        let groups = std::mem::take(&mut *self.groups.lock().unwrap_or_else(|p| p.into_inner()));
        for pgid in groups {
            kill_group(pgid);
        }
        Ok(())
    }

    async fn destroy(&self) -> Result<()> {
        self.kill().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn sandbox(dir: &Path) -> Arc<dyn Sandbox> {
        ProcessProvider::new(ProcessConfig {
            allow_insecure: true,
            path: None,
        })
        .unwrap()
        .create(&SandboxRequest {
            session_id: uuid::Uuid::new_v4(),
            workspace_dir: dir.to_path_buf(),
            image: None,
        })
        .await
        .unwrap()
    }

    fn exec(cmd: &str, args: &[&str], timeout: Duration) -> ExecRequest {
        ExecRequest {
            command: cmd.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: WORKSPACE.into(),
            env: Default::default(),
            timeout,
            max_output_bytes: 16,
        }
    }

    #[test]
    fn refuses_without_opt_in() {
        assert!(ProcessProvider::new(ProcessConfig::default()).is_err());
    }

    #[tokio::test]
    async fn file_roundtrip_and_confinement() {
        let dir = tempfile::tempdir().unwrap();
        let sb = sandbox(dir.path()).await;
        sb.write_file("/workspace/a/b.txt", b"hello").await.unwrap();
        let (data, truncated) = sb.read_file("/workspace/a/b.txt", 3).await.unwrap();
        assert_eq!((data.as_slice(), truncated), (&b"hel"[..], true));
        assert!(matches!(
            sb.read_file("/etc/passwd", 10).await,
            Err(SandboxError::OutsideWorkspace(_))
        ));
        assert!(matches!(
            sb.read_file("/workspace/../etc/passwd", 10).await,
            Err(SandboxError::OutsideWorkspace(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_is_blocked() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc", dir.path().join("etc")).unwrap();
        let sb = sandbox(dir.path()).await;
        assert!(matches!(
            sb.read_file("/workspace/etc/passwd", 10).await,
            Err(SandboxError::OutsideWorkspace(_))
        ));
        std::os::unix::fs::symlink("/tmp/agentcore-escape", dir.path().join("link")).unwrap();
        assert!(sb.write_file("/workspace/link", b"x").await.is_err());
    }

    #[tokio::test]
    async fn exec_truncates_and_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let sb = sandbox(dir.path()).await;
        let out = sb
            .exec(exec(
                "sh",
                &["-c", "echo 0123456789abcdefghij"],
                Duration::from_secs(5),
            ))
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.truncated);
        assert_eq!(out.stdout.len(), 16);

        let out = sb
            .exec(exec("sh", &["-c", "sleep 30"], Duration::from_millis(200)))
            .await
            .unwrap();
        assert!(out.timed_out);
    }

    #[tokio::test]
    async fn git_does_not_escape_to_a_parent_repository() {
        let outer = tempfile::tempdir().unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(outer.path())
            .status();
        if !matches!(init, Ok(s) if s.success()) {
            return; // git not installed
        }
        let workspace = outer.path().join("data/ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let sb = sandbox(&workspace).await;
        let mut request = exec(
            "git",
            &["rev-parse", "--show-toplevel"],
            Duration::from_secs(5),
        );
        request.max_output_bytes = 4096;
        let out = sb.exec(request).await.unwrap();
        assert_ne!(
            out.exit_code,
            Some(0),
            "git found the parent repo: {}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn kill_stops_everything() {
        let dir = tempfile::tempdir().unwrap();
        let sb = sandbox(dir.path()).await;
        let mut child = sb
            .spawn(&LaunchPlan {
                program: "sh".into(),
                args: vec!["-c".into(), "sleep 30 & sleep 30".into()],
                env: Default::default(),
            })
            .await
            .unwrap();
        sb.kill().await.unwrap();
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("agent did not die")
            .unwrap();
        assert!(!status.success());
        assert!(matches!(
            sb.exec(exec("true", &[], Duration::from_secs(1))).await,
            Err(SandboxError::Killed)
        ));
    }
}
