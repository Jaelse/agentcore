//! HTTP surface of agentcore:
//!
//! * `/api/v1/...`: operator API (sessions, approvals, stop, audit, settings).
//! * `/mcp/{session}`: MCP tool gateway used by agents inside sandboxes.
//! * `/llm/{session}/{provider}/...`: model gateway (LLM API reverse proxy).
//! * everything else: the React web UI (static files from `server.ui_dir`).

pub mod api;
pub mod auth;
pub mod budgets;
pub mod catalog;
pub mod cluster;
pub mod config;
mod error;
pub mod github;
pub mod insights;
pub mod live;
pub mod llm;
pub mod mcp;
pub mod org;
pub mod sessions;
pub mod teamwork;
pub mod templates;

use std::sync::Arc;
use std::time::Duration;

use agentcore_core::Principal;
use agentcore_policy::PolicySet;
use agentcore_runtime::{AdapterRegistry, RuntimeConfig, SessionManager};
use agentcore_store::{Cipher, Store};
use anyhow::Context;
use axum::Router;
use axum::routing::{any, get, patch, post, put};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

pub use config::Config;

#[derive(Clone)]
pub struct AppState {
    pub manager: Arc<SessionManager>,
    pub config: Arc<Config>,
    pub store: Store,
    /// Client for upstream model providers and GitHub.
    pub http: reqwest::Client,
    pub roles: Arc<agentcore_roles::RoleSet>,
    /// Sessions with a delivery in progress.
    pub delivering: Arc<tokio::sync::Mutex<std::collections::HashSet<uuid::Uuid>>>,
    /// This node's name in the cluster.
    pub node: Arc<str>,
    /// Organisation change notifications (from every node), for `/org/stream`.
    pub org_events: tokio::sync::broadcast::Sender<serde_json::Value>,
    /// Wakes this node's reconciler.
    pub reconcile: Arc<tokio::sync::Notify>,
    /// Client for node-to-node requests (never through an HTTP proxy).
    pub cluster_http: reqwest::Client,
    /// Department templates and blueprints.
    pub templates: Arc<templates::Catalog>,
    /// Open-source agents that can be added from the web UI.
    pub agent_catalog: Arc<catalog::AgentCatalog>,
}

