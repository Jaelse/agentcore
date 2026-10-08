use agentcore_core::{ModelCallOutcome, Principal, ProviderKind, SessionInfo, SessionStatus};
use agentcore_store::testing::fresh_store;
use agentcore_store::{
    BoardConfig, GitHubConfig, GitHubUpdate, ModelCallRecord, NewProvider, ProjectInput,
    ProviderUpdate, SessionRecord, StoreError,
};
use chrono::Utc;
use uuid::Uuid;

fn session(status: SessionStatus) -> SessionRecord {
    SessionRecord {
        info: SessionInfo {
            id: Uuid::now_v7(),
            agent: "opencode".into(),
            task: "fix it".into(),
            policy: "default".into(),
            status,
            created_by: Principal::human("alice"),
            created_at: Utc::now(),
            ended_at: None,
            pending_approvals: 0,
            actions: 3,
            model_calls: 0,
            context: Default::default(),
        },
        policy_digest: "abc".into(),
        audit_path: "/data/audit/x.jsonl".into(),
    }
}

#[tokio::test]
async fn sessions_persist_and_interrupted_ones_fail() {
    let Some((store, _)) = fresh_store().await else {
        return;
    };
    let done = session(SessionStatus::Completed);
    let live = session(SessionStatus::Running);
    store.upsert_session(&done).await.unwrap();
    store.upsert_session(&live).await.unwrap();

    let mut updated = live.clone();
    updated.info.actions = 9;
    updated.info.model_calls = 2;
    store.upsert_session(&updated).await.unwrap();
    let got = store.get_session(live.info.id).await.unwrap().unwrap();
    assert_eq!(got.info.actions, 9);
    assert_eq!(got.info.model_calls, 2);
    assert_eq!(got.policy_digest, "abc");
    assert_eq!(got.info.created_by, Principal::human("alice"));

    let interrupted = store.fail_interrupted_sessions().await.unwrap();
    assert_eq!(interrupted.len(), 1);
    assert_eq!(interrupted[0].info.id, live.info.id);
    let list = store.list_sessions(10).await.unwrap();
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|s| s.info.status.is_terminal()));
}

#[tokio::test]
async fn providers_keep_keys_encrypted_and_audited() {
    let Some((store, _)) = fresh_store().await else {
        return;
    };
    let created = store
        .create_provider(
            NewProvider {
                name: "Anthropic".into(),
                kind: ProviderKind::Anthropic,
                base_url: None,
                api_key: "sk-ant-secret-1234".into(),
                allowed_models: vec!["claude-*".into()],
                enabled: true,
            },
            "alice",
        )
        .await
        .unwrap();
    assert_eq!(created.name, "anthropic");
    assert_eq!(created.base_url, "https://api.anthropic.com");
    assert_eq!(created.api_key_hint, "…1234");

    // The key is not stored in plain text.
    let raw: (Vec<u8>,) = sqlx::query_as("SELECT api_key_ciphertext FROM model_providers")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&raw.0).contains("sk-ant"));

    let (_, key) = store
        .provider_credentials("anthropic")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(key, "sk-ant-secret-1234");

    let dup = NewProvider {
        name: "anthropic".into(),
        kind: ProviderKind::Anthropic,
        base_url: None,
        api_key: "x".into(),
        allowed_models: vec![],
        enabled: true,
    };
    assert!(matches!(
        store.create_provider(dup, "bob").await,
        Err(StoreError::Conflict(_))
    ));

    let updated = store
        .update_provider(
            "anthropic",
            ProviderUpdate {
                api_key: Some("sk-new-9999".into()),
                enabled: Some(false),
                ..Default::default()
            },
            "bob",
        )
        .await
        .unwrap();
    assert!(!updated.enabled);
    assert_eq!(updated.updated_by, "bob");
    assert!(store.enabled_endpoints().await.unwrap().is_empty());
    assert_eq!(
        store
            .provider_credentials("anthropic")
            .await
            .unwrap()
            .unwrap()
            .1,
        "sk-new-9999"
    );

    store.delete_provider("anthropic", "carol").await.unwrap();
    let events = store.admin_events(10).await.unwrap();
    let actions: Vec<_> = events
        .iter()
        .map(|e| (e.action.as_str(), e.actor.as_str()))
        .collect();
    assert_eq!(
        actions,
        [
            ("provider.delete", "carol"),
            ("provider.update", "bob"),
            ("provider.create", "alice")
        ]
    );
    // Rotated keys are never written to the admin log.
    assert!(
        !serde_json::to_string(&events[1].details.0)
            .unwrap()
            .contains("sk-new")
    );
}

