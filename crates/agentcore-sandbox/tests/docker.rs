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
        live: None,
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
            egress: false,
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

    // Agents run in a terminal of the live view's size.
    let mut tty = sandbox
        .spawn(&LaunchPlan {
            program: "sh".into(),
            args: vec!["-c".into(), "[ -t 1 ] && echo tty; stty size".into()],
            env: Default::default(),
            tty: true,
            ..Default::default()
        })
        .await
        .unwrap();
    let mut screen = Vec::new();
    {
        use tokio::io::AsyncReadExt;
        let mut reader = tty.stdout.take().unwrap();
        let _ =
            tokio::time::timeout(Duration::from_secs(10), reader.read_to_end(&mut screen)).await;
    }
    tty.child.wait().await.unwrap();
    let screen = String::from_utf8_lossy(&screen);
    assert!(
        screen.contains("tty") && screen.contains("32 120"),
        "{screen}"
    );

    // People can type into an agent's terminal (`docker exec -it` runs in
    // a host pseudo-terminal, whose master carries the keystrokes).
    let mut asker = sandbox
        .spawn(&LaunchPlan {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "printf 'code? '; read code; echo \"got:$code\"".into(),
            ],
            tty: true,
            ..Default::default()
        })
        .await
        .unwrap();
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut reader = asker.stdout.take().unwrap();
        let mut keyboard = asker.stdin.take().expect("a terminal has a keyboard");
        let mut screen = Vec::new();
        let mut buf = [0u8; 4096];
        // Wait for the question, then answer it.
        while !String::from_utf8_lossy(&screen).contains("code?") {
            let n = tokio::time::timeout(Duration::from_secs(10), reader.read(&mut buf))
                .await
                .expect("the question appears")
                .unwrap();
            assert!(n > 0, "{}", String::from_utf8_lossy(&screen));
            screen.extend_from_slice(&buf[..n]);
        }
        keyboard.write_all(b"42\r").await.unwrap();
        let _ =
            tokio::time::timeout(Duration::from_secs(10), reader.read_to_end(&mut screen)).await;
        let screen = String::from_utf8_lossy(&screen);
        assert!(screen.contains("got:42"), "{screen}");
    }
    tokio::time::timeout(Duration::from_secs(10), asker.child.wait())
        .await
        .unwrap()
        .unwrap();

    let mut agent = sandbox
        .spawn(&LaunchPlan {
            program: "sleep".into(),
            args: vec!["300".into()],
            env: Default::default(),
            ..Default::default()
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let procs = sandbox.processes().await.unwrap();
    assert!(procs.iter().any(|p| p.command == "sleep 300"), "{procs:?}");
    sandbox.pause().await.unwrap();
    sandbox.resume().await.unwrap();
    sandbox.resume().await.unwrap();
    let started = std::time::Instant::now();
    sandbox.kill().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), agent.child.wait())
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

    // Agents that reach their model provider themselves need an egress
    // network; without one they do not start.
    let refused = provider
        .create(&SandboxRequest {
            session_id: uuid::Uuid::new_v4(),
            workspace_dir: dir.path().to_path_buf(),
            image: None,
            egress: true,
        })
        .await;
    assert!(
        matches!(&refused, Err(SandboxError::Config(m)) if m.contains("egress_network")),
        "{:?}",
        refused.err()
    );

    // A sandbox left behind by a crashed agentcore is removed on startup.
    let orphan = provider
        .create(&SandboxRequest {
            session_id: uuid::Uuid::new_v4(),
            workspace_dir: dir.path().to_path_buf(),
            image: None,
            egress: false,
        })
        .await
        .unwrap();
    drop(orphan);
    assert!(provider.cleanup_orphans().await.unwrap() >= 1);
    assert_eq!(provider.cleanup_orphans().await.unwrap(), 0);
}
