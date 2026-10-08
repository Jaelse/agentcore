//! Agent on a GitHub issue, end to end: mock GitHub API (REST + GraphQL),
//! a real git remote (bare repo), role prompt, board moves, two turns,
//! failing then passing checks, delivery as a pull request.
//! Requires `AGENTCORE_TEST_DATABASE_URL` (skipped otherwise).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentcore_core::AgentSpec;
use agentcore_sandbox::{BackendKind, SandboxConfig};
use agentcore_server::config::{Operator, Role};
use agentcore_server::{AppState, Config, auth::hash_token, router};
use axum::Json;
use axum::extract::Path as UrlPath;
use axum::routing::{get, post};
use serde_json::{Value, json};

type Log = Arc<Mutex<Vec<(String, Value)>>>;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare "GitHub" repo at <root>/acme/demo.git with the team's conventions.
fn remote(root: &Path) -> std::path::PathBuf {
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q"]);
    std::fs::write(
        seed.join("CONTRIBUTING.md"),
        "Commits use Conventional Commits. Tabs, not spaces.\n",
    )
    .unwrap();
    std::fs::write(seed.join(".github-pr-template.md"), "").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-qm", "chore: initial"]);
    let bare = root.join("remotes/acme/demo.git");
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    bare
}

async fn mock_github(log: Log) -> String {
    let rec =
        |log: &Log, what: &str, body: Value| log.lock().unwrap().push((what.to_string(), body));
    let l1 = log.clone();
    let l2 = log.clone();
    let l3 = log.clone();
    let l4 = log.clone();
    let app = axum::Router::new()
        .route("/user", get(|| async { Json(json!({ "login": "agentcore-bot" })) }))
        .route(
            "/repos/acme/demo/issues/{n}",
            get(|UrlPath(n): UrlPath<u64>| async move {
                Json(json!({
                    "number": n, "title": "Say hello", "state": "open",
                    "body": "We need a hello.txt file.", "html_url": format!("https://github.example/acme/demo/issues/{n}"),
                    "labels": [{ "name": "good first issue" }], "assignees": [], "milestone": { "title": "Sprint 12" },
                }))
            }),
        )
        .route(
            "/repos/acme/demo/issues/{n}/comments",
            get(|| async { Json(json!([{ "user": { "login": "pm" }, "body": "Keep it short.", "created_at": "2026-10-01" }])) })
                .post(move |UrlPath(n): UrlPath<u64>, Json(body): Json<Value>| {
                    let log = l1.clone();
                    async move {
                        rec(&log, &format!("comment #{n}"), body);
                        Json(json!({ "html_url": "https://github.example/c/1" }))
                    }
                }),
        )
        .route(
            "/repos/acme/demo/pulls",
            get(|| async { Json(json!([])) }).post(move |Json(body): Json<Value>| {
                let log = l2.clone();
                async move {
                    rec(&log, "create pr", body);
                    Json(json!({ "number": 12, "html_url": "https://github.example/acme/demo/pull/12" }))
                }
            }),
        )
        .route(
            "/graphql",
            post(move |Json(req): Json<Value>| {
                let log = l3.clone();
                async move {
                    let q = req["query"].as_str().unwrap_or_default();
                    if q.contains("updateProjectV2ItemFieldValue") {
                        rec(&log, "move card", req["variables"].clone());
                        return Json(json!({ "data": { "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item7" } } } }));
                    }
                    let options = ["Todo", "In Progress", "In Review", "Done"]
                        .iter()
                        .map(|n| json!({ "id": format!("opt-{n}"), "name": n }))
                        .collect::<Vec<_>>();
                    Json(json!({ "data": { "repositoryOwner": { "projectV2": {
                        "id": "board1", "title": "Team board", "url": "https://github.example/orgs/acme/projects/1",
                        "fields": { "nodes": [ { "id": "status-field", "name": "Status", "options": options } ] },
                        "items": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [ {
                            "id": "item7",
                            "fieldValues": { "nodes": [ { "name": "Todo", "field": { "name": "Status" } } ] },
                            "content": { "__typename": "Issue", "number": 7, "title": "Say hello", "url": "u", "state": "OPEN",
                                         "repository": { "nameWithOwner": "acme/demo" }, "labels": { "nodes": [] },
                                         "assignees": { "nodes": [] }, "milestone": null }
                        } ] }
                    } } } }))
                }
            }),
        )
        .fallback(move |uri: axum::http::Uri| {
            let log = l4.clone();
            async move {
                rec(&log, &format!("unexpected {uri}"), Value::Null);
                (axum::http::StatusCode::NOT_FOUND, "nope")
            }
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

const TURN_ONE: &str = r#"
printf '%s' "$0" > "$HOME/prompt.txt"
mcp() { curl -s -X POST "$AGENTCORE_GATEWAY_URL" -H "Authorization: Bearer $AGENTCORE_GATEWAY_TOKEN" -H 'Content-Type: application/json' -d "$1"; echo; }
mcp '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | tr ',' '\n' | grep -o '"name":"[a-z_]*"' | tr '\n' ' '; echo
mcp '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"github_get_issue","arguments":{"number":7}}}' | grep -o 'Keep it short' 
echo hello > hello.txt
git add hello.txt && git commit -qm 'add hello'
mcp '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"propose_pull_request","arguments":{"title":"feat: say hello","body":"Adds hello.txt as requested in #7."}}}'
echo turn one done
"#;

const TURN_TWO: &str = r#"git commit -q --amend -m 'feat: add hello file' && echo "fixed: $0""#;

async fn http(method: &str, url: &str, token: &str, body: Option<Value>) -> (u16, Value) {
    let mut args = vec![
        "-s".to_string(),
        "-X".into(),
        method.into(),
        "-w".into(),
        "\n%{http_code}".into(),
        "-H".into(),
        format!("Authorization: Bearer {token}"),
    ];
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
async fn agent_works_an_issue_and_delivers_a_pull_request() {
    let Some((_, db)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let bare = remote(dir.path());
    let log: Log = Arc::default();
    let api = mock_github(log.clone()).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config: Config = toml::from_str("").unwrap();
    config.server.bind = addr;
    config.server.operators = vec![
        Operator {
            name: "root".into(),
            token_sha256: hash_token("admin"),
            role: Role::Admin,
        },
        Operator {
            name: "alice".into(),
            token_sha256: hash_token("alice"),
            role: Role::Operator,
        },
    ];
    config.database.url = Some(db);
    config.secrets.master_key_file = Some(dir.path().join("master.key"));
    config.storage.data_dir = dir.path().join("data");
    config.storage.audit_fsync = false;
    config.policies.dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../policies").into();
    config.roles.dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../roles").into();
    config.sandbox = SandboxConfig {
        backend: BackendKind::Process,
        ..Default::default()
    };
    config.sandbox.process.allow_insecure = true;
    config.agents = vec![AgentSpec {
        name: "scripted".into(),
        adapter: "command".into(),
        description: String::new(),
        image: None,
        command: Some("sh".into()),
        args: vec!["-c".into(), TURN_ONE.into(), "{task}".into()],
        env: Default::default(),
        policy: None,
        tty: None,
        follow_up_args: vec!["-c".into(), TURN_TWO.into(), "{task}".into()],
        ..Default::default()
    }];
    let state = AppState::new(config).await.unwrap();
    let app = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{addr}/api/v1");

    // Admin connects GitHub and sets up the project with its board.
    let github = json!({ "token": "ghp_test_token", "api_url": api, "web_url": format!("file://{}", dir.path().join("remotes").display()),
                         "commit_name": "agentcore[bot]", "commit_email": "bot@example.com" });
    assert_eq!(
        http(
            "PUT",
            &format!("{base}/integrations/github"),
            "alice",
            Some(github.clone())
        )
        .await
        .0,
        403
    );
    let (code, conn) = http(
        "PUT",
        &format!("{base}/integrations/github"),
        "admin",
        Some(github),
    )
    .await;
    assert_eq!(code, 200, "{conn}");
    assert!(!conn.to_string().contains("ghp_test_token"));
    assert_eq!(
        http(
            "POST",
            &format!("{base}/integrations/github/test"),
            "admin",
            None
        )
        .await
        .1["login"],
        "agentcore-bot"
    );

    let (code, project) = http("POST", &format!("{base}/projects"), "admin", Some(json!({
        "name": "Demo", "repository": "acme/demo", "default_branch": "main", "agent": "scripted", "role": "developer",
        "board": { "owner": "acme", "number": 1 }, "notes": "Ship small.",
    }))).await;
    assert_eq!(code, 201, "{project}");
    let pid = project["id"].as_str().unwrap();

    let (code, board) = http(
        "GET",
        &format!("{base}/projects/{pid}/board"),
        "alice",
        None,
    )
    .await;
    assert_eq!(code, 200, "{board}");
    assert_eq!(
        board["board"]["columns"],
        json!(["Todo", "In Progress", "In Review", "Done"])
    );
    assert_eq!(board["board"]["items"][0]["number"], 7);

    // An operator puts the agent on issue #7.
    let (code, session) = http(
        "POST",
        &format!("{base}/projects/{pid}/sessions"),
        "alice",
        Some(json!({ "issue_number": 7 })),
    )
    .await;
    assert_eq!(code, 201, "{session}");
    let sid = session["id"].as_str().unwrap().to_string();
    assert_eq!(session["context"]["issue"]["number"], 7);
    assert_eq!(session["context"]["role"], "developer");
    let branch = session["context"]["work_branch"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(branch, "agent/7-say-hello");

    let wait = |status: &'static str, turns: usize| {
        let base = base.clone();
        let sid = sid.clone();
        async move {
            for _ in 0..400 {
                let (_, info) = http("GET", &format!("{base}/sessions/{sid}"), "alice", None).await;
                let (_, events) = http(
                    "GET",
                    &format!("{base}/sessions/{sid}/events"),
                    "alice",
                    None,
                )
                .await;
                let ended = events
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|e| e["event"] == "turn_ended")
                    .count();
                if info["session"]["status"] == status && ended >= turns {
                    return events;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("timed out waiting for {status}");
        }
    };
    let events = wait("awaiting_input", 1).await;
    let output: String = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event"] == "output")
        .map(|e| format!("{}\n", e["line"].as_str().unwrap()))
        .collect();
    assert!(
        output.contains("propose_pull_request") && output.contains("github_get_issue"),
        "{output}"
    );
    assert!(
        !output.contains("github_update_issue"),
        "developer role must not get write tools: {output}"
    );
    assert!(
        output.contains("Keep it short"),
        "agent could read the issue via its tool: {output}"
    );

    // The prompt carried the playbook, the repo's own conventions, the issue and project notes.
    let home = dir
        .path()
        .join("data/workspaces")
        .join(format!("{sid}.home"));
    let prompt = std::fs::read_to_string(home.join("prompt.txt")).unwrap();
    for needle in [
        "Software developer",
        "## CONTRIBUTING.md",
        "Tabs, not spaces",
        "Issue #7: Say hello",
        "Keep it short.",
        "Ship small.",
        "propose_pull_request",
    ] {
        assert!(prompt.contains(needle), "prompt misses {needle}");
    }
    // Starting moved the card to In Progress and announced it on the issue.
    tokio::time::sleep(Duration::from_millis(300)).await;
    {
        let log = log.lock().unwrap();
        assert!(
            log.iter()
                .any(|(w, v)| w == "move card" && v["o"] == "opt-In Progress"),
            "{log:?}"
        );
        assert!(
            log.iter().any(|(w, v)| w == "comment #7"
                && v["body"].as_str().unwrap().contains("started working"))
        );
    }

    // Changes are visible; delivery is refused because the commit message breaks the convention.
    let (_, changes) = http(
        "GET",
        &format!("{base}/sessions/{sid}/changes"),
        "alice",
        None,
    )
    .await;
    assert_eq!(changes["commits"][0]["subject"], "add hello");
    assert!(changes["patch"].as_str().unwrap().contains("+hello"));
    let (code, refused) = http(
        "POST",
        &format!("{base}/sessions/{sid}/deliver"),
        "alice",
        Some(json!({})),
    )
    .await;
    assert_eq!(code, 409, "{refused}");
    let failed: Vec<_> = refused["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["passed"] == false)
        .map(|c| c["name"].clone())
        .collect();
    assert_eq!(failed, [json!("Commit messages follow the convention")]);

    // The human asks for a fix in the same conversation.
    let (code, _) = http(
        "POST",
        &format!("{base}/sessions/{sid}/messages"),
        "alice",
        Some(json!({ "text": "Please use a conventional commit message" })),
    )
    .await;
    assert_eq!(code, 202);
    let events = wait("awaiting_input", 2).await;
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"] == "user_message")
    );

    // Now delivery passes: branch pushed, PR opened, card moved to In Review.
    let (code, delivered) = http(
        "POST",
        &format!("{base}/sessions/{sid}/deliver"),
        "alice",
        Some(json!({})),
    )
    .await;
    assert_eq!(code, 200, "{delivered}");
    assert_eq!(
        delivered["pull_request"]["url"],
        "https://github.example/acme/demo/pull/12"
    );
    let pushed = git(
        &bare,
        &[
            "log",
            "--format=%an|%s",
            "-1",
            &format!("refs/heads/{branch}"),
        ],
    );
    assert_eq!(pushed, "agentcore[bot]|feat: add hello file");
    assert_eq!(
        git(&bare, &["rev-parse", &format!("refs/heads/{branch}")]),
        delivered["commit"].as_str().unwrap()
    );
    // main is untouched.
    assert_eq!(
        git(&bare, &["log", "--format=%s", "-1", "main"]),
        "chore: initial"
    );
    {
        let log = log.lock().unwrap();
        let pr = &log.iter().find(|(w, _)| w == "create pr").unwrap().1;
        assert_eq!(pr["title"], "feat: say hello");
        assert_eq!(
            (
                pr["head"].as_str().unwrap(),
                pr["base"].as_str().unwrap(),
                pr["draft"].as_bool().unwrap()
            ),
            (branch.as_str(), "main", true)
        );
        let body = pr["body"].as_str().unwrap();
        assert!(
            body.contains("Adds hello.txt")
                && body.contains("Prepared by an AI agent")
                && body.contains("delivered by alice"),
            "{body}"
        );
        assert!(
            log.iter()
                .any(|(w, v)| w == "move card" && v["o"] == "opt-In Review")
        );
        assert!(
            log.iter().all(|(w, _)| !w.starts_with("unexpected")),
            "{log:?}"
        );
    }

    // Finish: the session ends, its record keeps the PR link and the changes.
    assert_eq!(
        http(
            "POST",
            &format!("{base}/sessions/{sid}/finish"),
            "alice",
            None
        )
        .await
        .0,
        202
    );
    wait("completed", 2).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let stored = state
        .store
        .get_session(sid.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.info.context.pull_request_url.as_deref(),
        Some("https://github.example/acme/demo/pull/12")
    );
    let (_, archived_changes) = http(
        "GET",
        &format!("{base}/sessions/{sid}/changes"),
        "alice",
        None,
    )
    .await;
    assert_eq!(
        archived_changes["commits"][0]["subject"],
        "feat: add hello file"
    );
    let (_, verify) = http(
        "GET",
        &format!("{base}/sessions/{sid}/audit/verify"),
        "alice",
        None,
    )
    .await;
    assert_eq!(verify["valid"], true);
    // The token never reached the agent's workspace.
    let ws = dir.path().join("data/workspaces").join(&sid);
    let grep = std::process::Command::new("grep")
        .args(["-r", "ghp_test_token", ws.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(grep.stdout.is_empty(), "token leaked into the workspace");
}

/// A department agent: records what it got, and with a checkout commits a
/// change and proposes a pull request.
const DEPARTMENT_AGENT: &str = r#"
case "$0" in "You have "*) echo "woken"; exit 0 ;; esac
printf '%s' "$0" > "$HOME/prompt.txt"
mcp() { curl -s -X POST "$AGENTCORE_GATEWAY_URL" -H "Authorization: Bearer $AGENTCORE_GATEWAY_TOKEN" -H 'Content-Type: application/json' -d "$1"; echo; }
echo "tools: $(mcp '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | tr ',' '\n' | grep -o '"name":"[a-z_]*"' | cut -d'"' -f4 | tr '\n' ' ')"
if git rev-parse --git-dir >/dev/null 2>&1; then
  echo "branch: $(git rev-parse --abbrev-ref HEAD)"
  f="note-$(date +%s%N).txt"; echo "from $AGENTCORE_SESSION_ID" > "$f"
  git add "$f" && git commit -qm "feat: add a note" && echo committed
  mcp '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"propose_pull_request","arguments":{"title":"feat: add a note","body":"A small note."}}}' >/dev/null
fi
"#;

#[tokio::test]
async fn departments_work_on_a_linked_repository() {
    let Some((_, db)) = agentcore_store::testing::fresh_store().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let bare = remote(dir.path());
    let log: Log = Arc::default();
    let api = mock_github(log.clone()).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config: Config = toml::from_str("").unwrap();
    config.server.bind = addr;
    config.server.operators = vec![Operator {
        name: "root".into(),
        token_sha256: hash_token("admin"),
        role: Role::Admin,
    }];
    config.database.url = Some(db);
    config.secrets.master_key_file = Some(dir.path().join("master.key"));
    config.storage.data_dir = dir.path().join("data");
    config.storage.audit_fsync = false;
    config.policies.dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../policies").into();
    config.roles.dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../roles").into();
    config.templates.dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates").into();
    config.cluster.node_name = Some("n1".into());
    config.cluster.reconcile_millis = 200;
    config.sandbox = SandboxConfig {
        backend: BackendKind::Process,
        ..Default::default()
    };
    config.sandbox.process.allow_insecure = true;
    config.agents = vec![AgentSpec {
        name: "dept-agent".into(),
        adapter: "command".into(),
        command: Some("sh".into()),
        args: vec!["-c".into(), DEPARTMENT_AGENT.into(), "{task}".into()],
        follow_up_args: vec!["-c".into(), DEPARTMENT_AGENT.into(), "{task}".into()],
        tty: Some(false),
        ..Default::default()
    }];
    let state = AppState::new(config).await.unwrap();
    let cluster = agentcore_server::cluster::start(&state).await.unwrap();
    let app = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{addr}/api/v1");

    let github = json!({ "token": "ghp_test_token", "api_url": api, "web_url": format!("file://{}", dir.path().join("remotes").display()),
                         "commit_name": "agentcore[bot]", "commit_email": "bot@example.com" });
    assert_eq!(
        http(
            "PUT",
            &format!("{base}/integrations/github"),
            "admin",
            Some(github)
        )
        .await
        .0,
        200
    );
    let (code, project) = http("POST", &format!("{base}/projects"), "admin", Some(json!({
        "name": "Demo", "repository": "acme/demo", "default_branch": "main", "agent": "dept-agent",
        "role": "developer", "notes": "Ship small.",
    }))).await;
    assert_eq!(code, 201, "{project}");
    let pid = project["id"].as_str().unwrap();

    // Build a small company working on the repository.
    let (code, built) = http("POST", &format!("{base}/org/build"), "admin", Some(json!({
        "profile": { "company_name": "Acme", "company_about": "We sell demos.", "blueprint": "saas-startup" },
        "departments": ["engineering", "product", "finance"], "size": "lean",
        "agent": "dept-agent", "project_id": pid, "start": true,
    }))).await;
    assert_eq!(code, 200, "{built}");
    let created = built["created"].as_array().unwrap();
    let dept = |name: &str| created.iter().find(|d| d["name"] == name).unwrap().clone();
    assert_eq!(dept("Engineering")["project_id"], pid);
    assert_eq!(dept("Engineering")["role"], "developer");
    assert_eq!(dept("Product")["role"], "project-manager");
    assert!(
        dept("Finance")["project_id"].is_null(),
        "finance has no role on a repository"
    );

    // Wait until the engineering developer and the product manager are up.
    let agent_session = |dept_name: &'static str, agent: &'static str| {
        let base = base.clone();
        async move {
            for _ in 0..400 {
                let (_, org) = http("GET", &format!("{base}/org"), "admin", None).await;
                let found = org["departments"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|d| d["name"] == dept_name)
                    .flat_map(|d| d["agents"].as_array().unwrap().clone())
                    .find(|a| a["name"] == agent && a["status"] == "awaiting_input");
                if let Some(a) = found {
                    return a["session_id"].as_str().unwrap().to_string();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("{dept_name}/{agent} did not start");
        }
    };
    let dev = agent_session("Engineering", "developer").await;
    let pm = agent_session("Product", "manager").await;

    let (_, info) = http("GET", &format!("{base}/sessions/{dev}"), "admin", None).await;
    let ctx = &info["session"]["context"];
    assert_eq!(ctx["repository"], "acme/demo");
    assert_eq!(ctx["role"], "developer");
    assert_eq!(ctx["department_name"], "Engineering");
    let branch = ctx["work_branch"].as_str().unwrap().to_string();
    assert!(
        branch.starts_with("agent/task-engineering-developer-"),
        "{branch}"
    );
    assert_eq!(info["role"]["delivery"], "pull_request");

    let output = |sid: String| {
        let base = base.clone();
        async move {
            let (_, events) = http(
                "GET",
                &format!("{base}/sessions/{sid}/events"),
                "admin",
                None,
            )
            .await;
            events
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["event"] == "output")
                .map(|e| format!("{}\n", e["line"].as_str().unwrap()))
                .collect::<String>()
        }
    };
    let out = output(dev.clone()).await;
    assert!(out.contains(&format!("branch: {branch}")), "{out}");
    assert!(out.contains("committed"), "{out}");
    for tool in [
        "run_command",
        "github_get_issue",
        "propose_pull_request",
        "team_send_message",
    ] {
        assert!(out.contains(tool), "{tool} missing: {out}");
    }
    // The prompt: role playbook, the team's conventions, the department, the repository.
    let prompt = std::fs::read_to_string(
        dir.path()
            .join("data/workspaces")
            .join(format!("{dev}.home"))
            .join("prompt.txt"),
    )
    .unwrap();
    for part in [
        "# Your role: Software developer",
        "Commits use Conventional Commits",
        "Ship small.",
        "**Engineering** department",
        "Acme: We sell demos.",
        "## Your repository",
        &branch,
        "propose_pull_request",
    ] {
        assert!(
            prompt.contains(part),
            "`{part}` missing from the prompt:\n{prompt}"
        );
    }

    // Product: GitHub tools for its role, no checkout and no sandbox tools.
    let pm_out = output(pm.clone()).await;
    assert!(pm_out.contains("github_create_issue"), "{pm_out}");
    assert!(!pm_out.contains("run_command"), "{pm_out}");
    assert!(!pm_out.contains("branch:"), "{pm_out}");

    // A person delivers the developer's work as a pull request.
    let (code, delivered) = http(
        "POST",
        &format!("{base}/sessions/{dev}/deliver"),
        "admin",
        Some(json!({})),
    )
    .await;
    assert_eq!(code, 200, "{delivered}");
    assert_eq!(delivered["pull_request"]["number"], 12);
    assert_eq!(
        git(
            &bare,
            &["log", "--format=%s", "-1", &format!("refs/heads/{branch}")]
        ),
        "feat: add a note"
    );
    let pr = log
        .lock()
        .unwrap()
        .iter()
        .find(|(w, _)| w == "create pr")
        .cloned()
        .unwrap();
    assert_eq!(pr.1["head"], branch.as_str());
    assert_eq!(pr.1["base"], "main");

    http("POST", &format!("{base}/stop-all"), "admin", None).await;
    cluster.cancel();
}
