//! Cloudflare: the connection admins set up under Settings, the client for
//! Cloudflare's API, and the `cloudflare_*` tools of agents in a role with
//! Cloudflare capabilities (see `roles/cloudflare-deployer.toml`).
//!
//! The API token never reaches an agent or a sandbox. Agents ask agentcore
//! to read and create resources; every call is a tool call, so it goes
//! through policy, approvals and the audit log. The only place the token
//! leaves agentcore is `cloudflare_connect_repository`, which stores it as an
//! encrypted GitHub Actions secret of the project's own repository, so the
//! repository's CI can deploy (an admin approves that call under the default
//! policy).

use std::collections::HashSet;

use agentcore_roles::{Role, tools_for};
use agentcore_runtime::ToolHandler;
use agentcore_store::{CloudflareConfig, Project};
use async_trait::async_trait;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::github::urlencode;

type ApiResult<T> = Result<T, ApiError>;

/// Lines of a build log handed to an agent.
const LOG_LINES: usize = 150;

// ---- client --------------------------------------------------------------------

/// A client for one Cloudflare account.
#[derive(Clone)]
pub struct Cloudflare {
    http: reqwest::Client,
    api: String,
    token: String,
    account: String,
}

impl Cloudflare {
    pub fn new(http: reqwest::Client, api: &str, token: String, account: String) -> Self {
        Self {
            http,
            api: api.trim_end_matches('/').to_string(),
            token,
            account,
        }
    }

    pub fn account_id(&self) -> &str {
        &self.account
    }

    /// `/accounts/{id}` + `path`.
    fn acct(&self, path: &str) -> String {
        format!("/accounts/{}{path}", self.account)
    }

    /// Call the API and return the whole response envelope (`result`,
    /// `result_info`, ...). Errors carry Cloudflare's own messages.
    async fn envelope(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.api))
            .bearer_auth(&self.token)
            .header("user-agent", "agentcore");
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("Cloudflare request failed: {}", without_url(&e)))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let ok = status.is_success() && value["success"].as_bool() != Some(false);
        if !ok {
            let errors: Vec<String> = value["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| match (e["code"].as_u64(), e["message"].as_str()) {
                    (Some(code), Some(m)) => format!("{m} (code {code})"),
                    (_, Some(m)) => m.to_string(),
                    _ => e.to_string(),
                })
                .collect();
            let message = if errors.is_empty() {
                text.chars().take(200).collect()
            } else {
                errors.join("; ")
            };
            return Err(format!("Cloudflare answered {status}: {message}"));
        }
        Ok(value)
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, String> {
        Ok(self.envelope(method, path, body).await?["result"].take())
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        self.call(Method::GET, path, None).await
    }

    /// Whether the token is active. User tokens and account-owned tokens are
    /// verified at different addresses; either is fine.
    pub async fn verify(&self) -> Result<(), String> {
        let result = match self.get("/user/tokens/verify").await {
            Ok(r) => r,
            // Not a user token: maybe one owned by the account. If that does
            // not answer clearly either, Cloudflare's first answer says why.
            Err(user_err) => match self.get(&self.acct("/tokens/verify")).await {
                Ok(r) if r["status"].is_string() => r,
                _ => return Err(user_err),
            },
        };
        match result["status"].as_str() {
            Some("active") => Ok(()),
            Some(other) => Err(format!("the token is {other}")),
            None => Err("Cloudflare did not confirm the token is active".into()),
        }
    }

    /// The account's name (also proves the token can see the account).
    pub async fn account_name(&self) -> Result<String, String> {
        let account = self.get(&self.acct("")).await?;
        Ok(account["name"].as_str().unwrap_or_default().to_string())
    }

    /// Every page of a list (`page`/`per_page` pagination), up to `max` items.
    async fn list(&self, path: &str, max: usize) -> Result<Vec<Value>, String> {
        let mut out = Vec::new();
        let sep = if path.contains('?') { '&' } else { '?' };
        for page in 1..=20 {
            let env = self
                .envelope(
                    Method::GET,
                    &format!("{path}{sep}page={page}&per_page=50"),
                    None,
                )
                .await?;
            let items = match &env["result"] {
                Value::Array(items) => items.clone(),
                // R2 lists buckets inside an object.
                other => other["buckets"].as_array().cloned().unwrap_or_default(),
            };
            let n = items.len();
            out.extend(items);
            let total = env["result_info"]["total_pages"].as_u64().unwrap_or(1);
            if n == 0 || page as u64 >= total || out.len() >= max {
                break;
            }
        }
        out.truncate(max);
        Ok(out)
    }

    /// [`Self::list`] with an owned path (for `join!`).
    async fn list_at(&self, path: String, max: usize) -> Result<Vec<Value>, String> {
        self.list(&path, max).await
    }

    /// [`Self::get`] with an owned path (for `join!`).
    async fn get_at(&self, path: String) -> Result<Value, String> {
        self.get(&path).await
    }

    /// The account's zone whose name `hostname` is in (the longest match).
    pub async fn zone_for(&self, hostname: &str) -> Result<(String, String), String> {
        let host = hostname.trim().trim_end_matches('.').to_ascii_lowercase();
        let zones = self
            .list(&format!("/zones?account.id={}", self.account), 1000)
            .await?;
        zones
            .iter()
            .filter_map(|z| Some((z["id"].as_str()?, z["name"].as_str()?)))
            .filter(|(_, name)| host == *name || host.ends_with(&format!(".{name}")))
            .max_by_key(|(_, name)| name.len())
            .map(|(id, name)| (id.to_string(), name.to_string()))
            .ok_or_else(|| {
                format!(
                    "no zone in this Cloudflare account contains `{host}`; a person adds the \
                     domain to the account first"
                )
            })
    }

    /// The log of a Pages deployment (the last lines).
    pub async fn pages_log(&self, project: &str, deployment: &str) -> Result<String, String> {
        let r = self
            .get(&self.acct(&format!(
                "/pages/projects/{}/deployments/{}/history/logs",
                urlencode(project),
                urlencode(deployment)
            )))
            .await?;
        let lines: Vec<&str> = r["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l["line"].as_str())
            .collect();
        Ok(tail(&lines, LOG_LINES))
    }

    /// The log of a Workers Builds build (the last lines).
    pub async fn workers_build_log(&self, build: &str) -> Result<String, String> {
        let r = self
            .get(&self.acct(&format!("/builds/builds/{}/logs", urlencode(build))))
            .await?;
        let lines: Vec<&str> = r["lines"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| {
                l.as_str()
                    .or_else(|| l.as_array().and_then(|a| a.last()).and_then(Value::as_str))
                    .or_else(|| l["line"].as_str())
                    .or_else(|| l["message"].as_str())
            })
            .collect();
        Ok(tail(&lines, LOG_LINES))
    }
}

