use agentcore_core::{
    Delivery, DepartmentState, Desired, MessageScope, MessageStatus, OrgSettings, Principal,
    RouteApproval, RoutingPolicy, SessionInfo, SessionStatus,
};
use agentcore_store::testing::fresh_store;
use agentcore_store::{
    AgentInput, AgentScope, DepartmentInput, MessageFilter, NewMessage, SessionRecord, Store,
    StoreError,
};
use uuid::Uuid;

fn dept(name: &str) -> DepartmentInput {
    DepartmentInput {
        name: name.into(),
        description: String::new(),
        mission: format!("{name} things"),
        policy: "default".into(),
        tools: vec!["files".into(), "sandbox".into()],
        routing: RoutingPolicy::default(),
        template: None,
        project_id: None,
        role: None,
        requires_repository: false,
    }
}

fn worker(name: &str) -> AgentInput {
    AgentInput {
        name: name.into(),
        agent: "opencode".into(),
        provider: None,
        model: None,
        instructions: String::new(),
    }
}

async fn node(store: &Store, name: &str, capacity: u32) {
    store
        .heartbeat(name, &format!("http://{name}"), capacity, "test", true)
        .await
        .unwrap();
}

#[tokio::test]
async fn limits_are_enforced_and_departments_start_empty() {
    let Some((store, _)) = fresh_store().await else {
        return;
    };
    store
        .set_org_settings(
            OrgSettings {
                max_departments: 2,
                max_agents_per_department: 1,
            },
            "admin",
        )
        .await
        .unwrap();
    // Seeding from the configuration no longer overrides an admin's choice.
    store
        .seed_org_settings(OrgSettings {
            max_departments: 50,
            max_agents_per_department: 50,
        })
        .await
        .unwrap();
    assert_eq!(store.org_settings().await.unwrap().max_departments, 2);

    let research = store
        .create_department(dept("Research"), "alice")
        .await
        .unwrap();
    assert_eq!(research.tools, ["files", "sandbox"]);
    store
        .create_department(dept("Sales"), "alice")
        .await
        .unwrap();
    assert!(matches!(
        store.create_department(dept("Legal"), "alice").await,
        Err(StoreError::Limit(_))
    ));
    // Names are unique regardless of case.
    store
        .set_org_settings(
            OrgSettings {
                max_departments: 5,
                max_agents_per_department: 1,
            },
            "admin",
        )
        .await
        .unwrap();
    assert!(matches!(
        store.create_department(dept("research"), "alice").await,
        Err(StoreError::Conflict(_))
    ));
    let mut bad = dept("Ops");
    bad.tools = vec!["root".into()];
    assert!(matches!(
        store.create_department(bad, "alice").await,
        Err(StoreError::Invalid(_))
    ));

    assert!(
        store
            .list_agents(Some(research.id))
            .await
            .unwrap()
            .is_empty()
    );

    store
        .add_agent(research.id, worker("analyst"), "alice")
        .await
        .unwrap();
    // The limit is one worker per department: a second one is refused.
    assert!(matches!(
        store
            .add_agent(research.id, worker("writer"), "alice")
            .await,
        Err(StoreError::Limit(_))
    ));
    assert!(matches!(
        store
            .add_agent(research.id, worker("everyone"), "alice")
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .add_agent(research.id, worker("Not Valid!"), "alice")
            .await,
        Err(StoreError::Invalid(_))
    ));
    let found = store.department_by_name("RESEARCH").await.unwrap().unwrap();
    assert_eq!(found.id, research.id);
}

