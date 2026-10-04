use std::time::Duration;

use agentcore_core::AgentSpec;
use agentcore_sandbox::{BackendKind, SandboxConfig};
use agentcore_server::config::{Operator, Role};
use agentcore_server::{AppState, Config, auth::hash_token, router};
use serde_json::{Value, json};

/// The agent calls the gateway with curl, using the token agentcore injected.
const AGENT_SCRIPT: &str = r#"
call() {
  curl -sf -X POST "$AGENTCORE_GATEWAY_URL" \
    -H "Authorization: Bearer $AGENTCORE_GATEWAY_TOKEN" \
    -H 'Content-Type: application/json' -d "$1"
}
call '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
call '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"write_file","arguments":{"path":"hello.txt","content":"hi from agent"}}}'
call '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_file","arguments":{"path":".env"}}}'
"#;

async fn start(dir: &std::path::Path) -> (String, AppState) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config: Config = toml::from_str("").unwrap();
    config.server.bind = addr;
    config.server.operators = vec![
        Operator {
            name: "alice".into(),
            token_sha256: hash_token("alice-token"),
            role: Role::Operator,
        },
        Operator {
            name: "victor".into(),
            token_sha256: hash_token("viewer-token"),
            role: Role::Viewer,
        },
    ];
    config.storage.data_dir = dir.to_path_buf();
    config.storage.audit_fsync = false;
    config.policies.dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../policies").into();
    config.sandbox = SandboxConfig {
        backend: BackendKind::Process,
        ..Default::default()
    };
    config.sandbox.process.allow_insecure = true;
    config.agents = vec![AgentSpec {
        name: "curl-agent".into(),
        adapter: "command".into(),
        description: String::new(),
        image: None,
        command: Some("sh".into()),
        args: vec!["-c".into(), AGENT_SCRIPT.into()],
        env: Default::default(),
        policy: None,
    }];
    let state = AppState::new(config).unwrap();
    let app = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state)
}

async fn http(method: &str, url: &str, token: Option<&str>, body: Option<Value>) -> (u16, Value) {
    let mut args = vec![
        "-s".to_string(),
        "-X".into(),
        method.into(),
        "-w".into(),
        "\n%{http_code}".into(),
    ];
    if let Some(t) = token {
        args.extend(["-H".into(), format!("Authorization: Bearer {t}")]);
    }
    if let Some(b) = body {
        args.extend([
            "-H".into(),
            "Content-Type: application/json".into(),
            "-d".into(),
            b.to_string(),
        ]);
    }
    args.push(url.into());
    let out = tokio::process::Command::new("curl")
        .args(&args)
        .output()
        .await
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let (body, code) = text.rsplit_once('\n').unwrap();
    (
        code.parse().unwrap(),
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn agent_uses_gateway_under_policy() {
    let dir = tempfile::tempdir().unwrap();
    let (base, state) = start(dir.path()).await;

    // Authentication and roles.
    assert_eq!(
        http("GET", &format!("{base}/api/v1/sessions"), None, None)
            .await
            .0,
        401
    );
    let create = json!({ "agent": "curl-agent", "task": "say hi" });
    let (code, _) = http(
        "POST",
        &format!("{base}/api/v1/sessions"),
        Some("viewer-token"),
        Some(create.clone()),
    )
    .await;
    assert_eq!(code, 403);
    let (code, session) = http(
        "POST",
        &format!("{base}/api/v1/sessions"),
        Some("alice-token"),
        Some(create),
    )
    .await;
    assert_eq!(code, 201, "{session}");
    let id = session["id"].as_str().unwrap().to_string();
    assert_eq!(
        session["created_by"],
        json!({ "kind": "human", "id": "alice" })
    );

    // The gateway rejects callers without the session token.
    let rpc = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
    assert_eq!(
        http(
            "POST",
            &format!("{base}/mcp/{id}"),
            Some("alice-token"),
            Some(rpc)
        )
        .await
        .0,
        401
    );

    // Wait for the agent to finish.
    let session = state.manager.get(id.parse().unwrap()).unwrap();
    for _ in 0..200 {
        if session.status().is_terminal() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let (_, info) = http(
        "GET",
        &format!("{base}/api/v1/sessions/{id}"),
        Some("viewer-token"),
        None,
    )
    .await;
    assert_eq!(info["session"]["status"], "completed", "{info}");

    let written =
        std::fs::read_to_string(dir.path().join("workspaces").join(&id).join("hello.txt")).unwrap();
    assert_eq!(written, "hi from agent");

    let (_, events) = http(
        "GET",
        &format!("{base}/api/v1/sessions/{id}/events"),
        Some("viewer-token"),
        None,
    )
    .await;
    let verdicts: Vec<_> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event"] == "policy_evaluated")
        .map(|e| e["verdict"]["verdict"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(verdicts, ["allow", "deny"]);
    let denied_output = events.as_array().unwrap().iter().any(|e| {
        e["event"] == "output" && e["line"].as_str().unwrap_or_default().contains("DENIED")
    });
    assert!(denied_output, "agent should have been told it was denied");

    let (_, verify) = http(
        "GET",
        &format!("{base}/api/v1/sessions/{id}/audit/verify"),
        Some("viewer-token"),
        None,
    )
    .await;
    assert_eq!(verify["valid"], true);

    let (_, card) = http(
        "GET",
        &format!("{base}/api/v1/system-card"),
        Some("viewer-token"),
        None,
    )
    .await;
    assert_eq!(card["ai_system"], true);
    assert_eq!(card["sandbox_backend"], "process");
}