/// A reqwest error without the URL (it may name the account).
fn without_url(err: &reqwest::Error) -> String {
    let mut msg = err.to_string();
    if let Some(url) = err.url() {
        msg = msg.replace(url.as_str(), "");
    }
    msg
}

fn tail(lines: &[&str], n: usize) -> String {
    lines[lines.len().saturating_sub(n)..].join("\n")
}

impl AppState {
    /// Cloudflare client from the stored connection, if one is configured.
    pub async fn cloudflare(&self) -> ApiResult<Option<Cloudflare>> {
        Ok(self
            .store
            .cloudflare_credentials()
            .await?
            .map(|(config, token)| self.cloudflare_client(&config, token)))
    }

    fn cloudflare_client(&self, config: &CloudflareConfig, token: String) -> Cloudflare {
        Cloudflare::new(
            self.http.clone(),
            &self.config.cloudflare.api_url,
            token,
            config.account_id.clone(),
        )
    }
}

// ---- build logs of failed checks -------------------------------------------------

/// Where the log of a failed Cloudflare check is, from its link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BuildRef {
    /// `https://dash.cloudflare.com/<account>/pages/view/<project>/<deployment>`
    Pages { project: String, deployment: String },
    /// `https://dash.cloudflare.com/<account>/workers/services/view/<script>/<env>/builds/<build>`
    WorkersBuild { build: String },
}

/// The build a Cloudflare dashboard link names, if it is one.
pub(crate) fn build_ref(url: &str) -> Option<BuildRef> {
    let rest = url.strip_prefix("https://dash.cloudflare.com/")?;
    let path = rest.split(['?', '#']).next()?;
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    };
    if !parts.iter().all(|p| ok(p)) {
        return None;
    }
    match parts.as_slice() {
        [_, "pages", "view", project, deployment, ..] => Some(BuildRef::Pages {
            project: project.to_string(),
            deployment: deployment.to_string(),
        }),
        [_, "workers", "services", "view", _, _, "builds", build, ..] => {
            Some(BuildRef::WorkersBuild {
                build: build.to_string(),
            })
        }
        _ => None,
    }
}

/// The build log behind a failed Cloudflare check, for the agent. `None`
/// when the link is not a Cloudflare build.
pub(crate) async fn failed_build_log(state: &AppState, url: Option<&str>) -> Option<String> {
    let build = url.and_then(build_ref)?;
    let cf = match state.cloudflare().await {
        Ok(Some(cf)) => cf,
        _ => {
            return Some(
                "(agentcore cannot read Cloudflare build logs: Cloudflare is not connected \
                 under Settings.)"
                    .into(),
            );
        }
    };
    let log = match &build {
        BuildRef::Pages {
            project,
            deployment,
        } => cf.pages_log(project, deployment).await,
        BuildRef::WorkersBuild { build } => cf.workers_build_log(build).await,
    };
    Some(match log {
        Ok(log) if !log.trim().is_empty() => log,
        Ok(_) => "(the Cloudflare build log is empty)".into(),
        Err(err) => format!("(could not read the Cloudflare build log: {err})"),
    })
}

// ---- tools for agents ---------------------------------------------------------------

fn def(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
    })
}

