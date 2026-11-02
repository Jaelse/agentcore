use std::sync::Arc;
use std::time::Duration;

use agentcore_core::{Action, ActionOutcome, AgentSpec, EventKind, Principal, SessionStatus};
use agentcore_policy::{Policy, PolicySet};
use agentcore_runtime::{
    AdapterRegistry, ApprovalDecision, CreateSession, Resume, RuntimeConfig, RuntimeError, Session,
    SessionManager,
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
            AgentSpec {
                follow_up_args: vec!["-c".into(), "echo \"got: $0\"".into(), "{task}".into()],
                ..agent("slow-listener", "echo started; sleep 60")
            },
            AgentSpec {
                follow_up_args: vec![
                    "-c".into(),
                    "echo \"$0\" >> \"$HOME/diary\"; echo \"got: $0 ($(wc -l < \"$HOME/diary\" | tr -d ' ') in the diary)\"; case \"$0\" in SAVE*) echo saved > notes.md ;; esac".into(),
                    "{task}".into(),
                ],
                ..agent("diarist", "echo \"$0\" >> \"$HOME/diary\"; echo draft > work.txt; echo started")
            },
            AgentSpec {
                follow_up_args: vec!["-c".into(), "echo \"got: $0\"".into(), "{task}".into()],
                ..agent("slow-diarist", "echo started; sleep 60")
            },
            agent(
                "asker",
                "printf 'login code? '; read code; echo \"thanks ${#code}\"",
            ),
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

fn request(agent: &str) -> CreateSession {
    CreateSession {
        agent: agent.into(),
        task: "the task".into(),
        policy: None,
        provider: None,
        model: None,
    }
}

/// Carry a session on in another manager (another node), as a node does
/// after the one that ran it went away.
fn carry_on(manager: &SessionManager, gone: &Session) -> Result<Arc<Session>, RuntimeError> {
    manager.resume(
        request(&gone.info().agent),
        Resume {
            id: gone.id(),
            audit_path: gone.audit_path().to_path_buf(),
            reason: "node a went away".into(),
            node: "b".into(),
        },
        Default::default(),
    )
}

fn outputs(session: &Session) -> Vec<String> {
    session
        .subscribe()
        .0
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::Output { line, .. } => Some(line),
            _ => None,
        })
        .collect()
}

fn create(manager: &SessionManager, agent: &str) -> Arc<Session> {
    manager
        .create(
            request(agent),
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
    let ticks = session.paths().workspace.join("ticks");
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
    assert_eq!(
        session.paths().recording.file_name().unwrap(),
        file.as_str()
    );
    let cast = std::fs::read(&session.paths().recording).unwrap();
    use sha2::Digest;
    assert_eq!(hex::encode(sha2::Sha256::digest(&cast)), sha256);
    let cast = String::from_utf8(cast).unwrap();
    assert!(cast.contains("paused by alice"));
    assert!(cast.contains("\"m\",\"turn 1\""));
}

#[tokio::test]
async fn people_type_into_the_agents_terminal() {
    use agentcore_core::LiveFrame;
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let session = create(&manager, "asker");
    wait_for(&session, |s| s == SessionStatus::Running).await;
    for _ in 0..200 {
        if session.accepts_input() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let (initial, _) = session.live().subscribe();
    assert!(
        initial.contains(&LiveFrame::Keyboard { open: true }),
        "{initial:?}"
    );

    session
        .type_input(Principal::human("alice"), b"s3cr")
        .unwrap();
    session
        .type_input(Principal::human("alice"), b"et\r")
        .unwrap();
    assert_eq!(
        wait_for(&session, SessionStatus::is_terminal).await,
        SessionStatus::Completed
    );
    assert!(!session.accepts_input());
    assert!(matches!(
        session.type_input(Principal::human("alice"), b"x"),
        Err(agentcore_runtime::RuntimeError::NoTerminal)
    ));

    let (events, _) = session.subscribe();
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::Output { line, .. } if line.contains("thanks 6")
    )));
    // Who typed is recorded once per turn; the event holds no keystrokes
    // (the screen, with whatever the program echoes, is recorded as usual).
    let typed: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::TerminalInput { by, turn } => Some((by.clone(), *turn)),
            _ => None,
        })
        .collect();
    assert_eq!(typed, [(Principal::human("alice"), 1)]);
    let audit = std::fs::read_to_string(session.audit_path()).unwrap();
    let line = audit
        .lines()
        .find(|l| l.contains("terminal_input"))
        .unwrap();
    assert!(!line.contains("s3cr"), "{line}");
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