#[tokio::test]
async fn model_calls_roundtrip() {
    let Some((store, _)) = fresh_store().await else {
        return;
    };
    let s = session(SessionStatus::Running);
    store.upsert_session(&s).await.unwrap();
    let mut call = ModelCallRecord {
        id: Uuid::now_v7(),
        session_id: s.info.id,
        provider: "anthropic".into(),
        model: Some("claude-x".into()),
        method: "POST".into(),
        path: "v1/messages".into(),
        http_status: Some(200),
        outcome: String::new(),
        detail: None,
        input_tokens: Some(10),
        output_tokens: Some(20),
        started_at: Utc::now(),
        duration_ms: 123,
        request_body: Some("{}".into()),
        response_body: Some("{}".into()),
        bodies_truncated: false,
        request_sha256: "a".into(),
        response_sha256: Some("b".into()),
    };
    call.set_outcome(ModelCallOutcome::Completed);
    store.insert_model_call(&call).await.unwrap();
    let got = store
        .get_model_call(s.info.id, call.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.outcome, "completed");
    assert_eq!(got.output_tokens, Some(20));
    assert!(
        store
            .get_model_call(Uuid::now_v7(), call.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn opencode_zen_defaults_to_the_free_public_key() {
    let Some((store, _)) = fresh_store().await else {
        return;
    };
    let zen = store
        .create_provider(
            NewProvider {
                name: "opencode".into(),
                kind: ProviderKind::OpencodeZen,
                base_url: None,
                api_key: String::new(),
                allowed_models: vec![],
                enabled: true,
            },
            "alice",
        )
        .await
        .unwrap();
    assert_eq!(zen.base_url, "https://opencode.ai/zen");
    let (_, key) = store
        .provider_credentials("opencode")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(key, "public");

    // Other kinds still require a key.
    let anthropic = NewProvider {
        name: "anthropic".into(),
        kind: ProviderKind::Anthropic,
        base_url: None,
        api_key: " ".into(),
        allowed_models: vec![],
        enabled: true,
    };
    assert!(matches!(
        store.create_provider(anthropic, "alice").await,
        Err(StoreError::Invalid(_))
    ));
}

#[tokio::test]
async fn github_connection_and_projects() {
    let Some((store, _)) = fresh_store().await else {
        return;
    };
    assert!(store.github_connection().await.unwrap().is_none());
    // A token is required the first time.
    let no_token = GitHubUpdate {
        token: None,
        config: GitHubConfig::default(),
    };
    assert!(store.set_github(no_token, "root").await.is_err());
    let conn = store
        .set_github(
            GitHubUpdate {
                token: Some("ghp_secret1234".into()),
                config: GitHubConfig::default(),
            },
            "root",
        )
        .await
        .unwrap();
    assert_eq!(conn.token_hint, "…1234");
    // Updating settings keeps the token.
    let config = GitHubConfig {
        commit_name: "team-bot".into(),
        ..Default::default()
    };
    store
        .set_github(
            GitHubUpdate {
                token: None,
                config,
            },
            "root",
        )
        .await
        .unwrap();
    let (config, token) = store.github_credentials().await.unwrap().unwrap();
    assert_eq!(
        (config.commit_name.as_str(), token.as_str()),
        ("team-bot", "ghp_secret1234")
    );
    let events = store.admin_events(10).await.unwrap();
    assert!(
        events
            .iter()
            .all(|e| !e.details.0.to_string().contains("ghp_"))
    );

    let input = |name: &str| ProjectInput {
        name: name.into(),
        repository: "https://github.com/acme/api".into(),
        default_branch: "main".into(),
        agent: "opencode".into(),
        role: "developer".into(),
        board: Some(BoardConfig {
            owner: "acme".into(),
            number: 3,
            status_field: "Status".into(),
            iteration_field: Some("Sprint".into()),
            columns: Default::default(),
        }),
        notes: "Be nice.".into(),
    };
    let p = store
        .save_project(None, input("API"), "root")
        .await
        .unwrap();
    assert_eq!(p.repository(), "acme/api");
    assert_eq!(p.board.as_ref().unwrap().columns.in_review, "In Review");
    assert!(matches!(
        store.save_project(None, input("API"), "root").await,
        Err(StoreError::Conflict(_))
    ));
    let mut renamed = input("API v2");
    renamed.repository = "acme/api2".into();
    let p2 = store
        .save_project(Some(p.id), renamed, "root")
        .await
        .unwrap();
    assert_eq!((p2.id, p2.repo_name.as_str()), (p.id, "api2"));
    let mut bad = input("bad");
    bad.repository = "nope".into();
    assert!(matches!(
        store.save_project(None, bad, "root").await,
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(store.list_projects().await.unwrap().len(), 1);
    store.delete_project(p.id, "root").await.unwrap();
    assert!(store.get_project(p.id).await.unwrap().is_none());
}