fn all_tool_definitions() -> Vec<Value> {
    let text = json!({ "type": "string" });
    let target = json!({
        "worker": { "type": "string", "description": "Worker (script) name" },
        "pages_project": { "type": "string", "description": "Pages project name" },
    });
    let with = |mut base: Value, extra: Value| {
        if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        base
    };
    vec![
        def(
            "cloudflare_get_account",
            "Overview of the connected Cloudflare account: Workers, Pages projects, D1 \
             databases, KV namespaces, R2 buckets, Queues, Hyperdrive configs and zones \
             (domains), with their names and ids, and the account's workers.dev subdomain. \
             Look here before creating anything.",
            json!({}),
            &[],
        ),
        def(
            "cloudflare_get_worker",
            "A Worker in detail: settings and bindings, recent deployments, custom domains, \
             whether it is on workers.dev, and the names of its secrets.",
            json!({ "name": text }),
            &["name"],
        ),
        def(
            "cloudflare_get_pages_project",
            "A Pages project in detail: build settings, domains, environment variable names \
             and its latest deployments (with their status and URLs).",
            json!({ "name": text }),
            &["name"],
        ),
        def(
            "cloudflare_get_pages_deployment_log",
            "The build log of a Pages deployment (the last lines).",
            json!({ "project": text, "deployment_id": text }),
            &["project", "deployment_id"],
        ),
        def(
            "cloudflare_list_dns_records",
            "DNS records of a zone (domain) in the account. Optionally only those of one name.",
            json!({ "zone": { "type": "string", "description": "Zone name, e.g. example.com" }, "name": { "type": "string", "description": "Full record name, e.g. app.example.com" } }),
            &["zone"],
        ),
        def(
            "cloudflare_list_secrets",
            "Names (never values) of the secrets of a Worker or a Pages project.",
            target.clone(),
            &[],
        ),
        def(
            "cloudflare_create_d1_database",
            "Create a D1 (SQLite) database, or return the existing one of that name. Returns \
             its id for the `d1_databases` binding.",
            json!({ "name": text, "location_hint": { "type": "string", "enum": ["wnam", "enam", "weur", "eeur", "apac", "oc"] } }),
            &["name"],
        ),
        def(
            "cloudflare_create_kv_namespace",
            "Create a Workers KV namespace, or return the existing one of that title. Returns \
             its id for the `kv_namespaces` binding.",
            json!({ "title": text }),
            &["title"],
        ),
        def(
            "cloudflare_create_r2_bucket",
            "Create an R2 bucket, or return the existing one of that name (lowercase letters, \
             digits and hyphens).",
            json!({ "name": text, "location_hint": { "type": "string", "enum": ["wnam", "enam", "weur", "eeur", "apac", "oc"] } }),
            &["name"],
        ),
        def(
            "cloudflare_create_queue",
            "Create a Queue, or return the existing one of that name.",
            json!({ "name": text }),
            &["name"],
        ),
        def(
            "cloudflare_create_pages_project",
            "Create a Pages project deployed from CI with `wrangler pages deploy` (direct \
             upload), or return the existing one. Prefer Workers with static assets for new \
             applications.",
            json!({ "name": text, "production_branch": text }),
            &["name", "production_branch"],
        ),
        def(
            "cloudflare_set_secret",
            "Set a secret on a Worker or a Pages project. With `generate: true` agentcore \
             makes a strong random value (64 hex characters) and never shows it to anyone: \
             use that for keys the app defines itself (sessions, JWT signing). Otherwise pass \
             the `value` a person gave you. The Worker must already exist (deployed once).",
            with(
                target,
                json!({
                    "name": { "type": "string", "description": "Secret name, e.g. SESSION_SECRET" },
                    "value": text,
                    "generate": { "type": "boolean" },
                    "environment": { "type": "string", "enum": ["production", "preview", "both"], "description": "Pages only; default both" }
                }),
            ),
            &["name"],
        ),
        def(
            "cloudflare_create_dns_record",
            "Create a DNS record in a zone of the account. Refuses to replace an existing \
             record of the same name and type.",
            json!({
                "zone": text,
                "type": { "type": "string", "enum": ["A", "AAAA", "CNAME", "TXT", "MX"] },
                "name": text,
                "content": text,
                "proxied": { "type": "boolean" },
                "ttl": { "type": "integer", "description": "Seconds; 1 = automatic" },
                "comment": text
            }),
            &["zone", "type", "name", "content"],
        ),
        def(
            "cloudflare_add_custom_domain",
            "Serve a Worker or a Pages project on a hostname of a zone in the account. For a \
             Worker it creates the DNS record and certificate; for Pages it adds the domain and \
             a CNAME to <project>.pages.dev when the hostname has no record yet.",
            with(target_schema(), json!({ "hostname": text })),
            &["hostname"],
        ),
        def(
            "cloudflare_connect_repository",
            "Store the Cloudflare credentials as GitHub Actions secrets of this project's \
             repository (CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID), so its deploy \
             workflow can use `${{ secrets.CLOUDFLARE_API_TOKEN }}`. You never see the token.",
            json!({}),
            &[],
        ),
    ]
}

fn target_schema() -> Value {
    json!({
        "worker": { "type": "string", "description": "Worker (script) name" },
        "pages_project": { "type": "string", "description": "Pages project name" },
    })
}

/// The Cloudflare tools of one project session, filtered by the role's
/// capabilities. The connection is read on every call, so connecting or
/// changing it takes effect for running sessions too.
pub struct CloudflareTools {
    state: AppState,
    owner: String,
    repo: String,
    allowed: HashSet<&'static str>,
}

impl CloudflareTools {
    /// `None` unless the role has a Cloudflare capability.
    pub(crate) fn for_role(state: &AppState, project: &Project, role: &Role) -> Option<Self> {
        let allowed: HashSet<&'static str> = role
            .capabilities
            .iter()
            .filter(|c| c.is_cloudflare())
            .flat_map(|c| tools_for(*c).iter().copied())
            .collect();
        (!allowed.is_empty()).then(|| Self {
            state: state.clone(),
            owner: project.repo_owner.clone(),
            repo: project.repo_name.clone(),
            allowed,
        })
    }
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("`{key}` is required"))
}

fn opt<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args[key].as_str().map(str::trim).filter(|s| !s.is_empty())
}

/// Exactly one of `worker` / `pages_project`.
enum Target<'a> {
    Worker(&'a str),
    Pages(&'a str),
}

fn target(args: &Value) -> Result<Target<'_>, String> {
    match (opt(args, "worker"), opt(args, "pages_project")) {
        (Some(w), None) => Ok(Target::Worker(w)),
        (None, Some(p)) => Ok(Target::Pages(p)),
        _ => Err("give either `worker` or `pages_project`".into()),
    }
}

/// Names Cloudflare accepts for secrets and environment variables.
fn valid_secret_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= 128
}

/// A strong random secret (256 bits, hex).
fn random_secret() -> Result<String, String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "could not generate a random value".to_string())?;
    Ok(hex::encode(bytes))
}

/// `[{name, id}]` of a list, for overviews.
fn summary(items: &[Value], name: &str, id: &str) -> Value {
    Value::Array(
        items
            .iter()
            .map(|i| json!({ "name": i[name], "id": i[id] }))
            .collect(),
    )
}

#[async_trait]
impl ToolHandler for CloudflareTools {
    fn definitions(&self) -> Vec<Value> {
        all_tool_definitions()
            .into_iter()
            .filter(|d| d["name"].as_str().is_some_and(|n| self.allowed.contains(n)))
            .collect()
    }