#[tokio::test]
async fn placement_desired_state_and_dead_nodes() {
    let Some((store, _)) = fresh_store().await else {
        return;
    };
    let d = store.create_department(dept("Eng"), "alice").await.unwrap();
    let a = store.add_agent(d.id, worker("dev"), "alice").await.unwrap();
    let b = store.add_agent(d.id, worker("qa"), "alice").await.unwrap();

    // No node yet: nothing can run.
    assert!(matches!(
        store.start_agent(a.id, 30, "alice").await,
        Err(StoreError::Unavailable(_))
    ));
    node(&store, "vm-1", 1).await;
    node(&store, "vm-2", 2).await;
    let started = store.start_agent(a.id, 30, "alice").await.unwrap().unwrap();
    assert_eq!(started.desired, Desired::Running);
    // vm-1 and vm-2 are both empty: name order breaks the tie.
    assert_eq!(started.node.as_deref(), Some("vm-1"));
    // Starting again is a no-op.
    assert!(
        store
            .start_agent(a.id, 30, "alice")
            .await
            .unwrap()
            .is_none()
    );
    // vm-1 is full (capacity 1), so the next agent goes to vm-2.
    let started = store.start_agent(b.id, 30, "alice").await.unwrap().unwrap();
    assert_eq!(started.node.as_deref(), Some("vm-2"));
    let nodes = store.list_nodes(30).await.unwrap();
    assert!(nodes.iter().all(|n| n.alive && n.agents == 1));

    // Pause the department's running agents, then stop everything.
    let paused = store
        .set_desired(
            AgentScope::Department(d.id),
            &[Desired::Running],
            Desired::Paused,
            None,
            "alice",
        )
        .await
        .unwrap();
    assert_eq!(paused.len(), 2);
    let stopped = store
        .set_desired(
            AgentScope::All,
            &[Desired::Running, Desired::Paused],
            Desired::Stopped,
            Some("stop all"),
            "alice",
        )
        .await
        .unwrap();
    assert_eq!(stopped.len(), 2);

    // A session ends: the agent records it and becomes stopped.
    store.start_agent(a.id, 30, "alice").await.unwrap().unwrap();
    let session = Uuid::now_v7();
    store
        .agent_session_started(a.id, session, "running")
        .await
        .unwrap();
    assert_eq!(
        store.agent_by_session(session).await.unwrap().unwrap().id,
        a.id
    );
    // A stale end report for another session changes nothing.
    store
        .agent_ended(a.id, Some(Uuid::now_v7()), "failed", None)
        .await
        .unwrap();
    assert_eq!(
        store.get_agent(a.id).await.unwrap().desired,
        Desired::Running
    );
    store
        .agent_ended(a.id, Some(session), "completed", None)
        .await
        .unwrap();
    let ended = store.get_agent(a.id).await.unwrap();
    assert_eq!(ended.desired, Desired::Stopped);
    assert_eq!(ended.status.as_deref(), Some("completed"));

    // A node that stops sending heartbeats loses its agents.
    store.start_agent(b.id, 30, "alice").await.unwrap().unwrap();
    let placed = store.get_agent(b.id).await.unwrap().node.unwrap();
    sqlx::query("UPDATE nodes SET last_seen = now() - interval '5 minutes' WHERE name = $1")
        .bind(&placed)
        .execute(store.pool())
        .await
        .unwrap();
    let lost = store.fail_agents_on_dead_nodes(30).await.unwrap();
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].id, b.id);
    assert!(
        lost[0]
            .note
            .as_deref()
            .unwrap()
            .contains("stopped responding")
    );
    // And dead nodes get no new agents.
    let again = store.start_agent(b.id, 30, "alice").await.unwrap().unwrap();
    assert_ne!(again.node.as_deref(), Some(placed.as_str()));

    // Departments can be paused, and only deleted once everything stopped.
    assert_eq!(
        store
            .set_department_state(None, DepartmentState::Paused, "alice")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store.get_department(d.id).await.unwrap().state,
        DepartmentState::Paused
    );
    assert!(matches!(
        store.delete_department(d.id, "alice").await,
        Err(StoreError::Conflict(_))
    ));
    store
        .set_desired(
            AgentScope::All,
            &[Desired::Running, Desired::Paused],
            Desired::Stopped,
            None,
            "alice",
        )
        .await
        .unwrap();
    store.delete_department(d.id, "alice").await.unwrap();
    assert!(store.list_agents(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn messages_fan_out_to_inboxes() {
    let Some((store, _)) = fresh_store().await else {
        return;
    };
    let r = store
        .create_department(dept("Research"), "alice")
        .await
        .unwrap();
    let e = store.create_department(dept("Eng"), "alice").await.unwrap();
    let analyst = store
        .add_agent(r.id, worker("analyst"), "alice")
        .await
        .unwrap();
    let dev = store.add_agent(e.id, worker("dev"), "alice").await.unwrap();
    let r_writer = store
        .add_agent(r.id, worker("writer"), "alice")
        .await
        .unwrap();

    let msg = store
        .insert_message(NewMessage {
            scope: MessageScope::Internal,
            from_agent: Some(analyst.id),
            from_department: Some(r.id),
            from_name: "analyst (Research)".into(),
            to_kind: "agent",
            to_agent: Some(r_writer.id),
            to_department: Some(r.id),
            to_name: "writer (Research)".into(),
            text: "ask Eng for limits".into(),
            status: MessageStatus::Delivered,
            decision: String::new(),
            recipients: vec![(r_writer.id, r.id)],
        })
        .await
        .unwrap();
    assert_eq!(msg.recipients, [r_writer.id]);
    let human = store
        .insert_message(NewMessage {
            scope: MessageScope::Human,
            from_agent: None,
            from_department: None,
            from_name: "alice".into(),
            to_kind: "department",
            to_agent: None,
            to_department: Some(e.id),
            to_name: "Eng".into(),
            text: "hello Eng".into(),
            status: MessageStatus::Delivered,
            decision: String::new(),
            recipients: vec![(dev.id, e.id)],
        })
        .await
        .unwrap();

    assert_eq!(
        store
            .agents_with_mail(&[r_writer.id, dev.id, analyst.id])
            .await
            .unwrap()
            .len(),
        2
    );
    let pending = store.pending_messages(dev.id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].text, "hello Eng");
    store.mark_delivered(dev.id, &[human.id]).await.unwrap();
    assert!(store.pending_messages(dev.id).await.unwrap().is_empty());

    let research_feed = store
        .list_messages(MessageFilter {
            department: Some(r.id),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(research_feed.len(), 1);
    let all = store.list_messages(MessageFilter::default()).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, msg.id, "oldest first");
    let newer = store
        .list_messages(MessageFilter {
            after: Some(msg.id),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(newer.len(), 1);
    let devs = store
        .list_messages(MessageFilter {
            agent: Some(dev.id),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(devs.len(), 1);

    // Department files.
    store
        .write_department_file(r.id, "notes/plan.md", b"# plan", "analyst")
        .await
        .unwrap();
    assert_eq!(
        store
            .read_department_file(r.id, "notes/plan.md")
            .await
            .unwrap()
            .as_deref(),
        Some(&b"# plan"[..])
    );
    assert!(
        store
            .read_department_file(e.id, "notes/plan.md")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(store.list_department_files(r.id).await.unwrap()[0].size, 6);
    assert!(matches!(
        store
            .write_department_file(r.id, "big", &vec![0; 2 * 1024 * 1024], "analyst")
            .await,
        Err(StoreError::Invalid(_))
    ));
}

#[tokio::test]
async fn sessions_remember_their_node() {
    let Some((store, _)) = fresh_store().await else {
        return;
    };
    node(&store, "vm-1", 4).await;
    let id = Uuid::now_v7();
    store
        .upsert_session(&SessionRecord {
            info: SessionInfo {
                id,
                agent: "opencode".into(),
                task: "x".into(),
                policy: "default".into(),
                status: SessionStatus::Running,
                created_by: Principal::System,
                created_at: chrono::Utc::now(),
                ended_at: None,
                pending_approvals: 0,
                actions: 0,
                model_calls: 0,
                context: Default::default(),
            },
            policy_digest: "d".into(),
            audit_path: "/a".into(),
            node: Some("vm-1".into()),
        })
        .await
        .unwrap();
    let (name, url, alive) = store.session_node(id, 30).await.unwrap().unwrap();
    assert_eq!(
        (name.as_str(), url.as_str(), alive),
        ("vm-1", "http://vm-1", true)
    );

    // A node that still sends heartbeats cannot be removed; a dead one can,
    // and its sessions are failed and archived rather than deleted.
    assert!(store.remove_node("vm-1", 30, "alice").await.is_err());
    sqlx::query("UPDATE nodes SET last_seen = now() - interval '5 minutes' WHERE name = 'vm-1'")
        .execute(store.pool())
        .await
        .unwrap();
    assert_eq!(store.remove_node("vm-1", 30, "alice").await.unwrap().0, 1);
    assert!(
        store
            .list_nodes(30)
            .await
            .unwrap()
            .iter()
            .all(|n| n.name != "vm-1")
    );
    let gone = store.get_session(id).await.unwrap().unwrap();
    assert_eq!(gone.info.status, SessionStatus::Failed);
    assert!(store.archived_session_ids().await.unwrap().contains(&id));
}

#[tokio::test]
async fn goals_and_check_ins() {
    let Some((store, url)) = fresh_store().await else {
        return;
    };
    let d = store.create_department(dept("Eng"), "alice").await.unwrap();
    let goal = store
        .create_goal(
            agentcore_store::GoalInput {
                title: "Reach 100 paying customers".into(),
                description: "By the end of the quarter.".into(),
                department_id: Some(d.id),
            },
            "alice",
        )
        .await
        .unwrap();
    assert_eq!(goal.status, agentcore_core::GoalStatus::Active);
    let goal = store
        .report_goal_progress(goal.id, "12 so far", "Sales/sdr")
        .await
        .unwrap();
    assert_eq!(goal.progress_by.as_deref(), Some("Sales/sdr"));
    let goal = store
        .update_goal(
            goal.id,
            agentcore_store::GoalUpdate {
                status: Some(agentcore_core::GoalStatus::Achieved),
                department_id: Some(None),
                ..Default::default()
            },
            "alice",
        )
        .await
        .unwrap();
    assert!(goal.department_id.is_none());
    // Only active goals take progress reports.
    assert!(matches!(
        store.report_goal_progress(goal.id, "more", "x").await,
        Err(StoreError::Invalid(_))
    ));

    let check = store
        .create_schedule(
            d.id,
            agentcore_store::ScheduleInput {
                name: "Daily stand-up".into(),
                message: "What did you do, what next?".into(),
                every_minutes: 60 * 24,
                agent_id: None,
                first_run_at: None,
            },
            "alice",
        )
        .await
        .unwrap();
    assert!(check.next_run_at > chrono::Utc::now());
    assert!(store.claim_due_schedules().await.unwrap().is_empty());
    store.run_schedule_now(check.id).await.unwrap();

    // Two nodes claim at the same time: the check-in fires once.
    let other = agentcore_store::Store::connect(&url, agentcore_store::Cipher::from_key(&[42; 32]))
        .await
        .unwrap();
    let (a, b) = tokio::join!(store.claim_due_schedules(), other.claim_due_schedules());
    assert_eq!(a.unwrap().len() + b.unwrap().len(), 1);
    let after = store.get_schedule(check.id).await.unwrap();
    assert!(after.last_run_at.is_some());
    assert!(after.next_run_at > chrono::Utc::now() + chrono::Duration::hours(23));
    assert!(store.claim_due_schedules().await.unwrap().is_empty());

    let off = store
        .update_schedule(
            check.id,
            agentcore_store::ScheduleUpdate {
                enabled: Some(false),
                ..Default::default()
            },
            "alice",
        )
        .await
        .unwrap();
    assert!(!off.enabled);
    store.run_schedule_now(check.id).await.unwrap();
    assert!(
        store.claim_due_schedules().await.unwrap().is_empty(),
        "disabled"
    );
    assert!(matches!(
        store
            .create_schedule(
                d.id,
                agentcore_store::ScheduleInput {
                    name: "x".into(),
                    message: "y".into(),
                    every_minutes: 0,
                    ..Default::default()
                },
                "alice",
            )
            .await,
        Err(StoreError::Invalid(_))
    ));
    // Deleting the department removes its check-ins; goals stay.
    store.delete_department(d.id, "alice").await.unwrap();
    assert!(store.list_schedules(None).await.unwrap().is_empty());
    assert_eq!(store.list_goals().await.unwrap().len(), 1);
}

#[tokio::test]
async fn proposals_data_sources_and_metrics() {
    use agentcore_core::{ActionResult, DataSourceKind, ProposalAction, ProposalStatus};
    use agentcore_store::{DataSourceInput, DataSourceUpdate, NewProposal, ProposalRevision};

    let Some((store, url)) = fresh_store().await else {
        return;
    };
    let d = store.create_department(dept("Eng"), "alice").await.unwrap();
    let dev = store.add_agent(d.id, worker("dev"), "alice").await.unwrap();

    // Proposals: revise, ask for changes, apply exactly once.
    let new = |title: &str| NewProposal {
        title: title.into(),
        problem: "The tech lead sleeps all day while work waits.".into(),
        evidence: "0 sessions in 7 days, 4 messages waiting.".into(),
        solution: "Check in twice a day.".into(),
        actions: vec![ProposalAction::UpdateInstructions {
            agent: dev.id,
            instructions: "Check the inbox first.".into(),
        }],
        proposed_by: "retro (Retrospective)".into(),
        proposer_agent: None,
    };
    let p = store.create_proposal(new("Wake the lead")).await.unwrap();
    assert_eq!(p.status, ProposalStatus::Open);
    let p = store
        .request_proposal_changes(p.id, "Once a day is enough", "alice")
        .await
        .unwrap();
    assert_eq!(p.status, ProposalStatus::ChangesRequested);
    let p = store
        .revise_proposal(
            p.id,
            ProposalRevision {
                solution: Some("Check in once a day.".into()),
                note: "as asked".into(),
                ..Default::default()
            },
            "retro (Retrospective)",
        )
        .await
        .unwrap();
    assert_eq!((p.revision, p.status), (2, ProposalStatus::Open));
    assert_eq!(p.history.len(), 2, "feedback and the replaced revision");
    assert!(matches!(
        store.claim_proposal(p.id, 1, "alice").await,
        Err(StoreError::Conflict(_))
    ));
    let other = Store::connect(&url, agentcore_store::Cipher::from_key(&[42; 32]))
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        store.claim_proposal(p.id, 2, "alice"),
        other.claim_proposal(p.id, 2, "bob")
    );
    assert!(a.is_ok() != b.is_ok(), "applied once");
    let results = vec![ActionResult {
        kind: "update_instructions".into(),
        ok: true,
        detail: "done".into(),
    }];
    let p = store
        .finish_proposal(p.id, &results, "alice")
        .await
        .unwrap();
    assert_eq!(p.status, ProposalStatus::Applied);
    assert!(store.reject_proposal(p.id, "late", "bob").await.is_err());
    let r = store.create_proposal(new("Other")).await.unwrap();
    let r = store
        .reject_proposal(r.id, "not now", "alice")
        .await
        .unwrap();
    assert_eq!(r.status, ProposalStatus::Rejected);
    assert_eq!(
        store
            .list_proposals(Some(ProposalStatus::Applied))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .set_auto_apply(&["drop".into()], "root")
            .await
            .is_err()
    );
    store
        .set_auto_apply(&["create_goal".into()], "root")
        .await
        .unwrap();
    assert_eq!(
        store.auto_apply().await.unwrap(),
        (vec!["create_goal".to_string()], Some("root".to_string()))
    );

    // Data sources: secrets are sealed; departments are granted.
    let input = |name: &str, kind, secret: Option<&str>| DataSourceInput {
        name: name.into(),
        kind,
        description: "sales".into(),
        config: serde_json::json!({ "base_url": "https://api.example.com" }),
        secret: secret.map(String::from),
        content: None,
        departments: vec![d.id],
    };
    assert!(
        store
            .create_data_source(input("db", DataSourceKind::Postgres, None), "root")
            .await
            .is_err(),
        "postgres needs a connection string"
    );
    let db = store
        .create_data_source(
            input(
                "db",
                DataSourceKind::Postgres,
                Some("postgres://ro:secret@db/sales"),
            ),
            "root",
        )
        .await
        .unwrap();
    assert_eq!(db.secret_hint.as_deref(), Some("…ales"));
    assert_eq!(
        store.data_source_secret(db.id).await.unwrap().as_deref(),
        Some("postgres://ro:secret@db/sales")
    );
    assert_eq!(store.data_sources_for(d.id).await.unwrap().len(), 1);
    store
        .update_data_source(
            db.id,
            DataSourceUpdate {
                enabled: Some(false),
                ..Default::default()
            },
            "root",
        )
        .await
        .unwrap();
    assert!(store.data_sources_for(d.id).await.unwrap().is_empty());
    assert!(matches!(
        store
            .create_data_source(input("Bad Name", DataSourceKind::Http, None), "root")
            .await,
        Err(StoreError::Invalid(_))
    ));

    // Metrics run over everything recorded.
    store
        .record_activity(agentcore_store::Activity {
            department: Some(d.id),
            agent: Some(dev.id),
            session: None,
            kind: "denied",
            value: None,
            detail: Some("run_command"),
        })
        .await
        .unwrap();
    let m = store.org_metrics(7).await.unwrap();
    assert_eq!(m.daily.len(), 7);
    assert_eq!(m.daily.iter().map(|d| d.denied).sum::<i64>(), 1);
    let eng = m.departments.iter().find(|x| x.id == d.id).unwrap();
    assert_eq!((eng.workers, eng.denied), (1, 1));
    assert_eq!(m.agents.len(), 1);
    assert_eq!(m.proposals.applied, 1);
}

#[tokio::test]
async fn working_hours_pause_and_resume_only_what_they_paused() {
    use agentcore_core::{BudgetAction, BudgetPeriod, BudgetUnit, HoursWindow};
    use agentcore_store::{BudgetInput, HoursInput};

    let Some((store, _)) = fresh_store().await else {
        return;
    };
    let ops = store.create_department(dept("Ops"), "alice").await.unwrap();

    // A token budget sits next to a money budget for the same scope and period.
    let tokens = store
        .set_budget(
            BudgetInput {
                department_id: Some(ops.id),
                period: BudgetPeriod::Day,
                unit: BudgetUnit::Tokens,
                limit_micros: 1_000,
                action: BudgetAction::Pause,
                warn_percent: 80,
            },
            "alice",
        )
        .await
        .unwrap();
    let money = store
        .set_budget(
            BudgetInput {
                department_id: Some(ops.id),
                period: BudgetPeriod::Day,
                unit: BudgetUnit::Money,
                limit_micros: 5_000_000,
                action: BudgetAction::Warn,
                warn_percent: 80,
            },
            "alice",
        )
        .await
        .unwrap();
    assert_ne!(tokens.id, money.id);
    let statuses = store.budget_statuses().await.unwrap();
    assert_eq!(statuses.len(), 2);
    assert!(
        statuses
            .iter()
            .all(|s| s.spent_tokens == 0 && !s.exhausted())
    );

    // Hours: no window is refused, a bad zone is refused.
    let window = HoursWindow {
        days: vec![1, 2, 3, 4, 5, 6, 7],
        start: "00:00".into(),
        end: "23:59".into(),
    };
    assert!(matches!(
        store
            .set_hours(
                ops.id,
                HoursInput {
                    enabled: true,
                    timezone: "UTC".into(),
                    windows: vec![]
                },
                "alice"
            )
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .set_hours(
                ops.id,
                HoursInput {
                    enabled: true,
                    timezone: "Mars/Olympus".into(),
                    windows: vec![window.clone()]
                },
                "alice"
            )
            .await,
        Err(StoreError::Invalid(_))
    ));
    let hours = store
        .set_hours(
            ops.id,
            HoursInput {
                enabled: true,
                timezone: "Europe/Berlin".into(),
                windows: vec![window],
            },
            "alice",
        )
        .await
        .unwrap();
    assert!(hours.is_open(store.local_now(&hours.timezone).await.unwrap()));
    assert!(!store.outside_hours(ops.id).await.unwrap());

    // The hours pause the department and resume it again; a person's pause
    // is left alone.
    assert!(store.pause_for_hours(ops.id).await.unwrap());
    assert!(!store.pause_for_hours(ops.id).await.unwrap());
    assert_eq!(
        store.get_department(ops.id).await.unwrap().state,
        DepartmentState::Paused
    );
    assert!(store.resume_for_hours(ops.id).await.unwrap());
    assert_eq!(
        store.get_department(ops.id).await.unwrap().state,
        DepartmentState::Active
    );
    store
        .set_department_state(Some(ops.id), DepartmentState::Paused, "alice")
        .await
        .unwrap();
    assert!(!store.resume_for_hours(ops.id).await.unwrap());

    store.delete_hours(ops.id, "alice").await.unwrap();
    assert!(store.get_hours(ops.id).await.unwrap().is_none());
}

fn work(dept: Uuid, project: Uuid, branch: &str) -> agentcore_store::NewWorkItem {
    agentcore_store::NewWorkItem {
        department_id: dept,
        project_id: project,
        title: format!("work on {branch}"),
        branch: branch.into(),
        base_branch: "main".into(),
    }
}

#[tokio::test]
async fn work_items_are_borrowed_by_one_session_at_a_time() {
    let Some((store, _)) = fresh_store().await else {
        return;
    };
    let project = store
        .save_project(
            None,
            agentcore_store::ProjectInput {
                name: "Demo".into(),
                repository: "acme/demo".into(),
                default_branch: "main".into(),
                agent: "opencode".into(),
                role: "developer".into(),
                board: None,
                notes: String::new(),
            },
            "root",
        )
        .await
        .unwrap();
    let eng = store.create_department(dept("Eng"), "root").await.unwrap();
    let ana = store
        .add_agent(eng.id, worker("ana"), "root")
        .await
        .unwrap();
    let ben = store
        .add_agent(eng.id, worker("ben"), "root")
        .await
        .unwrap();
    node(&store, "n1", 5).await;
    node(&store, "n2", 5).await;

    // A worker without work gets a new item, held by its session.
    let lease = Uuid::now_v7();
    let first = store
        .acquire_work_item(ana.id, "n1", lease, work(eng.id, project.id, "agent/a1"))
        .await
        .unwrap();
    assert!(!first.resumed);
    let item = first.item.clone();
    assert_eq!(item.assignee, Some(ana.id));
    assert_eq!(item.branch, "agent/a1");
    let held = item.lease.clone().unwrap();
    assert_eq!((held.node.as_str(), held.generation), ("n1", 1));
    let s1 = Uuid::now_v7();
    assert!(store.bind_work_lease(lease, s1).await.unwrap());
    assert_eq!(
        store.work_item_of_session(s1).await.unwrap().unwrap().id,
        item.id
    );
    // Checkpoints count while the lease is held.
    assert!(store.checkpoint_work_item(item.id, 1, "c1").await.unwrap());
    // While it is held, a second session of the same worker gets a new item
    // rather than the same one.
    let other = store
        .acquire_work_item(
            ana.id,
            "n2",
            Uuid::now_v7(),
            work(eng.id, project.id, "agent/a2"),
        )
        .await
        .unwrap();
    assert_ne!(other.item.id, item.id);
    store
        .drop_work_lease(other.item.lease.unwrap().id)
        .await
        .unwrap();
    store
        .close_work_item(other.item.id, None, "duplicate", "root")
        .await
        .unwrap();
    // Nobody else may close or hand over a held item.
    assert!(matches!(
        store.close_work_item(item.id, None, "no", "root").await,
        Err(StoreError::Busy(_))
    ));
    assert!(matches!(
        store.assign_work_item(item.id, ben.id, "root").await,
        Err(StoreError::Busy(_))
    ));

    // The session ends: the item stays with ana, free, at its checkpoint.
    let released = store
        .release_work_item(s1, "its session ended (stopped)")
        .await
        .unwrap()
        .unwrap();
    assert!(released.lease.is_none());
    assert_eq!(released.head_commit.as_deref(), Some("c1"));
    // The old session's generation no longer writes.
    assert!(
        !store
            .checkpoint_work_item(item.id, 1, "stale")
            .await
            .unwrap()
    );

    // Her next session carries it on, under a new generation.
    let lease2 = Uuid::now_v7();
    let again = store
        .acquire_work_item(ana.id, "n2", lease2, work(eng.id, project.id, "agent/a3"))
        .await
        .unwrap();
    assert!(again.resumed);
    assert_eq!(again.item.id, item.id);
    assert_eq!(again.item.branch, "agent/a1", "the item keeps its branch");
    assert_eq!(again.item.head_commit.as_deref(), Some("c1"));
    assert_eq!(
        again.item.note.as_deref(),
        Some("its session ended (stopped)")
    );
    assert_eq!(again.item.lease.as_ref().unwrap().generation, 2);
    store.bind_work_lease(lease2, Uuid::now_v7()).await.unwrap();
    assert!(store.has_open_work_items(ana.id).await.unwrap());
    assert!(!store.has_open_work_items(ben.id).await.unwrap());

    // Node n2 stops responding: its lease is released, the work stays.
    sqlx::query("UPDATE nodes SET last_seen = now() - interval '1 hour' WHERE name = 'n2'")
        .execute(store.pool())
        .await
        .unwrap();
    let expired = store.expire_work_leases_on_dead_nodes(30).await.unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(
        expired[0].note.as_deref(),
        Some("its session was lost with node n2")
    );
    assert!(
        !store
            .checkpoint_work_item(item.id, 2, "late")
            .await
            .unwrap()
    );

    // Work items only go to workers of the item's own department.
    let ops = store.create_department(dept("Ops"), "root").await.unwrap();
    let olga = store
        .add_agent(ops.id, worker("olga"), "root")
        .await
        .unwrap();
    assert!(matches!(
        store.assign_work_item(item.id, olga.id, "root").await,
        Err(StoreError::Invalid(_))
    ));
    // A person hands it to ben; it is his next work.
    let handed = store
        .assign_work_item(item.id, ben.id, "root")
        .await
        .unwrap();
    assert_eq!(handed.assignee, Some(ben.id));
    let bens = store
        .acquire_work_item(
            ben.id,
            "n1",
            Uuid::now_v7(),
            work(eng.id, project.id, "agent/b1"),
        )
        .await
        .unwrap();
    assert!(bens.resumed);
    assert_eq!(bens.item.id, item.id);
    assert_eq!(bens.item.lease.as_ref().unwrap().generation, 3);
    assert!(
        store
            .release_work_item(Uuid::nil(), "nothing")
            .await
            .unwrap()
            .is_none()
    );
    let lease3 = bens.item.lease.clone().unwrap();
    // The holder delivers it, then closes it.
    assert!(
        store
            .work_item_delivered(item.id, lease3.generation, "c2", 12, "https://pr/12")
            .await
            .unwrap()
    );
    let closed = store
        .close_work_item(item.id, Some(lease3.generation), "merged", "Eng/ben")
        .await
        .unwrap();
    assert_eq!(closed.state, agentcore_core::WorkItemState::Closed);
    assert_eq!(closed.pull_request_number, Some(12));
    assert!(closed.lease.is_none());
    assert!(matches!(
        store.assign_work_item(item.id, ben.id, "root").await,
        Err(StoreError::Invalid(_))
    ));
    // Open items are listed per department; closed ones on request.
    assert!(
        store
            .list_work_items(Some(eng.id), false)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .list_work_items(Some(eng.id), true)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn routing_policies_persist() {
    let Some((store, _)) = fresh_store().await else {
        return;
    };
    let other = store.create_department(dept("Eng"), "alice").await.unwrap();
    assert_eq!(other.routing, RoutingPolicy::default());

    let mut input = dept("Research");
    input.routing = RoutingPolicy {
        approval: RouteApproval::Broadcast,
        max_per_hour: Some(5),
        blocked_phrases: vec!["  secret ".into(), "".into()],
        delivery: Delivery::Lead(" lead ".into()),
        ..Default::default()
    };
    input.routing.outbound.allow = Some(vec![other.id]);
    let research = store
        .create_department(input.clone(), "alice")
        .await
        .unwrap();
    assert_eq!(research.routing.approval, RouteApproval::Broadcast);
    assert_eq!(research.routing.max_per_hour, Some(5));
    assert_eq!(research.routing.blocked_phrases, ["secret"]);
    assert_eq!(research.routing.delivery, Delivery::Lead("lead".into()));
    assert_eq!(research.routing.outbound.allow, Some(vec![other.id]));

    input.routing.max_per_hour = Some(0);
    assert!(matches!(
        store
            .update_department(research.id, input.clone(), "alice")
            .await,
        Err(StoreError::Invalid(_))
    ));
    input.routing = RoutingPolicy::default();
    let updated = store
        .update_department(research.id, input, "alice")
        .await
        .unwrap();
    assert_eq!(updated.routing, RoutingPolicy::default());
}

#[tokio::test]
async fn held_messages_are_released_or_rejected_once() {
    let Some((store, _)) = fresh_store().await else {
        return;
    };
    let r = store
        .create_department(dept("Research"), "alice")
        .await
        .unwrap();
    let e = store.create_department(dept("Eng"), "alice").await.unwrap();
    let analyst = store
        .add_agent(r.id, worker("analyst"), "alice")
        .await
        .unwrap();
    let dev = store.add_agent(e.id, worker("dev"), "alice").await.unwrap();

    let new = |status, text: &str, recipients| NewMessage {
        scope: MessageScope::InterDepartment,
        from_agent: Some(analyst.id),
        from_department: Some(r.id),
        from_name: "analyst (Research)".into(),
        to_kind: "department",
        to_agent: None,
        to_department: Some(e.id),
        to_name: "Eng".into(),
        text: text.into(),
        status,
        decision: "waiting".into(),
        recipients,
    };
    let held = store
        .insert_message(new(MessageStatus::Held, "approve me", vec![]))
        .await
        .unwrap();
    let refused = store
        .insert_message(new(MessageStatus::Held, "reject me", vec![]))
        .await
        .unwrap();
    let denied = store
        .insert_message(new(MessageStatus::Rejected, "denied", vec![]))
        .await
        .unwrap();
    assert!(held.recipients.is_empty());
    assert_eq!(store.agents_with_mail(&[dev.id]).await.unwrap().len(), 0);
    // Held and refused messages do not count towards the hourly limit when
    // refused; held ones do.
    assert_eq!(store.count_sent_last_hour(r.id).await.unwrap(), 2);

    let held_only = store
        .list_messages(MessageFilter {
            status: Some(MessageStatus::Held),
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(held_only.len(), 2);
    assert!(held_only.iter().all(|m| m.id != denied.id));

    let released = store
        .release_message(held.id, &[(dev.id, e.id)], "approved by bob")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(released.status, MessageStatus::Delivered);
    assert_eq!(released.recipients, [dev.id]);
    assert_eq!(released.decision, "approved by bob");
    assert_eq!(store.pending_messages(dev.id).await.unwrap().len(), 1);
    // Decided already: nothing happens twice.
    assert!(
        store
            .release_message(held.id, &[], "again")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .reject_message(held.id, "late")
            .await
            .unwrap()
            .is_none()
    );

    let rejected = store
        .reject_message(refused.id, "rejected by bob")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rejected.status, MessageStatus::Rejected);
    assert!(rejected.recipients.is_empty());
}
