//! Releases and distribution: shipping what agents build.
//!
//! * Agents of a product's departments **propose** releases
//!   (`release_propose`): version, commit, notes, artifacts with digests,
//!   SBOM, checks, and their own risk assessment.
//! * The server scores the **risk** from facts (the version change, the size
//!   of the change from GitHub, checks, the product's track record, timing)
//!   and **adapts the rollout**: riskier releases start smaller, soak longer
//!   and need one more approval.
//! * **People approve** the revision they read: distinct people, never the
//!   proposer. Before the last approval the server downloads every artifact
//!   from an allowed host and checks its SHA-256.
//! * The rollout goes **stage by stage**. On every heartbeat the **health
//!   gates** are read (crash-free and install-success rates from update
//!   clients, or any business data source): a failing gate **rolls the
//!   release back** on its own; missing data holds it; a change freeze holds
//!   it. Agents can halt a rollout, never start or advance one.
//! * Update clients get a **signed manifest** (see `distribution`); every
//!   step is announced to the UI, the proposing agent and a signed webhook.

use std::time::Duration;

use agentcore_core::releases::{
    self, Bump, ChangeSize, CheckOutcome, GateSource, GateStatus, RiskInput, RiskLevel,
};
use agentcore_core::{
    Artifact, DepartmentId, GateResult, OrgAgentId, Product, Release, ReleaseStatus, Version,
};
use agentcore_runtime::ToolHandler;
use agentcore_store::{NewRelease, ProductInput, ProductUpdate, ReleaseContent, StoreError};
use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::Utc;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::AppState;
use crate::auth::{Caller, hash_token};
use crate::error::ApiError;
use crate::org::{self, Org, Recipient, Sender, def, notify, text_arg};
use crate::signing;

type ApiResult<T> = Result<T, ApiError>;

/// Largest artifact the server downloads to verify.
pub const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_SBOM_BYTES: usize = 2 * 1024 * 1024;
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(10);
/// Who acts when the server moves a rollout on its own.
const SYSTEM: &str = "rollout";

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, msg)
}

fn conflict(msg: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, msg)
}

// ---- proposing ----------------------------------------------------------------------

/// What a proposal (or a revision) says.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProposalInput {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    #[serde(default)]
    pub sbom: Option<Value>,
    #[serde(default)]
    pub checks: Vec<CheckOutcome>,
    #[serde(default)]
    pub changes: ChangeSize,
    /// The proposer's own rating.
    #[serde(default)]
    pub risk: Option<RiskLevel>,
    /// The proposer's own risk assessment, in words.
    #[serde(default)]
    pub assessment: String,
}

fn clean_commit(commit: Option<String>) -> Result<Option<String>, String> {
    let Some(c) = commit.map(|c| c.trim().to_ascii_lowercase()) else {
        return Ok(None);
    };
    if c.is_empty() {
        return Ok(None);
    }
    if !(7..=64).contains(&c.len()) || !c.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("`commit` is a git commit hash (7 to 64 hex digits)".into());
    }
    Ok(Some(c))
}

/// Releases of a product that used their version (all but rejected ones),
/// other than `except`.
async fn used_releases(
    state: &AppState,
    product: Uuid,
    except: Option<Uuid>,
) -> Result<Vec<Release>, StoreError> {
    Ok(state
        .store
        .list_releases(Some(product), None, None, 500)
        .await?
        .into_iter()
        .filter(|r| r.status != ReleaseStatus::Rejected && Some(r.id) != except)
        .collect())
}

/// Change size from GitHub, when the product's repository is connected.
async fn github_changes(
    state: &AppState,
    product: &Product,
    base: Option<&str>,
    head: Option<&str>,
) -> Option<ChangeSize> {
    let (repo, base, head) = (product.repository.as_deref()?, base?, head?);
    let (owner, name) = repo.split_once('/')?;
    let (gh, _, _) = state.github().await.ok()??;
    match tokio::time::timeout(Duration::from_secs(20), gh.compare(owner, name, base, head)).await {
        Ok(Ok(size)) => Some(size),
        Ok(Err(err)) => {
            tracing::debug!(error = %err, "could not compare commits on GitHub");
            None
        }
        Err(_) => None,
    }
}

