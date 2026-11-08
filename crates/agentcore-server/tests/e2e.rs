//! End-to-end tests: real HTTP server, PostgreSQL, agents as local processes
//! (curl scripts) and a mock Anthropic upstream.
//! Requires `AGENTCORE_TEST_DATABASE_URL` (skipped otherwise).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentcore_core::{AgentSpec, SessionStatus};
use agentcore_sandbox::{BackendKind, SandboxConfig};
use agentcore_server::config::{Operator, Role};
use agentcore_server::{AppState, Config, auth::hash_token, router};
use axum::body::Body;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

/// The agent calls the MCP gateway with curl, using the token agentcore injected.
const TOOLS_AGENT: &str = r#"
call() {
  curl -sf -X POST "$AGENTCORE_GATEWAY_URL" \
    -H "Authorization: Bearer $AGENTCORE_GATEWAY_TOKEN" \
    -H 'Content-Type: application/json' -d "$1"
}
call '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
call '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"write_file","arguments":{"path":"hello.txt","content":"hi from agent"}}}'
call '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_file","arguments":{"path":".env"}}}'
"#;

/// The agent talks to "Anthropic" through the model gateway, like an SDK would.
const MODEL_AGENT: &str = r#"
echo "key=$ANTHROPIC_API_KEY"
curl -sN "$ANTHROPIC_BASE_URL/v1/messages" -H "x-api-key: $ANTHROPIC_API_KEY" \
  -H 'anthropic-version: 2023-06-01' -H 'content-type: application/json' \
  -d '{"model":"claude-test","stream":true,"max_tokens":10,"messages":[{"role":"user","content":"hi"}]}'
echo
curl -s "$ANTHROPIC_BASE_URL/v1/messages" -H "x-api-key: $ANTHROPIC_API_KEY" \
  -H 'content-type: application/json' -d '{"model":"gpt-forbidden","messages":[]}'
echo
"#;

/// Waits for a viewer, then talks to the model and keeps working.
const LIVE_AGENT: &str = r#"
sleep 1
printf '\033[1mthinking...\033[0m\n'
curl -sN "$ANTHROPIC_BASE_URL/v1/messages" -H "x-api-key: $ANTHROPIC_API_KEY" \
  -H 'content-type: application/json' \
  -d '{"model":"claude-test","stream":true,"messages":[{"role":"user","content":"hi"}]}' > /dev/null
sleep 30
"#;

const SLOW_MODEL_AGENT: &str = r#"
curl -sN "$ANTHROPIC_BASE_URL/v1/slow" -H "x-api-key: $ANTHROPIC_API_KEY" \
  -H 'content-type: application/json' -d '{"model":"claude-test","stream":true}'
sleep 30
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

/// Headers the mock upstream received.
type Seen = Arc<Mutex<Vec<HeaderMap>>>;

