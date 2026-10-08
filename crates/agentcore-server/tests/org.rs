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
    ];
    config
}

struct Node {
    base: String,
    state: AppState,
    cluster: CancellationToken,
}

async fn start(dir: &std::path::Path, db_url: &str, name: &str, capacity: u32) -> Node {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = config(dir, db_url, addr, name);
    config.cluster.max_agents = capacity;
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
                json!({ "raise_limits": true, "size": "full" }),
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

    node.ok("POST", "/stop-all", None).await;
    node.cluster.cancel();
}