/// Check a proposal and turn it into a release's content: normalised
/// fields, the risk and the rollout plan for that risk.
async fn content(
    state: &AppState,
    product: &Product,
    input: ProposalInput,
    except: Option<Uuid>,
) -> ApiResult<(Version, ReleaseContent)> {
    let version: Version = input.version.parse().map_err(bad)?;
    let commit = clean_commit(input.commit).map_err(bad)?;
    let mut artifacts = input.artifacts;
    releases::check_artifacts(&mut artifacts, &product.artifact_hosts).map_err(bad)?;
    if let Some(sbom) = &input.sbom {
        if !sbom.is_object() {
            return Err(bad(
                "`sbom` is a JSON document (CycloneDX or SPDX), not a string",
            ));
        }
        if serde_json::to_vec(sbom).map_or(0, |b| b.len()) > MAX_SBOM_BYTES {
            return Err(bad("the SBOM is larger than 2 MiB"));
        }
    }
    for c in &input.checks {
        if c.name.trim().is_empty() || c.name.len() > 100 || c.detail.len() > 2000 {
            return Err(bad("every check has a name (and a short detail)"));
        }
    }
    let used = used_releases(state, product.id, except).await?;
    let previous = used
        .iter()
        .filter_map(|r| r.parsed_version().map(|v| (v, r)))
        .filter(|(v, _)| *v < version)
        .max_by(|a, b| a.0.cmp(&b.0));
    let changes = match github_changes(
        state,
        product,
        previous.as_ref().and_then(|(_, r)| r.commit.as_deref()),
        commit.as_deref(),
    )
    .await
    {
        Some(from_github) => from_github,
        None => input.changes,
    };
    let mut finished: Vec<&Release> = used
        .iter()
        .filter(|r| {
            matches!(
                r.status,
                ReleaseStatus::Released | ReleaseStatus::RolledBack
            )
        })
        .collect();
    finished.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    finished.truncate(10);
    let last_release = used.iter().filter_map(|r| r.released_at).max();
    let risk = releases::assess(&RiskInput {
        bump: Bump::between(previous.as_ref().map(|(v, _)| v), &version),
        changes: &changes,
        checks: &input.checks,
        has_sbom: input.sbom.is_some(),
        gates: product.gates.len(),
        recent_finished: finished.len() as u32,
        recent_rollbacks: finished
            .iter()
            .filter(|r| r.status == ReleaseStatus::RolledBack)
            .count() as u32,
        days_since_last: last_release.map(|at| (Utc::now() - at).num_days()),
        at: Utc::now(),
        declared: input.risk,
        assessment: input.assessment.chars().take(5000).collect(),
    });
    let plan = releases::adapt_plan(&product.plan, risk.level);
    let approvals_required =
        (product.approvals_required + releases::extra_approvals(risk.level)).min(5);
    Ok((
        version,
        ReleaseContent {
            commit,
            notes: input.notes,
            artifacts,
            sbom: input.sbom,
            checks: input.checks,
            changes,
            risk,
            plan,
            approvals_required,
        },
    ))
}

/// Who proposes.
pub struct Proposer {
    pub label: String,
    pub department: Option<DepartmentId>,
    pub agent: Option<OrgAgentId>,
}

pub async fn propose(
    state: &AppState,
    product: &Product,
    input: ProposalInput,
    by: Proposer,
) -> ApiResult<Release> {
    if !product.enabled {
        return Err(conflict(format!(
            "product `{}` is turned off",
            product.name
        )));
    }
    let (version, content) = content(state, product, input, None).await?;
    let release = state
        .store
        .create_release(NewRelease {
            product_id: product.id,
            version: version.to_string(),
            content,
            department_id: by.department,
            agent_id: by.agent,
            proposed_by: by.label,
        })
        .await?;
    notify(&state.store, json!({ "kind": "releases" })).await;
    record(state, &release, "release_proposed").await;
    Ok(release)
}

// ---- approving and starting ---------------------------------------------------------

/// Download an artifact (only from allowed hosts, redirects included) and
/// check its size and SHA-256.
pub async fn verify_artifact(product: &Product, artifact: &Artifact) -> Result<(), String> {
    let hosts = product.artifact_hosts.clone();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            let allowed = releases::url_host(attempt.url().as_str())
                .is_some_and(|h| releases::host_allowed(&h, &hosts));
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if allowed {
                attempt.follow()
            } else {
                attempt.error("a redirect to a host that is not allowed")
            }
        }))
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let host = releases::url_host(&artifact.url).unwrap_or_default();
    if !releases::host_allowed(&host, &product.artifact_hosts) {
        return Err(format!("`{host}` is not an allowed artifact host"));
    }
    let response = client
        .get(&artifact.url)
        .send()
        .await
        .map_err(|e| format!("could not download it: {}", without_url(&e)))?;
    if !response.status().is_success() {
        return Err(format!("the download answered {}", response.status()));
    }
    let limit = artifact
        .size
        .unwrap_or(MAX_ARTIFACT_BYTES)
        .min(MAX_ARTIFACT_BYTES);
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("the download broke off: {}", without_url(&e)))?;
        size += chunk.len() as u64;
        if size > limit {
            return Err(format!("it is larger than {limit} bytes"));
        }
        hasher.update(&chunk);
    }
    if let Some(expected) = artifact.size
        && expected != size
    {
        return Err(format!("it has {size} bytes, not {expected}"));
    }
    let digest = hex::encode(hasher.finalize());
    if digest != artifact.sha256 {
        return Err(format!(
            "its SHA-256 is {digest}, not {} (the file is not what was proposed)",
            artifact.sha256
        ));
    }
    Ok(())
}

fn without_url(e: &reqwest::Error) -> String {
    let mut text = e.to_string();
    if let Some(url) = e.url() {
        text = text.replace(url.as_str(), "the artifact");
    }
    text
}

/// The in-toto statement for a release: what its artifacts are, where they
/// came from, who (agent and people) proposed and approved them.
pub fn statement(product: &Product, release: &Release, approvers: &[String]) -> Value {
    json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": release.artifacts.iter().map(|a| json!({
            "name": a.name,
            "digest": { "sha256": a.sha256 },
            "annotations": { "platform": a.platform, "url": a.url },
        })).collect::<Vec<_>>(),
        "predicateType": "https://agentcore.dev/attestations/release/v1",
        "predicate": {
            "product": product.name,
            "version": release.version,
            "release_id": release.id,
            "source": {
                "repository": product.repository,
                "commit": release.commit,
            },
            "proposed_by": release.proposed_by,
            "ai_generated": release.agent_id.is_some(),
            "marker": agentcore_core::AI_GENERATED_MARKER,
            "approved_by": approvers,
            "risk": { "score": release.risk.score, "level": release.risk.level },
            "checks": release.checks,
            "plan": release.plan,
            "notes_sha256": hex::encode(Sha256::digest(release.notes.as_bytes())),
            "sbom_sha256": release.sbom.as_ref().map(|s| {
                hex::encode(Sha256::digest(serde_json::to_vec(s).unwrap_or_default()))
            }),
            "artifacts_verified": release.artifacts.iter().all(|a| a.verified_at.is_some()),
            "issued_at": Utc::now(),
        },
    })
}

