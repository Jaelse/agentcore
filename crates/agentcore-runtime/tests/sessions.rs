use std::sync::Arc;
use std::time::Duration;

use agentcore_core::{Action, ActionOutcome, AgentSpec, EventKind, Principal, SessionStatus};
use agentcore_policy::{Policy, PolicySet};
use agentcore_runtime::{
    AdapterRegistry, ApprovalDecision, CreateSession, RuntimeConfig, Session, SessionManager,
};
use agentcore_sandbox::process::{ProcessConfig, ProcessProvider};

const POLICY: &str = r#"
name = "test"
default = "require_approval"

[limits]
approval_timeout_secs = 30

[[rules]]
id = "files"
effect = "allow"
kinds = ["file_read", "file_write"]
paths = ["/workspace/**"]

[[rules]]
id = "secrets"
effect = "deny"
kinds = ["file_read"]
paths = ["**/.env"]

[[rules]]
id = "echo"
effect = "allow"
kinds = ["exec"]
commands = ["echo *"]
"#;

fn agent(name: &str, script: &str) -> AgentSpec {
    AgentSpec {
        name: name.into(),
        adapter: "command".into(),
        description: String::new(),
        image: None,
        command: Some("sh".into()),
        args: vec!["-c".into(), script.into()],
        env: Default::default(),
        policy: None,
    }
}

fn manager(dir: &std::path::Path) -> SessionManager {
    let mut policies = PolicySet::default();
    policies
        .insert(
            Policy::from_toml(POLICY, "test")
                .unwrap()
                .compile()
                .unwrap(),
        )
        .unwrap();
    SessionManager::new(
        RuntimeConfig {
            data_dir: dir.to_path_buf(),
            gateway_url: "http://127.0.0.1:0".into(),
            default_policy: "test".into(),
            audit_fsync: false,
        },
        policies,
        vec![
            agent("hello", "echo \"working on: $0\"; echo oops >&2"),
            agent("sleepy", "sleep 60"),
        ],
        AdapterRegistry::default(),
        Arc::new(
            ProcessProvider::new(ProcessConfig {
                allow_insecure: true,
                path: None,
            })
            .unwrap(),
        ),
    )
    .unwrap()
}

fn create(manager: &SessionManager, agent: &str) -> Arc<Session> {
    manager
        .create(
            CreateSession {
                agent: agent.into(),
                task: "the task".into(),
                policy: None,
            },
            Principal::human("alice"),
        )
        .unwrap()
}

async fn wait_for(session: &Session, pred: impl Fn(SessionStatus) -> bool) -> SessionStatus {
    for _ in 0..200 {
        let status = session.status();
        if pred(status) {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out; status = {:?}", session.status());
}

fn kinds(session: &Session) -> Vec<&'static str> {
    session
        .subscribe()
        .0
        .iter()
        .map(|e| e.kind.name())
        .collect()
}

#[tokio::test]
async fn agent_runs_to_completion_and_is_audited() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let session = create(&manager, "hello");
    assert_eq!(
        wait_for(&session, SessionStatus::is_terminal).await,
        SessionStatus::Completed
    );

    let (events, _) = session.subscribe();
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::Output { line, .. } if line.contains("oops")
    )));
    let names = kinds(&session);
    assert_eq!(names.first(), Some(&"session_created"));
    assert_eq!(names.last(), Some(&"session_ended"));

    let audited = agentcore_audit::read_events(session.audit_path()).unwrap();
    assert_eq!(audited.len(), events.len());
    assert!(audited.iter().zip(&events).all(|(a, e)| a.event == *e));
}

