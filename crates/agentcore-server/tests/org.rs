//! Multi-agent organisations end to end: departments, communicators,
//! message routing, pause/stop at every level, and two nodes sharing one
//! database. Agents are shell scripts calling the MCP gateway with curl.
//! Requires `AGENTCORE_TEST_DATABASE_URL` (skipped otherwise).

use std::time::Duration;

use agentcore_core::AgentSpec;
use agentcore_sandbox::{BackendKind, SandboxConfig};
use agentcore_server::config::{Operator, Role};
use agentcore_server::{AppState, Config, auth::hash_token, cluster, router};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// `call <tool> <json arguments>`: prints the tool result.
const CALL: &str = r#"
call() {
  curl -s -X POST "$AGENTCORE_GATEWAY_URL" \
    -H "Authorization: Bearer $AGENTCORE_GATEWAY_TOKEN" -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"$1\",\"arguments\":$2}}"
  echo
}
"#;

/// Research worker: asks its communicator for help (first turn only) and
/// tries what it must not.
const ANALYST: &str = r##"
case "$0" in
  "You have "*" new message(s):"*) ;;
  *)
    call team_send_message '{"to":"communicator","text":"NEED the API rate limits"}'
    call team_send_message '{"to":"dev","text":"hi"}'
    call run_command '{"command":"echo","args":["x"]}'
    call team_write_file '{"path":"/notes/plan.md","content":"# plan"}' ;;
esac
"##;

/// Both communicators: pass each marker on to the next hop.
const RELAY: &str = r#"
case "$0" in
  *NEED*) call team_send_to_department '{"department":"eng","text":"ASK rate limits for Research"}' ;;
  *ASK*) call run_command '{"command":"echo","args":["x"]}'
         call team_send_message '{"to":"everyone","text":"FROM-RESEARCH rate limits?"}' ;;
  *ANSWER*) call team_send_to_department '{"department":"Research","text":"REPLY 100 req/s"}' ;;
  *REPLY*) call team_send_message '{"to":"analyst","text":"DONE 100 req/s"}' ;;
esac
"#;

/// Eng worker: answers questions from Research.
const DEV: &str = r#"
case "$0" in
  *FROM-RESEARCH*) call team_send_message '{"to":"communicator","text":"ANSWER 100 req/s per key"}' ;;
esac
"#;

/// Long-running worker: reports progress on the first turn of every
/// session and says what its prompt told it.
const GOAL_WORKER: &str = r###"
case "$0" in
  "You have "*" new message(s):"*) ;;
  *)
    call team_list_goals '{}'
    call team_report_progress '{"goal":"Reach 10 paying customers","text":"Drafted the pricing page"}'
    case "$0" in *"Reach 10 paying customers"*) echo GOAL-IN-PROMPT ;; esac
    case "$0" in *"You are continuing"*) echo CONTINUING ;; esac
    case "$0" in *"## New messages"*"check-in: daily"*) echo WOKEN-BY-CHECK-IN ;; esac ;;
esac
"###;

/// Retrospective: reads metrics and business data, proposes, revises when
/// asked, proposes again on request.
const RETRO: &str = r###"
case "$0" in
  *"asked for changes"*)
    id=$(printf %s "$0" | grep "asked for changes" | sed -n 's/.*(id \([0-9a-f-]*\)).*/\1/p' | head -n 1)
    call insights_revise "{\"proposal\":\"$id\",\"note\":\"as asked\",\"actions\":[{\"kind\":\"create_goal\",\"title\":\"Ship v2\"}]}" ;;
  *PROPOSE-AGAIN*)
    call insights_propose '{"title":"Auto","problem":"p","solution":"s","actions":[{"kind":"create_goal","title":"Auto goal"}]}' ;;
  "You have "*) ;;
  *)
    call insights_metrics '{"days":7}' | head -c 100; echo
    call data_query '{"source":"customers","filter":{"plan":"pro"}}'
    call data_query '{"source":"warehouse","sql":"SELECT 41 + 1 AS answer"}'
    call data_query '{"source":"warehouse","sql":"SELECT 1; DROP TABLE sessions"}'
    call insights_propose '{"title":"More focus","problem":"No goals","evidence":"0 goals","solution":"Add a goal","actions":[{"kind":"create_goal","title":"Ship v1"}]}'
    call insights_propose '{"title":"Room to grow","problem":"Limits too low","solution":"Raise them","actions":[{"kind":"set_limits","max_departments":7}]}' ;;
esac
"###;

/// Drafts messages for the outside world; revises when asked.
const DRAFTER: &str = r###"
case "$0" in
  *"asked for changes"*)
    id=$(printf %s "$0" | grep "asked for changes" | sed -n 's/.*(id \([0-9a-f-]*\)).*/\1/p' | head -n 1)
    call outbox_revise "{\"draft\":\"$id\",\"text\":\"Launch post FINAL\",\"note\":\"as asked\"}" ;;
  "You have "*) ;;
  *)
    call outbox_channels '{}'
    call outbox_draft '{"channel":"mail","to":["Ana <ana@customer.com>"],"subject":"Hello","text":"Welcome aboard"}'
    call outbox_draft '{"channel":"mail","to":["eve@evil.com"],"text":"x"}'
    call outbox_draft '{"channel":"hooks","subject":"Launch","text":"Launch post DRAFT-1"}'
    call outbox_draft '{"channel":"team-slack","text":"Status: all good"}'
    call outbox_draft '{"channel":"team-slack","text":"Second status"}' ;;
esac
"###;

/// Calls the model once per turn (25 input and 15 output tokens).
const SPENDER: &str = r#"
curl -sN "$ANTHROPIC_BASE_URL/v1/messages" -H "x-api-key: $ANTHROPIC_API_KEY" \
  -H 'anthropic-version: 2023-06-01' -H 'content-type: application/json' \
  -d '{"model":"claude-test","stream":true,"max_tokens":10,"messages":[{"role":"user","content":"hi"}]}'
echo
"#;

/// Runs a script on the first turn and on every message (`$0` = text).
fn agent(name: &str, script: &str) -> AgentSpec {
    let first = format!("{CALL}\nset -- \"$0\"\n{script}\necho \"{name} turn: $0\" | head -c 300");
    let follow = format!("{CALL}\n{script}\necho \"{name} got: $0\"");
    AgentSpec {
        name: name.into(),
        adapter: "command".into(),
        description: String::new(),
        image: None,
        command: Some("sh".into()),
        args: vec!["-c".into(), first, "{task}".into()],
        env: Default::default(),
        policy: None,
        tty: Some(false),
        follow_up_args: vec!["-c".into(), follow, "{task}".into()],
        ..Default::default()
    }
}

fn config(dir: &std::path::Path, db_url: &str, bind: std::net::SocketAddr, node: &str) -> Config {
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
    config.storage.data_dir = dir.join(node);
    config.storage.audit_fsync = false;
    config.policies.dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../policies").into();
    config.templates.dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates").into();
    config.roles.dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../roles").into();
    config.sandbox = SandboxConfig {
        backend: BackendKind::Process,
        ..Default::default()
    };
    config.sandbox.process.allow_insecure = true;
    config.cluster.node_name = Some(node.into());
    config.cluster.reconcile_millis = 200;
    config.cluster.heartbeat_secs = 1;
    config.cluster.node_timeout_secs = 3;
    config.agents = vec![
        agent("analyst-bot", ANALYST),
        agent("relay-bot", RELAY),
        agent("dev-bot", DEV),
        agent("idle-bot", ""),
        agent("goal-bot", GOAL_WORKER),
        agent("retro-bot", RETRO),
        agent("spender", SPENDER),
        agent("drafter", DRAFTER),
    ];
    config
}

struct Node {
    base: String,
    state: AppState,
    cluster: CancellationToken,
}

async fn start(dir: &std::path::Path, db_url: &str, name: &str, capacity: u32) -> Node {
    start_with(dir, db_url, name, capacity, |_| {}).await
}