async fn mock_anthropic() -> (String, Seen) {
    let seen: Seen = Arc::default();
    let s1 = seen.clone();
    let app = axum::Router::new()
        .route(
            "/v1/messages",
            axum::routing::post(move |headers: HeaderMap, body: String| {
                let seen = s1.clone();
                async move {
                    seen.lock().unwrap().push(headers.clone());
                    if headers.get("x-api-key").map(|v| v.as_bytes()) != Some(b"sk-real-key") {
                        return (axum::http::StatusCode::UNAUTHORIZED, "bad key").into_response();
                    }
                    assert!(body.contains("claude-test"));
                    let sse = concat!(
                        "event: message_start\n",
                        "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-test\",\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
                        "event: content_block_delta\n",
                        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello from the model\"}}\n\n",
                        "event: message_delta\n",
                        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":15}}\n\n",
                    );
                    ([("content-type", "text/event-stream")], sse).into_response()
                }
            }),
        )
        .route(
            "/v1/slow",
            axum::routing::post(|| async {
                let chunks = futures::stream::unfold(0u32, |i| async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Some((Ok::<_, std::io::Error>(format!("data: {{\"n\":{i}}}\n\n")), i + 1))
                });
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(chunks))
                    .unwrap()
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

async fn config(dir: &std::path::Path, db_url: &str, bind: std::net::SocketAddr) -> Config {
    let mut config: Config = toml::from_str("").unwrap();
    config.server.bind = bind;
    config.server.operators = vec![
        Operator {
            name: "root".into(),
            token_sha256: hash_token("admin-token"),
            role: Role::Admin,
        },
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
    config.database.url = Some(db_url.into());
    config.secrets.master_key_file = Some(dir.join("master.key"));
    config.storage.data_dir = dir.to_path_buf();
    config.storage.audit_fsync = false;
    config.policies.dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../policies").into();
    config.sandbox = SandboxConfig {
        backend: BackendKind::Process,
        ..Default::default()
    };
    config.sandbox.process.allow_insecure = true;
    config.agents = vec![
        agent("tools-agent", TOOLS_AGENT),
        agent("model-agent", MODEL_AGENT),
        agent("slow-model-agent", SLOW_MODEL_AGENT),
        agent("sleepy", "sleep 60"),
        agent("live-agent", LIVE_AGENT),
    ];
    config
}

struct Server {
    base: String,
    state: AppState,
}

async fn start(dir: &std::path::Path, db_url: &str) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::new(config(dir, db_url, addr).await)
        .await
        .unwrap();
    let app = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        base: format!("http://{addr}"),
        state,
    }
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

impl Server {
    async fn create(&self, agent: &str) -> String {
        let (code, session) = http(
            "POST",
            &format!("{}/api/v1/sessions", self.base),
            Some("alice-token"),
            Some(json!({ "agent": agent, "task": "do it" })),
        )
        .await;
        assert_eq!(code, 201, "{session}");
        session["id"].as_str().unwrap().to_string()
    }

    async fn wait(&self, id: &str, pred: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..300 {
            let (_, info) = http(
                "GET",
                &format!("{}/api/v1/sessions/{id}", self.base),
                Some("viewer-token"),
                None,
            )
            .await;
            if pred(&info) {
                return info;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for session {id}");
    }

    async fn events(&self, id: &str) -> Vec<Value> {
        let (_, events) = http(
            "GET",
            &format!("{}/api/v1/sessions/{id}/events", self.base),
            Some("viewer-token"),
            None,
        )
        .await;
        events.as_array().cloned().unwrap_or_default()
    }

    async fn add_provider(&self, base_url: &str) {
        let provider = json!({
            "name": "anthropic", "kind": "anthropic", "base_url": base_url,
            "api_key": "sk-real-key", "allowed_models": ["claude-*"],
        });
        let url = format!("{}/api/v1/providers", self.base);
        assert_eq!(
            http("POST", &url, Some("alice-token"), Some(provider.clone()))
                .await
                .0,
            403,
            "operators cannot manage keys"
        );
        let (code, created) = http("POST", &url, Some("admin-token"), Some(provider)).await;
        assert_eq!(code, 201, "{created}");
        assert!(
            !created.to_string().contains("sk-real-key"),
            "key must never be returned"
        );
    }
}

fn terminal(info: &Value) -> bool {
    matches!(
        info["session"]["status"].as_str(),
        Some("completed" | "failed" | "stopped")
    )
}

#[tokio::test]
async fn agent_uses_tool_gateway_under_policy() {
    let Some((_, db)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let server = start(dir.path(), &db).await;
    let base = &server.base;

    assert_eq!(
        http("GET", &format!("{base}/api/v1/sessions"), None, None)
            .await
            .0,
        401
    );
    let create = json!({ "agent": "tools-agent", "task": "say hi" });
    let (code, _) = http(
        "POST",
        &format!("{base}/api/v1/sessions"),
        Some("viewer-token"),
        Some(create),
    )
    .await;
    assert_eq!(code, 403);
    let id = server.create("tools-agent").await;

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

    let info = server.wait(&id, terminal).await;
    assert_eq!(info["session"]["status"], "completed", "{info}");
    let written = std::fs::read_to_string(
        dir.path()
            .join("sessions")
            .join(&id)
            .join("workspace/hello.txt"),
    )
    .unwrap();
    assert_eq!(written, "hi from agent");

    let events = server.events(&id).await;
    let verdicts: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "policy_evaluated")
        .map(|e| e["verdict"]["verdict"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(verdicts, ["allow", "deny"]);
    assert!(events.iter().any(
        |e| e["event"] == "output" && e["line"].as_str().unwrap_or_default().contains("DENIED")
    ));

    let (_, verify) = http(
        "GET",
        &format!("{base}/api/v1/sessions/{id}/audit/verify"),
        Some("viewer-token"),
        None,
    )
    .await;
    assert_eq!(verify["valid"], true);

    // The session is in PostgreSQL.
    let stored = server
        .state
        .store
        .get_session(id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.info.status, SessionStatus::Completed);
}

#[tokio::test]
async fn model_gateway_injects_keys_and_records_calls() {
    let Some((_, db)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (upstream, seen) = mock_anthropic().await;
    let server = start(dir.path(), &db).await;
    server.add_provider(&upstream).await;

    let id = server.create("model-agent").await;
    let info = server.wait(&id, terminal).await;
    assert_eq!(info["session"]["status"], "completed", "{info}");
    assert_eq!(info["session"]["model_calls"], 2);

    let events = server.events(&id).await;
    let output: String = events
        .iter()
        .filter(|e| e["event"] == "output")
        .map(|e| format!("{}\n", e["line"].as_str().unwrap()))
        .collect();
    assert!(
        !output.contains("sk-real-key"),
        "real key leaked into the sandbox:\n{output}"
    );
    assert!(output.contains("Hello from the model"), "{output}");
    assert!(output.contains("not allowed"), "{output}");

    // Upstream saw the real key and nothing of the session token.
    let token = output
        .lines()
        .find_map(|l| l.strip_prefix("key="))
        .unwrap()
        .to_string();
    assert_eq!(token.len(), 64);
    let headers = seen.lock().unwrap().clone();
    assert_eq!(
        headers.len(),
        1,
        "the forbidden model must not reach upstream"
    );
    assert_eq!(headers[0]["x-api-key"], "sk-real-key");
    assert!(
        headers[0]
            .iter()
            .all(|(_, v)| !v.to_str().unwrap_or_default().contains(&token))
    );
    assert_eq!(headers[0]["anthropic-version"], "2023-06-01");

    let calls: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "model_call")
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["outcome"], "completed");
    assert_eq!(calls[0]["model"], "claude-test");
    assert_eq!(
        (
            calls[0]["input_tokens"].clone(),
            calls[0]["output_tokens"].clone()
        ),
        (json!(25), json!(15))
    );
    assert_eq!(calls[0]["http_status"], 200);
    assert_eq!(calls[1]["outcome"], "rejected");
    assert_eq!(calls[1]["http_status"], 403);

    // Full bodies are in PostgreSQL and served to viewers.
    let call_id = calls[0]["call_id"].as_str().unwrap();
    // The row is written just after the audit event: wait for it.
    let mut found = (0, Value::Null);
    for _ in 0..50 {
        found = http(
            "GET",
            &format!("{}/api/v1/sessions/{id}/model-calls/{call_id}", server.base),
            Some("viewer-token"),
            None,
        )
        .await;
        if found.0 == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (code, detail) = found;
    assert_eq!(code, 200);
    assert!(
        detail["request_body"]
            .as_str()
            .unwrap()
            .contains("\"content\":\"hi\"")
    );
    assert!(
        detail["response_body"]
            .as_str()
            .unwrap()
            .contains("Hello from the model")
    );
    assert_eq!(detail["response_sha256"], calls[0]["response_sha256"]);

    let (code, log) = http(
        "GET",
        &format!("{}/api/v1/admin-events", server.base),
        Some("admin-token"),
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(log[0]["action"], "provider.create");
    assert_eq!(log[0]["actor"], "root");
}

#[tokio::test]
async fn stop_aborts_streaming_model_call() {
    let Some((_, db)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (upstream, _) = mock_anthropic().await;
    let server = start(dir.path(), &db).await;
    server.add_provider(&upstream).await;

    let id = server.create("slow-model-agent").await;
    // Wait until the stream is flowing.
    for _ in 0..100 {
        if server
            .events(&id)
            .await
            .iter()
            .any(|e| e["event"] == "output")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (code, _) = http(
        "POST",
        &format!("{}/api/v1/sessions/{id}/stop", server.base),
        Some("alice-token"),
        None,
    )
    .await;
    assert_eq!(code, 200);
    server.wait(&id, terminal).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let events = server.events(&id).await;
    let call = events
        .iter()
        .find(|e| e["event"] == "model_call")
        .expect("call recorded");
    assert_eq!(call["outcome"], "aborted", "{call}");
}

#[tokio::test]
async fn restart_carries_interrupted_sessions_on() {
    let Some((_, db)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let first = start(dir.path(), &db).await;
    let id = first.create("sleepy").await;
    first
        .wait(&id, |i| i["session"]["status"] == "running")
        .await;

    // agentcore dies: the session stops without an end in its log.
    first.state.manager.fence_all().await;
    // It starts again on the same database and data volume, and carries
    // the session on: same id, same log.
    let second = start(dir.path(), &db).await;
    let cluster = agentcore_server::cluster::start(&second.state)
        .await
        .unwrap();
    let info = second
        .wait(&id, |i| {
            i["live"] == true && i["session"]["status"] == "running"
        })
        .await;
    assert_eq!(info["session"]["created_by"]["id"], "alice");

    let events = second.events(&id).await;
    let kinds: Vec<&str> = events.iter().filter_map(|e| e["event"].as_str()).collect();
    assert_eq!(kinds.iter().filter(|k| **k == "session_created").count(), 1);
    assert_eq!(
        kinds.iter().filter(|k| **k == "session_recovered").count(),
        1
    );
    assert!(!kinds.contains(&"session_ended"), "{kinds:?}");
    let (_, verify) = http(
        "GET",
        &format!("{}/api/v1/sessions/{id}/audit/verify", second.base),
        Some("viewer-token"),
        None,
    )
    .await;
    assert_eq!(verify["valid"], true, "{verify}");

    let (code, _) = http(
        "POST",
        &format!("{}/api/v1/sessions/{id}/stop", second.base),
        Some("alice-token"),
        Some(json!({ "reason": "done" })),
    )
    .await;
    assert_eq!(code, 200);
    second
        .wait(&id, |i| i["session"]["status"] == "stopped")
        .await;
    cluster.cancel();
}

#[tokio::test]
async fn live_view_shows_screen_and_model_stream_and_can_pause() {
    let Some((_, db)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (upstream, _) = mock_anthropic().await;
    let server = start(dir.path(), &db).await;
    server.add_provider(&upstream).await;
    let id = server.create("live-agent").await;
    let base = server.base.clone();

    // A viewer watches for a few seconds.
    let watch = tokio::process::Command::new("curl")
        .args([
            "-sN",
            "--max-time",
            "4",
            "-H",
            "Authorization: Bearer viewer-token",
            &format!("{base}/api/v1/sessions/{id}/live"),
        ])
        .output();
    let out = watch.await.unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let frames: Vec<Value> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect();
    let kinds: Vec<&str> = frames.iter().filter_map(|f| f["frame"].as_str()).collect();
    assert_eq!(kinds.first(), Some(&"terminal_reset"), "{kinds:?}");
    assert!(
        frames.iter().any(|f| f["frame"] == "terminal"
            && f["data"].as_str().unwrap().contains("\u{1b}[1mthinking")),
        "{text}"
    );
    for expected in ["model_start", "model_delta", "model_end", "processes"] {
        assert!(kinds.contains(&expected), "missing {expected}: {kinds:?}");
    }
    let delta = frames.iter().find(|f| f["frame"] == "model_delta").unwrap();
    assert_eq!(delta["kind"], "text");
    assert_eq!(delta["text"], "Hello from the model");

    // Viewers watch; operators pause and resume.
    let url = |what: &str| format!("{base}/api/v1/sessions/{id}/{what}");
    assert_eq!(
        http("POST", &url("pause"), Some("viewer-token"), None)
            .await
            .0,
        403
    );
    let (code, info) = http("POST", &url("pause"), Some("alice-token"), None).await;
    assert_eq!(code, 200);
    assert_eq!(info["status"], "paused");
    let (code, info) = http("POST", &url("resume"), Some("alice-token"), None).await;
    assert_eq!(code, 200);
    assert_eq!(info["status"], "running");
    http("POST", &url("stop"), Some("alice-token"), None).await;
    server.wait(&id, terminal).await;

    // The recording replays what the viewer saw; its hash is audited.
    let cast = tokio::process::Command::new("curl")
        .args([
            "-s",
            "-H",
            "Authorization: Bearer viewer-token",
            &url("recording"),
        ])
        .output()
        .await
        .unwrap()
        .stdout;
    let cast_text = String::from_utf8_lossy(&cast);
    assert!(
        cast_text.lines().next().unwrap().contains("\"version\":2"),
        "{cast_text}"
    );
    assert!(cast_text.contains("stopped by alice"), "{cast_text}");
    assert!(cast_text.contains("thinking..."));
    let events = server.events(&id).await;
    let closed = events
        .iter()
        .find(|e| e["event"] == "recording_closed")
        .expect("recording audited");
    use sha2::Digest;
    assert_eq!(closed["sha256"], hex::encode(sha2::Sha256::digest(&cast)));
    for expected in ["paused", "resumed"] {
        let event = events.iter().find(|e| e["event"] == expected).unwrap();
        assert_eq!(event["by"]["id"], "alice", "{event}");
    }
}

async fn get_text(url: &str, token: &str) -> (u16, String) {
    let out = tokio::process::Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}", "-H"])
        .arg(format!("Authorization: Bearer {token}"))
        .arg(url)
        .output()
        .await
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let (body, code) = text.rsplit_once('\n').unwrap();
    (code.parse().unwrap(), body.to_string())
}

#[tokio::test]
async fn admins_export_logs_from_everywhere() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    // Other tests in this binary may have installed it already.
    let _ = tracing_subscriber::registry()
        .with(agentcore_server::logs::capture())
        .try_init();
    let Some((_, db)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (upstream, _) = mock_anthropic().await;
    let server = start(dir.path(), &db).await;
    server.add_provider(&upstream).await;
    let base = &server.base;
    let tools = server.create("tools-agent").await;
    let model = server.create("model-agent").await;
    server.wait(&tools, terminal).await;
    server.wait(&model, terminal).await;
    let marker = uuid::Uuid::new_v4().to_string();
    tracing::warn!(marker = %marker, "disk almost full on the test node");
    let url = |query: &str| format!("{base}/api/v1/logs/export?{query}");

    for token in ["alice-token", "viewer-token"] {
        assert_eq!(get_text(&url("format=md"), token).await.0, 403);
    }
    assert_eq!(get_text(&url("sources=nope"), "admin-token").await.0, 400);
    assert_eq!(
        get_text(
            &url("since=2026-02-01T00:00:00Z&until=2026-01-01T00:00:00Z"),
            "admin-token"
        )
        .await
        .0,
        400
    );

    // Markdown, for a model: context, the sessions, problems, the timeline.
    let (code, md) = get_text(&url("format=md"), "admin-token").await;
    assert_eq!(code, 200, "{md}");
    for expected in [
        "## About agentcore and this export",
        "## Sessions",
        &tools,
        &model,
        "## Problems",
        "policy denied",
        "disk almost full on the test node",
        "## Timeline",
    ] {
        assert!(md.contains(expected), "missing {expected}:\n{md}");
    }

    // JSON Lines: a meta line, then every record.
    let (code, jsonl) = get_text(&url("format=jsonl"), "admin-token").await;
    assert_eq!(code, 200);
    let lines: Vec<Value> = jsonl
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["type"], "meta");
    assert_eq!(lines[0]["meta"]["generated_by"], "root");
    assert_eq!(lines[0]["meta"]["records"], lines.len() - 1);
    let records = &lines[1..];
    let of = |source: &str| records.iter().filter(|r| r["source"] == source).count();
    assert!(of("audit") > 0 && of("server") > 0 && of("admin") > 0);
    assert!(records.iter().any(|r| r["source"] == "server"
        && r["level"] == "warn"
        && r["fields"]["marker"] == marker.as_str()));
    assert!(records.iter().any(|r| r["kind"] == "model_call"));
    // The model agent printed its (per-session) key; it is redacted.
    let printed = records
        .iter()
        .find(|r| r["kind"] == "output" && r["message"].as_str().unwrap().starts_with("key="))
        .expect("the agent's output");
    assert_eq!(printed["message"], "key=[REDACTED]");
    assert!(!jsonl.contains("sk-real-key"));

    // One session only; warnings and up.
    let (_, jsonl) = get_text(
        &url(&format!("format=jsonl&session={tools}&level=warn")),
        "admin-token",
    )
    .await;
    let records: Vec<Value> = jsonl
        .lines()
        .skip(1)
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(!records.is_empty());
    assert!(records.iter().all(|r| r["session"] == tools.as_str()));
    assert!(
        records
            .iter()
            .all(|r| r["level"] == "warn" || r["level"] == "error")
    );

    let (_, csv) = get_text(&url("format=csv&sources=audit"), "admin-token").await;
    assert!(csv.starts_with("time,source,level,node,session,agent,kind,message,fields\n"));
    // Cells may span lines (quoted); records start with their timestamp.
    let starts: Vec<&str> = csv
        .lines()
        .skip(1)
        .filter(|l| l.len() > 25 && l.as_bytes()[10] == b'T' && l[..30].contains("Z,"))
        .collect();
    assert!(!starts.is_empty());
    assert!(starts.iter().all(|l| l.contains(",audit,")), "{csv}");
    let (_, json) = get_text(&url("format=json"), "admin-token").await;
    let json: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(json["sessions"].as_array().unwrap().len(), 2);
    let (_, txt) = get_text(&url("format=txt"), "admin-token").await;
    assert!(txt.starts_with("# agentcore "));

    // Exports are in the change history.
    let (_, history) = http(
        "GET",
        &format!("{base}/api/v1/admin-events"),
        Some("admin-token"),
        None,
    )
    .await;
    assert!(
        history
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "logs.export" && e["actor"] == "root")
    );

    // Warnings outlive the node: they are saved to PostgreSQL.
    agentcore_server::logs::persist(&server.state).await;
    let now = chrono::Utc::now();
    let saved = server
        .state
        .store
        .node_logs(now - chrono::Duration::hours(1), now, 10_000)
        .await
        .unwrap();
    assert!(
        saved
            .iter()
            .any(|l| l.fields.0["marker"] == marker.as_str() && l.level == "warn")
    );
}