/// A person approves a revision; the last approval needed starts the rollout.
pub async fn approve(state: &AppState, id: Uuid, revision: u32, by: &str) -> ApiResult<Value> {
    let store = &state.store;
    let release = store.get_release(id).await?;
    let product = store.get_product(release.product_id).await?;
    // Cheap checks first: no download for an approval that cannot count.
    if release.proposed_by == by {
        return Err(bad("nobody approves a release they proposed (four eyes)"));
    }
    if release.approvals.iter().any(|a| a.by == by) {
        return Err(conflict(format!("{by} already approved this release")));
    }
    if release.status == ReleaseStatus::Proposed && release.revision == revision {
        // Verify before the approval counts: nobody approves bytes nobody checked.
        let mut artifacts = release.artifacts.clone();
        let mut changed = false;
        for a in artifacts.iter_mut().filter(|a| a.verified_at.is_none()) {
            verify_artifact(&product, a)
                .await
                .map_err(|e| conflict(format!("artifact `{}`: {e}", a.name)))?;
            a.verified_at = Some(Utc::now());
            changed = true;
        }
        if changed {
            store
                .set_release_artifacts(id, revision, &artifacts)
                .await?;
        }
    }
    let release = store.approve_release(id, revision, by).await?;
    notify(store, json!({ "kind": "releases" })).await;
    if (release.approvals.len() as u32) < release.approvals_required {
        let text = format!(
            "{by} approved {} {} ({} of {} approvals).",
            product.name,
            release.version,
            release.approvals.len(),
            release.approvals_required
        );
        tell(state, &release, &text).await;
        return Ok(json!({ "release": release, "started": false }));
    }
    match start(state, &product, release, by).await {
        Ok(started) => Ok(json!({ "release": started, "started": true })),
        Err(err) => Ok(json!({
            "release": store.get_release(id).await?,
            "started": false,
            "note": format!("approved, but the rollout did not start: {}", err.message()),
        })),
    }
}

/// Start the rollout of a fully approved release.
pub async fn start(
    state: &AppState,
    product: &Product,
    release: Release,
    by: &str,
) -> ApiResult<Release> {
    let approvers: Vec<String> = release.approvals.iter().map(|a| a.by.clone()).collect();
    let attestation = signing::sign_json(
        &state.store,
        signing::IN_TOTO_TYPE,
        &statement(product, &release, &approvers),
    )
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let started = state
        .store
        .start_release(release.id, release.revision, &attestation, by)
        .await?;
    let stage = started.current_stage().cloned();
    announce(
        state,
        product,
        &started,
        "started",
        &format!(
            "The rollout of {} {} started (approved by {}): stage {} at {}% of units.",
            product.name,
            started.version,
            approvers.join(", "),
            stage.as_ref().map_or("?", |s| s.name.as_str()),
            stage.as_ref().map_or(0.0, |s| s.percent)
        ),
    )
    .await;
    Ok(started)
}

// ---- the rollout --------------------------------------------------------------------

/// Read every health gate of a product for a release.
pub async fn read_gates(state: &AppState, product: &Product, release: &Release) -> Vec<GateResult> {
    if product.gates.is_empty() {
        return Vec::new();
    }
    let stats = state.store.report_stats(release.id).await.ok();
    let sources = state.store.list_data_sources().await.unwrap_or_default();
    let since = release
        .started_at
        .unwrap_or(release.created_at)
        .to_rfc3339();
    let fill = |s: &str| {
        s.replace("{version}", &release.version)
            .replace("{release}", &release.id.to_string())
            .replace("{since}", &since)
    };
    let mut out = Vec::new();
    for gate in &product.gates {
        let result = match &gate.metric {
            GateSource::CrashFree => match &stats {
                Some(s) => match s.crash_free() {
                    Some(rate) => GateResult::judge(gate, rate, Some(s.units as u64)),
                    None => GateResult::waiting(gate, 0),
                },
                None => GateResult::error(gate, "could not read client reports".into()),
            },
            GateSource::InstallSuccess => match &stats {
                Some(s) => {
                    let attempts = (s.installed + s.install_failed) as u64;
                    match s.install_success() {
                        Some(rate) => GateResult::judge(gate, rate, Some(attempts)),
                        None => GateResult::waiting(gate, 0),
                    }
                }
                None => GateResult::error(gate, "could not read client reports".into()),
            },
            GateSource::Data {
                source,
                sql,
                path,
                field,
            } => match sources.iter().find(|s| &s.name == source && s.enabled) {
                None => GateResult::error(gate, format!("no enabled data source `{source}`")),
                Some(s) => {
                    let args = match (sql, path) {
                        (Some(sql), _) => json!({ "sql": fill(sql), "limit": 1 }),
                        (_, Some(path)) => json!({ "path": fill(path).replace('+', "%2B") }),
                        _ => json!({}),
                    };
                    match crate::insights::query_source(&state.store, s, &args).await {
                        Ok(answer) => match releases::extract_number(&answer, field) {
                            Ok(value) => GateResult::judge(gate, value, None),
                            Err(e) => GateResult::error(gate, e),
                        },
                        Err(e) => GateResult::error(gate, e),
                    }
                }
            },
        };
        out.push(result);
    }
    out
}

