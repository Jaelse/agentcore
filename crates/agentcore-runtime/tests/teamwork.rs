//! A repository session end to end: workspace setup, role prompt with the
//! team's own conventions, two conversation turns, checks, changes, export.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agentcore_core::{AgentSpec, EventKind, Principal, SessionStatus};
use agentcore_policy::{Policy, PolicySet};
use agentcore_roles::{Role, WorkItem};
use agentcore_runtime::{
    AdapterRegistry, CreateSession, PreparedWorkspace, RuntimeConfig, Session, SessionManager,
    SessionOptions, WorkspaceSetup,
};
use agentcore_sandbox::process::{ProcessConfig, ProcessProvider};
use async_trait::async_trait;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Stands in for the GitHub clone: a repo with the team's CONTRIBUTING.md.
struct TestRepo;

#[async_trait]
impl WorkspaceSetup for TestRepo {
    async fn prepare(&self, dir: &Path) -> Result<PreparedWorkspace, String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        git(dir, &["init", "-q"]);
        std::fs::write(
            dir.join("CONTRIBUTING.md"),
            "We use Conventional Commits.\n",
        )
        .unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-qm", "chore: initial"]);
        Ok(PreparedWorkspace {
            repository: "acme/demo".into(),
            branch: "main".into(),
            base_commit: git(dir, &["rev-parse", "HEAD"]),
        })
    }
}

const ROLE: &str = r#"
name = "developer"
capabilities = ["pull_requests:propose"]
repo_docs = ["CONTRIBUTING.md", "MISSING.md"]
instructions = "Be careful."
[delivery]
kind = "pull_request"
[[checks]]
name = "conventional commits"
kind = "commit_message"
pattern = '^(feat|fix|chore): .+'
[[checks]]
name = "clean"
kind = "clean_worktree"
[[checks]]
name = "hello exists"
kind = "command"
run = "test -f hello.txt"
[[checks]]
name = "only for rust"
kind = "command"
run = "false"
when_exists = "Cargo.toml"
"#;

const GIT_ID: &str = "git -c user.name=agent -c user.email=agent@example.com";

fn agent() -> AgentSpec {
    AgentSpec {
        name: "scripted".into(),
        adapter: "command".into(),
        description: String::new(),
        image: None,
        command: Some("sh".into()),
        // Turn 1: save the prompt, commit with a non-conventional message.
        args: vec![
            "-c".into(),
            format!(
                "printf '%s' \"$0\" > \"$HOME/prompt.txt\"; echo hello > hello.txt; \
                 {GIT_ID} add hello.txt; {GIT_ID} commit -qm 'add hello'; echo turn one done"
            ),
            "{task}".into(),
        ],
        env: Default::default(),
        policy: None,
        // Follow-up: fix the commit message as asked.
        tty: None,
        follow_up_args: vec![
            "-c".into(),
            format!("{GIT_ID} commit -q --amend -m 'feat: add hello'; echo \"got: $0\""),
            "{task}".into(),
        ],
        ..Default::default()
    }
}

async fn wait_for(session: &Session, status: SessionStatus) {
    for _ in 0..400 {
        if session.status() == status {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "timed out waiting for {status:?}; status = {:?}",
        session.status()
    );
}

#[tokio::test]
async fn repository_session_with_role_turns_checks_and_export() {
    let dir = tempfile::tempdir().unwrap();
    let mut policies = PolicySet::default();
    policies
        .insert(
            Policy::from_toml("name = \"p\"\ndefault = \"allow\"", "p")
                .unwrap()
                .compile()
                .unwrap(),
        )
        .unwrap();
    let manager = SessionManager::new(
        RuntimeConfig {
            data_dir: dir.path().to_path_buf(),
            gateway_url: "http://127.0.0.1:0".into(),
            default_policy: "p".into(),
            audit_fsync: false,
        },
        policies,
        vec![agent()],
        AdapterRegistry::default(),
        Arc::new(
            ProcessProvider::new(ProcessConfig {
                allow_insecure: true,
                path: None,
            })
            .unwrap(),
        ),
    )
    .unwrap();

    let role = Arc::new(Role::from_toml(ROLE, "role").unwrap());
    let session = manager
        .create(
            CreateSession {
                agent: "scripted".into(),
                task: "Say hello in a file".into(),
                policy: None,
            },
            Principal::human("alice"),
            SessionOptions {
                role: Some(role),
                workspace: Some(Arc::new(TestRepo)),
                work_item: Some(WorkItem {
                    repository: Some("acme/demo".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .unwrap();

    // Turn 1 ends and the agent waits for a human.
    wait_for(&session, SessionStatus::AwaitingInput).await;
    let home = dir
        .path()
        .join("workspaces")
        .join(format!("{}.home", session.id()));
    let prompt = std::fs::read_to_string(home.join("prompt.txt")).unwrap();
    assert!(prompt.contains("## CONTRIBUTING.md"), "{prompt}");
    assert!(prompt.contains("We use Conventional Commits."));
    assert!(prompt.contains("Say hello in a file"));
    assert!(!prompt.contains("MISSING.md"));
    assert_eq!(session.context().repository.as_deref(), Some("acme/demo"));

    let changes = session.changes().await.unwrap().unwrap();
    assert_eq!(changes.commits.len(), 1);
    assert_eq!(changes.commits[0].subject, "add hello");
    assert!(!changes.uncommitted, "{changes:?}");
    assert_eq!(changes.files[0].path, "hello.txt");
    assert!(changes.patch.contains("+hello"));

    let (passed, results) = session.run_checks(Principal::human("alice")).await.unwrap();
    assert!(!passed);
    let by_name = |n: &str| results.iter().find(|r| r.name == n).unwrap().clone();
    assert!(!by_name("conventional commits").passed);
    assert!(by_name("clean").passed);
    assert!(by_name("hello exists").passed);
    assert!(by_name("only for rust").skipped);

    // Nothing can be exported or checked while a turn runs; a message starts one.
    session
        .send_message(Principal::human("alice"), "Fix your commit message".into())
        .unwrap();
    assert!(
        session
            .send_message(Principal::human("bob"), "twice".into())
            .is_err()
    );
    for _ in 0..400 {
        let turns = session
            .subscribe()
            .0
            .iter()
            .filter(|e| e.kind.name() == "turn_ended")
            .count();
        if turns == 2 && session.status() == SessionStatus::AwaitingInput {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(session.status(), SessionStatus::AwaitingInput);

    let (passed, _) = session.run_checks(Principal::human("alice")).await.unwrap();
    assert!(passed);

    let (bundle, head) = session.export_bundle().await.unwrap();
    let bundle_file = dir.path().join("delivery.bundle");
    std::fs::write(&bundle_file, &bundle).unwrap();
    let heads = git(
        dir.path(),
        &["bundle", "list-heads", bundle_file.to_str().unwrap()],
    );
    assert!(heads.starts_with(&head), "{heads} vs {head}");

    session.finish(Principal::human("alice")).unwrap();
    wait_for(&session, SessionStatus::Completed).await;

    let (events, _) = session.subscribe();
    let names: Vec<_> = events.iter().map(|e| e.kind.name()).collect();
    for expected in [
        "workspace_prepared",
        "role_applied",
        "turn_started",
        "user_message",
        "checks_completed",
        "turn_ended",
    ] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }
    assert_eq!(names.iter().filter(|n| **n == "turn_started").count(), 2);
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Output { line, .. } if line == "got: Fix your commit message")));
    let last = session.last_changes().unwrap();
    assert_eq!(last.commits[0].subject, "feat: add hello");
    agentcore_audit::verify_file(session.audit_path()).unwrap();
}
