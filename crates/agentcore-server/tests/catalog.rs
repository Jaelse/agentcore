//! The agent catalogue end to end: adding a catalogue agent through the API,
//! checking it is installed in the sandbox, and launching it with its files,
//! environment and placeholders. A fake `codex` stands in for the real one
//! (the real agents are verified manually, see docs/AGENT_CATALOG.md).
//! Requires `AGENTCORE_TEST_DATABASE_URL` (skipped otherwise).

use std::time::Duration;

use agentcore_sandbox::{BackendKind, SandboxConfig};
use agentcore_server::config::{Operator, Role};
use agentcore_server::{AppState, Config, auth::hash_token, router};
use serde_json::{Value, json};

/// Prints what agentcore handed over, then calls the model gateway the way
/// Codex would (base URL from its config file, key from the environment).
const FAKE_CODEX: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "codex-cli 0.161.0 (fake)"; exit 0; fi
cfg="$HOME/.codex/config.toml"
[ -f "$cfg" ] && echo "config: present"
grep -q "model = \"gpt-5.1\"" "$cfg" && echo "config: model"
grep -q "url = \"$AGENTCORE_GATEWAY_URL\"" "$cfg" && echo "config: mcp gateway"
[ -n "$OPENAI_API_KEY" ] && [ "$OPENAI_API_KEY" = "$AGENTCORE_GATEWAY_TOKEN" ] && echo "env: key is the session token"
case "$*" in *"$AGENTCORE_GATEWAY_TOKEN"*) echo "LEAK: token in arguments" ;; esac
case "$*" in *"--disable shell_tool"*) echo "args: native shell off" ;; esac
base=$(sed -n 's/^base_url = "\(.*\)"$/\1/p' "$cfg")
curl -s "$base/responses" -H "Authorization: Bearer $OPENAI_API_KEY" -H 'content-type: application/json' \
  -d '{"model":"gpt-5.1","input":"hi"}'
echo
echo "args: $*" | head -c 200
"#;

fn config(dir: &std::path::Path, db_url: &str, bind: std::net::SocketAddr, bin: &str) -> Config {
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
    ];
    config.database.url = Some(db_url.into());
    config.secrets.master_key_file = Some(dir.join("master.key"));
    config.storage.data_dir = dir.join("data");
    config.storage.audit_fsync = false;
    config.policies.dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../policies").into();
    config.templates.dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates").into();
    config.sandbox = SandboxConfig {
        backend: BackendKind::Process,
        ..Default::default()
    };
    config.sandbox.process.allow_insecure = true;
    config.sandbox.process.path = Some(format!("{bin}:/usr/local/bin:/usr/bin:/bin"));
    config
}

/// Answers like the OpenAI Responses API and remembers the key it got.
async fn mock_openai() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let log = seen.clone();
    let app = axum::Router::new().fallback(move |headers: axum::http::HeaderMap| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push(
                headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string(),
            );
            axum::Json(json!({
                "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-5.1",
                "output": [], "usage": { "input_tokens": 3, "output_tokens": 2 },
            }))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

async fn http(method: &str, url: &str, token: &str, body: Option<Value>) -> (u16, Value) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut req = client
        .request(method.parse().unwrap(), url)
        .bearer_auth(token);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let res = req.send().await.unwrap();
    let code = res.status().as_u16();
    let text = res.text().await.unwrap();
    (code, serde_json::from_str(&text).unwrap_or(Value::Null))
}