/// Move every rollout on (each node, on its heartbeat; every step is
/// claimed by one node).
pub async fn evaluate(state: &AppState) -> anyhow::Result<()> {
    for release in state.store.rolling_releases().await? {
        if let Err(err) = step(state, release).await {
            tracing::warn!(error = %err, "checking a rollout failed");
        }
    }
    Ok(())
}

async fn step(state: &AppState, release: Release) -> anyhow::Result<()> {
    let store = &state.store;
    let product = store.get_product(release.product_id).await?;
    let results = read_gates(state, &product, &release).await;
    store.record_gates(release.id, &results).await?;
    if let Some(failed) = results.iter().find(|r| r.status == GateStatus::Fail) {
        let reason = format!("health gate `{}` failed: {}", failed.name, failed.detail);
        match store.roll_back_release(release.id, &reason, SYSTEM).await {
            Ok(rolled_back) => {
                tracing::warn!(release = %release.id, %reason, "rollout rolled back");
                announce(
                    state,
                    &product,
                    &rolled_back,
                    "rolled_back",
                    &format!(
                        "{} {} was rolled back automatically: {reason}. Units are offered the \
                         previous version again. Find the cause before proposing a fix (a new \
                         version).",
                        product.name, release.version
                    ),
                )
                .await;
            }
            Err(StoreError::Conflict(_)) => {}
            Err(e) => return Err(e.into()),
        }
        return Ok(());
    }
    if product.frozen.is_some() {
        return Ok(());
    }
    // Gates without enough data (or unreadable) hold the rollout.
    if results.iter().any(|r| r.status != GateStatus::Pass) {
        notify(store, json!({ "kind": "releases" })).await;
        return Ok(());
    }
    let Some(stage) = release.current_stage() else {
        return Ok(());
    };
    let soaked = release.stage_started_at.is_none_or(|at| {
        Utc::now() - at >= chrono::Duration::minutes(i64::from(stage.soak_minutes))
    });
    if !soaked {
        return Ok(());
    }
    move_on(state, &product, &release, SYSTEM).await?;
    Ok(())
}

/// The next stage, or done after the last one. `false`: another node (or
/// person) moved it first.
async fn move_on(
    state: &AppState,
    product: &Product,
    release: &Release,
    by: &str,
) -> Result<bool, StoreError> {
    let store = &state.store;
    let last = release.stage as usize + 1 >= release.plan.len();
    if last {
        if !store
            .complete_release(release.id, release.stage, by)
            .await?
        {
            return Ok(false);
        }
        let done = store.get_release(release.id).await?;
        announce(
            state,
            product,
            &done,
            "released",
            &format!(
                "{} {} is released: every unit is offered it.",
                product.name, done.version
            ),
        )
        .await;
        crate::launch::brief(state, product, &done).await;
    } else {
        if !store.advance_release(release.id, release.stage, by).await? {
            return Ok(false);
        }
        let next = store.get_release(release.id).await?;
        let stage = next.current_stage().cloned();
        announce(
            state,
            product,
            &next,
            "advanced",
            &format!(
                "{} {} moved to stage {} ({}% of units).",
                product.name,
                next.version,
                stage.as_ref().map_or("?", |s| s.name.as_str()),
                stage.as_ref().map_or(0.0, |s| s.percent)
            ),
        )
        .await;
    }
    Ok(true)
}

// ---- telling people -----------------------------------------------------------------

async fn record(state: &AppState, release: &Release, kind: &str) {
    let detail = format!("{} {}", release.product_id, release.version);
    let _ = state
        .store
        .record_activity(agentcore_store::Activity {
            department: release.department_id,
            agent: release.agent_id,
            session: None,
            kind,
            value: Some(i64::from(release.risk.score)),
            detail: Some(&detail),
        })
        .await;
}

async fn tell(state: &AppState, release: &Release, text: &str) {
    let Some(agent) = release.agent_id else {
        return;
    };
    if let Err(err) = org::send(
        &state.store,
        &Sender::Human("releases".into()),
        Recipient::Agent(agent),
        text,
    )
    .await
    {
        tracing::debug!(error = %err, "could not tell the proposing agent");
    }
}

/// Tell the UI, the proposing agent, the activity log and the product's
/// webhook that a rollout moved.
async fn announce(state: &AppState, product: &Product, release: &Release, event: &str, text: &str) {
    notify(&state.store, json!({ "kind": "releases" })).await;
    let kind = match event {
        "started" => "release_started",
        "advanced" => "release_advanced",
        "released" => "release_released",
        "rolled_back" => "release_rolled_back",
        "paused" => "release_paused",
        _ => "release_resumed",
    };
    record(state, release, kind).await;
    tell(state, release, text).await;
    if let Some(url) = product.webhook_url.clone() {
        let secret = state.store.webhook_secret(product.id).await.ok().flatten();
        let body = json!({
            "event": event,
            "product": product.name,
            "release_id": release.id,
            "version": release.version,
            "status": release.status,
            "stage": release.current_stage().map(|s| &s.name),
            "percent": release.percent(),
            "reason": release.reason,
            "text": text,
            "at": Utc::now(),
        });
        let event = event.to_string();
        tokio::spawn(async move {
            if let Err(err) = post_webhook(&url, secret.as_deref(), &event, &body).await {
                tracing::warn!(error = %err, "release webhook failed");
            }
        });
    }
}