    async fn call(&self, tool: &str, args: &Value) -> Result<Value, String> {
        if !self.allowed.contains(tool) {
            return Err(format!("`{tool}` is not available in this role"));
        }
        if tool == "cloudflare_connect_repository" {
            return self.connect_repository().await;
        }
        let cf = self
            .state
            .cloudflare()
            .await
            .map_err(|e| e.message().to_string())?
            .ok_or(
                "Cloudflare is not connected: an admin connects it under Settings → Cloudflare",
            )?;
        run_tool(&cf, tool, args).await
    }
}

impl CloudflareTools {
    /// Give the repository's GitHub Actions the Cloudflare credentials.
    async fn connect_repository(&self) -> Result<Value, String> {
        let Some((config, token)) = self
            .state
            .store
            .cloudflare_credentials()
            .await
            .map_err(|e| e.to_string())?
        else {
            return Err("Cloudflare is not connected".into());
        };
        let (gh, _, _) = self
            .state
            .require_github()
            .await
            .map_err(|e| e.message().to_string())?;
        let (o, r) = (self.owner.as_str(), self.repo.as_str());
        for (name, value) in [
            ("CLOUDFLARE_API_TOKEN", token.as_str()),
            ("CLOUDFLARE_ACCOUNT_ID", config.account_id.as_str()),
        ] {
            gh.set_actions_secret(o, r, name, value)
                .await
                .map_err(|e| {
                    format!(
                        "could not set the Actions secret {name} on {o}/{r} ({e}); the \
                     GitHub token needs the `secrets: write` permission on the repository"
                    )
                })?;
        }
        Ok(json!({
            "repository": format!("{o}/{r}"),
            "secrets": ["CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID"],
            "use": "apiToken: ${{ secrets.CLOUDFLARE_API_TOKEN }} and accountId: ${{ secrets.CLOUDFLARE_ACCOUNT_ID }} in cloudflare/wrangler-action",
        }))
    }
}

/// Run a Cloudflare tool (all but `cloudflare_connect_repository`).
async fn run_tool(cf: &Cloudflare, tool: &str, args: &Value) -> Result<Value, String> {
    match tool {
        "cloudflare_get_account" => account_overview(cf).await,
        "cloudflare_get_worker" => worker_detail(cf, arg(args, "name")?).await,
        "cloudflare_get_pages_project" => pages_detail(cf, arg(args, "name")?).await,
        "cloudflare_get_pages_deployment_log" => cf
            .pages_log(arg(args, "project")?, arg(args, "deployment_id")?)
            .await
            .map(|log| json!({ "log": log })),
        "cloudflare_list_dns_records" => {
            let (zone_id, zone) = cf.zone_for(arg(args, "zone")?).await?;
            let mut path = format!("/zones/{zone_id}/dns_records");
            if let Some(name) = opt(args, "name") {
                path.push_str(&format!("?name={}", urlencode(name)));
            }
            let records = cf.list(&path, 500).await?;
            let records: Vec<Value> = records
                .iter()
                .map(|r| {
                    json!({
                        "id": r["id"], "type": r["type"], "name": r["name"],
                        "content": r["content"], "proxied": r["proxied"], "ttl": r["ttl"],
                    })
                })
                .collect();
            Ok(json!({ "zone": zone, "records": records }))
        }
        "cloudflare_list_secrets" => match target(args)? {
            Target::Worker(w) => {
                let secrets = cf
                    .get(&cf.acct(&format!("/workers/scripts/{}/secrets", urlencode(w))))
                    .await?;
                let names: Vec<&Value> = secrets
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| &s["name"])
                    .collect();
                Ok(json!({ "worker": w, "secrets": names }))
            }
            Target::Pages(p) => {
                let project = cf
                    .get(&cf.acct(&format!("/pages/projects/{}", urlencode(p))))
                    .await?;
                Ok(json!({ "pages_project": p, "variables": pages_env_names(&project) }))
            }
        },
        "cloudflare_create_d1_database" => {
            let name = arg(args, "name")?;
            let existing = cf.list(&cf.acct("/d1/database"), 1000).await?;
            if let Some(db) = existing.iter().find(|d| d["name"] == name) {
                return Ok(json!({ "name": name, "id": db["uuid"], "created": false }));
            }
            let mut body = json!({ "name": name });
            if let Some(hint) = opt(args, "location_hint") {
                body["primary_location_hint"] = json!(hint);
            }
            let db = cf
                .call(Method::POST, &cf.acct("/d1/database"), Some(body))
                .await?;
            Ok(json!({ "name": name, "id": db["uuid"], "created": true,
                       "binding_example": { "binding": "DB", "database_name": name, "database_id": db["uuid"] } }))
        }
        "cloudflare_create_kv_namespace" => {
            let title = arg(args, "title")?;
            let existing = cf.list(&cf.acct("/storage/kv/namespaces"), 1000).await?;
            if let Some(ns) = existing.iter().find(|n| n["title"] == title) {
                return Ok(json!({ "title": title, "id": ns["id"], "created": false }));
            }
            let ns = cf
                .call(
                    Method::POST,
                    &cf.acct("/storage/kv/namespaces"),
                    Some(json!({ "title": title })),
                )
                .await?;
            Ok(json!({ "title": title, "id": ns["id"], "created": true }))
        }
        "cloudflare_create_r2_bucket" => {
            let name = arg(args, "name")?;
            let existing = cf.list(&cf.acct("/r2/buckets"), 1000).await?;
            if existing.iter().any(|b| b["name"] == name) {
                return Ok(json!({ "name": name, "created": false }));
            }
            let mut body = json!({ "name": name });
            if let Some(hint) = opt(args, "location_hint") {
                body["locationHint"] = json!(hint);
            }
            cf.call(Method::POST, &cf.acct("/r2/buckets"), Some(body))
                .await?;
            Ok(json!({ "name": name, "created": true }))
        }
        "cloudflare_create_queue" => {
            let name = arg(args, "name")?;
            let existing = cf.list(&cf.acct("/queues"), 1000).await?;
            if let Some(q) = existing.iter().find(|q| q["queue_name"] == name) {
                return Ok(json!({ "name": name, "id": q["queue_id"], "created": false }));
            }
            let q = cf
                .call(
                    Method::POST,
                    &cf.acct("/queues"),
                    Some(json!({ "queue_name": name })),
                )
                .await?;
            Ok(json!({ "name": name, "id": q["queue_id"], "created": true }))
        }
        "cloudflare_create_pages_project" => {
            let name = arg(args, "name")?;
            let branch = arg(args, "production_branch")?;
            let path = cf.acct(&format!("/pages/projects/{}", urlencode(name)));
            if let Ok(p) = cf.get(&path).await {
                return Ok(json!({ "name": name, "subdomain": p["subdomain"], "created": false }));
            }
            let p = cf
                .call(
                    Method::POST,
                    &cf.acct("/pages/projects"),
                    Some(json!({ "name": name, "production_branch": branch })),
                )
                .await?;
            Ok(json!({ "name": name, "subdomain": p["subdomain"], "created": true }))
        }
        "cloudflare_set_secret" => set_secret(cf, args).await,
        "cloudflare_create_dns_record" => {
            let (zone_id, _) = cf.zone_for(arg(args, "zone")?).await?;
            let (kind, name) = (arg(args, "type")?, arg(args, "name")?);
            let existing = cf
                .list(
                    &format!(
                        "/zones/{zone_id}/dns_records?type={}&name={}",
                        urlencode(kind),
                        urlencode(name)
                    ),
                    10,
                )
                .await?;
            if !existing.is_empty() {
                return Err(format!(
                    "a {kind} record for `{name}` already exists ({}); not replacing it",
                    existing[0]["content"].as_str().unwrap_or("?")
                ));
            }
            let mut body = json!({
                "type": kind, "name": name, "content": arg(args, "content")?,
                "ttl": args["ttl"].as_u64().unwrap_or(1),
                "comment": opt(args, "comment").unwrap_or("Created by an agentcore agent"),
            });
            if let Some(p) = args["proxied"].as_bool() {
                body["proxied"] = json!(p);
            }
            let r = cf
                .call(
                    Method::POST,
                    &format!("/zones/{zone_id}/dns_records"),
                    Some(body),
                )
                .await?;
            Ok(
                json!({ "id": r["id"], "type": r["type"], "name": r["name"], "content": r["content"], "proxied": r["proxied"] }),
            )
        }
        "cloudflare_add_custom_domain" => add_custom_domain(cf, args).await,
        other => Err(format!("unknown tool `{other}`")),
    }
}