#[tokio::test]
async fn add_a_catalogue_agent_check_it_and_run_it() {
    let Some((_, db_url)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::new(config(dir.path(), &db_url, addr, bin.to_str().unwrap()))
        .await
        .unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    let base = format!("http://{addr}/api/v1");
    let (upstream, seen) = mock_openai().await;

    // The catalogue: permissive licenses only, with the facts people need.
    let (_, catalog) = http(
        "GET",
        &format!("{base}/agents/catalog"),
        "alice-token",
        None,
    )
    .await;
    let agents = catalog["agents"].as_array().unwrap();
    assert!(agents.len() >= 7);
    for a in agents {
        assert!(
            ["MIT", "Apache-2.0"].contains(&a["license"].as_str().unwrap()),
            "{}",
            a["id"]
        );
    }
    let codex = agents.iter().find(|a| a["id"] == "codex").unwrap();
    assert_eq!(codex["guardrails"], "full");
    assert_eq!(codex["protocols"], json!(["openai"]));

    // Providers to choose from.
    for (name, kind) in [("gpt", "openai"), ("claude", "anthropic")] {
        let (code, _) = http(
            "POST",
            &format!("{base}/providers"),
            "admin-token",
            Some(json!({
                "name": name, "kind": kind, "base_url": upstream, "api_key": "sk-real",
                "allowed_models": ["gpt-5*", "claude-*"],
            })),
        )
        .await;
        assert_eq!(code, 201);
    }
    let add = |provider: &str, model: &str| json!({ "catalog": "codex", "name": "codex", "provider": provider, "model": model });
    let url = format!("{base}/agents");
    assert_eq!(
        http("POST", &url, "alice-token", Some(add("gpt", "gpt-5.1")))
            .await
            .0,
        403
    );
    let (code, err) = http("POST", &url, "admin-token", Some(add("claude", "claude-x"))).await;
    assert_eq!(code, 400, "Codex cannot use an Anthropic provider: {err}");
    let (code, err) = http("POST", &url, "admin-token", Some(add("gpt", "o3"))).await;
    assert_eq!(code, 400, "model not allowed by the provider: {err}");
    let (code, added) = http("POST", &url, "admin-token", Some(add("gpt", "gpt-5.1"))).await;
    assert_eq!(code, 201, "{added}");
    assert_eq!(
        http("POST", &url, "admin-token", Some(add("gpt", "gpt-5.1")))
            .await
            .0,
        409
    );

    let (_, list) = http("GET", &url, "alice-token", None).await;
    let entry = list
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["spec"]["name"] == "codex")
        .unwrap();
    assert_eq!(entry["source"], "catalog");
    assert_eq!(entry["spec"]["model"], "gpt-5.1");
    let (_, card) = http("GET", &format!("{base}/system-card"), "alice-token", None).await;
    assert!(
        card["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "codex")
    );

    // Not installed in the sandbox yet...
    let (_, check) = http(
        "POST",
        &format!("{base}/agents/codex/check"),
        "alice-token",
        None,
    )
    .await;
    assert_eq!(check["available"], false, "{check}");
    assert!(check["hint"].as_str().unwrap().contains("not installed"));
    // ... now it is.
    let fake = bin.join("codex");
    std::fs::write(&fake, FAKE_CODEX).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (_, check) = http(
        "POST",
        &format!("{base}/agents/codex/check"),
        "alice-token",
        None,
    )
    .await;
    assert_eq!(check["available"], true, "{check}");
    assert!(check["version"].as_str().unwrap().contains("0.161.0"));

    // Run it: files, environment and placeholders as Codex expects them.
    let (code, session) = http(
        "POST",
        &format!("{base}/sessions"),
        "alice-token",
        Some(json!({ "agent": "codex", "task": "say hi" })),
    )
    .await;
    assert_eq!(code, 201, "{session}");
    let id = session["id"].as_str().unwrap();
    let mut events = Vec::new();
    for _ in 0..200 {
        let (_, info) = http("GET", &format!("{base}/sessions/{id}"), "alice-token", None).await;
        if info["session"]["status"] == "awaiting_input" {
            let (_, e) = http(
                "GET",
                &format!("{base}/sessions/{id}/events"),
                "alice-token",
                None,
            )
            .await;
            events = e.as_array().unwrap().clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let out: String = events
        .iter()
        .filter(|e| e["event"] == "output")
        .map(|e| format!("{}\n", e["line"].as_str().unwrap()))
        .collect();
    for line in [
        "config: present",
        "config: model",
        "config: mcp gateway",
        "env: key is the session token",
        "args: native shell off",
        "resp_1",
    ] {
        assert!(out.contains(line), "missing `{line}` in:\n{out}");
    }
    assert!(!out.contains("LEAK"), "{out}");
    // The model call went through the gateway with the real key.
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "model_call" && e["provider"] == "gpt")
    );
    assert_eq!(seen.lock().unwrap().as_slice(), ["Bearer sk-real"]);
    let started = events
        .iter()
        .find(|e| e["event"] == "agent_started")
        .unwrap();
    assert!(!started.to_string().contains("sk-real"));

    // Change the model; remove the agent.
    let (code, updated) = http(
        "PUT",
        &format!("{base}/agents/codex"),
        "admin-token",
        Some(json!({ "model": "gpt-5.1-codex" })),
    )
    .await;
    assert_eq!(code, 200, "{updated}");
    assert_eq!(updated["spec"]["model"], "gpt-5.1-codex");
    assert_eq!(
        http(
            "DELETE",
            &format!("{base}/agents/codex"),
            "admin-token",
            None
        )
        .await
        .0,
        204
    );
    let (code, _) = http(
        "POST",
        &format!("{base}/sessions"),
        "alice-token",
        Some(json!({ "agent": "codex", "task": "x" })),
    )
    .await;
    assert_eq!(code, 400, "removed agents cannot be started");
}
