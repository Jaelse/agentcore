use agentcore_core::{ModelCallOutcome, Principal, ProviderKind, SessionInfo, SessionStatus};
use agentcore_store::testing::fresh_store;
use agentcore_store::{ModelCallRecord, NewProvider, ProviderUpdate, SessionRecord, StoreError};
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