/// Names (and kinds) of a Pages project's environment variables.
fn pages_env_names(project: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for env in ["production", "preview"] {
        let vars = &project["deployment_configs"][env]["env_vars"];
        let names: Vec<Value> = vars
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| json!({ "name": k, "type": v["type"] }))
            .collect();
        out.insert(env.into(), Value::Array(names));
    }
    Value::Object(out)
}

async fn account_overview(cf: &Cloudflare) -> Result<Value, String> {
    let name = cf.account_name().await?;
    // Each part on its own: a token without access to one product still
    // shows the others.
    let part = |r: Result<Vec<Value>, String>, f: &dyn Fn(&[Value]) -> Value| match r {
        Ok(items) => f(&items),
        Err(e) => json!({ "error": e }),
    };
    let (workers, pages, d1, kv, r2, queues, hyperdrive, zones, subdomain) = tokio::join!(
        cf.list_at(cf.acct("/workers/scripts"), 500),
        cf.list_at(cf.acct("/pages/projects"), 500),
        cf.list_at(cf.acct("/d1/database"), 500),
        cf.list_at(cf.acct("/storage/kv/namespaces"), 500),
        cf.list_at(cf.acct("/r2/buckets"), 500),
        cf.list_at(cf.acct("/queues"), 500),
        cf.list_at(cf.acct("/hyperdrive/configs"), 500),
        cf.list_at(format!("/zones?account.id={}", cf.account_id()), 500),
        cf.get_at(cf.acct("/workers/subdomain")),
    );
    Ok(json!({
        "account": { "id": cf.account_id(), "name": name },
        "workers_dev_subdomain": subdomain.ok().and_then(|s| s["subdomain"].as_str().map(|d| format!("{d}.workers.dev"))),
        "workers": part(workers, &|w| Value::Array(w.iter().map(|s| json!({ "name": s["id"], "modified_on": s["modified_on"] })).collect())),
        "pages_projects": part(pages, &|p| Value::Array(p.iter().map(|s| json!({ "name": s["name"], "subdomain": s["subdomain"], "production_branch": s["production_branch"], "domains": s["domains"] })).collect())),
        "d1_databases": part(d1, &|d| summary(d, "name", "uuid")),
        "kv_namespaces": part(kv, &|k| summary(k, "title", "id")),
        "r2_buckets": part(r2, &|b| Value::Array(b.iter().map(|s| json!({ "name": s["name"] })).collect())),
        "queues": part(queues, &|q| summary(q, "queue_name", "queue_id")),
        "hyperdrive_configs": part(hyperdrive, &|h| summary(h, "name", "id")),
        "zones": part(zones, &|z| Value::Array(z.iter().map(|s| json!({ "name": s["name"], "id": s["id"], "status": s["status"] })).collect())),
    }))
}