/// POST an event; signed with HMAC-SHA256 when the product has a key.
pub async fn post_webhook(
    url: &str,
    secret: Option<&str>,
    event: &str,
    body: &Value,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(WEBHOOK_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let mut request = client
        .post(url)
        .header("content-type", "application/json")
        .header("x-agentcore-event", event);
    if let Some(secret) = secret {
        request = request.header(
            "x-agentcore-signature",
            signing::hmac_signature(secret, &bytes),
        );
    }
    let response = request
        .body(bytes)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("the webhook answered {}", response.status()))
    }
}

// ---- views --------------------------------------------------------------------------

/// A release with what people need to judge it.
async fn detail(state: &AppState, release: &Release) -> ApiResult<Value> {
    let stats = state.store.report_stats(release.id).await?;
    Ok(json!({
        "release": release,
        "percent": release.percent(),
        "reports": stats,
        "crash_free": stats.crash_free(),
        "install_success": stats.install_success(),
    }))
}

fn brief(r: &Release) -> Value {
    json!({
        "id": r.id, "version": r.version, "status": r.status, "commit": r.commit,
        "stage": r.current_stage().map(|s| &s.name), "percent": r.percent(),
        "risk": { "score": r.risk.score, "level": r.risk.level },
        "approvals": r.approvals.len(), "approvals_required": r.approvals_required,
        "gates": r.gates, "feedback": r.feedback, "reason": r.reason,
        "revision": r.revision, "proposed_by": r.proposed_by,
    })
}

// ---- agent tools --------------------------------------------------------------------

/// `release_*` tools of a worker whose department ships a product.
pub struct ReleaseTools {
    state: AppState,
    agent: OrgAgentId,
    department: DepartmentId,
}

impl ReleaseTools {
    pub fn new(state: AppState, agent: OrgAgentId, department: DepartmentId) -> Self {
        Self {
            state,
            agent,
            department,
        }
    }

    async fn release_of_mine(&self, args: &Value, products: &[Product]) -> Result<Release, String> {
        let id: Uuid = text_arg(args, "release")?
            .trim()
            .parse()
            .map_err(|_| "`release` is a release id".to_string())?;
        let release = self
            .state
            .store
            .get_release(id)
            .await
            .map_err(|e| e.to_string())?;
        if !products.iter().any(|p| p.id == release.product_id) {
            return Err("that release is not of a product your department ships".into());
        }
        Ok(release)
    }
}

fn proposal_args(args: &Value) -> Result<ProposalInput, String> {
    serde_json::from_value(args.clone()).map_err(|e| format!("invalid arguments: {e}"))
}

fn artifact_schema() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "platform": { "type": "string" },
                "url": { "type": "string" },
                "sha256": { "type": "string" },
                "size": { "type": "integer" },
            },
            "required": ["name", "url", "sha256"],
        },
    })
}

fn proposal_schema() -> Value {
    json!({
        "product": { "type": "string" },
        "version": { "type": "string" },
        "commit": { "type": "string" },
        "notes": { "type": "string" },
        "artifacts": artifact_schema(),
        "sbom": { "type": "object" },
        "checks": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "passed": { "type": "boolean" },
                    "detail": { "type": "string" },
                },
                "required": ["name", "passed"],
            },
        },
        "changes": {
            "type": "object",
            "properties": {
                "files_changed": { "type": "integer" },
                "lines_added": { "type": "integer" },
                "lines_removed": { "type": "integer" },
                "commits": { "type": "integer" },
            },
        },
        "risk": { "type": "string", "enum": ["low", "medium", "high"] },
        "assessment": { "type": "string" },
    })
}

#[async_trait]
impl ToolHandler for ReleaseTools {
    fn definitions(&self) -> Vec<Value> {
        let mut revise = proposal_schema();
        revise["release"] = json!({ "type": "string" });
        revise["note"] = json!({ "type": "string" });
        vec![
            def(
                "release_products",
                "Products your department ships: their rollout plan, health gates, allowed \
                 artifact hosts, the version units run now, and any rollout in progress.",
                json!({}),
                &[],
            ),
            def(
                "release_propose",
                "Propose a release. People approve it before anything ships, and the rollout \
                 then goes out in stages watched by health gates. Give a semantic `version` \
                 above every earlier one, the `commit`, `notes` for the people who use the \
                 product (what changed, what to watch for), `artifacts` (name, platform, url on \
                 an allowed host, sha256 of the file, size), and what you know: `checks` \
                 (tests, lint, scans with passed true/false), `changes` (size), an `sbom` \
                 (CycloneDX/SPDX JSON) and your own `risk` (low/medium/high) with an \
                 `assessment`. Be honest: a failing check or a risk you hide hurts users.",
                proposal_schema(),
                &["product", "version", "notes", "artifacts"],
            ),
            def(
                "release_status",
                "Releases of your products (or one `release`): status, stage, share of units, \
                 health gates, crash-free and install-success rates, approvals, people's \
                 feedback.",
                json!({ "product": { "type": "string" }, "release": { "type": "string" } }),
                &[],
            ),
            def(
                "release_revise",
                "Revise a release that is still waiting for people (usually after they asked \
                 for changes): the `release` id, the fields that change, and a `note`. \
                 Approvals start over. The version stays; propose a new version instead of \
                 changing what was approved.",
                revise,
                &["release", "note"],
            ),
            def(
                "release_halt",
                "Pause a rollout now (units keep what they were offered) when you see a \
                 problem: users report errors, data looks wrong, a bug was found. Say why. \
                 People decide whether it resumes or rolls back.",
                json!({ "release": { "type": "string" }, "reason": { "type": "string" } }),
                &["release", "reason"],
            ),
        ]
    }

