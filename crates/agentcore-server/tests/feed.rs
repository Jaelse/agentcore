//! The home feed end to end: events from goals, sessions and messages show up
//! newest first, with the right tone, and can be filtered and paged.
//! Requires `AGENTCORE_TEST_DATABASE_URL` (skipped otherwise).

use std::time::Duration;

use agentcore_core::AgentSpec;
use agentcore_sandbox::{BackendKind, SandboxConfig};
use agentcore_server::config::{Operator, Role};
use agentcore_server::{AppState, Config, auth::hash_token, router};
use serde_json::{Value, json};

fn quick(name: &str, script: &str) -> AgentSpec {
    AgentSpec {
        name: name.into(),
        adapter: "command".into(),
        command: Some("sh".into()),
        args: vec!["-c".into(), script.into(), "{task}".into()],
        tty: Some(false),
        ..Default::default()
    }
}

fn config(dir: &std::path::Path, db_url: &str, bind: std::net::SocketAddr) -> Config {
    let mut config: Config = toml::from_str("").unwrap();
    config.server.bind = bind;
    config.server.operators = ["admin", "alice", "victor"]
        .iter()
        .zip([Role::Admin, Role::Operator, Role::Viewer])
        .map(|(name, role)| Operator {
            name: (*name).into(),
            token_sha256: hash_token(&format!("{name}-token")),
            role,
        })
        .collect();
    config.database.url = Some(db_url.into());
    config.secrets.master_key_file = Some(dir.join("master.key"));
    config.storage.data_dir = dir.join("data");
    config.storage.audit_fsync = false;
    config.policies.dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../policies").into();
    config.templates.dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates").into();
    config.roles.dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../roles").into();
    config.sandbox = SandboxConfig {
        backend: BackendKind::Process,
        ..Default::default()
    };
    config.sandbox.process.allow_insecure = true;
    config.agents = vec![quick("quick", "echo done"), quick("relay-bot", "true")];
    config
}

async fn call(
    base: &str,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut req = client
        .request(method.parse().unwrap(), format!("{base}/api/v1{path}"))
        .bearer_auth(token);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let res = req.send().await.unwrap();
    let code = res.status().as_u16();
    let text = res.text().await.unwrap();
    (code, serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn titles(feed: &Value) -> Vec<String> {
    feed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn the_feed_merges_what_happened() {
    let Some((_, db_url)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::new(config(dir.path(), &db_url, addr))
        .await
        .unwrap();
    let app = router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{addr}");

    // Anyone signed in may read it; nobody else.
    let (code, empty) = call(&base, "GET", "/feed", "victor-token", None).await;
    assert_eq!(code, 200, "{empty}");
    assert_eq!(empty["items"], json!([]));
    assert_eq!(empty["has_more"], false);
    assert_eq!(call(&base, "GET", "/feed", "nope", None).await.0, 401);

    // A goal and a report on it.
    let (code, goal) = call(
        &base,
        "POST",
        "/org/goals",
        "alice-token",
        Some(json!({
            "title": "Reach 10 customers", "description": "By March."
        })),
    )
    .await;
    assert_eq!(code, 201, "{goal}");
    let goal_id = goal["id"].as_str().unwrap();
    let (code, _) = call(
        &base,
        "POST",
        &format!("/org/goals/{goal_id}/progress"),
        "alice-token",
        Some(json!({ "text": "Pricing page drafted" })),
    )
    .await;
    assert_eq!(code, 200);

    // A session that finishes by itself.
    let (code, session) = call(
        &base,
        "POST",
        "/sessions",
        "alice-token",
        Some(json!({ "agent": "quick", "task": "say done" })),
    )
    .await;
    assert_eq!(code, 201, "{session}");
    let mut feed = Value::Null;
    for _ in 0..200 {
        feed = call(&base, "GET", "/feed", "victor-token", None).await.1;
        if titles(&feed).contains(&"quick finished".to_string()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let titles_now = titles(&feed);
    assert!(titles_now.contains(&"quick finished".to_string()), "{feed}");
    assert!(
        titles_now.contains(&"quick started a session".to_string()),
        "{feed}"
    );
    assert!(
        titles_now.contains(&"alice set a goal: Reach 10 customers".to_string()),
        "{feed}"
    );
    assert!(
        titles_now.contains(&"alice reported progress on Reach 10 customers".to_string()),
        "{feed}"
    );

    // Newest first, with tone, actor, detail and a way to open the session.
    let items = feed["items"].as_array().unwrap();
    assert!(
        items
            .windows(2)
            .all(|w| w[0]["at"].as_str() >= w[1]["at"].as_str()),
        "{feed}"
    );
    let finished = items
        .iter()
        .find(|i| i["title"] == "quick finished")
        .unwrap();
    assert_eq!(finished["tone"], "good");
    assert_eq!(finished["kind"], "session");
    assert_eq!(finished["detail"], "say done");
    assert_eq!(finished["session"], session["id"]);
    let progress = items
        .iter()
        .find(|i| i["kind"] == "goal" && i["detail"] == "Pricing page drafted")
        .unwrap();
    assert_eq!(progress["actor"], "alice");

    // Messages: one to a department is shown, with the sender's name.
    let (code, dep) = call(&base, "POST", "/org/departments", "admin-token", Some(json!({
        "name": "Ops", "mission": "Ops work", "tools": ["files"], "communicator_agent": "relay-bot",
    }))).await;
    assert_eq!(code, 201, "{dep}");
    let (code, _) = call(
        &base,
        "POST",
        "/org/messages",
        "alice-token",
        Some(json!({ "to": { "department": dep["id"] }, "text": "hello Ops" })),
    )
    .await;
    assert_eq!(code, 201);
    let (_, feed) = call(&base, "GET", "/feed?kind=message", "victor-token", None).await;
    let items = feed["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{feed}");
    assert_eq!(items[0]["detail"], "hello Ops");
    assert_eq!(items[0]["kind"], "message");

    // Filters keep only the asked kinds; unknown kinds are ignored.
    let (_, goals) = call(
        &base,
        "GET",
        "/feed?kind=goal,nonsense",
        "victor-token",
        None,
    )
    .await;
    assert!(
        goals["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["kind"] == "goal")
    );
    assert_eq!(goals["items"].as_array().unwrap().len(), 2);

    // Paging: one at a time, walking back in time without repeats.
    let (_, all) = call(&base, "GET", "/feed?limit=200", "victor-token", None).await;
    let total = all["items"].as_array().unwrap().len();
    assert!(total >= 5);
    let mut seen = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let path = match &before {
            Some(b) => format!("/feed?limit=2&before={}", b.replace('+', "%2B")),
            None => "/feed?limit=2".to_string(),
        };
        let (_, page) = call(&base, "GET", &path, "victor-token", None).await;
        let items = page["items"].as_array().unwrap();
        assert!(items.len() <= 2);
        seen.extend(items.iter().map(|i| i["id"].as_str().unwrap().to_string()));
        if page["has_more"] != true {
            break;
        }
        before = Some(items.last().unwrap()["at"].as_str().unwrap().to_string());
    }
    assert_eq!(seen.len(), total, "{seen:?}");
}
