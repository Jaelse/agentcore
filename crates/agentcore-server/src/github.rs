//! Minimal GitHub client (REST + GraphQL) used by agentcore on behalf of
//! agents. The token never leaves agentcore.

use agentcore_store::{BoardConfig, GitHubConfig};
use reqwest::Method;
use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
pub enum GhError {
    #[error("GitHub request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("GitHub returned {status}: {message}")]
    Api { status: u16, message: String },
    #[error("GitHub GraphQL error: {0}")]
    GraphQl(String),
    #[error("{0}")]
    NotFound(String),
}

#[derive(Clone)]
pub struct GitHub {
    http: reqwest::Client,
    api: String,
    graphql: String,
    token: String,
}

/// A board as agentcore shows it: columns and cards.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Board {
    pub id: String,
    pub title: String,
    pub url: Option<String>,
    pub status_field_id: Option<String>,
    pub columns: Vec<String>,
    pub current_iteration: Option<String>,
    pub items: Vec<BoardItem>,
    #[serde(skip)]
    options: Vec<(String, String)>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BoardItem {
    pub id: String,
    pub status: Option<String>,
    pub iteration: Option<String>,
    pub content_type: String,
    pub number: Option<u64>,
    pub title: String,
    pub url: Option<String>,
    pub state: Option<String>,
    pub repository: Option<String>,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub milestone: Option<String>,
}

/// A pull request to open or update.
#[derive(Debug, Clone, Copy)]
pub struct PullRequest<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
    pub branch: &'a str,
    pub base: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub draft: bool,
}

/// An issue with its discussion, as given to agents and prompts.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    pub state: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub milestone: Option<String>,
    pub comments: Vec<Comment>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Comment {
    pub author: String,
    pub body: String,
    pub created_at: String,
}