    async fn call(&self, tool: &str, arguments: &Value) -> Result<Value, String> {
        let state = &self.state;
        let store = &state.store;
        let e = |e: StoreError| e.to_string();
        let products = store.products_for(self.department).await.map_err(e)?;
        let product_named = |name: &str| {
            products
                .iter()
                .find(|p| p.name == name.trim())
                .cloned()
                .ok_or_else(|| {
                    format!("your department ships no product `{name}` (see release_products)")
                })
        };
        match tool {
            "release_products" => {
                let mut list = Vec::new();
                for p in &products {
                    let baseline = store.baseline_release(p.id).await.map_err(e)?;
                    let active = store.active_release(p.id).await.map_err(e)?;
                    list.push(json!({
                        "name": p.name,
                        "description": p.description,
                        "repository": p.repository,
                        "current_version": baseline.map(|r| r.version),
                        "rollout": active.as_ref().map(brief),
                        "plan": p.plan,
                        "gates": p.gates,
                        "approvals_required": p.approvals_required,
                        "artifact_hosts": p.artifact_hosts,
                        "frozen": p.frozen,
                    }));
                }
                Ok(json!({ "products": list }))
            }
            "release_propose" => {
                let product = product_named(&text_arg(arguments, "product")?)?;
                let org = Org::load(store).await.map_err(e)?;
                let me = org
                    .agent(self.agent)
                    .ok_or("you are no longer a member of the organisation")?;
                let release = propose(
                    state,
                    &product,
                    proposal_args(arguments)?,
                    Proposer {
                        label: org.label(me),
                        department: Some(self.department),
                        agent: Some(self.agent),
                    },
                )
                .await
                .map_err(|e| e.message().to_string())?;
                Ok(json!({
                    "release": release.id,
                    "version": release.version,
                    "status": release.status,
                    "risk": release.risk,
                    "plan": release.plan,
                    "approvals_required": release.approvals_required,
                    "note": "People review it (and the server verifies every artifact's digest) \
                             before the rollout starts; you get a message at every step.",
                }))
            }
            "release_status" => {
                if arguments["release"].as_str().is_some() {
                    let release = self.release_of_mine(arguments, &products).await?;
                    return detail(state, &release)
                        .await
                        .map_err(|e| e.message().to_string());
                }
                let wanted: Vec<&Product> = match arguments["product"].as_str() {
                    Some(name) if !name.is_empty() => {
                        products.iter().filter(|p| p.name == name.trim()).collect()
                    }
                    _ => products.iter().collect(),
                };
                let mut out = Vec::new();
                for p in wanted {
                    let list = store
                        .list_releases(Some(p.id), None, None, 20)
                        .await
                        .map_err(e)?;
                    out.push(json!({
                        "product": p.name,
                        "releases": list.iter().map(brief).collect::<Vec<_>>(),
                    }));
                }
                Ok(json!({ "products": out }))
            }
            "release_revise" => {
                let current = self.release_of_mine(arguments, &products).await?;
                let product = store.get_product(current.product_id).await.map_err(e)?;
                let input = merged(&current, arguments)?;
                let (_, content) = content(state, &product, input, Some(current.id))
                    .await
                    .map_err(|e| e.message().to_string())?;
                let org = Org::load(store).await.map_err(e)?;
                let by = org
                    .agent(self.agent)
                    .map_or_else(|| "agent".to_string(), |a| org.label(a));
                let release = store
                    .revise_release(
                        current.id,
                        current.revision,
                        content,
                        &text_arg(arguments, "note")?,
                        &by,
                    )
                    .await
                    .map_err(e)?;
                notify(store, json!({ "kind": "releases" })).await;
                Ok(json!({
                    "release": release.id, "revision": release.revision,
                    "status": release.status, "risk": release.risk, "plan": release.plan,
                }))
            }
            "release_halt" => {
                let release = self.release_of_mine(arguments, &products).await?;
                let reason = text_arg(arguments, "reason")?;
                let org = Org::load(store).await.map_err(e)?;
                let by = org
                    .agent(self.agent)
                    .map_or_else(|| "agent".to_string(), |a| org.label(a));
                let paused = pause(state, release.id, &reason, &by)
                    .await
                    .map_err(|e| e.message().to_string())?;
                Ok(json!({
                    "release": paused.id, "status": paused.status,
                    "note": "Paused. People decide whether it resumes or rolls back.",
                }))
            }
            other => Err(format!("unknown tool `{other}`")),
        }
    }
}

/// A revision: the current content with the given fields replaced.
fn merged(current: &Release, args: &Value) -> Result<ProposalInput, String> {
    let mut input = ProposalInput {
        version: current.version.clone(),
        commit: current.commit.clone(),
        notes: current.notes.clone(),
        artifacts: current.artifacts.clone(),
        sbom: current.sbom.clone(),
        checks: current.checks.clone(),
        changes: current.changes.clone(),
        risk: None,
        assessment: current.risk.assessment.clone(),
    };
    let patch = proposal_args(args)?;
    if args.get("commit").is_some() {
        input.commit = patch.commit;
    }
    if args.get("notes").is_some() {
        input.notes = patch.notes;
    }
    if args.get("artifacts").is_some() {
        input.artifacts = patch.artifacts;
    }
    if args.get("sbom").is_some() {
        input.sbom = patch.sbom;
    }
    if args.get("checks").is_some() {
        input.checks = patch.checks;
    }
    if args.get("changes").is_some() {
        input.changes = patch.changes;
    }
    if args.get("assessment").is_some() {
        input.assessment = patch.assessment;
    }
    // The proposer states their rating again with each revision.
    input.risk = patch.risk;
    Ok(input)
}