impl AppState {
    /// Load policies, connect to PostgreSQL (running migrations), and recover
    /// from a previous unclean shutdown.
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let policies = PolicySet::load_dir(&config.policies.dir)
            .with_context(|| format!("loading policies from {}", config.policies.dir.display()))?;
        let roles = agentcore_roles::RoleSet::load_dir(&config.roles.dir)
            .with_context(|| format!("loading roles from {}", config.roles.dir.display()))?;
        for role in roles.iter() {
            if let Some(policy) = &role.policy
                && policies.get(policy).is_none()
            {
                anyhow::bail!("role `{}` uses unknown policy `{policy}`", role.name);
            }
        }
        let catalog = templates::Catalog::load_dir(&config.templates.dir).with_context(|| {
            format!("loading templates from {}", config.templates.dir.display())
        })?;
        if catalog.departments.is_empty() {
            tracing::warn!(dir = %config.templates.dir.display(), "no department templates found");
        }
        for role in catalog.roles() {
            if roles.get(role).is_none() {
                anyhow::bail!("a department template uses unknown role `{role}`");
            }
        }
        for policy in catalog.policies() {
            if policies.get(policy).is_none() {
                anyhow::bail!("a department template uses unknown policy `{policy}`");
            }
        }
        let agent_catalog = catalog::AgentCatalog::load_dir(&config.templates.dir.join("agents"))
            .with_context(|| {
            format!(
                "loading the agent catalogue from {}",
                config.templates.dir.join("agents").display()
            )
        })?;
        let provider = config.sandbox.provider()?;
        let cipher = Cipher::load_or_create(&config.master_key_file())?;
        let store = Store::connect(&config.database_url()?, cipher)
            .await
            .context("connecting to PostgreSQL")?;
        let node = config.node_name();
        sessions::recover(&store, provider.as_ref(), &node).await;
        if policies.get(&config.org.communicator_policy).is_none() {
            tracing::warn!(
                policy = %config.org.communicator_policy,
                "the communicator policy does not exist: departments cannot be created"
            );
        }
        store
            .seed_org_settings(agentcore_core::OrgSettings {
                max_departments: config.org.default_max_departments,
                max_agents_per_department: config.org.default_max_agents_per_department,
            })
            .await?;
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
        let cluster_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building cluster HTTP client")?;
        let state = Self {
            manager: Arc::new(manager),
            config: Arc::new(config),
            store,
            http,
            roles: Arc::new(roles),
            delivering: Arc::default(),
            node: node.into(),
            org_events: tokio::sync::broadcast::channel(256).0,
            reconcile: Arc::default(),
            cluster_http,
            templates: Arc::new(catalog),
            agent_catalog: Arc::new(agent_catalog),
        };
        state.reload_agents().await;
        Ok(state)
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
        .route("/sessions/{id}/live", get(live::stream))
        .route("/sessions/{id}/pause", post(live::pause))
        .route("/sessions/{id}/resume", post(live::resume))
        .route("/sessions/{id}/recording", get(live::recording))
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
        .route("/admin-events", get(api::admin_events))
        .route("/roles", get(teamwork::list_roles))
        .route(
            "/integrations/github",
            get(teamwork::get_github)
                .put(teamwork::put_github)
                .delete(teamwork::delete_github),
        )
        .route("/integrations/github/test", post(teamwork::test_github))
        .route(
            "/projects",
            get(teamwork::list_projects).post(teamwork::create_project),
        )
        .route(
            "/projects/{id}",
            put(teamwork::update_project).delete(teamwork::delete_project),
        )
        .route("/projects/{id}/board", get(teamwork::project_board))
        .route("/projects/{id}/issues", get(teamwork::project_issues))
        .route(
            "/projects/{id}/sessions",
            post(teamwork::start_project_session),
        )
        .route("/sessions/{id}/messages", post(teamwork::send_message))
        .route("/sessions/{id}/finish", post(teamwork::finish_session))
        .route("/sessions/{id}/changes", get(teamwork::changes))
        .route("/sessions/{id}/checks", post(teamwork::run_checks))
        .route("/sessions/{id}/deliver", post(teamwork::deliver_session))
        .route("/agents", get(catalog::list_agents).post(catalog::install))
        .route("/agents/catalog", get(catalog::list_catalog))
        .route(
            "/agents/{name}",
            put(catalog::update).delete(catalog::uninstall),
        )
        .route("/agents/{name}/check", post(catalog::check))
        .route("/org", get(org::overview))
        .route("/org/stream", get(org::stream))
        .route("/org/templates", get(org::templates))
        .route("/org/suggestions", get(org::suggestions))
        .route("/org/profile", get(org::get_profile).put(org::put_profile))
        .route("/org/build", post(org::build))
        .route(
            "/org/settings",
            get(org::get_settings).put(org::put_settings),
        )
        .route("/org/departments", post(org::create_department))
        .route(
            "/org/departments/{id}",
            put(org::update_department).delete(org::delete_department),
        )
        .route("/org/departments/{id}/agents", post(org::add_agent))
        .route("/org/departments/{id}/files", get(org::list_files))
        .route(
            "/org/departments/{id}/checkins",
            get(org::list_check_ins).post(org::create_check_in),
        )
        .route(
            "/org/checkins/{id}",
            put(org::update_check_in).delete(org::delete_check_in),
        )
        .route("/org/checkins/{id}/run", post(org::run_check_in))
        .route("/org/goals", get(org::list_goals).post(org::create_goal))
        .route("/org/metrics", get(insights::metrics))
        .route("/org/spending", get(budgets::spending))
        .route(
            "/org/prices",
            put(budgets::put_price).delete(budgets::delete_price),
        )
        .route("/org/prices/backfill", post(budgets::price_past_calls))
        .route("/org/currency", put(budgets::put_currency))
        .route("/org/budgets", post(budgets::create_budget))
        .route(
            "/org/budgets/{id}",
            put(budgets::update_budget).delete(budgets::delete_budget),
        )
        .route(
            "/org/proposals",
            get(insights::list_proposals).post(insights::create_proposal),
        )
        .route(
            "/org/proposals/{id}",
            get(insights::get_proposal).put(insights::update_proposal),
        )
        .route("/org/proposals/{id}/apply", post(insights::apply_proposal))
        .route(
            "/org/proposals/{id}/changes",
            post(insights::request_changes),
        )
        .route(
            "/org/proposals/{id}/reject",
            post(insights::reject_proposal),
        )
        .route(
            "/org/auto-apply",
            get(insights::get_auto_apply).put(insights::put_auto_apply),
        )
        .route(
            "/org/data-sources",
            get(insights::list_data_sources).post(insights::create_data_source),
        )
        .route(
            "/org/data-sources/{id}",
            put(insights::update_data_source).delete(insights::delete_data_source),
        )
        .route(
            "/org/data-sources/{id}/test",
            post(insights::test_data_source),
        )
        .route(
            "/org/goals/{id}",
            put(org::update_goal).delete(org::delete_goal),
        )
        .route("/org/goals/{id}/progress", post(org::goal_progress))
        .route(
            "/org/departments/{id}/files/{*path}",
            get(org::get_file).put(org::put_file),
        )
        .route(
            "/org/departments/{id}/{control}",
            post(org::control_department),
        )
        .route(
            "/org/agents/{id}",
            put(org::update_agent).delete(org::delete_agent),
        )
        .route("/org/agents/{id}/{control}", post(org::control_agent))
        .route("/org/pause-all", post(org::pause_all))
        .route("/org/resume-all", post(org::resume_all))
        .route(
            "/org/messages",
            get(org::list_messages).post(org::post_message),
        )
        // Session endpoints are served by the node that runs the session.
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            cluster::forward_sessions,
        ));

    let ui_dir = &state.config.server.ui_dir;
    let ui = ServeDir::new(ui_dir).fallback(ServeFile::new(ui_dir.join("index.html")));

    Router::new()
        .nest("/api/v1", api)
        .route("/mcp/{id}", post(mcp::handle).get(mcp::method_not_allowed))
        // Some MCP clients (fast-agent) append `/mcp` to every server URL.
        .route(
            "/mcp/{id}/mcp",
            post(mcp::handle).get(mcp::method_not_allowed),
        )
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
    let cluster = cluster::start(&state).await?;
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
    cluster.cancel();
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
