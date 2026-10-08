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
        tty: None,
        follow_up_args: vec![],
        ..Default::default()
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
            AgentSpec {
                follow_up_args: vec!["-c".into(), "echo \"got: $0\"".into(), "{task}".into()],
                ..agent("listener", "echo ready")
            },
            agent(
                "ticker",
                "[ -t 1 ] && printf '\\033[32mon a terminal\\033[0m\\n'; \
                 while true; do date +%s%N >> ticks; sleep 0.05; done",
            ),
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
            Default::default(),
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

#[tokio::test]
async fn live_view_pause_resume_and_recording() {
    use agentcore_core::LiveFrame;
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let session = create(&manager, "ticker");
    wait_for(&session, |s| s == SessionStatus::Running).await;
    let ticks = dir
        .path()
        .join("workspaces")
        .join(session.id().to_string())
        .join("ticks");
    for _ in 0..100 {
        if ticks.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // A viewer joining now sees the terminal so far (with colours) ...
    let (initial, mut frames) = session.live().subscribe();
    let LiveFrame::TerminalReset { data, cols, .. } = &initial[0] else {
        panic!("{initial:?}")
    };
    assert_eq!(*cols, 120);
    assert!(data.contains("\u{1b}[32mon a terminal"), "{data:?}");
    // ... and the audit log has the text without escape sequences.
    assert!(session.subscribe().0.iter().any(|e| matches!(
        &e.kind,
        EventKind::Output { line, .. } if line == "on a terminal"
    )));
    // ... file changes and processes follow.
    let mut saw_file = false;
    let mut saw_process = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !(saw_file && saw_process) && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(3), frames.recv()).await {
            Ok(Ok(LiveFrame::Files { changes })) => {
                saw_file |= changes.iter().any(|c| c.path == "ticks")
            }
            Ok(Ok(LiveFrame::Processes { processes })) => {
                saw_process |= processes.iter().any(|p| p.command.contains("sh -c"))
            }
            _ => {}
        }
    }
    assert!(
        saw_file && saw_process,
        "file {saw_file} process {saw_process}"
    );

    // Pause freezes the agent; resume continues where it was.
    session.pause(Principal::human("alice")).await.unwrap();
    assert_eq!(session.status(), SessionStatus::Paused);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let frozen = std::fs::read_to_string(&ticks).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(frozen, std::fs::read_to_string(&ticks).unwrap());
    session.resume(Principal::human("bob")).await.unwrap();
    assert_eq!(session.status(), SessionStatus::Running);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_ne!(frozen, std::fs::read_to_string(&ticks).unwrap());

    // Stop works while paused, too.
    session.pause(Principal::human("alice")).await.unwrap();
    session
        .stop(Principal::human("alice"), "done watching")
        .await;
    assert_eq!(
        wait_for(&session, SessionStatus::is_terminal).await,
        SessionStatus::Stopped
    );

    let names = kinds(&session);
    for expected in ["paused", "resumed", "recording_closed"] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
    let (events, _) = session.subscribe();
    let (file, sha256) = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::RecordingClosed { file, sha256, .. } => Some((file.clone(), sha256.clone())),
            _ => None,
        })
        .unwrap();
    let cast = std::fs::read(dir.path().join("recordings").join(file)).unwrap();
    use sha2::Digest;
    assert_eq!(hex::encode(sha2::Sha256::digest(&cast)), sha256);
    let cast = String::from_utf8(cast).unwrap();
    assert!(cast.contains("paused by alice"));
    assert!(cast.contains("\"m\",\"turn 1\""));
}

#[tokio::test]
async fn messages_wake_an_agent_waiting_for_input() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let session = create(&manager, "listener");
    wait_for(&session, |s| s == SessionStatus::AwaitingInput).await;

    let message = agentcore_core::DeliveredMessage {
        message_id: uuid::Uuid::now_v7(),
        from: "analyst (Research)".into(),
        to: "communicator (Research)".into(),
        scope: agentcore_core::MessageScope::Internal,
        text: "ask Eng for the limits".into(),
        sent_at: chrono::Utc::now(),
    };
    session.deliver_messages(vec![message.clone()]).unwrap();
    // A second delivery while the turn runs is refused (it stays queued).
    assert!(session.deliver_messages(vec![message]).is_err());
    for _ in 0..200 {
        if kinds(&session)
            .iter()
            .filter(|k| **k == "turn_ended")
            .count()
            == 2
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let (events, _) = session.subscribe();
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::MessagesDelivered { messages } if messages[0].from == "analyst (Research)"
    )));
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::Output { line, .. } if line.contains("got: You have 1 new message(s)")
    )));
    session.stop(Principal::human("alice"), "done").await;
}
