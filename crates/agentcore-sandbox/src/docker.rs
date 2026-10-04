//! Docker / OCI backend.
//!
//! Each session gets its own container, started idle (`sleep infinity`); the
//! agent and every tool call are `docker exec`ed into it. The container is
//! hardened by default: no capabilities, no privilege escalation, read-only
//! root filesystem, non-root user, pid/memory/cpu limits and no network.
//!
//! Set `runtime = "runsc"` to run under gVisor (user-space kernel), which is
//! strongly recommended when running untrusted agents on shared hosts.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agentcore_core::LaunchPlan;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::{
    ExecOutput, ExecRequest, Result, Sandbox, SandboxError, SandboxProvider, SandboxRequest,
    WORKSPACE,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DockerConfig {
    /// `docker` compatible CLI (`docker`, `podman`, `nerdctl`).
    pub cli: String,
    /// Default image for agents that do not specify one.
    pub image: String,
    /// OCI runtime, e.g. `runsc` (gVisor) or `kata-runtime`.
    pub runtime: Option<String>,
    /// Docker network. `none` disables networking entirely; use an
    /// `--internal` network shared with agentcore to allow only the gateway.
    pub network: String,
    pub user: String,
    pub memory: String,
    pub cpus: String,
    pub pids_limit: u32,
    /// Size of the writable tmpfs mounted at `/tmp` and the agent `HOME`.
    pub tmpfs_size: String,
    /// Extra `docker run` arguments, appended verbatim (use with care).
    pub extra_args: Vec<String>,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            cli: "docker".into(),
            image: "agentcore-sandbox:latest".into(),
            runtime: None,
            network: "none".into(),
            user: "1000:1000".into(),
            memory: "4g".into(),
            cpus: "2".into(),
            pids_limit: 512,
            tmpfs_size: "1g".into(),
            extra_args: Vec::new(),
        }
    }
}

pub const AGENT_HOME: &str = "/home/agent";

pub struct DockerProvider {
    config: DockerConfig,
}

impl DockerProvider {
    pub fn new(config: DockerConfig) -> Self {
        Self { config }
    }

    /// Give a new workspace to the (non-root) sandbox user so the agent can
    /// write to it. Only possible when agentcore runs as root, as it does in
    /// the docker-compose deployment; otherwise the operator must arrange it.
    fn hand_over(&self, dir: &std::path::Path) {
        let mut ids = self
            .config
            .user
            .splitn(2, ':')
            .map(|p| p.parse::<u32>().ok());
        let (Some(Some(uid)), gid) = (ids.next(), ids.next().flatten()) else {
            return;
        };
        #[cfg(unix)]
        if let Err(err) = std::os::unix::fs::chown(dir, Some(uid), gid) {
            tracing::warn!(
                dir = %dir.display(),
                error = %err,
                "could not hand the workspace to the sandbox user; the agent may not be able to write to it"
            );
        }
    }

    /// Arguments for `docker run`, exposed for testing and auditing.
    pub fn run_args(&self, request: &SandboxRequest, name: &str, workspace: &str) -> Vec<String> {
        let c = &self.config;
        let mut args: Vec<String> = vec![
            "run".into(),
            "--detach".into(),
            "--init".into(),
            format!("--name={name}"),
            format!("--label=agentcore.session={}", request.session_id),
            format!("--network={}", c.network),
            "--cap-drop=ALL".into(),
            "--security-opt=no-new-privileges".into(),
            "--read-only".into(),
            format!("--tmpfs=/tmp:rw,nosuid,nodev,size={}", c.tmpfs_size),
            format!(
                "--tmpfs={AGENT_HOME}:rw,nosuid,nodev,size={},uid=1000,gid=1000",
                c.tmpfs_size
            ),
            format!("--env=HOME={AGENT_HOME}"),
            format!("--user={}", c.user),
            format!("--memory={}", c.memory),
            format!("--memory-swap={}", c.memory),
            format!("--cpus={}", c.cpus),
            format!("--pids-limit={}", c.pids_limit),
            format!("--volume={workspace}:{WORKSPACE}:rw"),
            format!("--workdir={WORKSPACE}"),
        ];
        if let Some(runtime) = &c.runtime {
            args.push(format!("--runtime={runtime}"));
        }
        args.extend(c.extra_args.iter().cloned());
        args.push(request.image.clone().unwrap_or_else(|| c.image.clone()));
        args.extend(["sleep".into(), "infinity".into()]);
        args
    }
}

#[async_trait]
impl SandboxProvider for DockerProvider {
    fn name(&self) -> &'static str {
        "docker"
    }

    async fn create(&self, request: &SandboxRequest) -> Result<Arc<dyn Sandbox>> {
        let fresh = !request.workspace_dir.exists();
        tokio::fs::create_dir_all(&request.workspace_dir)
            .await
            .map_err(SandboxError::io("create workspace"))?;
        if fresh {
            self.hand_over(&request.workspace_dir);
        }
        let workspace: PathBuf = tokio::fs::canonicalize(&request.workspace_dir)
            .await
            .map_err(SandboxError::io("resolve workspace"))?;
        let name = format!("agentcore-{}", request.session_id);
        let args = self.run_args(request, &name, &workspace.to_string_lossy());
        let output = Command::new(&self.config.cli)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(SandboxError::io(format!("run `{}`", self.config.cli)))?;
        if !output.status.success() {
            return Err(SandboxError::Command(format!(
                "failed to start container: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(Arc::new(DockerSandbox {
            cli: self.config.cli.clone(),
            name,
            details: serde_json::json!({
                "container": String::from_utf8_lossy(&output.stdout).trim(),
                "run_args": args,
            }),
            killed: AtomicBool::new(false),
        }))
    }
}