async fn start_with(
    dir: &std::path::Path,
    db_url: &str,
    name: &str,
    capacity: u32,
    change: impl FnOnce(&mut Config),
) -> Node {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = config(dir, db_url, addr, name);
    config.cluster.max_agents = capacity;
    change(&mut config);
    let state = AppState::new(config).await.unwrap();
    let cluster = cluster::start(&state).await.unwrap();
    let app = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Node {
        base: format!("http://{addr}"),
        state,
        cluster,
    }
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

impl Node {
    async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        http(method, &format!("{}/api/v1{path}", self.base), token, body).await
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (code, value) = self.call(method, path, "admin-token", body).await;
        assert!(
            (200..300).contains(&code),
            "{method} {path}: {code} {value}"
        );
        value
    }

    async fn agent(&self, id: &str) -> Value {
        let org = self.ok("GET", "/org", None).await;
        org["departments"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|d| d["agents"].as_array().unwrap().clone())
            .find(|a| a["id"] == id)
            .unwrap()
    }

    async fn wait_agent(&self, id: &str, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..300 {
            let agent = self.agent(id).await;
            if pred(&agent) {
                return agent;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {what}: {}", self.agent(id).await);
    }

    async fn wait_session(&self, id: &str, pred: impl Fn(&str) -> bool) -> String {
        for _ in 0..300 {
            let (_, info) = self
                .call("GET", &format!("/sessions/{id}"), "viewer-token", None)
                .await;
            if let Some(status) = info["session"]["status"].as_str()
                && pred(status)
            {
                return status.to_string();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for session {id}");
    }

    async fn events(&self, session: &str) -> Vec<Value> {
        self.ok("GET", &format!("/sessions/{session}/events"), None)
            .await
            .as_array()
            .cloned()
            .unwrap()
    }

    async fn messages(&self, query: &str) -> Vec<Value> {
        self.ok("GET", &format!("/org/messages{query}"), None)
            .await
            .as_array()
            .cloned()
            .unwrap()
    }
}

fn output(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["event"] == "output")
        .map(|e| e["line"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

fn department(name: &str, tools: &[&str]) -> Value {
    json!({
        "name": name, "mission": format!("{name} work"), "tools": tools,
        "communicator_agent": "relay-bot",
    })
}

#[tokio::test]
async fn departments_talk_through_their_communicators() {
    let Some((_, db_url)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let node = start(dir.path(), &db_url, "n1", 10).await;

    // Limits: an admin sets them; departments need an admin.
    node.ok(
        "PUT",
        "/org/settings",
        Some(json!({ "max_departments": 2, "max_agents_per_department": 1 })),
    )
    .await;
    let (code, _) = node
        .call(
            "POST",
            "/org/departments",
            "alice-token",
            Some(department("X", &[])),
        )
        .await;
    assert_eq!(code, 403, "operators cannot create departments");
    let research = node
        .ok(
            "POST",
            "/org/departments",
            Some(department("Research", &["files"])),
        )
        .await;
    let eng = node
        .ok(
            "POST",
            "/org/departments",
            Some(department("Eng", &["sandbox"])),
        )
        .await;
    assert_eq!(research["policy"], "department");
    assert_eq!(research["agents"][0]["kind"], "communicator");
    let (code, err) = node
        .call(
            "POST",
            "/org/departments",
            "admin-token",
            Some(department("Legal", &[])),
        )
        .await;
    assert_eq!(code, 409, "{err}");
    let rid = research["id"].as_str().unwrap();
    let eid = eng["id"].as_str().unwrap();

    let analyst = node
        .ok(
            "POST",
            &format!("/org/departments/{rid}/agents"),
            Some(json!({ "name": "analyst", "agent": "analyst-bot" })),
        )
        .await;
    let (code, _) = node
        .call(
            "POST",
            &format!("/org/departments/{rid}/agents"),
            "alice-token",
            Some(json!({ "name": "writer", "agent": "idle-bot" })),
        )
        .await;
    assert_eq!(code, 409, "one worker per department");
    let dev = node
        .ok(
            "POST",
            &format!("/org/departments/{eid}/agents"),
            Some(json!({ "name": "dev", "agent": "dev-bot", "instructions": "Answer questions." })),
        )
        .await;

    // Start Eng first so it is ready when Research asks.
    let started = node
        .ok("POST", &format!("/org/departments/{eid}/start"), None)
        .await;
    assert_eq!(started["changed"], 2);
    let dev = node
        .wait_agent(dev["id"].as_str().unwrap(), "dev waiting", |a| {
            a["status"] == "awaiting_input"
        })
        .await;
    node.ok("POST", &format!("/org/departments/{rid}/start"), None)
        .await;

    // The question travels analyst → Research communicator → Eng
    // communicator → Eng room → dev, and the answer comes back.
    let analyst_id = analyst["id"].as_str().unwrap();
    let mut done = false;
    for _ in 0..300 {
        if node
            .messages("")
            .await
            .iter()
            .any(|m| m["text"].as_str().unwrap().starts_with("DONE"))
        {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(done, "messages: {:#?}", node.messages("").await);
    let flow: Vec<(String, String, String, String)> = node
        .messages("")
        .await
        .iter()
        .map(|m| {
            (
                m["text"]
                    .as_str()
                    .unwrap()
                    .split(' ')
                    .next()
                    .unwrap()
                    .to_string(),
                m["scope"].as_str().unwrap().to_string(),
                m["from_name"].as_str().unwrap().to_string(),
                m["to_name"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let expected = [
        (
            "NEED",
            "internal",
            "analyst (Research)",
            "communicator (Research)",
        ),
        ("ASK", "inter_department", "communicator (Research)", "Eng"),
        (
            "FROM-RESEARCH",
            "internal",
            "communicator (Eng)",
            "everyone in Eng",
        ),
        ("ANSWER", "internal", "dev (Eng)", "communicator (Eng)"),
        (
            "REPLY",
            "inter_department",
            "communicator (Eng)",
            "Research",
        ),
        (
            "DONE",
            "internal",
            "communicator (Research)",
            "analyst (Research)",
        ),
    ];
    assert_eq!(
        flow,
        expected
            .iter()
            .map(|(a, b, c, d)| (a.to_string(), b.to_string(), c.to_string(), d.to_string()))
            .collect::<Vec<_>>()
    );

    // The analyst got the answer as a new turn, recorded in its audit log.
    let analyst_now = node
        .wait_agent(analyst_id, "analyst answered", |a| {
            a["status"] == "awaiting_input"
        })
        .await;
    let analyst_session = analyst_now["session_id"].as_str().unwrap().to_string();
    let mut events = Vec::new();
    for _ in 0..100 {
        events = node.events(&analyst_session).await;
        if output(&events).contains("analyst-bot got: You have 1 new message(s)") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let out = output(&events);
    assert!(out.contains("DONE 100 req/s"), "{out}");
    assert!(events.iter().any(|e| e["event"] == "messages_delivered"
        && e["messages"][0]["from"] == "communicator (Research)"));
    // It could not reach Eng directly, nor use a sandbox its department lacks.
    assert!(
        out.contains("there is no `dev` in your department"),
        "{out}"
    );
    assert!(out.contains("unknown tool `run_command`"), "{out}");
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "action_requested"
                && e["requested_by"]["id"] == "Research/analyst")
    );

    // Communicators never get sandbox tools.
    let eng_comm = eng["agents"][0]["id"].as_str().unwrap();
    let eng_comm = node.agent(eng_comm).await;
    let comm_out = output(&node.events(eng_comm["session_id"].as_str().unwrap()).await);
    assert!(
        comm_out.contains("unknown tool `run_command`"),
        "{comm_out}"
    );

    // Department files are shared within Research only.
    let files = node
        .ok("GET", &format!("/org/departments/{rid}/files"), None)
        .await;
    assert_eq!(files[0]["path"], "notes/plan.md");
    let file = node
        .ok(
            "GET",
            &format!("/org/departments/{rid}/files/notes/plan.md"),
            None,
        )
        .await;
    assert_eq!(file["content"], "# plan");
    assert!(
        node.ok("GET", &format!("/org/departments/{eid}/files"), None)
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Feeds per department and per agent.
    assert_eq!(node.messages(&format!("?department={rid}")).await.len(), 4);
    assert_eq!(
        node.messages(&format!("?agent={analyst_id}")).await.len(),
        2
    );

    // A person posts in the Eng room: everyone there receives it.
    let posted = node
        .ok(
            "POST",
            "/org/messages",
            Some(json!({ "to": { "department": eid }, "text": "hello Eng" })),
        )
        .await;
    assert_eq!(posted["scope"], "human");
    assert_eq!(posted["recipients"].as_array().unwrap().len(), 2);
    let dev_session = dev["session_id"].as_str().unwrap().to_string();
    for _ in 0..100 {
        if output(&node.events(&dev_session).await).contains("hello Eng") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(output(&node.events(&dev_session).await).contains("dev-bot got:"));

    // Pause the department: its agents freeze; resume; then stop it.
    node.ok("POST", &format!("/org/departments/{eid}/pause"), None)
        .await;
    assert_eq!(
        node.wait_session(&dev_session, |s| s == "paused").await,
        "paused"
    );
    node.ok("POST", &format!("/org/departments/{eid}/resume"), None)
        .await;
    node.wait_session(&dev_session, |s| s == "awaiting_input")
        .await;
    // Pause one agent.
    node.ok(
        "POST",
        &format!("/org/agents/{}/pause", dev["id"].as_str().unwrap()),
        None,
    )
    .await;
    node.wait_session(&dev_session, |s| s == "paused").await;
    let events = node.events(&dev_session).await;
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "paused" && e["by"]["id"] == "root")
    );
    node.ok("POST", &format!("/org/departments/{eid}/stop"), None)
        .await;
    node.wait_session(&dev_session, |s| s == "stopped").await;
    let dev_now = node
        .wait_agent(dev["id"].as_str().unwrap(), "dev stopped", |a| {
            a["desired"] == "stopped" && a["status"] == "stopped"
        })
        .await;
    assert_eq!(dev_now["changed_by"], "root");
    // Only empty departments can be deleted.
    let (code, _) = node
        .call(
            "DELETE",
            &format!("/org/departments/{rid}"),
            "admin-token",
            None,
        )
        .await;
    assert_eq!(code, 409);

    // Emergency stop: everything, everywhere.
    let stopped = node
        .ok("POST", "/stop-all", Some(json!({ "reason": "test" })))
        .await;
    assert_eq!(stopped["department_agents"], 2);
    node.wait_agent(analyst_id, "analyst stopped", |a| a["status"] == "stopped")
        .await;
    node.ok("DELETE", &format!("/org/departments/{rid}"), None)
        .await;
    node.cluster.cancel();
}

#[tokio::test]
async fn agents_spread_over_nodes_and_nodes_can_be_lost() {
    let Some((_, db_url)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let n1 = start(dir.path(), &db_url, "n1", 1).await;
    let n2 = start(dir.path(), &db_url, "n2", 1).await;

    let dept = n1
        .ok(
            "POST",
            "/org/departments",
            Some(json!({ "name": "Ops", "tools": [], "communicator_agent": "idle-bot" })),
        )
        .await;
    let id = dept["id"].as_str().unwrap();
    let worker = n1
        .ok(
            "POST",
            &format!("/org/departments/{id}/agents"),
            Some(json!({ "name": "w", "agent": "idle-bot" })),
        )
        .await;
    let extra = n1
        .ok(
            "POST",
            &format!("/org/departments/{id}/agents"),
            Some(json!({ "name": "x", "agent": "idle-bot" })),
        )
        .await;
    // Two agents fit (one per node); the third does not.
    let comm_id = dept["agents"][0]["id"].as_str().unwrap();
    n1.ok("POST", &format!("/org/agents/{comm_id}/start"), None)
        .await;
    n1.ok(
        "POST",
        &format!("/org/agents/{}/start", worker["id"].as_str().unwrap()),
        None,
    )
    .await;
    let (code, err) = n1
        .call(
            "POST",
            &format!("/org/agents/{}/start", extra["id"].as_str().unwrap()),
            "alice-token",
            None,
        )
        .await;
    assert_eq!(code, 503, "{err}");

    let comm = n1
        .wait_agent(comm_id, "communicator running", |a| {
            a["status"] == "awaiting_input"
        })
        .await;
    let worker = n1
        .wait_agent(worker["id"].as_str().unwrap(), "worker running", |a| {
            a["status"] == "awaiting_input"
        })
        .await;
    let nodes: Vec<&str> = [&comm, &worker]
        .iter()
        .map(|a| a["node"].as_str().unwrap())
        .collect();
    assert_eq!(
        {
            let mut n = nodes.clone();
            n.sort();
            n
        },
        ["n1", "n2"]
    );
    let overview = n2.ok("GET", "/org", None).await;
    assert!(
        overview["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["alive"] == true && n["agents"] == 1)
    );

    // Each node answers for sessions on the other one (forwarded).
    let (on_n2, other) = if comm["node"] == "n2" {
        (comm.clone(), worker.clone())
    } else {
        (worker.clone(), comm.clone())
    };
    let remote = on_n2["session_id"].as_str().unwrap();
    assert!(n2.state.manager.get(remote.parse().unwrap()).is_ok());
    assert!(n1.state.manager.get(remote.parse().unwrap()).is_err());
    let info = n1.ok("GET", &format!("/sessions/{remote}"), None).await;
    assert_eq!(info["live"], true, "served live by its own node: {info}");
    assert!(!n1.events(remote).await.is_empty());
    let (code, _) = n1
        .call("GET", &format!("/sessions/{remote}"), "bad-token", None)
        .await;
    assert_eq!(code, 401);
    // Pausing through n1 freezes the session on n2.
    n1.ok("POST", &format!("/sessions/{remote}/pause"), None)
        .await;
    assert_eq!(n2.wait_session(remote, |s| s == "paused").await, "paused");
    n1.ok("POST", &format!("/sessions/{remote}/resume"), None)
        .await;
    n2.wait_session(remote, |s| s == "awaiting_input").await;

    // n2 disappears: its agent is marked lost, its sessions unreachable.
    n2.cluster.cancel();
    let lost = n1
        .wait_agent(on_n2["id"].as_str().unwrap(), "agent lost", |a| {
            a["desired"] == "stopped" && a["status"] == "failed"
        })
        .await;
    assert!(
        lost["note"]
            .as_str()
            .unwrap()
            .contains("n2 stopped responding")
    );
    let (code, err) = n1
        .call("GET", &format!("/sessions/{remote}"), "viewer-token", None)
        .await;
    assert_eq!(code, 503, "{err}");
    // The agent on n1 is unaffected; stop-all from n1 stops it.
    let survivor = n1.agent(other["id"].as_str().unwrap()).await;
    assert_eq!(survivor["desired"], "running");
    n1.ok("POST", "/stop-all", None).await;
    n1.wait_agent(other["id"].as_str().unwrap(), "survivor stopped", |a| {
        a["status"] == "stopped"
    })
    .await;
    // Leave nothing running in this process.
    n2.state
        .manager
        .stop_all(agentcore_core::Principal::System, "test over")
        .await;
    n1.cluster.cancel();
}

#[tokio::test]
async fn build_an_organisation_from_templates_and_grow_it() {
    let Some((_, db_url)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let node = start(dir.path(), &db_url, "n1", 50).await;

    let catalog = node.ok("GET", "/org/templates", None).await;
    assert!(catalog["departments"].as_array().unwrap().len() >= 25);
    assert_eq!(catalog["categories"].as_array().unwrap().len(), 5);
    // Nothing yet: no suggestions to grow from.
    assert!(
        node.ok("GET", "/org/suggestions", None)
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Start small: the first stage of the solo developer path, lean.
    let profile = json!({
        "company_name": "Acme", "company_about": "We make rockets.",
        "blueprint": "solo-developer",
    });
    let build = |departments: Value, extra: Value| {
        let mut body = json!({
            "profile": profile, "departments": departments, "size": "lean",
            "agent": "idle-bot",
        });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        body
    };
    let (code, _) = node
        .call(
            "POST",
            "/org/build",
            "alice-token",
            Some(build(json!(["engineering"]), json!({}))),
        )
        .await;
    assert_eq!(code, 403, "only admins build the organisation");
    let dry = node
        .ok(
            "POST",
            "/org/build",
            Some(build(json!(["engineering"]), json!({ "dry_run": true }))),
        )
        .await;
    assert_eq!(dry["plan"]["new_departments"], 1);
    assert!(
        node.ok("GET", "/org", None).await["departments"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let built = node
        .ok(
            "POST",
            "/org/build",
            Some(build(json!(["engineering"]), json!({ "start": true }))),
        )
        .await;
    assert_eq!(built["created"].as_array().unwrap().len(), 1, "{built}");
    assert!(built["errors"].as_array().unwrap().is_empty(), "{built}");
    let eng = &built["created"][0];
    assert_eq!(eng["template"], "engineering");
    assert!(eng["mission"].as_str().unwrap().contains("Acme"));

    // The agents know their company.
    let org = node.ok("GET", "/org", None).await;
    let members = org["departments"][0]["agents"].as_array().unwrap().clone();
    let names: Vec<&str> = members
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["lead", "developer", "communicator"]);
    let lead = node
        .wait_agent(members[0]["id"].as_str().unwrap(), "lead running", |a| {
            a["status"] == "awaiting_input"
        })
        .await;
    let events = node.events(lead["session_id"].as_str().unwrap()).await;
    let out = output(&events);
    assert!(out.contains("Acme: We make rockets."), "{out}");

    // Suggestions: the next stage first, then neighbours and more agents.
    let suggestions = node.ok("GET", "/org/suggestions", None).await;
    let first = &suggestions[0];
    assert_eq!(first["kind"], "stage");
    assert_eq!(first["templates"], json!(["qa"]));
    assert!(
        suggestions
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["kind"] == "agent" && s["agent"]["name"] == "reviewer")
    );

    // Going big: the complete company does not fit the limits...
    let all: Vec<Value> = catalog["blueprints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == "complete-company")
        .unwrap()["stages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| s["departments"].as_array().unwrap().clone())
        .collect();
    let (code, err) = node
        .call(
            "POST",
            "/org/build",
            "admin-token",
            Some(build(json!(all), json!({}))),
        )
        .await;
    assert_eq!(code, 409, "{err}");
    assert!(err["plan"]["needs"]["max_departments"].as_u64().unwrap() > 10);
    // ... unless the admin raises them in the same step.
    let big = node
        .ok(
            "POST",
            "/org/build",
            Some(build(
                json!(all),
                json!({
                    "raise_limits": true, "size": "full", "check_ins": true,
                    "goal": "Ship version 1",
                }),
            )),
        )
        .await;
    assert_eq!(
        big["created"].as_array().unwrap().len(),
        all.len() - 1,
        "engineering exists"
    );
    assert!(big["errors"].as_array().unwrap().is_empty(), "{big}");
    assert_eq!(big["settings"]["max_departments"], all.len());
    let org = node.ok("GET", "/org", None).await;
    assert_eq!(org["departments"].as_array().unwrap().len(), all.len());
    // Kept running: a daily check-in for each new department's lead, a
    // weekly review in Strategy, and the first goal.
    assert_eq!(big["goal"]["title"], "Ship version 1");
    let check_ins = org["check_ins"].as_array().unwrap();
    let daily = check_ins.iter().filter(|c| c["name"] == "daily").count();
    assert_eq!(
        daily,
        all.len() - 2,
        "engineering exists; the retrospective has its own"
    );
    assert_eq!(
        check_ins
            .iter()
            .filter(|c| c["name"] == "daily retrospective")
            .count(),
        1
    );
    assert_eq!(
        check_ins
            .iter()
            .filter(|c| c["name"] == "weekly review")
            .count(),
        1
    );

    node.ok("POST", "/stop-all", None).await;
    node.cluster.cancel();
}

/// Policies of the repository plus two with short limits.
fn short_policies(dir: &std::path::Path) -> std::path::PathBuf {
    let policies = dir.join("policies");
    std::fs::create_dir_all(&policies).unwrap();
    let source = concat!(env!("CARGO_MANIFEST_DIR"), "/../../policies");
    for entry in std::fs::read_dir(source).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(&path, policies.join(path.file_name().unwrap())).unwrap();
    }
    let department = std::fs::read_to_string(policies.join("department.toml")).unwrap();
    for (name, max, idle) in [("short-idle", 3600, 2), ("short-life", 3, 3600)] {
        let text = department
            .replace("name = \"department\"", &format!("name = \"{name}\""))
            .replace(
                "max_session_secs = 28800",
                &format!("max_session_secs = {max}"),
            )
            .replace(
                "idle_timeout_secs = 28800",
                &format!("idle_timeout_secs = {idle}"),
            );
        std::fs::write(policies.join(format!("{name}.toml")), text).unwrap();
    }
    policies
}

#[tokio::test]
async fn goals_check_ins_and_agents_that_keep_going() {
    let Some((_, db_url)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let policies = short_policies(dir.path());
    let node = start_with(dir.path(), &db_url, "n1", 10, |c| c.policies.dir = policies).await;

    // Goals: operators set them, viewers cannot.
    let (code, _) = node
        .call(
            "POST",
            "/org/goals",
            "viewer-token",
            Some(json!({ "title": "x" })),
        )
        .await;
    assert_eq!(code, 403);
    let (code, goal) = node
        .call(
            "POST",
            "/org/goals",
            "alice-token",
            Some(json!({ "title": "Reach 10 paying customers", "description": "By March." })),
        )
        .await;
    assert_eq!(code, 201, "{goal}");
    let goal_id = goal["id"].as_str().unwrap().to_string();

    // A worker that sleeps when idle: it sees the goal and reports progress.
    let mut ops = department("Ops", &["files"]);
    ops["policy"] = json!("short-idle");
    let ops = node.ok("POST", "/org/departments", Some(ops)).await;
    let ops_id = ops["id"].as_str().unwrap();
    let worker = node
        .ok(
            "POST",
            &format!("/org/departments/{ops_id}/agents"),
            Some(json!({ "name": "worker", "agent": "goal-bot" })),
        )
        .await;
    let worker_id = worker["id"].as_str().unwrap();
    node.ok("POST", &format!("/org/departments/{ops_id}/start"), None)
        .await;
    let first = node
        .wait_agent(worker_id, "worker started", |a| a["session_id"].is_string())
        .await;
    let first_session = first["session_id"].as_str().unwrap().to_string();
    for _ in 0..200 {
        let goals = node.ok("GET", "/org/goals", None).await;
        if goals[0]["progress"] == "Drafted the pricing page" {
            assert_eq!(goals[0]["progress_by"], "worker (Ops)");
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let goals = node.ok("GET", "/org/goals", None).await;
    assert_eq!(goals[0]["progress"], "Drafted the pricing page", "{goals}");

    // Nothing to do: the session ends at the idle timeout and the agent
    // sleeps, still wanted.
    let asleep = node
        .wait_agent(worker_id, "worker asleep", |a| a["status"] == "asleep")
        .await;
    assert_eq!(asleep["desired"], "running");
    let out = output(&node.events(&first_session).await);
    assert!(out.contains("GOAL-IN-PROMPT"), "{out}");

    // A check-in wakes it in a fresh session with the message in its prompt.
    let (code, check_in) = node
        .call(
            "POST",
            &format!("/org/departments/{ops_id}/checkins"),
            "alice-token",
            Some(json!({
                "name": "daily", "message": "What moves the goal today?",
                "every_minutes": 1440, "agent_id": worker_id,
            })),
        )
        .await;
    assert_eq!(code, 201, "{check_in}");
    let check_id = check_in["id"].as_str().unwrap();
    let ran = node
        .ok("POST", &format!("/org/checkins/{check_id}/run"), None)
        .await;
    assert!(ran["last_run_at"].is_string());
    let woken = node
        .wait_agent(worker_id, "worker woken", |a| {
            a["session_id"].is_string() && a["session_id"] != first_session.as_str()
        })
        .await;
    let second = woken["session_id"].as_str().unwrap().to_string();
    node.wait_agent(worker_id, "worker asleep again", |a| {
        a["status"] == "asleep"
    })
    .await;
    let out = output(&node.events(&second).await);
    assert!(out.contains("WOKEN-BY-CHECK-IN"), "{out}");
    assert!(
        out.contains("CONTINUING"),
        "a woken agent starts from its notes: {out}"
    );
    let overview = node.ok("GET", "/org", None).await;
    assert_eq!(overview["goals"][0]["id"], goal_id.as_str());
    assert_eq!(overview["check_ins"][0]["name"], "daily");

    // Turned off, it cannot be run; stopping a sleeping agent stops it.
    node.ok(
        "PUT",
        &format!("/org/checkins/{check_id}"),
        Some(json!({ "enabled": false })),
    )
    .await;
    let (code, _) = node
        .call(
            "POST",
            &format!("/org/checkins/{check_id}/run"),
            "alice-token",
            None,
        )
        .await;
    assert_eq!(code, 409);
    node.ok("POST", &format!("/org/agents/{worker_id}/stop"), None)
        .await;
    node.wait_agent(worker_id, "worker stopped", |a| {
        a["status"] == "stopped" && a["desired"] == "stopped"
    })
    .await;

    // Out of time: the agent continues in a fresh session.
    let mut night = department("Night", &["files"]);
    night["policy"] = json!("short-life");
    let night = node.ok("POST", "/org/departments", Some(night)).await;
    let night_id = night["id"].as_str().unwrap();
    let owl = node
        .ok(
            "POST",
            &format!("/org/departments/{night_id}/agents"),
            Some(json!({ "name": "owl", "agent": "goal-bot" })),
        )
        .await;
    let owl_id = owl["id"].as_str().unwrap();
    node.ok("POST", &format!("/org/agents/{owl_id}/start"), None)
        .await;
    let first = node
        .wait_agent(owl_id, "owl started", |a| a["session_id"].is_string())
        .await;
    let first_session = first["session_id"].as_str().unwrap().to_string();
    let next = node
        .wait_agent(owl_id, "owl continued", |a| {
            a["session_id"].is_string() && a["session_id"] != first_session.as_str()
        })
        .await;
    let next_session = next["session_id"].as_str().unwrap().to_string();
    node.wait_session(&next_session, |s| s != "starting").await;
    for _ in 0..100 {
        if output(&node.events(&next_session).await).contains("CONTINUING") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let out = output(&node.events(&next_session).await);
    assert!(out.contains("CONTINUING"), "{out}");
    node.ok("POST", &format!("/org/agents/{owl_id}/stop"), None)
        .await;
    node.cluster.cancel();
}

#[tokio::test]
async fn retrospective_proposes_people_decide() {
    let Some((_, db_url)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let node = start(dir.path(), &db_url, "n1", 10).await;

    let mut retro = department("Retro", &["files", "insights"]);
    retro["mission"] = json!("Improve how we work");
    let retro = node.ok("POST", "/org/departments", Some(retro)).await;
    let rid = retro["id"].as_str().unwrap().to_string();

    // Business data: a table and a database, granted to Retro.
    let (code, _) = node
        .call(
            "POST",
            "/org/data-sources",
            "alice-token",
            Some(json!({ "name": "x", "kind": "table", "content": "a\n1\n" })),
        )
        .await;
    assert_eq!(code, 403, "admins add data sources");
    let table = node
        .ok(
            "POST",
            "/org/data-sources",
            Some(json!({
                "name": "customers", "kind": "table", "description": "Our customers",
                "content": "customer,plan,mrr\nAcme,pro,1200\nBeta,free,0\nCorp,pro,900\n",
                "departments": [rid],
            })),
        )
        .await;
    assert_eq!(table["rows"], 3);
    let warehouse = node
        .ok(
            "POST",
            "/org/data-sources",
            Some(json!({
                "name": "warehouse", "kind": "postgres", "description": "Sales database",
                "secret": db_url, "departments": [rid],
            })),
        )
        .await;
    assert!(warehouse.get("secret").is_none() && warehouse["secret_hint"].is_string());
    let tested = node
        .ok(
            "POST",
            &format!(
                "/org/data-sources/{}/test",
                warehouse["id"].as_str().unwrap()
            ),
            None,
        )
        .await;
    assert_eq!(tested["ok"], true, "{tested}");
    let tested = node
        .ok(
            "POST",
            &format!(
                "/org/data-sources/{}/test",
                warehouse["id"].as_str().unwrap()
            ),
            Some(json!({ "sql": "CREATE TABLE nope (x int)" })),
        )
        .await;
    assert_eq!(tested["ok"], false, "{tested}");

    let bot = node
        .ok(
            "POST",
            &format!("/org/departments/{rid}/agents"),
            Some(json!({ "name": "retro", "agent": "retro-bot" })),
        )
        .await;
    let bot_id = bot["id"].as_str().unwrap();
    node.ok("POST", &format!("/org/agents/{bot_id}/start"), None)
        .await;
    let session = node
        .wait_agent(bot_id, "retro started", |a| a["session_id"].is_string())
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Two proposals arrive.
    let mut proposals = Value::Null;
    for _ in 0..200 {
        proposals = node.ok("GET", "/org/proposals", None).await;
        if proposals.as_array().unwrap().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(proposals.as_array().unwrap().len(), 2, "{proposals}");
    let out = output(&node.events(&session).await);
    assert!(
        out.contains("Acme") && out.contains("Corp") && !out.contains("Beta"),
        "{out}"
    );
    assert!(out.contains("\\\"answer\\\":42"), "{out}");
    assert!(out.contains("one statement at a time"), "{out}");
    let find = |list: &Value, title: &str| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|p| p["title"] == title)
            .cloned()
            .unwrap()
    };
    let focus = find(&proposals, "More focus");
    let limits = find(&proposals, "Room to grow");
    assert_eq!(focus["status"], "open");
    assert_eq!(focus["proposed_by"], "retro (Retro)");
    let fid = focus["id"].as_str().unwrap().to_string();

    // A viewer cannot apply; an operator sends it back with changes.
    let (code, _) = node
        .call(
            "POST",
            &format!("/org/proposals/{fid}/apply"),
            "viewer-token",
            Some(json!({ "revision": 1 })),
        )
        .await;
    assert_eq!(code, 403);
    let (code, _) = node
        .call(
            "POST",
            &format!("/org/proposals/{fid}/changes"),
            "alice-token",
            Some(json!({ "text": "Call it v2" })),
        )
        .await;
    assert_eq!(code, 200);
    let mut revised = Value::Null;
    for _ in 0..200 {
        revised = node.ok("GET", &format!("/org/proposals/{fid}"), None).await;
        if revised["revision"] == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(revised["revision"], 2, "{revised}");
    assert_eq!(revised["status"], "open");
    assert_eq!(revised["actions"][0]["title"], "Ship v2");

    // Applying what one did not see is refused; the reviewed revision applies.
    let (code, _) = node
        .call(
            "POST",
            &format!("/org/proposals/{fid}/apply"),
            "alice-token",
            Some(json!({ "revision": 1 })),
        )
        .await;
    assert_eq!(code, 409);
    let (code, applied) = node
        .call(
            "POST",
            &format!("/org/proposals/{fid}/apply"),
            "alice-token",
            Some(json!({ "revision": 2 })),
        )
        .await;
    assert_eq!(code, 200, "{applied}");
    assert_eq!(applied["status"], "applied");
    assert_eq!(applied["result"][0]["ok"], true);
    let goals = node.ok("GET", "/org/goals", None).await;
    assert_eq!(goals[0]["title"], "Ship v2");
    assert_eq!(goals[0]["created_by"], "alice");

    // Limits need an admin; a person may change the proposal first.
    let lid = limits["id"].as_str().unwrap();
    let (code, _) = node
        .call(
            "POST",
            &format!("/org/proposals/{lid}/apply"),
            "alice-token",
            Some(json!({ "revision": 1 })),
        )
        .await;
    assert_eq!(code, 403);
    let edited = node
        .ok(
            "PUT",
            &format!("/org/proposals/{lid}"),
            Some(json!({
                "actions": [{ "kind": "set_limits", "max_departments": 9 }],
                "note": "a bit more room",
            })),
        )
        .await;
    assert_eq!(edited["revision"], 2);
    let applied = node
        .ok(
            "POST",
            &format!("/org/proposals/{lid}/apply"),
            Some(json!({ "revision": 2 })),
        )
        .await;
    assert_eq!(applied["status"], "applied", "{applied}");
    assert_eq!(
        node.ok("GET", "/org/settings", None).await["max_departments"],
        9
    );
    let (code, _) = node
        .call(
            "POST",
            &format!("/org/proposals/{lid}/reject"),
            "alice-token",
            Some(json!({ "text": "too late" })),
        )
        .await;
    assert_eq!(code, 409, "already decided");

    // Allowed kinds of change are applied without asking.
    let (code, _) = node
        .call(
            "PUT",
            "/org/auto-apply",
            "alice-token",
            Some(json!({ "kinds": ["create_goal"] })),
        )
        .await;
    assert_eq!(code, 403);
    node.ok(
        "PUT",
        "/org/auto-apply",
        Some(json!({ "kinds": ["create_goal"] })),
    )
    .await;
    node.ok(
        "POST",
        "/org/messages",
        Some(json!({ "to": { "agent": bot_id }, "text": "PROPOSE-AGAIN" })),
    )
    .await;
    let mut auto = Value::Null;
    for _ in 0..200 {
        let all = node.ok("GET", "/org/proposals", None).await;
        if let Some(p) = all
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["title"] == "Auto")
        {
            auto = p.clone();
            if auto["status"] == "applied" {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(auto["status"], "applied", "{auto}");
    assert_eq!(auto["decided_by"], "auto-apply (allowed by root)");

    // The metrics show it all.
    let m = node.ok("GET", "/org/metrics?days=7", None).await;
    assert_eq!(m["metrics"]["daily"].as_array().unwrap().len(), 7);
    let dept = m["metrics"]["departments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == rid.as_str())
        .unwrap()
        .clone();
    assert_eq!(dept["data_queries"], 3, "{dept}");
    assert!(dept["sessions"].as_i64().unwrap() >= 1);
    assert_eq!(m["metrics"]["proposals"]["applied"], 3);
    assert!(m["signals"].is_array());

    node.ok("POST", &format!("/org/agents/{bot_id}/stop"), None)
        .await;
    node.cluster.cancel();
}

/// An Anthropic-style upstream that reports 25 input and 15 output tokens.
async fn mock_model() -> String {
    use axum::response::IntoResponse;
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(|| async {
            let sse = concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-test\",\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"MODEL-ANSWER\"}}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":15}}\n\n",
            );
            ([("content-type", "text/event-stream")], sse).into_response()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn budgets_price_calls_and_stop_spending() {
    let Some((_, db_url)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let node = start(dir.path(), &db_url, "n1", 10).await;
    let upstream = mock_model().await;
    node.ok(
        "POST",
        "/providers",
        Some(json!({
            "name": "anthropic", "kind": "anthropic", "base_url": upstream,
            "api_key": "sk-real-key", "allowed_models": ["claude-*"],
        })),
    )
    .await;

    // Prices: the most specific one wins (claude-* over *).
    let (code, _) = node
        .call(
            "PUT",
            "/org/prices",
            "alice-token",
            Some(json!({ "provider": "anthropic", "model": "*", "input_per_mtok": 0, "output_per_mtok": 0 })),
        )
        .await;
    assert_eq!(code, 403, "admins set prices");
    for (model, input, output) in [("*", 0.0, 0.0), ("claude-*", 4000.0, 20000.0)] {
        node.ok(
            "PUT",
            "/org/prices",
            Some(json!({
                "provider": "anthropic", "model": model,
                "input_per_mtok": input, "output_per_mtok": output,
            })),
        )
        .await;
    }
    node.ok("PUT", "/org/currency", Some(json!({ "currency": "eur" })))
        .await;

    let ops = node
        .ok("POST", "/org/departments", Some(department("Ops", &[])))
        .await;
    let ops_id = ops["id"].as_str().unwrap().to_string();
    // 25 × 4000 + 15 × 20000 micros = 0.40 per call; 1.00 per day pauses.
    let budget = node
        .ok(
            "POST",
            "/org/budgets",
            Some(json!({
                "department_id": ops_id, "period": "day",
                "limit_micros": 1_000_000, "action": "pause",
            })),
        )
        .await;
    let budget_id = budget["id"].as_str().unwrap().to_string();
    let spender = node
        .ok(
            "POST",
            &format!("/org/departments/{ops_id}/agents"),
            Some(json!({ "name": "spender", "agent": "spender" })),
        )
        .await;
    let sid = spender["id"].as_str().unwrap().to_string();
    node.ok("POST", &format!("/org/agents/{sid}/start"), None)
        .await;
    let session = node
        .wait_agent(&sid, "spender waiting", |a| a["status"] == "awaiting_input")
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let out = output(&node.events(&session).await);
    assert!(out.contains("MODEL-ANSWER"), "{out}");

    // Two more calls: 1.20 spent, over the limit.
    for i in 0..2 {
        node.ok(
            "POST",
            "/org/messages",
            Some(json!({ "to": { "agent": sid }, "text": format!("again {i}") })),
        )
        .await;
        for _ in 0..200 {
            let a = node.agent(&sid).await;
            let calls = node.ok("GET", &format!("/sessions/{session}"), None).await["session"]
                ["model_calls"]
                .as_i64()
                .unwrap_or(0);
            if calls >= i + 2 && a["status"] == "awaiting_input" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // The heartbeat notices: the department is paused by the budget.
    let mut dept = Value::Null;
    for _ in 0..100 {
        let org = node.ok("GET", "/org", None).await;
        dept = org["departments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == ops_id.as_str())
            .unwrap()
            .clone();
        if dept["state"] == "paused" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(dept["state"], "paused", "{dept}");
    assert_eq!(dept["updated_by"], "budget");
    let spending = node.ok("GET", "/org/spending", None).await;
    assert_eq!(spending["currency"], "EUR");
    assert_eq!(
        spending["budgets"][0]["spent_micros"], 1_200_000,
        "{spending}"
    );
    assert_eq!(
        spending["budgets"][0]["paused_departments"][0],
        ops_id.as_str()
    );
    assert_eq!(spending["unpriced_calls"], 0);
    let metrics = node.ok("GET", "/org/metrics?days=1", None).await;
    assert!(
        metrics["signals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["kind"] == "budget_used_up"),
        "{}",
        metrics["signals"]
    );
    let ops_metrics = metrics["metrics"]["departments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == ops_id.as_str())
        .unwrap()
        .clone();
    assert_eq!(ops_metrics["cost_micros"], 1_200_000);

    // Resumed by hand, the work goes on, but model calls are refused.
    node.ok("POST", &format!("/org/departments/{ops_id}/resume"), None)
        .await;
    node.wait_agent(&sid, "spender resumed", |a| a["status"] == "awaiting_input")
        .await;
    node.ok(
        "POST",
        "/org/messages",
        Some(json!({ "to": { "agent": sid }, "text": "once more" })),
    )
    .await;
    let mut out = String::new();
    for _ in 0..200 {
        out = output(&node.events(&session).await);
        if out.contains("used up") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(out.contains("budget of 1.00 EUR is used up"), "{out}");

    // A higher limit lets it spend again.
    node.ok(
        "PUT",
        &format!("/org/budgets/{budget_id}"),
        Some(json!({ "limit_micros": 10_000_000 })),
    )
    .await;
    node.ok(
        "POST",
        "/org/messages",
        Some(json!({ "to": { "agent": sid }, "text": "and again" })),
    )
    .await;
    for _ in 0..200 {
        out = output(&node.events(&session).await);
        if out.matches("MODEL-ANSWER").count() >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(out.matches("MODEL-ANSWER").count(), 4, "{out}");
    let spending = node.ok("GET", "/org/spending", None).await;
    assert_eq!(spending["budgets"][0]["spent_micros"], 1_600_000);
    assert!(
        spending["budgets"][0]["paused_departments"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    node.ok("POST", &format!("/org/agents/{sid}/stop"), None)
        .await;
    node.cluster.cancel();
}

/// A minimal SMTP server that keeps every message it receives.
async fn fake_smtp() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mails = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let store = mails.clone();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let store = store.clone();
            tokio::spawn(async move {
                let (read, mut write) = socket.into_split();
                let mut lines = BufReader::new(read).lines();
                write.write_all(b"220 fake ESMTP\r\n").await.unwrap();
                let mut data: Option<String> = None;
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(body) = data.as_mut() {
                        if line == "." {
                            store.lock().unwrap().push(std::mem::take(body));
                            data = None;
                            write.write_all(b"250 queued\r\n").await.unwrap();
                        } else {
                            body.push_str(&line);
                            body.push('\n');
                        }
                        continue;
                    }
                    let verb = line
                        .split(' ')
                        .next()
                        .unwrap_or_default()
                        .to_ascii_uppercase();
                    let reply: &[u8] = match verb.as_str() {
                        "EHLO" => b"250-fake\r\n250 OK\r\n",
                        "DATA" => {
                            data = Some(String::new());
                            b"354 go on\r\n"
                        }
                        "QUIT" => {
                            write.write_all(b"221 bye\r\n").await.unwrap();
                            return;
                        }
                        _ => b"250 OK\r\n",
                    };
                    write.write_all(reply).await.unwrap();
                }
            });
        }
    });
    (port, mails)
}

type Posts = std::sync::Arc<std::sync::Mutex<Vec<(String, Value, Option<String>)>>>;

/// Slack and webhook endpoints that keep what they receive.
async fn fake_hooks() -> (String, Posts) {
    let posts: Posts = Default::default();
    let keep = |path: &'static str, posts: Posts| {
        axum::routing::post(
            move |headers: axum::http::HeaderMap, body: axum::Json<Value>| {
                let posts = posts.clone();
                async move {
                    let token = headers
                        .get("x-token")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from);
                    posts
                        .lock()
                        .unwrap()
                        .push((path.to_string(), body.0, token));
                    "ok"
                }
            },
        )
    };
    let app = axum::Router::new()
        .route("/slack", keep("slack", posts.clone()))
        .route("/hook", keep("hook", posts.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), posts)
}

#[tokio::test]
async fn outward_messages_wait_for_people() {
    let Some((_, db_url)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let node = start(dir.path(), &db_url, "n1", 10).await;
    let (smtp_port, mails) = fake_smtp().await;
    let (hooks, posts) = fake_hooks().await;

    let sales = node
        .ok("POST", "/org/departments", Some(department("Sales", &[])))
        .await;
    let sid = sales["id"].as_str().unwrap().to_string();
    let mail = json!({
        "name": "mail", "kind": "email", "description": "Email to customers",
        "config": {
            "host": "127.0.0.1", "port": smtp_port, "tls": "none",
            "from": "Acme <hello@acme.test>", "allowed_domains": ["customer.com"],
        },
        "departments": [sid],
    });
    let (code, _) = node
        .call("POST", "/org/channels", "alice-token", Some(mail.clone()))
        .await;
    assert_eq!(code, 403, "admins add channels");
    let mail = node.ok("POST", "/org/channels", Some(mail)).await;
    assert_eq!(mail["requires_approval"], true);
    node.ok(
        "POST",
        "/org/channels",
        Some(json!({
            "name": "team-slack", "kind": "slack", "secret": format!("{hooks}/slack"),
            "departments": [sid], "requires_approval": false, "max_per_day": 1,
        })),
    )
    .await;
    let hook = node
        .ok(
            "POST",
            "/org/channels",
            Some(json!({
                "name": "hooks", "kind": "webhook", "secret": "s3cret",
                "config": { "url": format!("{hooks}/hook"), "header": "X-Token" },
                "departments": [sid],
            })),
        )
        .await;
    assert!(hook.get("secret").is_none() && hook["secret_hint"] == "…cret");

    let drafter = node
        .ok(
            "POST",
            &format!("/org/departments/{sid}/agents"),
            Some(json!({ "name": "rep", "agent": "drafter" })),
        )
        .await;
    let aid = drafter["id"].as_str().unwrap().to_string();
    node.ok("POST", &format!("/org/agents/{aid}/start"), None)
        .await;
    let session = node
        .wait_agent(&aid, "drafter waiting", |a| a["status"] == "awaiting_input")
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let out = output(&node.events(&session).await);
    assert!(out.contains("only sends to @customer.com"), "{out}");

    // The internal Slack channel sends without approval, up to its limit.
    let outbox = node.ok("GET", "/org/outbox", None).await;
    let items = outbox.as_array().unwrap().clone();
    assert_eq!(items.len(), 4, "{outbox}");
    let find = |text: &str| {
        items
            .iter()
            .find(|i| i["body"].as_str().unwrap().contains(text))
            .unwrap()
            .clone()
    };
    assert_eq!(find("all good")["status"], "sent");
    assert_eq!(find("Second status")["status"], "pending");
    assert_eq!(find("Welcome")["status"], "pending");
    {
        let posts = posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        let text = posts[0].1["text"].as_str().unwrap();
        assert!(text.starts_with("Status: all good"), "{text}");
        assert!(
            text.ends_with("This message was written by an AI agent."),
            "{text}"
        );
    }
    assert!(
        mails.lock().unwrap().is_empty(),
        "nothing sent without approval"
    );

    // A person edits the email, then approves what they read.
    let welcome = find("Welcome");
    let wid = welcome["id"].as_str().unwrap();
    let (code, _) = node
        .call(
            "POST",
            &format!("/org/outbox/{wid}/send"),
            "viewer-token",
            Some(json!({ "revision": 1 })),
        )
        .await;
    assert_eq!(code, 403);
    let (code, edited) = node
        .call(
            "PUT",
            &format!("/org/outbox/{wid}"),
            "alice-token",
            Some(json!({ "subject": "Hello Ana", "note": "personal" })),
        )
        .await;
    assert_eq!(code, 200, "{edited}");
    assert_eq!(edited["revision"], 2);
    let (code, _) = node
        .call(
            "POST",
            &format!("/org/outbox/{wid}/send"),
            "alice-token",
            Some(json!({ "revision": 1 })),
        )
        .await;
    assert_eq!(code, 409, "not what alice read");
    let (code, sent) = node
        .call(
            "POST",
            &format!("/org/outbox/{wid}/send"),
            "alice-token",
            Some(json!({ "revision": 2 })),
        )
        .await;
    assert_eq!(code, 200, "{sent}");
    assert_eq!(sent["status"], "sent");
    assert_eq!(sent["decided_by"], "alice");
    {
        let mails = mails.lock().unwrap();
        assert_eq!(mails.len(), 1);
        let m = &mails[0];
        assert!(m.contains("Subject: Hello Ana"), "{m}");
        assert!(m.contains("ana@customer.com"), "{m}");
        assert!(m.contains("hello@acme.test"), "{m}");
        assert!(m.contains("Welcome aboard"), "{m}");
        assert!(m.contains("written by an AI agent"), "{m}");
    }

    // Sent back: the agent revises; then it goes out through the webhook.
    let launch = find("DRAFT-1");
    let lid = launch["id"].as_str().unwrap();
    node.ok(
        "POST",
        &format!("/org/outbox/{lid}/changes"),
        Some(json!({ "text": "Make it final" })),
    )
    .await;
    let mut revised = Value::Null;
    for _ in 0..200 {
        let all = node.ok("GET", "/org/outbox", None).await;
        revised = all
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == lid)
            .unwrap()
            .clone();
        if revised["revision"] == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(revised["body"], "Launch post FINAL", "{revised}");
    assert_eq!(revised["status"], "pending");
    node.ok(
        "POST",
        &format!("/org/outbox/{lid}/send"),
        Some(json!({ "revision": 2 })),
    )
    .await;
    {
        let posts = posts.lock().unwrap();
        let (path, body, token) = posts.last().unwrap();
        assert_eq!(path, "hook");
        assert_eq!(token.as_deref(), Some("s3cret"));
        assert_eq!(body["subject"], "Launch");
        assert_eq!(body["approved_by"], "root");
        assert_eq!(body["ai_generated"], true);
        assert!(body["text"].as_str().unwrap().contains("Launch post FINAL"));
    }

    // Over the daily limit: refused even with approval; a person rejects it.
    let second = find("Second status");
    let second_id = second["id"].as_str().unwrap();
    let (code, err) = node
        .call(
            "POST",
            &format!("/org/outbox/{second_id}/send"),
            "alice-token",
            Some(json!({ "revision": 1 })),
        )
        .await;
    assert_eq!(code, 409, "{err}");
    assert!(err.to_string().contains("limit of 1"), "{err}");
    let rejected = node
        .ok(
            "POST",
            &format!("/org/outbox/{second_id}/reject"),
            Some(json!({ "text": "not needed" })),
        )
        .await;
    assert_eq!(rejected["status"], "rejected");

    // The agent heard every decision; admins can test a channel.
    let mut heard = String::new();
    for _ in 0..100 {
        heard = output(&node.events(&session).await);
        if heard.contains("rejected your") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(heard.contains("was sent (approved by alice)"), "{heard}");
    assert!(heard.contains("rejected your"), "{heard}");
    let tested = node
        .ok(
            "POST",
            &format!("/org/channels/{}/test", mail["id"].as_str().unwrap()),
            Some(json!({ "to": ["ops@customer.com"] })),
        )
        .await;
    assert_eq!(tested["ok"], true, "{tested}");
    assert_eq!(mails.lock().unwrap().len(), 2);
    let overview = node.ok("GET", "/org", None).await;
    assert_eq!(overview["outbox_pending"], 0);
    assert_eq!(overview["channels"].as_array().unwrap().len(), 3);

    node.ok("POST", &format!("/org/agents/{aid}/stop"), None)
        .await;
    node.cluster.cancel();
}

// ---- releases ----------------------------------------------------------------------

/// What the fake download host serves, and what the webhook received.
type Hooked = std::sync::Arc<std::sync::Mutex<Vec<(axum::http::HeaderMap, axum::body::Bytes)>>>;

/// Artifacts at `/app-*.tar.gz` and a webhook receiver at `/hook`.
async fn fake_downloads() -> (String, Hooked) {
    let hooked: Hooked = Default::default();
    let keep = hooked.clone();
    let app = axum::Router::new()
        .route(
            "/app-1.0.0.tar.gz",
            axum::routing::get(|| async { "agentcore app 1.0.0" }),
        )
        .route(
            "/app-1.1.0.tar.gz",
            axum::routing::get(|| async { "agentcore app 1.1.0" }),
        )
        .route(
            "/app-1.2.0.tar.gz",
            axum::routing::get(|| async { "agentcore app 1.2.0" }),
        )
        .route(
            "/hook",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let keep = keep.clone();
                    async move {
                        keep.lock().unwrap().push((headers, body));
                        "ok"
                    }
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hooked)
}

fn sha256(text: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(text.as_bytes()))
}

fn artifact(base: &str, version: &str, content: &str) -> Value {
    json!({
        "name": format!("app-{version}.tar.gz"), "platform": "linux-x86_64",
        "url": format!("{base}/app-{version}.tar.gz"), "sha256": sha256(content),
        "size": content.len(),
    })
}

/// Proposes 1.0.0 on its first turn; revises when asked; halts on request.
fn shipper(base: &str) -> String {
    let good = json!({
        "product": "app", "version": "1.0.0", "commit": "0123abcd",
        "notes": "First release DRAFT", "risk": "low",
        "assessment": "Small, well tested first version.",
        "artifacts": [artifact(base, "1.0.0", "agentcore app 1.0.0")],
        "checks": [{ "name": "unit tests", "passed": true }],
        "changes": { "files_changed": 3, "lines_added": 30, "lines_removed": 2 },
        "sbom": { "bomFormat": "CycloneDX", "specVersion": "1.5", "components": [] },
    });
    let evil = json!({
        "product": "app", "version": "1.0.1", "notes": "x",
        "artifacts": [{ "name": "a", "url": "https://evil.example/a", "sha256": sha256("a") }],
    });
    format!(
        r###"
case "$0" in
  *"asked for changes"*)
    id=$(printf %s "$0" | grep "asked for changes" | sed -n 's/.*(id \([0-9a-f-]*\)).*/\1/p' | head -n 1)
    call release_revise "{{\"release\":\"$id\",\"notes\":\"First release FINAL\",\"note\":\"as asked\"}}" ;;
  *HALT*)
    id=$(printf %s "$0" | sed -n 's/.*HALT \([0-9a-f-]*\).*/\1/p' | head -n 1)
    call release_halt "{{\"release\":\"$id\",\"reason\":\"users report errors\"}}" ;;
  "You have "*) ;;
  *)
    call release_products '{{}}'
    call release_propose '{evil}'
    call release_propose '{good}' ;;
esac
"###
    )
}

#[tokio::test]
async fn releases_roll_out_in_stages_and_roll_back_on_their_own() {
    let Some((_, db_url)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (base, hooked) = fake_downloads().await;
    let script = shipper(&base);
    let node = start_with(dir.path(), &db_url, "n1", 10, |c| {
        c.agents.push(agent("shipper", &script));
    })
    .await;

    let ship = node
        .ok("POST", "/org/departments", Some(department("Ship", &[])))
        .await;
    let sid = ship["id"].as_str().unwrap().to_string();
    let product = json!({
        "name": "app", "description": "The desktop app", "departments": [sid],
        "plan": [
            { "name": "canary", "percent": 10, "soak_minutes": 0 },
            { "name": "everyone", "percent": 100, "soak_minutes": 0 },
        ],
        "gates": [{
            "name": "crash-free", "metric": { "kind": "crash_free" },
            "comparator": ">=", "threshold": 0.9, "min_samples": 3,
        }],
        "approvals_required": 2,
        "artifact_hosts": ["127.0.0.1"],
        "webhook_url": format!("{base}/hook"),
        "webhook_secret": "whsec",
    });
    let (code, _) = node
        .call(
            "POST",
            "/releases/products",
            "alice-token",
            Some(product.clone()),
        )
        .await;
    assert_eq!(code, 403, "admins add products");
    let created = node.ok("POST", "/releases/products", Some(product)).await;
    let pid = created["product"]["id"].as_str().unwrap().to_string();
    let token = created["client_token"].as_str().unwrap().to_string();
    assert!(created["product"].get("client_token_sha256").is_none());
    assert_eq!(created["product"]["webhook_secret_hint"], "…hsec");

    // The agent proposes; a download host that is not allowed is refused.
    let bot = node
        .ok(
            "POST",
            &format!("/org/departments/{sid}/agents"),
            Some(json!({ "name": "release-manager", "agent": "shipper" })),
        )
        .await;
    let aid = bot["id"].as_str().unwrap().to_string();
    node.ok("POST", &format!("/org/agents/{aid}/start"), None)
        .await;
    let session = node
        .wait_agent(&aid, "shipper waiting", |a| a["status"] == "awaiting_input")
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let out = output(&node.events(&session).await);
    assert!(out.contains("not an allowed artifact host"), "{out}");
    let list = node
        .ok("GET", &format!("/releases?product={pid}"), None)
        .await;
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    let release = list[0].clone();
    let rid = release["id"].as_str().unwrap().to_string();
    assert_eq!(release["version"], "1.0.0");
    assert_eq!(release["status"], "proposed");
    assert_eq!(release["proposed_by"], "release-manager (Ship)");
    assert!(release["risk"]["score"].as_u64().unwrap() < 60, "{release}");
    assert!(
        release["risk"]["factors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["name"] == "version"),
        "{release}"
    );
    assert_eq!(release["approvals_required"], 2);

    // People send it back; the agent revises.
    node.ok(
        "POST",
        &format!("/releases/{rid}/changes"),
        Some(json!({ "text": "Make the notes final" })),
    )
    .await;
    let mut revised = Value::Null;
    for _ in 0..200 {
        revised = node.ok("GET", &format!("/releases/{rid}"), None).await["release"].clone();
        if revised["revision"] == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(revised["notes"], "First release FINAL", "{revised}");
    assert_eq!(revised["status"], "proposed");

    // Distinct people approve the revision they read; the digest is checked.
    let approve = |token: &'static str, revision: u32| {
        let node = &node;
        let rid = rid.clone();
        async move {
            node.call(
                "POST",
                &format!("/releases/{rid}/approve"),
                token,
                Some(json!({ "revision": revision })),
            )
            .await
        }
    };
    assert_eq!(approve("viewer-token", 2).await.0, 403);
    assert_eq!(
        approve("alice-token", 1).await.0,
        409,
        "not what alice read"
    );
    let (code, first) = approve("alice-token", 2).await;
    assert_eq!(code, 200, "{first}");
    assert_eq!(first["started"], false);
    assert!(
        first["release"]["artifacts"][0]["verified_at"].is_string(),
        "{first}"
    );
    assert_eq!(approve("alice-token", 2).await.0, 409, "one approval each");
    let (code, second) = approve("admin-token", 2).await;
    assert_eq!(code, 200, "{second}");
    assert_eq!(second["started"], true, "{second}");
    assert_eq!(second["release"]["status"], "rolling_out");

    // Update clients: a client token, a signed manifest, 10% of units.
    let dist = |path: String, token: Option<&str>| {
        let url = format!("{}{path}", node.base);
        let token = token.unwrap_or("").to_string();
        async move { http("GET", &url, &token, None).await }
    };
    assert_eq!(dist("/dist/app/update?unit=u1".into(), None).await.0, 401);
    assert_eq!(
        dist("/dist/app/update?unit=u1".into(), Some("admin-token"))
            .await
            .0,
        401,
        "operator tokens are not client tokens"
    );
    let (_, keys) = dist("/dist/keys".into(), None).await;
    let keys: Vec<(String, Vec<u8>)> = keys["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| {
            use base64::Engine;
            (
                k["key_id"].as_str().unwrap().to_string(),
                base64::engine::general_purpose::STANDARD
                    .decode(k["public_key"].as_str().unwrap())
                    .unwrap(),
            )
        })
        .collect();
    assert_eq!(keys.len(), 1);
    let mut offered = Vec::new();
    for i in 0..100 {
        let (code, answer) = dist(
            format!("/dist/app/update?unit=unit-{i}&platform=linux-x86_64"),
            Some(&token),
        )
        .await;
        assert_eq!(code, 200, "{answer}");
        let verified = agentcore_server::signing::verify(&answer["envelope"], &keys).unwrap();
        assert_eq!(
            verified, answer["manifest"],
            "the signed payload is the manifest"
        );
        if answer["manifest"]["version"] == "1.0.0" {
            assert_eq!(answer["manifest"]["update"], true);
            assert_eq!(answer["manifest"]["stage"], "canary");
            assert_eq!(
                answer["manifest"]["artifacts"][0]["sha256"],
                sha256("agentcore app 1.0.0")
            );
            offered.push(format!("unit-{i}"));
        }
    }
    assert!(
        (2..=25).contains(&offered.len()),
        "{} of 100",
        offered.len()
    );

    // Healthy reports from three units open the gate: everyone gets it.
    let report = |unit: String, version: &'static str, event: &'static str| {
        let url = format!("{}/dist/app/report", node.base);
        let token = token.clone();
        async move {
            http(
                "POST",
                &url,
                &token,
                Some(json!({ "unit": unit, "version": version, "event": event })),
            )
            .await
            .0
        }
    };
    for unit in offered.iter().take(3) {
        assert_eq!(report(unit.clone(), "1.0.0", "installed").await, 202);
    }
    let wait_status = |status: &'static str| {
        let node = &node;
        async move {
            let mut last = Value::Null;
            for _ in 0..200 {
                let all = node.ok("GET", "/releases", None).await;
                if let Some(r) = all
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|r| r["status"] == status)
                {
                    return r.clone();
                }
                last = all;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("no release became {status}: {last}");
        }
    };
    let released = wait_status("released").await;
    assert_eq!(released["version"], "1.0.0");
    let (_, everyone) = dist("/dist/app/update?unit=someone-else".into(), Some(&token)).await;
    assert_eq!(everyone["manifest"]["version"], "1.0.0");
    assert_eq!(everyone["manifest"]["percent"], 100.0);

    // The attestation names the digests, the agent and the approvers.
    let (_, envelope) = dist("/dist/app/releases/1.0.0/attestation".into(), Some(&token)).await;
    let statement = agentcore_server::signing::verify(&envelope, &keys).unwrap();
    assert_eq!(statement["_type"], "https://in-toto.io/Statement/v1");
    assert_eq!(
        statement["subject"][0]["digest"]["sha256"],
        sha256("agentcore app 1.0.0")
    );
    assert_eq!(statement["predicate"]["ai_generated"], true);
    assert_eq!(
        statement["predicate"]["approved_by"],
        json!(["alice", "root"])
    );
    assert_eq!(statement["predicate"]["artifacts_verified"], true);

    // Versions only go up; a wrong digest is caught before anything ships.
    node.ok(
        "PUT",
        &format!("/releases/products/{pid}"),
        Some(json!({ "approvals_required": 1 })),
    )
    .await;
    let propose = |version: &'static str, content: &'static str| {
        let node = &node;
        let body = json!({
            "product": pid, "version": version, "notes": format!("Version {version}"),
            "artifacts": [{
                "name": format!("app-{version}.tar.gz"), "platform": "linux-x86_64",
                "url": format!("{base}/app-{version}.tar.gz"), "sha256": sha256(content),
            }],
        });
        async move {
            node.call("POST", "/releases", "alice-token", Some(body))
                .await
        }
    };
    let (code, err) = propose("0.9.0", "x").await;
    assert_eq!(code, 400, "{err}");
    assert!(err.to_string().contains("not above 1.0.0"), "{err}");
    let (code, bad) = propose("1.1.0", "tampered").await;
    assert_eq!(code, 201, "{bad}");
    let bad_id = bad["id"].as_str().unwrap();
    let (code, err) = node
        .call(
            "POST",
            &format!("/releases/{bad_id}/approve"),
            "admin-token",
            Some(json!({ "revision": 1 })),
        )
        .await;
    assert_eq!(code, 409, "{err}");
    assert!(err.to_string().contains("SHA-256"), "{err}");
    let (code, _) = node
        .call(
            "POST",
            &format!("/releases/{bad_id}/approve"),
            "alice-token",
            Some(json!({ "revision": 1 })),
        )
        .await;
    assert_eq!(code, 400, "nobody approves their own release");
    node.ok(
        "POST",
        &format!("/releases/{bad_id}/reject"),
        Some(json!({ "text": "wrong file" })),
    )
    .await;

    // 1.1.0 (the rejected version is free again) rolls out, is halted by
    // the agent, resumed, and rolled back by crash reports.
    let (code, good) = propose("1.1.0", "agentcore app 1.1.0").await;
    assert_eq!(code, 201, "{good}");
    let gid = good["id"].as_str().unwrap().to_string();
    let started = node
        .ok(
            "POST",
            &format!("/releases/{gid}/approve"),
            Some(json!({ "revision": 1 })),
        )
        .await;
    assert_eq!(started["started"], true, "{started}");
    let (code, err) = node
        .call(
            "POST",
            &format!("/releases/{gid}/promote"),
            "alice-token",
            None,
        )
        .await;
    assert_eq!(code, 409, "no promotion past a gate without data: {err}");
    node.ok(
        "POST",
        "/org/messages",
        Some(json!({ "to": { "agent": aid }, "text": format!("HALT {gid}") })),
    )
    .await;
    let paused = wait_status("paused").await;
    assert_eq!(paused["reason"], "users report errors");
    let (code, _) = node
        .call(
            "POST",
            &format!("/releases/{gid}/resume"),
            "viewer-token",
            None,
        )
        .await;
    assert_eq!(code, 403);
    node.call(
        "POST",
        &format!("/releases/{gid}/resume"),
        "alice-token",
        None,
    )
    .await;
    let rolling = wait_status("rolling_out").await;
    let canary: Vec<String> = (0..400)
        .map(|i| format!("unit-{i}"))
        .filter(|u| {
            agentcore_core::releases::in_rollout(
                rolling["id"].as_str().unwrap().parse().unwrap(),
                u,
                10.0,
            )
        })
        .take(3)
        .collect();
    for unit in &canary {
        let (_, m) = dist(format!("/dist/app/update?unit={unit}"), Some(&token)).await;
        assert_eq!(m["manifest"]["version"], "1.1.0", "{unit} is in the canary");
        assert_eq!(report(unit.clone(), "1.1.0", "crash").await, 202);
    }
    let rolled_back = wait_status("rolled_back").await;
    assert_eq!(rolled_back["version"], "1.1.0");
    assert_eq!(rolled_back["decided_by"], "rollout");
    assert!(
        rolled_back["reason"]
            .as_str()
            .unwrap()
            .contains("crash-free"),
        "{rolled_back}"
    );
    let (_, back) = dist(
        format!("/dist/app/update?unit={}&current=1.1.0", canary[0]),
        Some(&token),
    )
    .await;
    assert_eq!(back["manifest"]["version"], "1.0.0");
    assert_eq!(back["manifest"]["rollback"], true);
    let (code, _) = propose("1.1.0", "agentcore app 1.1.0").await;
    assert_eq!(code, 400, "a rolled-back version is never reused");

    // A change freeze holds the start; lifting it lets it go.
    node.ok(
        "PUT",
        &format!("/releases/products/{pid}/freeze"),
        Some(json!({ "reason": "launch week" })),
    )
    .await;
    let (_, frozen) = propose("1.2.0", "agentcore app 1.2.0").await;
    let fid = frozen["id"].as_str().unwrap().to_string();
    let held = node
        .ok(
            "POST",
            &format!("/releases/{fid}/approve"),
            Some(json!({ "revision": 1 })),
        )
        .await;
    assert_eq!(held["started"], false, "{held}");
    assert!(
        held["note"].as_str().unwrap().contains("launch week"),
        "{held}"
    );
    node.ok("DELETE", &format!("/releases/products/{pid}/freeze"), None)
        .await;
    let go = node
        .ok("POST", &format!("/releases/{fid}/start"), None)
        .await;
    assert_eq!(go["status"], "rolling_out");
    node.ok(
        "POST",
        &format!("/releases/{fid}/rollback"),
        Some(json!({ "text": "not needed after all" })),
    )
    .await;

    // Webhooks were signed; the agent heard; people see the signal.
    {
        let hooked = hooked.lock().unwrap();
        let events: Vec<&str> = hooked
            .iter()
            .map(|(h, _)| h["x-agentcore-event"].to_str().unwrap())
            .collect();
        for e in ["started", "advanced", "released", "paused", "rolled_back"] {
            assert!(events.contains(&e), "{e} in {events:?}");
        }
        for (headers, body) in hooked.iter() {
            assert_eq!(
                headers["x-agentcore-signature"].to_str().unwrap(),
                agentcore_server::signing::hmac_signature("whsec", body)
            );
        }
    }
    let mut heard = String::new();
    for _ in 0..100 {
        heard = output(&node.events(&session).await);
        if heard.contains("is released") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        heard.contains("approved app 1.0.0 (1 of 2 approvals)"),
        "{heard}"
    );
    assert!(heard.contains("rollout of app 1.0.0 started"), "{heard}");
    assert!(heard.contains("app 1.0.0 is released"), "{heard}");
    let metrics = node.ok("GET", "/org/metrics", None).await;
    assert!(
        metrics["signals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["kind"] == "release_rolled_back"),
        "{metrics}"
    );
    let overview = node.ok("GET", "/org", None).await;
    assert_eq!(overview["products"].as_array().unwrap().len(), 1);
    assert_eq!(
        report("unit-1".into(), "1.3.0", "crash").await,
        404,
        "reports only count for versions that were offered"
    );

    node.ok("POST", &format!("/org/agents/{aid}/stop"), None)
        .await;
    node.cluster.cancel();
}

// ---- telling people about it -------------------------------------------------------

/// Answers a launch brief: facts, a tracked link, a draft, the results.
const MARKETER: &str = r###"
case "$0" in
  *"Launch brief"*)
    link=$(printf %s "$0" | grep -o 'http[^ ]*src=newsletter' | head -n 1)
    call launch_products '{}'
    call launch_link '{"product":"kit","source":"newsletter"}'
    call launch_link '{"product":"kit","source":"<bad>"}'
    call outbox_draft "{\"channel\":\"news\",\"subject\":\"kit is out\",\"text\":\"kit 1.0.0 is here: $link\"}"
    call launch_stats '{"product":"kit"}' ;;
esac
"###;

/// GET or POST without following redirects; status, headers and body.
async fn raw(
    method: &str,
    url: &str,
    form: Option<&[(&str, &str)]>,
) -> (u16, reqwest::header::HeaderMap, String) {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut req = client.request(method.parse().unwrap(), url);
    if let Some(form) = form {
        req = req.form(form);
    }
    let res = req.send().await.unwrap();
    let code = res.status().as_u16();
    let headers = res.headers().clone();
    (code, headers, res.text().await.unwrap())
}

#[tokio::test]
async fn releases_reach_people_through_a_page_briefs_and_tracked_links() {
    let Some((_, db_url)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (base, hooked) = fake_downloads().await;
    let node = start_with(dir.path(), &db_url, "n1", 10, |c| {
        c.agents.push(agent("marketer", MARKETER));
    })
    .await;

    // A growth department with an agent and a channel for announcements.
    let growth = node
        .ok("POST", "/org/departments", Some(department("Growth", &[])))
        .await;
    let gid = growth["id"].as_str().unwrap().to_string();
    node.ok(
        "POST",
        "/org/channels",
        Some(json!({
            "name": "news", "kind": "webhook",
            "config": { "url": format!("{base}/hook") }, "departments": [gid],
        })),
    )
    .await;
    // A public product the growth department announces; another one private.
    let created = node
        .ok(
            "POST",
            "/releases/products",
            Some(json!({
                "name": "kit", "description": "A toolkit for shipping faster.",
                "tagline": "Ship in minutes", "audience": "both", "public": true,
                "pitch": "For teams that ship every day.",
                "announce_departments": [gid], "approvals_required": 1,
                "plan": [{ "name": "everyone", "percent": 100, "soak_minutes": 0 }],
                "artifact_hosts": ["127.0.0.1"],
                "webhook_url": format!("{base}/hook"), "webhook_secret": "whsec",
            })),
        )
        .await;
    let pid = created["product"]["id"].as_str().unwrap().to_string();
    assert_eq!(created["product"]["public"], true);
    assert_eq!(created["product"]["audience"], "both");
    node.ok(
        "POST",
        "/releases/products",
        Some(json!({ "name": "hidden", "artifact_hosts": ["127.0.0.1"] })),
    )
    .await;
    assert_eq!(
        raw("GET", &format!("{}/p/hidden", node.base), None).await.0,
        404
    );
    assert_eq!(
        raw("GET", &format!("{}/p/nope", node.base), None).await.0,
        404
    );

    // The marketer starts after the product names its department (a session
    // gets its tools when it starts).
    let bot = node
        .ok(
            "POST",
            &format!("/org/departments/{gid}/agents"),
            Some(json!({ "name": "marketer", "agent": "marketer" })),
        )
        .await;
    let aid = bot["id"].as_str().unwrap().to_string();
    node.ok("POST", &format!("/org/agents/{aid}/start"), None)
        .await;
    let session = node
        .wait_agent(&aid, "marketer waiting", |a| {
            a["status"] == "awaiting_input"
        })
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Release 1.0.0 (its notes try to inject a script).
    let ship = |version: &'static str| {
        let node = &node;
        let pid = pid.clone();
        let base = base.clone();
        async move {
            let (code, r) = node
                .call(
                    "POST",
                    "/releases",
                    "alice-token",
                    Some(json!({
                        "product": pid, "version": version,
                        "notes": format!("Version {version} <script>alert(1)</script>"),
                        "artifacts": [artifact(&base, "1.0.0", "agentcore app 1.0.0")],
                        "checks": [{ "name": "tests", "passed": true }],
                        "changes": { "lines_added": 10 },
                        "sbom": { "bomFormat": "CycloneDX" },
                    })),
                )
                .await;
            assert_eq!(code, 201, "{r}");
            let rid = r["id"].as_str().unwrap().to_string();
            node.ok(
                "POST",
                &format!("/releases/{rid}/approve"),
                Some(json!({ "revision": 1 })),
            )
            .await;
            for _ in 0..200 {
                let r = node.ok("GET", &format!("/releases/{rid}"), None).await;
                if r["release"]["status"] == "released" {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("{version} was not released");
        }
    };
    ship("1.0.0").await;

    // The brief reached the marketer, who drafted with a tracked link.
    let mut draft = Value::Null;
    for _ in 0..200 {
        let outbox = node.ok("GET", "/org/outbox", None).await;
        if let Some(d) = outbox.as_array().unwrap().first() {
            draft = d.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let text = draft["body"].as_str().unwrap_or_default();
    assert!(
        text.contains(&format!("{}/p/kit?src=newsletter", node.base)),
        "{draft}"
    );
    assert_eq!(
        draft["status"], "pending",
        "a person approves announcements"
    );
    let mut out = String::new();
    for _ in 0..200 {
        out = output(&node.events(&session).await);
        if out.contains("marketer got") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(out.contains("Launch brief: kit 1.0.0 is released"), "{out}");
    assert!(out.contains("Ship in minutes"), "{out}");
    assert!(out.contains("For teams that ship every day."), "{out}");
    assert!(out.contains("write separate drafts"), "{out}");
    assert!(out.contains("`source` is a short name"), "{out}");
    assert!(out.contains(r#"\"subscribers\":0"#), "{out}");

    // The public page: escaped, locked down, counted per source.
    let (code, headers, page) =
        raw("GET", &format!("{}/p/kit?src=newsletter", node.base), None).await;
    assert_eq!(code, 200);
    assert!(
        page.contains("Ship in minutes") && page.contains("1.0.0"),
        "{page}"
    );
    assert!(
        page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "{page}"
    );
    assert!(!page.contains("<script>"), "{page}");
    assert!(page.contains(&sha256("agentcore app 1.0.0")));
    assert!(
        headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("default-src 'none'")
    );
    raw("GET", &format!("{}/p/kit", node.base), None).await;
    let (code, headers, _) = raw(
        "GET",
        &format!(
            "{}/p/kit/download/app-1.0.0.tar.gz?src=newsletter",
            node.base
        ),
        None,
    )
    .await;
    assert_eq!(code, 303);
    assert_eq!(
        headers["location"],
        format!("{base}/app-1.0.0.tar.gz").as_str()
    );
    let (_, _, feed) = raw("GET", &format!("{}/p/kit/feed.xml", node.base), None).await;
    assert!(feed.contains("<entry><title>kit 1.0.0</title>"), "{feed}");

    // Sign-ups: consent required, duplicates and bots change nothing.
    let subscribe = |fields: Vec<(&'static str, &'static str)>| {
        let url = format!("{}/p/kit/subscribe", node.base);
        async move { raw("POST", &url, Some(&fields)).await }
    };
    let (code, _, thanks) = subscribe(vec![
        ("email", "ana@dev.io"),
        ("segment", "developer"),
        ("consent", "yes"),
        ("src", "newsletter"),
    ])
    .await;
    assert_eq!(code, 200, "{thanks}");
    assert!(thanks.contains("Thank you"));
    assert_eq!(
        subscribe(vec![("email", "ANA@dev.io"), ("consent", "yes")])
            .await
            .0,
        200
    );
    assert_eq!(
        subscribe(vec![("email", "bob@corp.com"), ("segment", "business")])
            .await
            .0,
        400,
        "no consent, no subscription"
    );
    assert_eq!(
        subscribe(vec![("email", "not-an-email"), ("consent", "yes")])
            .await
            .0,
        400
    );
    assert_eq!(
        subscribe(vec![
            ("email", "bot@spam.io"),
            ("consent", "yes"),
            ("website", "http://spam")
        ])
        .await
        .0,
        200
    );
    subscribe(vec![
        ("email", "bob@corp.com"),
        ("segment", "business"),
        ("company", "=cmd"),
        ("consent", "yes"),
        ("src", "linkedin"),
    ])
    .await;

    let stats = node
        .ok("GET", &format!("/releases/products/{pid}/launch"), None)
        .await;
    let s = &stats["stats"];
    assert_eq!(s["subscribers"], 2, "{stats}");
    assert_eq!(s["developers"], 1);
    assert_eq!(s["business"], 1);
    assert_eq!(s["signups"], 2);
    assert_eq!(s["downloads"], 1);
    let newsletter = s["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["source"] == "newsletter")
        .unwrap()
        .clone();
    assert_eq!(newsletter["signups"], 1, "{stats}");
    assert_eq!(newsletter["downloads"], 1);
    assert!(newsletter["visits"].as_i64().unwrap() >= 1);
    assert_eq!(stats["page"], format!("{}/p/kit", node.base));

    // The export is for admins, logged, and safe to open in a spreadsheet.
    let (code, _) = node
        .call(
            "GET",
            &format!("/releases/products/{pid}/subscribers.csv"),
            "alice-token",
            None,
        )
        .await;
    assert_eq!(code, 403);
    let csv = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!(
            "{}/api/v1/releases/products/{pid}/subscribers.csv",
            node.base
        ))
        .bearer_auth("admin-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(csv.contains("\"ana@dev.io\",\"developer\""), "{csv}");
    assert!(csv.contains("\"'=cmd\""), "{csv}");
    assert!(!csv.contains("bot@spam.io"), "{csv}");

    // Sign-ups go on to the newsletter service; unsubscribing works and
    // goes on too.
    let unsubscribe_url = {
        let hooked = hooked.lock().unwrap();
        let (headers, body) = hooked
            .iter()
            .find(|(h, _)| h["x-agentcore-event"] == "subscribed")
            .expect("a subscribed event");
        assert_eq!(
            headers["x-agentcore-signature"].to_str().unwrap(),
            agentcore_server::signing::hmac_signature("whsec", body)
        );
        let v: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(v["email"], "ana@dev.io");
        assert!(v["consent"].as_str().unwrap().contains("unsubscribe"));
        v["unsubscribe_url"].as_str().unwrap().to_string()
    };
    let (code, _, confirm) = raw("GET", &unsubscribe_url, None).await;
    assert_eq!(code, 200);
    assert!(
        confirm.contains("<form method=\"post\""),
        "a scanner opening the link changes nothing"
    );
    let token = unsubscribe_url.split("token=").nth(1).unwrap().to_string();
    let (code, _, done) = raw(
        "POST",
        &format!("{}/p/kit/unsubscribe", node.base),
        Some(&[("token", token.as_str())]),
    )
    .await;
    assert_eq!(code, 200);
    assert!(done.contains("Unsubscribed"));
    let stats = node
        .ok("GET", &format!("/releases/products/{pid}/launch"), None)
        .await;
    assert_eq!(stats["stats"]["subscribers"], 1);
    for _ in 0..100 {
        if hooked
            .lock()
            .unwrap()
            .iter()
            .any(|(h, _)| h["x-agentcore-event"] == "unsubscribed")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        hooked
            .lock()
            .unwrap()
            .iter()
            .any(|(h, _)| h["x-agentcore-event"] == "unsubscribed")
    );

    // A patch is not announced (unless the product asks for it).
    ship("1.0.1").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let briefs = node
        .messages("")
        .await
        .iter()
        .filter(|m| {
            m["text"]
                .as_str()
                .unwrap_or_default()
                .contains("Launch brief")
        })
        .count();
    assert_eq!(briefs, 1, "only 1.0.0 was briefed");

    // The dashboard and the retrospective see the results.
    let metrics = node.ok("GET", "/org/metrics", None).await;
    let kit = metrics["launch"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["product"] == "kit")
        .unwrap()
        .clone();
    assert_eq!(kit["signups"], 2, "{metrics}");
    assert_eq!(kit["released"], 2);

    node.ok("POST", &format!("/org/agents/{aid}/stop"), None)
        .await;
    node.cluster.cancel();
}