async fn worker_detail(cf: &Cloudflare, name: &str) -> Result<Value, String> {
    let script = urlencode(name);
    let settings = cf
        .get(&cf.acct(&format!("/workers/scripts/{script}/settings")))
        .await
        .map_err(|e| format!("no Worker `{name}`? {e}"))?;
    let (deployments, domains, subdomain, secrets) = tokio::join!(
        cf.get_at(cf.acct(&format!("/workers/scripts/{script}/deployments"))),
        cf.list_at(cf.acct(&format!("/workers/domains?service={script}")), 100),
        cf.get_at(cf.acct(&format!("/workers/scripts/{script}/subdomain"))),
        cf.get_at(cf.acct(&format!("/workers/scripts/{script}/secrets"))),
    );
    let bindings: Vec<Value> = settings["bindings"]
        .as_array()
        .into_iter()
        .flatten()
        // Never echo values of plain-text bindings that hold secrets.
        .map(|b| {
            let mut b = b.clone();
            if b["type"] == "secret_text" {
                b["text"] = Value::Null;
            }
            b
        })
        .collect();
    Ok(json!({
        "name": name,
        "compatibility_date": settings["compatibility_date"],
        "compatibility_flags": settings["compatibility_flags"],
        "bindings": bindings,
        "observability": settings["observability"],
        "deployments": deployments.map(|d| d["deployments"].as_array().map(|a| a.iter().take(5).cloned().collect::<Vec<_>>())).unwrap_or_default(),
        "custom_domains": domains.map(|d| d.iter().map(|x| x["hostname"].clone()).collect::<Vec<_>>()).unwrap_or_default(),
        "workers_dev": subdomain.ok().map(|s| s["enabled"].clone()),
        "secrets": secrets.map(|s| s.as_array().into_iter().flatten().map(|x| x["name"].clone()).collect::<Vec<_>>()).unwrap_or_default(),
    }))
}

async fn pages_detail(cf: &Cloudflare, name: &str) -> Result<Value, String> {
    let project = cf
        .get(&cf.acct(&format!("/pages/projects/{}", urlencode(name))))
        .await?;
    let deployments = cf
        .envelope(
            Method::GET,
            &cf.acct(&format!(
                "/pages/projects/{}/deployments?per_page=10",
                urlencode(name)
            )),
            None,
        )
        .await
        .map(|e| e["result"].clone())
        .unwrap_or(Value::Null);
    let deployments: Vec<Value> = deployments
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| {
            json!({
                "id": d["id"], "url": d["url"], "environment": d["environment"],
                "branch": d["deployment_trigger"]["metadata"]["branch"],
                "commit": d["deployment_trigger"]["metadata"]["commit_hash"],
                "stage": d["latest_stage"]["name"], "status": d["latest_stage"]["status"],
                "created_on": d["created_on"],
            })
        })
        .collect();
    Ok(json!({
        "name": name,
        "subdomain": project["subdomain"],
        "domains": project["domains"],
        "production_branch": project["production_branch"],
        "build_config": project["build_config"],
        "source": project["source"]["type"],
        "variables": pages_env_names(&project),
        "deployments": deployments,
    }))
}

async fn set_secret(cf: &Cloudflare, args: &Value) -> Result<Value, String> {
    let name = arg(args, "name")?;
    if !valid_secret_name(name) {
        return Err(format!(
            "`{name}` is not a valid secret name (letters, digits and _)"
        ));
    }
    let generate = args["generate"].as_bool().unwrap_or(false);
    let value = match (generate, args["value"].as_str()) {
        (true, None) => random_secret()?,
        (false, Some(v)) if !v.is_empty() => v.to_string(),
        (true, Some(_)) => return Err("give either `value` or `generate: true`, not both".into()),
        _ => return Err("give the `value`, or `generate: true` for a random one".into()),
    };
    match target(args)? {
        Target::Worker(w) => {
            cf.call(
                Method::PUT,
                &cf.acct(&format!("/workers/scripts/{}/secrets", urlencode(w))),
                Some(json!({ "name": name, "text": value, "type": "secret_text" })),
            )
            .await
            .map_err(|e| {
                format!("{e} (a Worker's secrets can only be set once it has been deployed)")
            })?;
            Ok(json!({ "worker": w, "secret": name, "set": true, "generated": generate }))
        }
        Target::Pages(p) => {
            let envs: &[&str] = match opt(args, "environment").unwrap_or("both") {
                "production" => &["production"],
                "preview" => &["preview"],
                "both" => &["production", "preview"],
                other => return Err(format!("unknown environment `{other}`")),
            };
            let mut configs = serde_json::Map::new();
            for env in envs {
                configs.insert(
                    (*env).into(),
                    json!({ "env_vars": { name: { "type": "secret_text", "value": value } } }),
                );
            }
            cf.call(
                Method::PATCH,
                &cf.acct(&format!("/pages/projects/{}", urlencode(p))),
                Some(json!({ "deployment_configs": configs })),
            )
            .await?;
            Ok(
                json!({ "pages_project": p, "secret": name, "environments": envs, "set": true, "generated": generate }),
            )
        }
    }
}