pub struct DockerSandbox {
    cli: String,
    name: String,
    details: serde_json::Value,
    killed: AtomicBool,
}

impl DockerSandbox {
    fn check_alive(&self) -> Result<()> {
        if self.killed.load(Ordering::SeqCst) {
            Err(SandboxError::Killed)
        } else {
            Ok(())
        }
    }

    fn exec_cmd(
        &self,
        cwd: &str,
        env: impl IntoIterator<Item = (impl AsRef<str>, impl AsRef<str>)>,
        interactive: bool,
    ) -> Command {
        let mut cmd = Command::new(&self.cli);
        cmd.arg("exec");
        if interactive {
            cmd.arg("--interactive");
        }
        cmd.arg(format!("--workdir={cwd}"));
        for (k, v) in env {
            cmd.arg(format!("--env={}={}", k.as_ref(), v.as_ref()));
        }
        cmd.arg(&self.name)
            .stdin(if interactive {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    fn check_path(path: &str) -> Result<()> {
        if path == WORKSPACE || path.starts_with(&format!("{WORKSPACE}/")) {
            Ok(())
        } else {
            Err(SandboxError::OutsideWorkspace(path.into()))
        }
    }

    async fn remove(&self) -> Result<()> {
        let output = Command::new(&self.cli)
            .args(["rm", "--force", "--volumes", &self.name])
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(SandboxError::io("docker rm"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(SandboxError::Command(format!(
                "failed to remove container {}: {}",
                self.name,
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }
}

#[async_trait]
impl Sandbox for DockerSandbox {
    fn backend(&self) -> &'static str {
        "docker"
    }

    fn describe(&self) -> serde_json::Value {
        self.details.clone()
    }

    async fn spawn(&self, plan: &LaunchPlan) -> Result<tokio::process::Child> {
        self.check_alive()?;
        let mut cmd = self.exec_cmd(WORKSPACE, &plan.env, false);
        cmd.arg(&plan.program).args(&plan.args);
        cmd.spawn().map_err(SandboxError::io("docker exec agent"))
    }

    async fn exec(&self, request: ExecRequest) -> Result<ExecOutput> {
        self.check_alive()?;
        Self::check_path(&request.cwd)?;
        // Killing the `docker exec` client does not kill the process inside the
        // container, so enforce the timeout inside as well.
        let secs = request.timeout.as_secs().max(1).to_string();
        let mut cmd = self.exec_cmd(&request.cwd, &request.env, false);
        cmd.args(["timeout", "-s", "KILL", &secs, &request.command])
            .args(&request.args);
        let child = cmd.spawn().map_err(SandboxError::io("docker exec"))?;
        crate::io::collect(
            child,
            request.timeout + Duration::from_secs(5),
            request.max_output_bytes,
            || {},
        )
        .await
        .map(|mut out| {
            // `timeout` exits with 137 (128 + SIGKILL) when it fires.
            if out.exit_code == Some(137) {
                out.timed_out = true;
            }
            out
        })
        .map_err(SandboxError::io("docker exec"))
    }

    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<(Vec<u8>, bool)> {
        self.check_alive()?;
        Self::check_path(path)?;
        let mut cmd = self.exec_cmd(WORKSPACE, std::iter::empty::<(&str, &str)>(), false);
        cmd.args(["cat", "--", path]);
        let child = cmd.spawn().map_err(SandboxError::io("docker exec cat"))?;
        let out = crate::io::collect(child, Duration::from_secs(30), max_bytes, || {})
            .await
            .map_err(SandboxError::io("read file"))?;
        if out.exit_code != Some(0) && !out.truncated {
            return Err(SandboxError::Command(format!(
                "read {path}: {}",
                out.stderr.trim()
            )));
        }
        Ok((out.stdout.into_bytes(), out.truncated))
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
        self.check_alive()?;
        Self::check_path(path)?;
        // The path is passed as a positional argument, never interpolated.
        let mut cmd = self.exec_cmd(WORKSPACE, std::iter::empty::<(&str, &str)>(), true);
        cmd.args([
            "sh",
            "-c",
            r#"umask 022 && mkdir -p -- "$(dirname -- "$1")" && cat > "$1""#,
            "sh",
            path,
        ]);
        let mut child = cmd.spawn().map_err(SandboxError::io("docker exec write"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(contents)
                .await
                .map_err(SandboxError::io("write file"))?;
        }
        let out = crate::io::collect(child, Duration::from_secs(30), 4096, || {})
            .await
            .map_err(SandboxError::io("write file"))?;
        if out.exit_code == Some(0) {
            Ok(())
        } else {
            Err(SandboxError::Command(format!(
                "write {path}: {}",
                out.stderr.trim()
            )))
        }
    }

    async fn kill(&self) -> Result<()> {
        if self.killed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // `rm --force` sends SIGKILL to every process in the container.
        self.remove().await
    }

    async fn destroy(&self) -> Result<()> {
        self.killed.store(true, Ordering::SeqCst);
        self.remove().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_args_are_hardened() {
        let provider = DockerProvider::new(DockerConfig {
            runtime: Some("runsc".into()),
            ..Default::default()
        });
        let request = SandboxRequest {
            session_id: uuid::Uuid::nil(),
            workspace_dir: "/data/ws".into(),
            image: Some("img:1".into()),
        };
        let args = provider.run_args(&request, "agentcore-x", "/data/ws");
        for expected in [
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--read-only",
            "--network=none",
            "--runtime=runsc",
            "--volume=/data/ws:/workspace:rw",
        ] {
            assert!(args.iter().any(|a| a == expected), "missing {expected}");
        }
        let image_pos = args.iter().position(|a| a == "img:1").unwrap();
        assert_eq!(&args[image_pos + 1..], ["sleep", "infinity"]);
    }
}