fn s(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn names(v: &Value, key: &str) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get(key).and_then(Value::as_str).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Compact issue summary (keeps agent context small).
fn issue_summary(v: &Value) -> Value {
    json!({
        "number": v["number"],
        "title": v["title"],
        "state": v["state"],
        "url": v["html_url"],
        "labels": names(&v["labels"], "name"),
        "assignees": names(&v["assignees"], "login"),
        "milestone": v["milestone"]["title"],
        "comments": v["comments"],
        "updated_at": v["updated_at"],
    })
}

fn milestone_summary(v: &Value) -> Value {
    json!({
        "number": v["number"], "title": v["title"], "state": v["state"],
        "description": v["description"], "due_on": v["due_on"],
        "open_issues": v["open_issues"], "closed_issues": v["closed_issues"], "url": v["html_url"],
    })
}

impl GitHub {
    pub fn new(http: reqwest::Client, config: &GitHubConfig, token: String) -> Self {
        let api = config.api_url.trim_end_matches('/').to_string();
        let graphql = match api.strip_suffix("/api/v3") {
            Some(host) => format!("{host}/api/graphql"),
            None => format!("{api}/graphql"),
        };
        Self {
            http,
            api,
            graphql,
            token,
        }
    }

    async fn rest(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, GhError> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.api))
            .bearer_auth(&self.token)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .header("user-agent", "agentcore");
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() {
            let message = value["message"].as_str().map(String::from).unwrap_or(text);
            return Err(GhError::Api {
                status: status.as_u16(),
                message,
            });
        }
        Ok(value)
    }

    async fn graphql(&self, query: &str, variables: Value) -> Result<Value, GhError> {
        let resp = self
            .http
            .post(&self.graphql)
            .bearer_auth(&self.token)
            .header("user-agent", "agentcore")
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await?;
        let status = resp.status();
        let value: Value = resp.json().await.unwrap_or(Value::Null);
        if let Some(errors) = value
            .get("errors")
            .and_then(Value::as_array)
            .filter(|e| !e.is_empty())
        {
            let msg = errors
                .iter()
                .filter_map(|e| e["message"].as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GhError::GraphQl(msg));
        }
        if !status.is_success() {
            return Err(GhError::Api {
                status: status.as_u16(),
                message: value.to_string(),
            });
        }
        Ok(value["data"].clone())
    }

    /// Login of the token's user (connection test).
    pub async fn viewer(&self) -> Result<String, GhError> {
        Ok(s(&self.rest(Method::GET, "/user", None).await?, "login"))
    }

    // ---- issues ------------------------------------------------------------------

    pub async fn list_issues(
        &self,
        owner: &str,
        repo: &str,
        args: &Value,
    ) -> Result<Value, GhError> {
        let mut query = vec![
            (
                "state",
                args["state"].as_str().unwrap_or("open").to_string(),
            ),
            (
                "per_page",
                args["limit"].as_u64().unwrap_or(30).min(100).to_string(),
            ),
        ];
        if let Some(labels) = args["labels"].as_str() {
            query.push(("labels", labels.to_string()));
        }
        if let Some(m) = args["milestone"]
            .as_str()
            .or(args["milestone"].as_u64().map(|_| ""))
        {
            let m = if m.is_empty() {
                args["milestone"].to_string()
            } else {
                m.to_string()
            };
            query.push(("milestone", m));
        }
        let qs: Vec<String> = query
            .iter()
            .map(|(k, v)| format!("{k}={}", urlencode(v)))
            .collect();
        let list = self
            .rest(
                Method::GET,
                &format!("/repos/{owner}/{repo}/issues?{}", qs.join("&")),
                None,
            )
            .await?;
        Ok(Value::Array(
            list.as_array()
                .into_iter()
                .flatten()
                .filter(|i| i.get("pull_request").is_none())
                .map(issue_summary)
                .collect(),
        ))
    }

    pub async fn get_issue(&self, owner: &str, repo: &str, number: u64) -> Result<Issue, GhError> {
        let v = self
            .rest(
                Method::GET,
                &format!("/repos/{owner}/{repo}/issues/{number}"),
                None,
            )
            .await?;
        let comments = self
            .rest(
                Method::GET,
                &format!("/repos/{owner}/{repo}/issues/{number}/comments?per_page=50"),
                None,
            )
            .await?;
        Ok(Issue {
            number,
            title: s(&v, "title"),
            body: s(&v, "body"),
            url: s(&v, "html_url"),
            state: s(&v, "state"),
            labels: names(&v["labels"], "name"),
            assignees: names(&v["assignees"], "login"),
            milestone: v["milestone"]["title"].as_str().map(String::from),
            comments: comments
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| Comment {
                    author: c["user"]["login"].as_str().unwrap_or("?").into(),
                    body: s(c, "body"),
                    created_at: s(c, "created_at"),
                })
                .collect(),
        })
    }

    pub async fn comment_on_issue(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<String, GhError> {
        let v = self
            .rest(
                Method::POST,
                &format!("/repos/{owner}/{repo}/issues/{number}/comments"),
                Some(json!({ "body": body })),
            )
            .await?;
        Ok(s(&v, "html_url"))
    }

    pub async fn create_issue(
        &self,
        owner: &str,
        repo: &str,
        fields: Value,
    ) -> Result<Value, GhError> {
        let v = self
            .rest(
                Method::POST,
                &format!("/repos/{owner}/{repo}/issues"),
                Some(fields),
            )
            .await?;
        Ok(issue_summary(&v))
    }

    pub async fn update_issue(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        fields: Value,
    ) -> Result<Value, GhError> {
        let v = self
            .rest(
                Method::PATCH,
                &format!("/repos/{owner}/{repo}/issues/{number}"),
                Some(fields),
            )
            .await?;
        Ok(issue_summary(&v))
    }

    // ---- milestones ------------------------------------------------------------

    pub async fn list_milestones(
        &self,
        owner: &str,
        repo: &str,
        state: &str,
    ) -> Result<Value, GhError> {
        let v = self
            .rest(
                Method::GET,
                &format!(
                    "/repos/{owner}/{repo}/milestones?state={}&per_page=50",
                    urlencode(state)
                ),
                None,
            )
            .await?;
        Ok(Value::Array(
            v.as_array()
                .into_iter()
                .flatten()
                .map(milestone_summary)
                .collect(),
        ))
    }

    pub async fn create_milestone(
        &self,
        owner: &str,
        repo: &str,
        fields: Value,
    ) -> Result<Value, GhError> {
        Ok(milestone_summary(
            &self
                .rest(
                    Method::POST,
                    &format!("/repos/{owner}/{repo}/milestones"),
                    Some(fields),
                )
                .await?,
        ))
    }

    pub async fn update_milestone(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        fields: Value,
    ) -> Result<Value, GhError> {
        Ok(milestone_summary(
            &self
                .rest(
                    Method::PATCH,
                    &format!("/repos/{owner}/{repo}/milestones/{number}"),
                    Some(fields),
                )
                .await?,
        ))
    }

    // ---- pull requests -----------------------------------------------------------

    /// Open a pull request, or update the open one for this branch.
    pub async fn upsert_pull_request(
        &self,
        pr: &PullRequest<'_>,
    ) -> Result<(u64, String), GhError> {
        let PullRequest {
            owner,
            repo,
            branch,
            base,
            title,
            body,
            draft,
        } = *pr;
        let existing = self
            .rest(
                Method::GET,
                &format!(
                    "/repos/{owner}/{repo}/pulls?state=open&head={}",
                    urlencode(&format!("{owner}:{branch}"))
                ),
                None,
            )
            .await?;
        let pr = match existing.as_array().and_then(|a| a.first()) {
            Some(pr) => {
                let number = pr["number"].as_u64().unwrap_or_default();
                self.rest(
                    Method::PATCH,
                    &format!("/repos/{owner}/{repo}/pulls/{number}"),
                    Some(json!({ "title": title, "body": body })),
                )
                .await?
            }
            None => {
                self.rest(
                    Method::POST,
                    &format!("/repos/{owner}/{repo}/pulls"),
                    Some(json!({ "title": title, "body": body, "head": branch, "base": base, "draft": draft })),
                )
                .await?
            }
        };
        Ok((
            pr["number"].as_u64().unwrap_or_default(),
            s(&pr, "html_url"),
        ))
    }

    // ---- discussions -------------------------------------------------------------

    pub async fn list_discussions(
        &self,
        owner: &str,
        repo: &str,
        limit: u64,
    ) -> Result<Value, GhError> {
        let data = self
            .graphql(
                "query($owner:String!,$name:String!,$n:Int!){repository(owner:$owner,name:$name){discussions(first:$n,orderBy:{field:UPDATED_AT,direction:DESC}){nodes{number title url updatedAt category{name} author{login} comments{totalCount}}}}}",
                json!({ "owner": owner, "name": repo, "n": limit.clamp(1, 50) }),
            )
            .await?;
        Ok(data["repository"]["discussions"]["nodes"].clone())
    }

    pub async fn get_discussion(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Value, GhError> {
        let data = self
            .graphql(
                "query($owner:String!,$name:String!,$n:Int!){repository(owner:$owner,name:$name){discussion(number:$n){id number title body url category{name} author{login} comments(first:50){nodes{author{login} body createdAt}}}}}",
                json!({ "owner": owner, "name": repo, "n": number }),
            )
            .await?;
        let d = data["repository"]["discussion"].clone();
        if d.is_null() {
            return Err(GhError::NotFound(format!("discussion #{number} not found")));
        }
        Ok(d)
    }

    pub async fn comment_on_discussion(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<String, GhError> {
        let d = self.get_discussion(owner, repo, number).await?;
        let data = self
            .graphql(
                "mutation($id:ID!,$body:String!){addDiscussionComment(input:{discussionId:$id,body:$body}){comment{url}}}",
                json!({ "id": d["id"], "body": body }),
            )
            .await?;
        Ok(data["addDiscussionComment"]["comment"]["url"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    // ---- project boards (GitHub Projects v2) --------------------------------------

    pub async fn board(&self, cfg: &BoardConfig) -> Result<Board, GhError> {
        const QUERY: &str = r#"
query($owner:String!,$number:Int!,$cursor:String){
  repositoryOwner(login:$owner){
    ... on Organization { projectV2(number:$number){ ...P } }
    ... on User { projectV2(number:$number){ ...P } }
  }
}
fragment P on ProjectV2 {
  id title url
  fields(first:50){ nodes{
    ... on ProjectV2SingleSelectField { id name options{ id name } }
    ... on ProjectV2IterationField { id name configuration{ iterations{ id title startDate duration } } }
  } }
  items(first:100, after:$cursor){
    pageInfo{ hasNextPage endCursor }
    nodes{ id
      fieldValues(first:20){ nodes{
        ... on ProjectV2ItemFieldSingleSelectValue { name field{ ... on ProjectV2SingleSelectField { name } } }
        ... on ProjectV2ItemFieldIterationValue { title field{ ... on ProjectV2IterationField { name } } }
      } }
      content{
        __typename
        ... on Issue { number title url state repository{ nameWithOwner } labels(first:10){ nodes{ name } } assignees(first:5){ nodes{ login } } milestone{ title } }
        ... on PullRequest { number title url state repository{ nameWithOwner } }
        ... on DraftIssue { title }
      }
    }
  }
}"#;
        let mut cursor: Option<String> = None;
        let mut board: Option<Board> = None;
        for _ in 0..10 {
            let data = self
                .graphql(
                    QUERY,
                    json!({ "owner": cfg.owner, "number": cfg.number, "cursor": cursor }),
                )
                .await?;
            let p = &data["repositoryOwner"]["projectV2"];
            if p.is_null() {
                return Err(GhError::NotFound(format!(
                    "project board {}/{} not found (or the token cannot read it)",
                    cfg.owner, cfg.number
                )));
            }
            let b = board.get_or_insert_with(|| {
                let fields = p["fields"]["nodes"].as_array().cloned().unwrap_or_default();
                let status = fields
                    .iter()
                    .find(|f| f["name"] == cfg.status_field.as_str() && f.get("options").is_some());
                let options: Vec<(String, String)> = status
                    .and_then(|f| f["options"].as_array())
                    .into_iter()
                    .flatten()
                    .map(|o| (s(o, "id"), s(o, "name")))
                    .collect();
                let current_iteration = cfg.iteration_field.as_ref().and_then(|name| {
                    let field = fields.iter().find(|f| f["name"] == name.as_str())?;
                    current_iteration(&field["configuration"]["iterations"])
                });
                Board {
                    id: s(p, "id"),
                    title: s(p, "title"),
                    url: p["url"].as_str().map(String::from),
                    status_field_id: status.map(|f| s(f, "id")),
                    columns: options.iter().map(|(_, n)| n.clone()).collect(),
                    current_iteration,
                    items: Vec::new(),
                    options,
                }
            });
            for node in p["items"]["nodes"].as_array().into_iter().flatten() {
                let values = node["fieldValues"]["nodes"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let field_value = |field: &str, key: &str| {
                    values
                        .iter()
                        .find(|v| v["field"]["name"] == field)
                        .and_then(|v| v[key].as_str().map(String::from))
                };
                let c = &node["content"];
                b.items.push(BoardItem {
                    id: s(node, "id"),
                    status: field_value(&cfg.status_field, "name"),
                    iteration: cfg
                        .iteration_field
                        .as_deref()
                        .and_then(|f| field_value(f, "title")),
                    content_type: s(c, "__typename"),
                    number: c["number"].as_u64(),
                    title: s(c, "title"),
                    url: c["url"].as_str().map(String::from),
                    state: c["state"].as_str().map(String::from),
                    repository: c["repository"]["nameWithOwner"].as_str().map(String::from),
                    labels: names(&c["labels"]["nodes"], "name"),
                    assignees: names(&c["assignees"]["nodes"], "login"),
                    milestone: c["milestone"]["title"].as_str().map(String::from),
                });
            }
            let page = &p["items"]["pageInfo"];
            if page["hasNextPage"].as_bool() != Some(true) {
                break;
            }
            cursor = page["endCursor"].as_str().map(String::from);
        }
        board.ok_or_else(|| GhError::NotFound("project board not found".into()))
    }

    /// Move the card of `repository#issue` to the column `status`. Returns
    /// false if the issue is not on the board.
    pub async fn move_issue(
        &self,
        cfg: &BoardConfig,
        repository: &str,
        issue: u64,
        status: &str,
    ) -> Result<bool, GhError> {
        let board = self.board(cfg).await?;
        let Some(item) = board
            .items
            .iter()
            .find(|i| i.number == Some(issue) && i.repository.as_deref() == Some(repository))
        else {
            return Ok(false);
        };
        self.set_status(&board, &item.id, status).await?;
        Ok(true)
    }

    async fn set_status(&self, board: &Board, item_id: &str, status: &str) -> Result<(), GhError> {
        let field = board
            .status_field_id
            .as_ref()
            .ok_or_else(|| GhError::NotFound("the board has no status field".into()))?;
        let option = board
            .options
            .iter()
            .find(|(_, name)| name.eq_ignore_ascii_case(status))
            .ok_or_else(|| {
                GhError::NotFound(format!(
                    "no column named `{status}`; columns: {}",
                    board.columns.join(", ")
                ))
            })?;
        self.graphql(
            "mutation($p:ID!,$i:ID!,$f:ID!,$o:String!){updateProjectV2ItemFieldValue(input:{projectId:$p,itemId:$i,fieldId:$f,value:{singleSelectOptionId:$o}}){projectV2Item{id}}}",
            json!({ "p": board.id, "i": item_id, "f": field, "o": option.0 }),
        )
        .await?;
        Ok(())
    }
}

/// The iteration whose date range contains today.
fn current_iteration(iterations: &Value) -> Option<String> {
    let today = chrono::Utc::now().date_naive();
    iterations.as_array()?.iter().find_map(|it| {
        let start =
            chrono::NaiveDate::parse_from_str(it["startDate"].as_str()?, "%Y-%m-%d").ok()?;
        let end = start + chrono::Duration::days(it["duration"].as_i64()?);
        (start <= today && today < end).then(|| s(it, "title"))
    })
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphql_url_for_enterprise() {
        let http = reqwest::Client::new();
        let mut cfg = GitHubConfig::default();
        assert_eq!(
            GitHub::new(http.clone(), &cfg, "t".into()).graphql,
            "https://api.github.com/graphql"
        );
        cfg.api_url = "https://ghe.acme.com/api/v3/".into();
        assert_eq!(
            GitHub::new(http, &cfg, "t".into()).graphql,
            "https://ghe.acme.com/api/graphql"
        );
    }

    #[test]
    fn finds_current_iteration() {
        let today = chrono::Utc::now().date_naive();
        let its = json!([
            { "title": "Sprint 1", "startDate": (today - chrono::Duration::days(20)).to_string(), "duration": 14 },
            { "title": "Sprint 2", "startDate": (today - chrono::Duration::days(6)).to_string(), "duration": 14 },
        ]);
        assert_eq!(current_iteration(&its).as_deref(), Some("Sprint 2"));
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(urlencode("acme:agent/1-fix"), "acme%3Aagent%2F1-fix");
    }
}
