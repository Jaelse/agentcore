//! Sandbox backends.
//!
//! A [`Sandbox`] is an isolated environment with a single writable workspace
//! mounted at [`WORKSPACE`]. agentcore launches the agent process inside it and
//! executes every policy-approved action inside it, so the sandbox is the
//! hard security boundary while the policy engine is the fine-grained one.
//!
//! Backends:
//! * [`docker::DockerProvider`]: hardened OCI container; optionally under
//!   gVisor (`runtime = "runsc"`) or Kata for a stronger kernel boundary.
//! * [`process::ProcessProvider`]: plain host process. **Not isolated**, for
//!   development only; must be enabled explicitly.

pub mod docker;
mod io;
pub mod process;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentcore_core::{LaunchPlan, SessionId};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Path of the workspace as seen by the agent and by policies.
pub const WORKSPACE: &str = "/workspace";

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("sandbox has been killed")]
    Killed,
    #[error("path `{0}` is outside the workspace")]
    OutsideWorkspace(String),
    #[error("sandbox backend misconfigured: {0}")]
    Config(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },
    #[error("{0}")]
    Command(String),
}

impl SandboxError {
    pub(crate) fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        let context = context.into();
        move |source| Self::Io { context, source }
    }
}

pub type Result<T, E = SandboxError> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    #[default]
    Docker,
    Process,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub backend: BackendKind,
    pub docker: docker::DockerConfig,
    pub process: process::ProcessConfig,
}

impl SandboxConfig {
    pub fn provider(&self) -> Result<Arc<dyn SandboxProvider>> {
        Ok(match self.backend {
            BackendKind::Docker => Arc::new(docker::DockerProvider::new(self.docker.clone())),
            BackendKind::Process => Arc::new(process::ProcessProvider::new(self.process.clone())?),
        })
    }
}

/// Parameters for creating one sandbox.
#[derive(Debug, Clone)]
pub struct SandboxRequest {
    pub session_id: SessionId,
    /// Host directory mounted as the workspace.
    pub workspace_dir: PathBuf,
    /// Image override (container backends).
    pub image: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExecRequest {
    pub command: String,
    pub args: Vec<String>,
    /// Absolute working directory inside the sandbox.
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
}

#[async_trait]
pub trait SandboxProvider: Send + Sync {
    fn name(&self) -> &'static str;

    async fn create(&self, request: &SandboxRequest) -> Result<Arc<dyn Sandbox>>;

    /// Remove sandboxes left behind by a previous agentcore process (crash,
    /// restart). Returns how many were removed.
    async fn cleanup_orphans(&self) -> Result<usize> {
        Ok(0)
    }
}

#[async_trait]
pub trait Sandbox: Send + Sync {
    fn backend(&self) -> &'static str;

    /// Backend-specific details for the audit log (image, limits, ...).
    fn describe(&self) -> serde_json::Value;

    /// Start the long-running agent process. stdout and stderr are piped.
    async fn spawn(&self, plan: &LaunchPlan) -> Result<tokio::process::Child>;

    /// Run a short-lived command (a tool call) to completion.
    async fn exec(&self, request: ExecRequest) -> Result<ExecOutput>;

    /// Read at most `max_bytes` of a file. `path` is absolute inside the sandbox.
    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<(Vec<u8>, bool)>;

    /// Create or replace a file. `path` is absolute inside the sandbox.
    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()>;

    /// Immediately and forcibly stop everything running in the sandbox.
    /// Idempotent; after this every other call fails with [`SandboxError::Killed`].
    async fn kill(&self) -> Result<()>;

    /// Release all resources (container, processes). The workspace is kept.
    async fn destroy(&self) -> Result<()>;
}