// ---- operator actions ---------------------------------------------------------------

pub async fn pause(state: &AppState, id: Uuid, reason: &str, by: &str) -> ApiResult<Release> {
    if reason.trim().is_empty() {
        return Err(bad("say why the rollout is paused"));
    }
    let release = state.store.pause_release(id, reason, by).await?;
    let product = state.store.get_product(release.product_id).await?;
    announce(
        state,
        &product,
        &release,
        "paused",
        &format!(
            "{by} paused the rollout of {} {}: {}",
            product.name,
            release.version,
            reason.trim()
        ),
    )
    .await;
    Ok(release)
}

// ---- API: products ------------------------------------------------------------------

/// A new client token: 32 random bytes, hex.
fn client_token() -> ApiResult<String> {
    let mut bytes = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "no randomness"))?;
    Ok(format!("acd_{}", hex::encode(bytes)))
}

pub async fn list_products(
    State(state): State<AppState>,
    _caller: Caller,
) -> ApiResult<Json<Value>> {
    let mut out = Vec::new();
    for p in state.store.list_products().await? {
        let baseline = state.store.baseline_release(p.id).await?;
        let active = state.store.active_release(p.id).await?;
        out.push(json!({
            "product": p,
            "current_version": baseline.map(|r| r.version),
            "rollout": active.as_ref().map(brief),
        }));
    }
    Ok(Json(json!(out)))
}

/// Create a product (admin); the client token is shown once.
pub async fn create_product(
    State(state): State<AppState>,
    caller: Caller,
    Json(input): Json<ProductInput>,
) -> ApiResult<impl IntoResponse> {
    caller.require_admin()?;
    let token = client_token()?;
    let hint = format!("…{}", &token[token.len() - 4..]);
    let product = state
        .store
        .create_product(input, (&hash_token(&token), &hint), &caller.name)
        .await?;
    notify(&state.store, json!({ "kind": "releases" })).await;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "product": product, "client_token": token })),
    ))
}

pub async fn update_product(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(update): Json<ProductUpdate>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let product = state.store.update_product(id, update, &caller.name).await?;
    notify(&state.store, json!({ "kind": "releases" })).await;
    Ok(Json(json!(product)))
}

pub async fn delete_product(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    state.store.delete_product(id, &caller.name).await?;
    notify(&state.store, json!({ "kind": "releases" })).await;
    Ok(StatusCode::NO_CONTENT)
}

/// A new client token (the old one stops working).
pub async fn rotate_token(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let token = client_token()?;
    let hint = format!("…{}", &token[token.len() - 4..]);
    let product = state
        .store
        .set_client_token(id, &hash_token(&token), &hint, &caller.name)
        .await?;
    Ok(Json(json!({ "product": product, "client_token": token })))
}

#[derive(Debug, Deserialize)]
pub struct FreezeInput {
    pub reason: String,
}

/// A change freeze: no rollout starts or advances (rollbacks still happen).
pub async fn freeze(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(input): Json<FreezeInput>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let product = state
        .store
        .set_freeze(id, Some(&input.reason), &caller.name)
        .await?;
    notify(&state.store, json!({ "kind": "releases" })).await;
    Ok(Json(json!(product)))
}

pub async fn unfreeze(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let product = state.store.set_freeze(id, None, &caller.name).await?;
    notify(&state.store, json!({ "kind": "releases" })).await;
    Ok(Json(json!(product)))
}

// ---- API: releases ------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ReleaseQuery {
    #[serde(default)]
    pub product: Option<Uuid>,
    #[serde(default)]
    pub status: Option<ReleaseStatus>,
}

pub async fn list(
    State(state): State<AppState>,
    _caller: Caller,
    Query(q): Query<ReleaseQuery>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(
        state
            .store
            .list_releases(q.product, q.status, None, 500)
            .await?
    )))
}

pub async fn get(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let release = state.store.get_release(id).await?;
    Ok(Json(detail(&state, &release).await?))
}

#[derive(Debug, Deserialize)]
pub struct PersonProposal {
    pub product: Uuid,
    #[serde(flatten)]
    pub input: ProposalInput,
}

/// A person proposes a release (someone else approves it).
pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    Json(p): Json<PersonProposal>,
) -> ApiResult<impl IntoResponse> {
    caller.require_operator()?;
    let product = state.store.get_product(p.product).await?;
    let release = propose(
        &state,
        &product,
        p.input,
        Proposer {
            label: caller.name.clone(),
            department: None,
            agent: None,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!(release))))
}

#[derive(Debug, Deserialize)]
pub struct RevisionInput {
    /// The revision the person read.
    pub revision: u32,
    #[serde(default)]
    pub note: String,
    #[serde(flatten)]
    pub fields: Value,
}