#[tokio::test]
async fn a_waiting_session_is_carried_on_by_another_node() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (manager(dir.path()), manager(dir.path()));
    let session = create(&a, "listener");
    wait_for(&session, |s| s == SessionStatus::AwaitingInput).await;
    session
        .send_message(Principal::human("alice"), "one".into())
        .unwrap();
    for _ in 0..200 {
        if outputs(&session).iter().any(|l| l == "got: one") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    wait_for(&session, |s| s == SessionStatus::AwaitingInput).await;
    assert!(outputs(&session).iter().any(|l| l == "got: one"));

    // Node a goes away: it lets go of the session without ending it.
    assert_eq!(a.fence_all().await, 1);
    assert!(a.get(session.id()).is_err());
    tokio::time::sleep(Duration::from_millis(200)).await;
    let written = agentcore_audit::read_events(session.audit_path()).unwrap();
    assert!(
        !written
            .iter()
            .any(|r| r.event.kind.name() == "session_ended")
    );

    // Node b carries it on: same id and log, waiting for input again,
    // without running a turn of its own.
    let moved = carry_on(&b, &session).unwrap();
    assert_eq!(moved.id(), session.id());
    assert_eq!(moved.info().created_by, Principal::human("alice"));
    assert_eq!(
        wait_for(&moved, |s| s == SessionStatus::AwaitingInput).await,
        SessionStatus::AwaitingInput
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    let turns = |s: &Session| kinds(s).iter().filter(|k| **k == "turn_started").count();
    assert_eq!(turns(&moved), 2);
    moved
        .send_message(Principal::human("alice"), "two".into())
        .unwrap();
    for _ in 0..200 {
        if outputs(&moved).iter().any(|l| l == "got: two") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(outputs(&moved).iter().any(|l| l == "got: two"));
    assert!(
        moved
            .subscribe()
            .0
            .iter()
            .any(|e| matches!(&e.kind, EventKind::TurnStarted { turn: 3 }))
    );
    moved.stop(Principal::human("alice"), "done").await;
    wait_for(&moved, SessionStatus::is_terminal).await;

    // One unbroken log, which records the move once.
    let log = agentcore_audit::read_events(moved.audit_path()).unwrap();
    assert!(agentcore_audit::verify_file(moved.audit_path()).is_ok());
    let names: Vec<_> = log.iter().map(|r| r.event.kind.name()).collect();
    assert_eq!(names.iter().filter(|n| **n == "session_created").count(), 1);
    assert_eq!(
        names.iter().filter(|n| **n == "session_recovered").count(),
        1
    );
    assert_eq!(names.last(), Some(&"session_ended"));
    assert!(log.iter().enumerate().all(|(i, r)| r.event.seq == i as u64));
    // The log lives in the session's own directory, with its workspace.
    let dir_of = moved.audit_path().parent().unwrap();
    assert_eq!(moved.paths().workspace, dir_of.join("workspace"));
}

#[tokio::test]
async fn a_turn_cut_off_is_carried_on() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (manager(dir.path()), manager(dir.path()));
    let session = create(&a, "slow-listener");
    wait_for(&session, |s| s == SessionStatus::Running).await;
    a.fence_all().await;

    let moved = carry_on(&b, &session).unwrap();
    wait_for(&moved, |s| s == SessionStatus::AwaitingInput).await;
    assert!(
        outputs(&moved)
            .iter()
            .any(|l| l.starts_with("got: agentcore: your session moved")),
        "{:?}",
        outputs(&moved)
    );
    moved.stop(Principal::human("alice"), "done").await;
}

#[tokio::test]
async fn an_ended_session_is_not_carried_on() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (manager(dir.path()), manager(dir.path()));
    let session = create(&a, "hello");
    wait_for(&session, SessionStatus::is_terminal).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(matches!(
        carry_on(&b, &session),
        Err(RuntimeError::Ended(SessionStatus::Completed))
    ));
}

