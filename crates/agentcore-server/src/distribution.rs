//! What update clients talk to (`/dist`): which version a unit should run,
//! as a signed manifest; reports of installs and crashes, which feed the
//! health gates; attestations; and the public keys to verify them.
//!
//! Clients authenticate with their product's client token (not an
//! operator token). Unit ids are hashed before they are stored.

use agentcore_core::releases::{self, Track};
use agentcore_core::{Product, ReleaseStatus};
use agentcore_store::{ReportEvent, StoreError};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::auth::hash_token;
use crate::error::ApiError;
use crate::signing;

type ApiResult<T> = Result<T, ApiError>;

/// How long a manifest may be trusted (clients refuse older ones, so a
/// stale manifest cannot be replayed to keep a unit on an old version).
const MANIFEST_HOURS: i64 = 24;

async fn client_product(state: &AppState, headers: &HeaderMap, name: &str) -> ApiResult<Product> {
    let unauthorized = || ApiError::new(StatusCode::UNAUTHORIZED, "unknown product or token");
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(unauthorized)?;
    match state
        .store
        .product_for_client(name, &hash_token(token))
        .await
    {
        Ok(product) => Ok(product),
        Err(StoreError::NotFound(_)) => Err(unauthorized()),
        Err(e) => Err(e.into()),
    }
}

fn check_unit(unit: &str) -> ApiResult<&str> {
    let unit = unit.trim();
    if unit.is_empty() || unit.len() > 200 || unit.chars().any(char::is_control) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "`unit` is the installation's stable id (up to 200 characters)",
        ));
    }
    Ok(unit)
}

/// What is stored instead of the unit id.
pub fn unit_hash(product: &Product, unit: &str) -> String {
    hex::encode(Sha256::digest(format!("{}:{unit}", product.id).as_bytes()))
}

pub async fn keys(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    // Make the first key, so clients can pin it before the first release.
    signing::active_key(&state.store)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(signing::published(&state.store.signing_keys().await?)))
}

#[derive(Debug, Deserialize)]
pub struct UpdateQuery {
    pub unit: String,
    /// The version the unit runs now.
    #[serde(default)]
    pub current: Option<String>,
    /// Only artifacts for this platform (and `any`).
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub track: Track,
}

/// The version a unit should run, signed.
pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(q): Query<UpdateQuery>,
) -> ApiResult<Json<Value>> {
    let product = client_product(&state, &headers, &name).await?;
    let unit = check_unit(&q.unit)?;
    let rollout = state.store.active_release(product.id).await?;
    let baseline = state.store.baseline_release(product.id).await?;
    let chosen = releases::choose(rollout.as_ref(), baseline.as_ref(), unit, q.track);
    let current: Option<agentcore_core::Version> =
        q.current.as_deref().and_then(|c| c.parse().ok());
    let now = Utc::now();
    let manifest = match chosen {
        None => json!({
            "product": product.name,
            "version": null,
            "update": false,
            "rollback": false,
            "issued_at": now,
            "expires_at": now + chrono::Duration::hours(MANIFEST_HOURS),
        }),
        Some(r) => {
            let target = r.parsed_version();
            let platform = q.platform.as_deref().map(str::to_ascii_lowercase);
            let artifacts: Vec<Value> = r
                .artifacts
                .iter()
                .filter(|a| {
                    platform
                        .as_deref()
                        .is_none_or(|p| a.platform == p || a.platform == "any")
                })
                .map(|a| {
                    json!({
                        "name": a.name, "platform": a.platform, "url": a.url,
                        "sha256": a.sha256, "size": a.size, "media_type": a.media_type,
                    })
                })
                .collect();
            json!({
                "product": product.name,
                "version": r.version,
                "update": current.as_ref() != target.as_ref(),
                // The unit runs a version that was taken back: go back down.
                "rollback": matches!((&current, &target), (Some(c), Some(t)) if c > t),
                "notes": r.notes,
                "commit": r.commit,
                "artifacts": artifacts,
                "status": r.status,
                "stage": if r.status == ReleaseStatus::Released {
                    None
                } else {
                    r.current_stage().map(|s| s.name.clone())
                },
                "percent": r.percent(),
                "released_at": r.released_at,
                "attestation": format!("/dist/{}/releases/{}/attestation", product.name, r.version),
                "issued_at": now,
                "expires_at": now + chrono::Duration::hours(MANIFEST_HOURS),
            })
        }
    };
    let envelope = signing::sign_json(&state.store, signing::MANIFEST_TYPE, &manifest)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "manifest": manifest, "envelope": envelope })))
}

#[derive(Debug, Deserialize)]
pub struct ReportInput {
    pub unit: String,
    pub version: String,
    pub event: ReportEvent,
}

/// A unit reports an install, a failed install, a crash or that it runs fine.
pub async fn report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(input): Json<ReportInput>,
) -> ApiResult<StatusCode> {
    let product = client_product(&state, &headers, &name).await?;
    let unit = check_unit(&input.unit)?;
    let release = state
        .store
        .release_by_version(product.id, input.version.trim().trim_start_matches('v'))
        .await?;
    if release.status.is_pending() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("release {} was never offered", release.version),
        ));
    }
    state
        .store
        .record_report(release.id, &unit_hash(&product, unit), input.event)
        .await?;
    Ok(StatusCode::ACCEPTED)
}

/// The signed attestation of a shipped release.
pub async fn attestation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((name, version)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let product = client_product(&state, &headers, &name).await?;
    let release = state.store.release_by_version(product.id, &version).await?;
    release.attestation.map(Json).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            format!("release {version} has no attestation"),
        )
    })
}