/// A person revises a release that waits (approvals start over).
pub async fn revise(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(input): Json<RevisionInput>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let current = state.store.get_release(id).await?;
    let product = state.store.get_product(current.product_id).await?;
    let merged = merged(&current, &input.fields).map_err(bad)?;
    let (_, content) = content(&state, &product, merged, Some(id)).await?;
    let release = state
        .store
        .revise_release(id, input.revision, content, &input.note, &caller.name)
        .await?;
    notify(&state.store, json!({ "kind": "releases" })).await;
    Ok(Json(json!(release)))
}

#[derive(Debug, Deserialize)]
pub struct ApproveInput {
    pub revision: u32,
}

pub async fn approve_release(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(input): Json<ApproveInput>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    Ok(Json(
        approve(&state, id, input.revision, &caller.name).await?,
    ))
}

/// Start a fully approved release that could not start before (a freeze
/// or another rollout was in the way).
pub async fn start_release(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let release = state.store.get_release(id).await?;
    let product = state.store.get_product(release.product_id).await?;
    Ok(Json(json!(
        start(&state, &product, release, &caller.name).await?
    )))
}

#[derive(Debug, Deserialize)]
pub struct TextInput {
    #[serde(default)]
    pub text: String,
}

pub async fn request_changes(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(input): Json<TextInput>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let release = state
        .store
        .request_release_changes(id, &input.text, &caller.name)
        .await?;
    notify(&state.store, json!({ "kind": "releases" })).await;
    tell(
        &state,
        &release,
        &format!(
            "{} asked for changes to release {} (id {}): {}\nRevise it with release_revise.",
            caller.name,
            release.version,
            release.id,
            input.text.trim()
        ),
    )
    .await;
    Ok(Json(json!(release)))
}

pub async fn reject(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(input): Json<TextInput>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let release = state
        .store
        .reject_release(id, &input.text, &caller.name)
        .await?;
    notify(&state.store, json!({ "kind": "releases" })).await;
    let why = if input.text.trim().is_empty() {
        String::new()
    } else {
        format!(": {}", input.text.trim())
    };
    tell(
        &state,
        &release,
        &format!(
            "{} rejected release {} (id {}){why}. It will not ship.",
            caller.name, release.version, release.id
        ),
    )
    .await;
    Ok(Json(json!(release)))
}

pub async fn pause_release(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(input): Json<TextInput>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    Ok(Json(json!(
        pause(&state, id, &input.text, &caller.name).await?
    )))
}

pub async fn resume_release(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let release = state.store.resume_release(id, &caller.name).await?;
    let product = state.store.get_product(release.product_id).await?;
    announce(
        &state,
        &product,
        &release,
        "resumed",
        &format!(
            "{} resumed the rollout of {} {}.",
            caller.name, product.name, release.version
        ),
    )
    .await;
    Ok(Json(json!(release)))
}

/// Move to the next stage now, without waiting for the soak time. Gates
/// must pass (a person cannot promote past a failing or unknown gate).
pub async fn promote(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let release = state.store.get_release(id).await?;
    let product = state.store.get_product(release.product_id).await?;
    if release.status != ReleaseStatus::RollingOut {
        return Err(conflict(format!(
            "only a release that is rolling out moves on (this one is {})",
            release.status.as_str()
        )));
    }
    if let Some(reason) = &product.frozen {
        return Err(conflict(format!("`{}` is frozen: {reason}", product.name)));
    }
    let results = read_gates(&state, &product, &release).await;
    state.store.record_gates(release.id, &results).await?;
    if let Some(g) = results.iter().find(|r| r.status != GateStatus::Pass) {
        return Err(conflict(format!(
            "health gate `{}` does not pass ({})",
            g.name, g.detail
        )));
    }
    if !move_on(&state, &product, &release, &caller.name).await? {
        return Err(conflict("the rollout moved on in the meantime"));
    }
    Ok(Json(json!(state.store.get_release(id).await?)))
}

pub async fn roll_back(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(input): Json<TextInput>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let release = state.store.get_release(id).await?;
    if release.status == ReleaseStatus::Released
        && state
            .store
            .baseline_release(release.product_id)
            .await?
            .is_none_or(|b| b.id != release.id)
    {
        return Err(conflict(
            "only the newest released version can be rolled back; a newer one replaced it",
        ));
    }
    let release = state
        .store
        .roll_back_release(id, &input.text, &caller.name)
        .await?;
    let product = state.store.get_product(release.product_id).await?;
    announce(
        &state,
        &product,
        &release,
        "rolled_back",
        &format!(
            "{} rolled back {} {}: {}. Units are offered the previous version again.",
            caller.name,
            product.name,
            release.version,
            input.text.trim()
        ),
    )
    .await;
    Ok(Json(json!(release)))
}

pub async fn attestation(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let release = state.store.get_release(id).await?;
    release.attestation.map(Json).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "the release has no attestation yet (it is made when the rollout starts)",
        )
    })
}

pub async fn keys(State(state): State<AppState>, _caller: Caller) -> ApiResult<Json<Value>> {
    Ok(Json(signing::published(&state.store.signing_keys().await?)))
}

/// A new signing key; the old one stays published to verify older
/// signatures.
pub async fn rotate_key(State(state): State<AppState>, caller: Caller) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let id = signing::new_key(&state.store, false, &caller.name)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "key_id": id })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits_are_hex() {
        assert_eq!(
            clean_commit(Some(" ABCDEF1 ".into())).unwrap().as_deref(),
            Some("abcdef1")
        );
        assert_eq!(clean_commit(Some(String::new())).unwrap(), None);
        assert!(clean_commit(Some("abc".into())).is_err());
        assert!(clean_commit(Some("not-a-commit".into())).is_err());
    }
}
