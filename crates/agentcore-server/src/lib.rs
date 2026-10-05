//! HTTP surface of agentcore:
//!
//! * `/api/v1/...`: operator API (sessions, approvals, stop, audit, settings).
//! * `/mcp/{session}`: MCP tool gateway used by agents inside sandboxes.
//! * `/llm/{session}/{provider}/...`: model gateway (LLM API reverse proxy).
//! * everything else: the React web UI (static files from `server.ui_dir`).

pub mod api;
pub mod auth;
pub mod config;
mod error;
pub mod llm;
pub mod mcp;
pub mod sessions;

use std::sync::Arc;
use std::time::Duration;

use agentcore_core::Principal;
use agentcore_policy::PolicySet;
use agentcore_runtime::{AdapterRegistry, RuntimeConfig, SessionManager};
use agentcore_store::{Cipher, Store};
use anyhow::Context;
use axum::Router;
use axum::routing::{any, get, patch, post};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

pub use config::Config;

#[derive(Clone)]
pub struct AppState {
    pub manager: Arc<SessionManager>,
    pub config: Arc<Config>,
    pub store: Store,
    /// Client for upstream model providers.
    pub http: reqwest::Client,
}

impl AppState {
    /// Load policies, connect to PostgreSQL (running migrations), and recover
    /// from a previous unclean shutdown.
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let policies = PolicySet::load_dir(&config.policies.dir)
            .with_context(|| format!("loading policies from {}", config.policies.dir.display()))?;
        let provider = config.sandbox.provider()?;
        let cipher = Cipher::load_or_create(&config.master_key_file())?;
        let store = Store::connect(&config.database_url()?, cipher)
            .await
            .context("connecting to PostgreSQL")?;
        sessions::recover(&store, provider.as_ref()).await;
        let manager = SessionManager::new(
            RuntimeConfig {
                data_dir: config.storage.data_dir.clone(),
                gateway_url: config.gateway_url(),
                default_policy: config.policies.default.clone(),
                audit_fsync: config.storage.audit_fsync,
            },
            policies,
            config.agents.clone(),
            AdapterRegistry::default(),
            provider,
        )?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(
                config.model_gateway.upstream_read_timeout_secs,
            ))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            manager: Arc::new(manager),
            config: Arc::new(config),
            store,
            http,
        })
    }
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/health", get(api::health))
        .route("/whoami", get(api::whoami))
        .route("/system-card", get(api::system_card))
        .route("/stop-all", post(api::stop_all))
        .route(
            "/sessions",
            get(api::list_sessions).post(api::create_session),
        )
        .route("/sessions/{id}", get(api::get_session))
        .route("/sessions/{id}/stop", post(api::stop_session))
        .route("/sessions/{id}/events", get(api::events))
        .route("/sessions/{id}/stream", get(api::stream))
        .route("/sessions/{id}/approvals", get(api::list_approvals))
        .route(
            "/sessions/{id}/approvals/{approval_id}",
            post(api::decide_approval),
        )
        .route("/sessions/{id}/model-calls/{call_id}", get(api::model_call))
        .route("/sessions/{id}/audit", get(api::download_audit))
        .route("/sessions/{id}/audit/verify", get(api::verify_audit))
        .route(
            "/providers",
            get(api::list_providers).post(api::create_provider),
        )
        .route(
            "/providers/{name}",
            patch(api::update_provider).delete(api::delete_provider),
        )
        .route("/admin-events", get(api::admin_events));

    let ui_dir = &state.config.server.ui_dir;
    let ui = ServeDir::new(ui_dir).fallback(ServeFile::new(ui_dir.join("index.html")));

    Router::new()
        .nest("/api/v1", api)
        .route("/mcp/{id}", post(mcp::handle).get(mcp::method_not_allowed))
        .route("/llm/{id}/{provider}/{*rest}", any(llm::proxy))
        .fallback_service(ui)
        .layer(RequestBodyLimitLayer::new(32 * 1024 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Run the server until Ctrl-C / SIGTERM, then stop every live session.
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let bind = config.server.bind;
    if !config.server.ui_dir.join("index.html").exists() {
        tracing::warn!(
            dir = %config.server.ui_dir.display(),
            "web UI not built; run `npm ci && npm run build` in web/"
        );
    }
    let state = AppState::new(config).await?;
    let manager = state.manager.clone();
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!(%bind, backend = manager.sandbox_backend(), "agentcore listening");
    axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    let stopped = manager
        .stop_all(Principal::System, "server shutting down")
        .await;
    // Give session tasks a moment to record their end and sync to the database.
    for _ in 0..50 {
        if manager.list().iter().all(|s| s.status.is_terminal()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for info in manager.list() {
        if let Ok(session) = manager.get(info.id) {
            state.persist(&session).await;
        }
    }
    tracing::info!(stopped, "shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
}