async fn add_custom_domain(cf: &Cloudflare, args: &Value) -> Result<Value, String> {
    let hostname = arg(args, "hostname")?.to_ascii_lowercase();
    let (zone_id, zone) = cf.zone_for(&hostname).await?;
    match target(args)? {
        Target::Worker(w) => {
            let d = cf
                .call(
                    Method::PUT,
                    &cf.acct("/workers/domains"),
                    Some(json!({
                        "hostname": hostname, "service": w, "zone_id": zone_id,
                        "environment": "production",
                    })),
                )
                .await?;
            Ok(json!({ "hostname": d["hostname"], "worker": w, "zone": zone, "id": d["id"] }))
        }
        Target::Pages(p) => {
            let project_path = cf.acct(&format!("/pages/projects/{}", urlencode(p)));
            let project = cf.get(&project_path).await?;
            cf.call(
                Method::POST,
                &format!("{project_path}/domains"),
                Some(json!({ "name": hostname })),
            )
            .await?;
            // Point the hostname at the project, unless something already
            // answers for it.
            let existing = cf
                .list(
                    &format!("/zones/{zone_id}/dns_records?name={}", urlencode(&hostname)),
                    10,
                )
                .await?;
            let dns = if existing.is_empty() {
                let target = project["subdomain"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| format!("{p}.pages.dev"));
                cf.call(
                    Method::POST,
                    &format!("/zones/{zone_id}/dns_records"),
                    Some(json!({
                        "type": "CNAME", "name": hostname, "content": target, "proxied": true,
                        "ttl": 1, "comment": "Created by an agentcore agent for Pages",
                    })),
                )
                .await?;
                json!({ "created": "CNAME", "target": target })
            } else {
                json!({ "created": null, "note": "a DNS record for this hostname already exists; check it points at the Pages project" })
            };
            Ok(json!({ "hostname": hostname, "pages_project": p, "zone": zone, "dns": dns }))
        }
    }
}

// ---- API: the connection --------------------------------------------------------------

pub async fn get_connection(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    Ok(Json(json!(state.store.cloudflare_connection().await?)))
}

#[derive(Debug, Deserialize)]
pub struct ConnectionUpdate {
    pub account_id: String,
    /// New token; omitted or empty keeps the stored one.
    #[serde(default)]
    pub token: Option<String>,
}

/// Connect Cloudflare: the token is checked against Cloudflare (active, and
/// can see the account) before it is stored.
pub async fn put_connection(
    State(state): State<AppState>,
    caller: Caller,
    Json(update): Json<ConnectionUpdate>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let account_id = update.account_id.trim().to_string();
    if !agentcore_store::valid_account_id(&account_id) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "the account id is the 32-character id shown in the Cloudflare dashboard's URL",
        ));
    }
    let new_token = update
        .token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from);
    let token = match &new_token {
        Some(t) => t.clone(),
        None => state
            .store
            .cloudflare_credentials()
            .await?
            .map(|(_, t)| t)
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "an API token is required to connect Cloudflare",
                )
            })?,
    };
    let mut config = CloudflareConfig {
        account_id,
        account_name: String::new(),
    };
    let cf = state.cloudflare_client(&config, token);
    let rejected = |e: String| ApiError::new(StatusCode::BAD_REQUEST, e);
    cf.verify().await.map_err(rejected)?;
    config.account_name = cf.account_name().await.map_err(|e| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("the token cannot read this account: {e}"),
        )
    })?;
    Ok(Json(json!(
        state
            .store
            .set_cloudflare(&config, new_token.as_deref(), &caller.name)
            .await?
    )))
}