/// A department agent's options: it keeps its directory, saves before it
/// goes off, and carries its conversation on.
fn keeps(dir: &std::path::Path) -> agentcore_runtime::SessionOptions {
    agentcore_runtime::SessionOptions {
        agent_dir: Some(dir.join("agent")),
        continue_conversation: true,
        wrap_up: Some("SAVE your work ({reason})".into()),
        ..Default::default()
    }
}

async fn ended(session: &Session) -> Vec<String> {
    wait_for(session, SessionStatus::is_terminal).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    outputs(session)
}

#[tokio::test]
async fn an_agent_saves_before_it_goes_off_and_carries_on_next_time() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let day1 = manager
        .create(
            request("diarist"),
            Principal::human("alice"),
            keeps(dir.path()),
        )
        .unwrap();
    wait_for(&day1, |s| s == SessionStatus::AwaitingInput).await;

    // Its working hours end: it gets one turn to save, then goes off.
    day1.go_off();
    let out = ended(&day1).await;
    assert!(
        out.iter()
            .any(|l| l.starts_with("got: SAVE your work (off duty")),
        "{out:?}"
    );
    assert_eq!(day1.end_cause(), Some(agentcore_runtime::EndCause::OffDuty));
    let workspace = &day1.paths().workspace;
    assert_eq!(workspace, &dir.path().join("agent/workspace"));
    assert!(
        workspace.join("notes.md").exists(),
        "saved in its workspace"
    );
    // The session's own log stays with the session.
    assert_ne!(
        day1.audit_path().parent(),
        Some(dir.path().join("agent").as_path())
    );

    // The next day: a new session, the same workspace, and the same
    // conversation, which the task continues.
    let day2 = manager
        .create(
            request("diarist"),
            Principal::human("alice"),
            keeps(dir.path()),
        )
        .unwrap();
    wait_for(&day2, |s| s == SessionStatus::AwaitingInput).await;
    let out = outputs(&day2);
    assert!(
        out.iter().any(|l| l == "got: the task (3 in the diary)"),
        "{out:?}"
    );
    assert!(day2.paths().workspace.join("work.txt").exists());
    assert!(!kinds(&day2).contains(&"role_applied"));
    day2.stop(Principal::human("alice"), "done").await;
}

#[tokio::test]
async fn going_off_cuts_a_running_turn_short() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let session = manager
        .create(
            request("slow-diarist"),
            Principal::human("alice"),
            keeps(dir.path()),
        )
        .unwrap();
    wait_for(&session, |s| s == SessionStatus::Running).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    session.go_off();
    let out = ended(&session).await;
    assert!(
        out.iter().any(|l| l.starts_with("got: SAVE your work")),
        "{out:?}"
    );
    assert_eq!(
        session.end_cause(),
        Some(agentcore_runtime::EndCause::OffDuty)
    );
    // Without a wrap-up, nothing changes: the session just ends when told.
    let plain = create(&manager, "listener");
    wait_for(&plain, |s| s == SessionStatus::AwaitingInput).await;
    plain.go_off();
    let out = ended(&plain).await;
    assert!(!out.iter().any(|l| l.contains("SAVE")), "{out:?}");
}
