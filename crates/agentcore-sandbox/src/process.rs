//! Host-process backend. **Provides no isolation**: the agent and its tool
//! calls run as the agentcore user on the host. Policy checks still apply to
//! gateway actions, but nothing stops the agent process itself from touching
//! the host. Use it only for local development and tests.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use agentcore_core::{LaunchPlan, ProcessInfo};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::{
    AgentProcess, ExecOutput, ExecRequest, Result, Sandbox, SandboxError, SandboxProvider,
    SandboxRequest, WORKSPACE,
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
        // HOME lives next to the workspace, so agent state (e.g. opencode's
        // sessions) does not show up as changes in the repository.
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let home = root.with_file_name(format!("{name}.home"));
        tokio::fs::create_dir_all(&home)
            .await
            .map_err(SandboxError::io("create agent home"))?;
        Ok(Arc::new(ProcessSandbox {
            home,
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
    home: PathBuf,
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
            .env("HOME", &self.home)
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

    fn groups(&self) -> Vec<u32> {
        self.groups
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
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

fn signal_group(pgid: u32, signal: nix::sys::signal::Signal) {
    use nix::unistd::Pid;
    let _ = nix::sys::signal::killpg(Pid::from_raw(pgid as i32), signal);
}

fn kill_group(pgid: u32) {
    signal_group(pgid, nix::sys::signal::Signal::SIGKILL);
}

/// Non-blocking reader for a PTY master (`tokio::fs::File` would park a
/// blocking thread on every read).
struct PtyReader(tokio::io::unix::AsyncFd<std::fs::File>);

impl PtyReader {
    fn new(master: std::fs::File) -> Result<Self> {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        let flags = fcntl(&master, FcntlArg::F_GETFL)
            .map_err(|e| SandboxError::Command(format!("pseudo-terminal: {e}")))?;
        fcntl(
            &master,
            FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
        )
        .map_err(|e| SandboxError::Command(format!("pseudo-terminal: {e}")))?;
        tokio::io::unix::AsyncFd::new(master)
            .map(Self)
            .map_err(SandboxError::io("register pseudo-terminal"))
    }
}

impl tokio::io::AsyncRead for PtyReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::io::Read;
        use std::task::Poll;
        loop {
            let mut guard = match self.0.poll_read_ready(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            let unfilled = buf.initialize_unfilled();
            match guard.try_io(|fd| fd.get_ref().read(unfilled)) {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                // EIO: every process holding the terminal has exited.
                Ok(Err(e)) if e.raw_os_error() == Some(nix::libc::EIO) => {
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

/// A pseudo-terminal of the live view's size: (master, slave).
fn open_pty() -> Result<(std::fs::File, std::os::fd::OwnedFd)> {
    use agentcore_core::live::{TERMINAL_COLS, TERMINAL_ROWS};
    let size = nix::pty::Winsize {
        ws_row: TERMINAL_ROWS,
        ws_col: TERMINAL_COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = nix::pty::openpty(Some(&size), None)
        .map_err(|e| SandboxError::Command(format!("cannot open a pseudo-terminal: {e}")))?;
    Ok((std::fs::File::from(pty.master), pty.slave))
}

#[async_trait]
impl Sandbox for ProcessSandbox {
    fn backend(&self) -> &'static str {
        "process"
    }

    fn describe(&self) -> serde_json::Value {
        serde_json::json!({ "workspace": self.root, "isolation": "none" })
    }

    fn home(&self) -> String {
        self.home.to_string_lossy().into_owned()
    }

    async fn spawn(&self, plan: &LaunchPlan) -> Result<AgentProcess> {
        self.check_alive()?;
        let mut cmd = self.command(&plan.program, &plan.args, &self.root);
        cmd.envs(&plan.env);
        if !plan.tty {
            let mut child = cmd
                .spawn()
                .map_err(SandboxError::io(format!("spawn `{}`", plan.program)))?;
            self.track(&child);
            return Ok(AgentProcess {
                stdout: child.stdout.take().map(|s| Box::pin(s) as _),
                stderr: child.stderr.take().map(|s| Box::pin(s) as _),
                child,
                tty: false,
            });
        }
        let (master, slave) = open_pty()?;
        let clone = |fd: &std::os::fd::OwnedFd| {
            fd.try_clone()
                .map_err(SandboxError::io("duplicate pseudo-terminal"))
        };
        // Like `docker exec --tty`: stdin, stdout and stderr are the terminal.
        cmd.stdin(Stdio::from(clone(&slave)?))
            .stdout(Stdio::from(clone(&slave)?))
            .stderr(Stdio::from(slave))
            .envs(crate::tty_env());
        let child = cmd
            .spawn()
            .map_err(SandboxError::io(format!("spawn `{}`", plan.program)))?;
        // `cmd` holds the slave's descriptors; close them so reading the
        // master ends once the agent exits.
        drop(cmd);
        self.track(&child);
        Ok(AgentProcess {
            child,
            stdout: Some(Box::pin(PtyReader::new(master)?)),
            stderr: None,
            tty: true,
        })
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
        crate::io::collect_live(
            child,
            request.timeout,
            request.max_output_bytes,
            move || {
                if let Some(pgid) = pgid {
                    kill_group(pgid);
                }
            },
            request.live,
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

    async fn pause(&self) -> Result<()> {
        self.check_alive()?;
        for pgid in self.groups() {
            signal_group(pgid, nix::sys::signal::Signal::SIGSTOP);
        }
        Ok(())
    }

    async fn resume(&self) -> Result<()> {
        for pgid in self.groups() {
            signal_group(pgid, nix::sys::signal::Signal::SIGCONT);
        }
        Ok(())
    }

    async fn processes(&self) -> Result<Vec<ProcessInfo>> {
        self.check_alive()?;
        let groups = self.groups();
        if groups.is_empty() {
            return Ok(Vec::new());
        }
        let out = Command::new("ps")
            .args(["-eo", "pid=,pgid=,etime=,args="])
            .output()
            .await
            .map_err(SandboxError::io("ps"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let pid = parts.next()?.parse().ok()?;
                let pgid: u32 = parts.next()?.parse().ok()?;
                let elapsed = parts.next()?.to_string();
                let command = parts.collect::<Vec<_>>().join(" ");
                groups.contains(&pgid).then_some(ProcessInfo {
                    pid,
                    elapsed,
                    command,
                })
            })
            .collect())
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
            live: None,
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
        let mut agent = sb
            .spawn(&LaunchPlan {
                program: "sh".into(),
                args: vec!["-c".into(), "sleep 30 & sleep 30".into()],
                env: Default::default(),
                tty: false,
            })
            .await
            .unwrap();
        sb.kill().await.unwrap();
        let status = tokio::time::timeout(Duration::from_secs(5), agent.child.wait())
            .await
            .expect("agent did not die")
            .unwrap();
        assert!(!status.success());
        assert!(matches!(
            sb.exec(exec("true", &[], Duration::from_secs(1))).await,
            Err(SandboxError::Killed)
        ));
    }

    #[tokio::test]
    async fn agents_get_a_terminal_and_live_output() {
        use tokio::io::AsyncReadExt;
        let dir = tempfile::tempdir().unwrap();
        let sb = sandbox(dir.path()).await;
        let mut agent = sb
            .spawn(&LaunchPlan {
                program: "sh".into(),
                args: vec![
                    "-c".into(),
                    "[ -t 1 ] && echo tty; stty size; echo err >&2".into(),
                ],
                env: Default::default(),
                tty: true,
            })
            .await
            .unwrap();
        let mut out = Vec::new();
        let mut reader = agent.stdout.take().unwrap();
        // Reading a PTY master after the slave closes fails with EIO.
        let _ = tokio::time::timeout(Duration::from_secs(5), reader.read_to_end(&mut out)).await;
        agent.child.wait().await.unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("tty"), "{text}");
        assert!(text.contains("32 120"), "{text}");
        assert!(text.contains("err"), "{text}");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut request = exec("sh", &["-c", "echo live"], Duration::from_secs(5));
        request.live = Some(tx);
        sb.exec(request).await.unwrap();
        let (_, chunk) = rx.recv().await.unwrap();
        assert_eq!(chunk, b"live\n");
    }

    #[tokio::test]
    async fn pause_freezes_and_resume_continues() {
        let dir = tempfile::tempdir().unwrap();
        let sb = sandbox(dir.path()).await;
        let mut agent = sb
            .spawn(&LaunchPlan {
                program: "sh".into(),
                args: vec![
                    "-c".into(),
                    "while true; do date +%s%N >> ticks; sleep 0.05; done".into(),
                ],
                env: Default::default(),
                tty: false,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!sb.processes().await.unwrap().is_empty());
        sb.pause().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let frozen = std::fs::read_to_string(dir.path().join("ticks")).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            frozen,
            std::fs::read_to_string(dir.path().join("ticks")).unwrap()
        );
        sb.resume().await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_ne!(
            frozen,
            std::fs::read_to_string(dir.path().join("ticks")).unwrap()
        );
        sb.kill().await.unwrap();
        let _ = agent.child.wait().await;
    }
}