pub async fn delete_connection(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    state.store.delete_cloudflare(&caller.name).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Check the stored connection still works, and what the token can see.
pub async fn test_connection(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let cf = state.cloudflare().await?.ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Cloudflare is not connected",
        )
    })?;
    let bad = |e: String| ApiError::new(StatusCode::BAD_GATEWAY, e);
    cf.verify().await.map_err(bad)?;
    let account = cf.account_name().await.map_err(bad)?;
    // What the token may use, product by product (a missing permission shows
    // as `false`, so the admin knows what to add).
    let probe = |path: String| {
        let cf = cf.clone();
        async move { cf.get(&path).await.is_ok() }
    };
    let (workers, pages, d1, kv, r2, queues, zones) = tokio::join!(
        probe(cf.acct("/workers/scripts")),
        probe(cf.acct("/pages/projects")),
        probe(cf.acct("/d1/database")),
        probe(cf.acct("/storage/kv/namespaces")),
        probe(cf.acct("/r2/buckets")),
        probe(cf.acct("/queues")),
        probe(format!("/zones?account.id={}", cf.account_id())),
    );
    Ok(Json(json!({
        "account": account,
        "access": {
            "workers": workers, "pages": pages, "d1": d1, "kv": kv,
            "r2": r2, "queues": queues, "zones": zones,
        },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_links_name_the_build() {
        assert_eq!(
            build_ref("https://dash.cloudflare.com/0123abcd/pages/view/my-app/5f3e1b2c-aaaa-bbbb"),
            Some(BuildRef::Pages {
                project: "my-app".into(),
                deployment: "5f3e1b2c-aaaa-bbbb".into()
            })
        );
        assert_eq!(
            build_ref(
                "https://dash.cloudflare.com/0123abcd/workers/services/view/api/production/builds/9d2a-11?x=1"
            ),
            Some(BuildRef::WorkersBuild {
                build: "9d2a-11".into()
            })
        );
        assert_eq!(
            build_ref("https://dash.cloudflare.com/0123abcd/pages/view/my-app"),
            None
        );
        assert_eq!(build_ref("https://example.com/a/pages/view/b/c"), None);
        assert_eq!(
            build_ref("https://dash.cloudflare.com/a/pages/view/../c"),
            None
        );
    }

    #[test]
    fn secret_names_and_values() {
        assert!(valid_secret_name("SESSION_SECRET"));
        assert!(valid_secret_name("_x1"));
        assert!(!valid_secret_name("1X"));
        assert!(!valid_secret_name("A-B"));
        assert!(!valid_secret_name(""));
        let (a, b) = (random_secret().unwrap(), random_secret().unwrap());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }

    #[test]
    fn tools_match_the_role_capabilities() {
        let defined: HashSet<String> = all_tool_definitions()
            .iter()
            .map(|d| d["name"].as_str().unwrap().to_string())
            .collect();
        let mut granted = HashSet::new();
        for c in [
            agentcore_roles::Capability::CloudflareRead,
            agentcore_roles::Capability::CloudflareProvision,
            agentcore_roles::Capability::CloudflareSecrets,
            agentcore_roles::Capability::CloudflareDomains,
            agentcore_roles::Capability::CloudflareCi,
        ] {
            granted.extend(tools_for(c).iter().map(|t| t.to_string()));
        }
        assert_eq!(defined, granted);
    }

    type Seen = std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>;

    /// A small Cloudflare API: one account `acc`, one zone `example.com`.
    async fn mock_cloudflare(seen: Seen) -> String {
        use axum::extract::{Path, Query};
        use axum::routing::{get, put};
        use std::collections::HashMap;
        fn ok(result: Value) -> Json<Value> {
            Json(json!({ "success": true, "errors": [], "result": result }))
        }
        fn rec(seen: &Seen, what: &str, body: Value) {
            seen.lock().unwrap().push((what.to_string(), body));
        }
        let (s1, s2, s3, s4) = (seen.clone(), seen.clone(), seen.clone(), seen.clone());
        let app = axum::Router::new()
            .route(
                "/accounts/acc/d1/database",
                get(move || async move {
                    ok(json!([{ "name": "shop-db", "uuid": "existing-uuid" }]))
                })
                .post(move |Json(body): Json<Value>| async move {
                    rec(&s1, "create d1", body);
                    ok(json!({ "uuid": "new-uuid" }))
                }),
            )
            .route(
                "/accounts/acc/workers/scripts/{name}/secrets",
                put(move |Path(name): Path<String>, Json(body): Json<Value>| async move {
                    rec(&s2, &format!("secret {name}"), body);
                    ok(json!({}))
                }),
            )
            .route(
                "/zones",
                get(|| async { ok(json!([{ "id": "z1", "name": "example.com" }])) }),
            )
            .route(
                "/zones/z1/dns_records",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    if q.get("name").map(String::as_str) == Some("taken.example.com") {
                        ok(json!([{ "id": "r1", "content": "192.0.2.1" }]))
                    } else {
                        ok(json!([]))
                    }
                })
                .post(move |Json(body): Json<Value>| async move {
                    rec(&s3, "dns", body.clone());
                    ok(body)
                }),
            )
            .route(
                "/accounts/acc/workers/domains",
                put(move |Json(body): Json<Value>| async move {
                    rec(&s4, "domain", body.clone());
                    ok(json!({ "id": "dom1", "hostname": body["hostname"] }))
                }),
            )
            .route(
                "/accounts/acc/pages/projects/site/deployments/dep1/history/logs",
                get(|| async {
                    ok(json!({ "data": [{ "line": "Installing" }, { "line": "Error: build failed" }] }))
                }),
            )
            .route(
                "/accounts/acc/r2/buckets",
                get(|| async {
                    (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "success": false, "errors": [{ "code": 10000, "message": "Authentication error" }] })),
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn tools_call_cloudflare() {
        let seen: Seen = Default::default();
        let api = mock_cloudflare(seen.clone()).await;
        let cf = Cloudflare::new(reqwest::Client::new(), &api, "tok".into(), "acc".into());

        // Existing resources are reused, new ones created.
        let r = run_tool(
            &cf,
            "cloudflare_create_d1_database",
            &json!({ "name": "shop-db" }),
        )
        .await
        .unwrap();
        assert_eq!(r["id"], "existing-uuid");
        assert_eq!(r["created"], false);
        let r = run_tool(
            &cf,
            "cloudflare_create_d1_database",
            &json!({ "name": "shop-db-preview", "location_hint": "weur" }),
        )
        .await
        .unwrap();
        assert_eq!(r["id"], "new-uuid");

        // A generated secret is set but never returned.
        let r = run_tool(
            &cf,
            "cloudflare_set_secret",
            &json!({ "worker": "api", "name": "SESSION_SECRET", "generate": true }),
        )
        .await
        .unwrap();
        assert_eq!(r["generated"], true);
        let set = seen
            .lock()
            .unwrap()
            .iter()
            .find(|(w, _)| w == "secret api")
            .cloned()
            .unwrap();
        let value = set.1["text"].as_str().unwrap().to_string();
        assert_eq!(value.len(), 64);
        assert!(!r.to_string().contains(&value));
        assert!(
            run_tool(
                &cf,
                "cloudflare_set_secret",
                &json!({ "worker": "api", "name": "X" })
            )
            .await
            .is_err()
        );

        // DNS: existing records are not replaced.
        let err = run_tool(
            &cf,
            "cloudflare_create_dns_record",
            &json!({ "zone": "example.com", "type": "A", "name": "taken.example.com", "content": "192.0.2.9" }),
        )
        .await
        .unwrap_err();
        assert!(err.contains("already exists"), "{err}");

        // A custom domain finds its zone.
        let r = run_tool(
            &cf,
            "cloudflare_add_custom_domain",
            &json!({ "worker": "api", "hostname": "api.example.com" }),
        )
        .await
        .unwrap();
        assert_eq!(r["zone"], "example.com");
        let domain = seen
            .lock()
            .unwrap()
            .iter()
            .find(|(w, _)| w == "domain")
            .cloned()
            .unwrap();
        assert_eq!(domain.1["zone_id"], "z1");
        assert_eq!(domain.1["service"], "api");
        assert!(
            run_tool(
                &cf,
                "cloudflare_add_custom_domain",
                &json!({ "worker": "api", "hostname": "x.other.org" })
            )
            .await
            .unwrap_err()
            .contains("no zone")
        );

        // Build logs and Cloudflare's own error messages.
        let log = cf.pages_log("site", "dep1").await.unwrap();
        assert_eq!(log, "Installing\nError: build failed");
        let err = run_tool(
            &cf,
            "cloudflare_create_r2_bucket",
            &json!({ "name": "files" }),
        )
        .await
        .unwrap_err();
        assert!(err.contains("Authentication error (code 10000)"), "{err}");
    }
}