#[tokio::test]
async fn policy_gates_actions() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let session = create(&manager, "sleepy");
    wait_for(&session, |s| s == SessionStatus::Running).await;

    let write = Action::FileWrite {
        path: "notes/a.txt".into(),
        bytes: 2,
    };
    let outcome = session
        .request_action(write, Some(b"hi".to_vec()))
        .await
        .unwrap();
    assert!(
        matches!(outcome, ActionOutcome::Succeeded { .. }),
        "{outcome:?}"
    );

    let read = Action::FileRead {
        path: "/workspace/notes/a.txt".into(),
    };
    let ActionOutcome::Succeeded { output } = session.request_action(read, None).await.unwrap()
    else {
        panic!("read failed");
    };
    assert_eq!(output["content"], "hi");

    let secret = Action::FileRead {
        path: ".env".into(),
    };
    assert!(matches!(
        session.request_action(secret, None).await.unwrap(),
        ActionOutcome::Denied { .. }
    ));

    let echo = Action::Exec {
        command: "echo".into(),
        args: vec!["hi".into()],
        cwd: None,
    };
    let ActionOutcome::Succeeded { output } = session.request_action(echo, None).await.unwrap()
    else {
        panic!("exec failed");
    };
    assert_eq!(output["stdout"], "hi\n");

    session.stop(Principal::human("alice"), "done").await;
    assert_eq!(
        wait_for(&session, SessionStatus::is_terminal).await,
        SessionStatus::Stopped
    );
}

#[tokio::test]
async fn human_approves_and_rejects() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let session = create(&manager, "sleepy");
    wait_for(&session, |s| s == SessionStatus::Running).await;

    for approve in [true, false] {
        let pending = {
            let session = session.clone();
            tokio::spawn(async move {
                let action = Action::Exec {
                    command: "true".into(),
                    args: vec![],
                    cwd: None,
                };
                session.request_action(action, None).await.unwrap()
            })
        };
        wait_for(&session, |s| s == SessionStatus::AwaitingApproval).await;
        let approvals = session.pending_approvals();
        assert_eq!(approvals.len(), 1);
        session
            .resolve_approval(
                approvals[0].approval_id,
                ApprovalDecision {
                    approved: approve,
                    by: Principal::human("bob"),
                    comment: Some("reviewed".into()),
                },
            )
            .unwrap();
        let outcome = pending.await.unwrap();
        if approve {
            assert!(
                matches!(outcome, ActionOutcome::Succeeded { .. }),
                "{outcome:?}"
            );
        } else {
            assert!(
                matches!(&outcome, ActionOutcome::Denied { reason } if reason.contains("human:bob")),
                "{outcome:?}"
            );
        }
        assert_eq!(session.status(), SessionStatus::Running);
    }
    session.stop(Principal::System, "test over").await;
}

#[tokio::test]
async fn stop_kills_agent_and_denies_pending_approvals() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let session = create(&manager, "sleepy");
    wait_for(&session, |s| s == SessionStatus::Running).await;

    let pending = {
        let session = session.clone();
        tokio::spawn(async move {
            let action = Action::Exec {
                command: "true".into(),
                args: vec![],
                cwd: None,
            };
            session.request_action(action, None).await.unwrap()
        })
    };
    wait_for(&session, |s| s == SessionStatus::AwaitingApproval).await;

    let started = std::time::Instant::now();
    assert_eq!(
        manager
            .stop_all(Principal::human("alice"), "emergency")
            .await,
        1
    );
    assert_eq!(
        wait_for(&session, SessionStatus::is_terminal).await,
        SessionStatus::Stopped
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(matches!(
        pending.await.unwrap(),
        ActionOutcome::Denied { .. }
    ));

    // Nothing more is accepted after a stop.
    let action = Action::FileRead { path: "x".into() };
    assert!(session.request_action(action, None).await.is_err());

    let names = kinds(&session);
    assert!(names.contains(&"stop_requested"));
    let stop = session
        .subscribe()
        .0
        .into_iter()
        .find_map(|e| match e.kind {
            EventKind::StopRequested { by, .. } => Some(by),
            _ => None,
        });
    assert_eq!(stop, Some(Principal::human("alice")));
    agentcore_audit::verify_file(session.audit_path()).unwrap();
}
