//! Runs against a real Docker daemon. Opt in with:
//! `cargo test -p agentcore-sandbox --test docker -- --ignored`

use std::time::Duration;

use agentcore_core::LaunchPlan;
use agentcore_sandbox::docker::{DockerConfig, DockerProvider};
use agentcore_sandbox::{ExecRequest, SandboxError, SandboxProvider, SandboxRequest};

fn exec(command: &str, args: &[&str], timeout: Duration) -> ExecRequest {
    ExecRequest {
        command: command.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        cwd: "/workspace".into(),
        env: Default::default(),
        timeout,
        max_output_bytes: 4096,
    }
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn docker_sandbox_lifecycle() {
    let image =
        std::env::var("AGENTCORE_TEST_IMAGE").unwrap_or_else(|_| "debian:bookworm-slim".into());
    let dir = tempfile::tempdir().unwrap();
    // The container user must be able to write the bind-mounted workspace.
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o777),
    )
    .unwrap();
    let provider = DockerProvider::new(DockerConfig {
        image,
        ..Default::default()
    });
    let sandbox = provider
        .create(&SandboxRequest {
            session_id: uuid::Uuid::new_v4(),
            workspace_dir: dir.path().to_path_buf(),
            image: None,
        })
        .await
        .expect("create container");

    sandbox
        .write_file("/workspace/src/a b.txt", b"hello")
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("src/a b.txt")).unwrap(),
        b"hello"
    );
    let (data, _) = sandbox
        .read_file("/workspace/src/a b.txt", 100)
        .await
        .unwrap();
    assert_eq!(data, b"hello");
    assert!(matches!(
        sandbox.read_file("/etc/passwd", 100).await,
        Err(SandboxError::OutsideWorkspace(_))
    ));

    let out = sandbox
        .exec(exec("id", &["-u"], Duration::from_secs(10)))
        .await
        .unwrap();
    assert_eq!(out.stdout.trim(), "1000", "runs as non-root");

    // Root filesystem is read-only and there is no network.
    let out = sandbox
        .exec(exec("touch", &["/usr/x"], Duration::from_secs(10)))
        .await
        .unwrap();
    assert_ne!(out.exit_code, Some(0));
    let out = sandbox
        .exec(exec(
            "sh",
            &["-c", "cat /proc/net/route | wc -l"],
            Duration::from_secs(10),
        ))
        .await
        .unwrap();
    assert_eq!(
        out.stdout.trim(),
        "1",
        "no routes besides the header: {}",
        out.stdout
    );

    let out = sandbox
        .exec(exec("sleep", &["30"], Duration::from_secs(1)))
        .await
        .unwrap();
    assert!(out.timed_out);

    let mut agent = sandbox
        .spawn(&LaunchPlan {
            program: "sleep".into(),
            args: vec!["300".into()],
            env: Default::default(),
        })
        .await
        .unwrap();
    let started = std::time::Instant::now();
    sandbox.kill().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), agent.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(matches!(
        sandbox
            .exec(exec("true", &[], Duration::from_secs(1)))
            .await,
        Err(SandboxError::Killed)
    ));
}
