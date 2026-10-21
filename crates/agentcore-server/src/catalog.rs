//! The agent catalogue: open-source agents that can be added with a few
//! clicks.
//!
//! Each entry (`templates/agents/*.toml`) says what the agent is, under which
//! license it is published, which model protocols it speaks, how well
//! agentcore's guardrails cover it, and how to launch it: an [`AgentSpec`]
//! template whose placeholders are filled in per session (the model gateway
//! URL for the chosen provider, the MCP gateway, the session token in env
//! and files only). Adding an agent stores the resulting spec in PostgreSQL;
//! every node loads it (and reloads it when told to).

use std::path::Path;
use std::time::Duration;

use agentcore_core::{AgentSpec, ProviderKind};
use agentcore_sandbox::{ExecRequest, SandboxRequest};
use anyhow::Context;
use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::Caller;
use crate::error::ApiError;

type ApiResult<T> = Result<T, ApiError>;

/// Only permissive open-source licenses: they allow commercial use,
/// modification and redistribution without copyleft obligations.
pub const ALLOWED_LICENSES: [&str; 6] = [
    "MIT",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "0BSD",
];

/// How much of an agent's work goes through agentcore's policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Guardrails {
    /// Its own side-effecting tools are off: every command and file change
    /// goes through the policy-checked gateway tools.
    Full,
    /// It acts with its own tools inside the sandbox; the policy does not see
    /// individual actions (the sandbox, the model gateway, the live view and
    /// the recording still apply).
    Sandbox,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub summary: String,
    pub description: String,
    /// `software`, `general`, `business`, `research`, ...
    pub domains: Vec<String>,
    pub homepage: String,
    pub repository: String,
    /// SPDX identifier; must be one of [`ALLOWED_LICENSES`].
    pub license: String,
    pub license_url: String,
    /// Where it is installed from (shown to people; the sandbox image
    /// installs it).
    pub package: String,
    /// The version agentcore's integration was verified with.
    pub verified_version: String,
    /// Model provider kinds it can use through the model gateway.
    pub protocols: Vec<ProviderKind>,
    #[serde(default)]
    pub suggested_models: Vec<String>,
    pub guardrails: Guardrails,
    /// Continues the conversation across turns (woken by messages).
    pub conversation: bool,
    /// Not open source: agentcore neither ships nor installs it by default.
    /// Admins install it into their own sandbox image and accept the
    /// vendor's terms (`terms_url`); `license` then names those terms.
    #[serde(default)]
    pub bring_your_own: bool,
    #[serde(default)]
    pub terms_url: Option<String>,
    /// Settings people choose when adding the agent (the adapter's
    /// `options`), e.g. how Claude Code signs in.
    #[serde(default)]
    pub options: Vec<CatalogOption>,
    /// Launch template (`name` is set when the agent is added).
    pub spec: SpecTemplate,
}

/// One choice offered when adding an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogOption {
    pub key: String,
    pub label: String,
    pub default: String,
    pub values: Vec<OptionValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionValue {
    pub value: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    /// The agent then reaches its model provider itself: no model provider
    /// of agentcore's is needed (and the sandbox needs internet access).
    #[serde(default)]
    pub direct: bool,
}

/// [`AgentSpec`] without the per-installation fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecTemplate {
    #[serde(default = "command_adapter")]
    pub adapter: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub follow_up_args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub files: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub tty: Option<bool>,
}

fn command_adapter() -> String {
    "command".into()
}

impl CatalogEntry {
    /// The chosen options, defaults filled in; unknown keys are dropped.
    pub fn options(&self, choice: &Choice) -> std::collections::BTreeMap<String, String> {
        self.options
            .iter()
            .map(|o| {
                let value = choice
                    .options
                    .get(&o.key)
                    .filter(|v| !v.trim().is_empty())
                    .cloned()
                    .unwrap_or_else(|| o.default.clone());
                (o.key.clone(), value)
            })
            .collect()
    }

    /// The chosen options make the agent reach its model provider itself.
    pub fn is_direct(&self, choice: &Choice) -> bool {
        let chosen = self.options(choice);
        self.options.iter().any(|o| {
            o.values
                .iter()
                .any(|v| v.direct && chosen.get(&o.key) == Some(&v.value))
        })
    }

    /// The agent definition for one installation.
    pub fn instantiate(&self, choice: &Choice) -> AgentSpec {
        let s = &self.spec;
        let direct = self.is_direct(choice);
        AgentSpec {
            name: choice.name.clone(),
            adapter: s.adapter.clone(),
            description: choice
                .description
                .clone()
                .filter(|d| !d.trim().is_empty())
                .unwrap_or_else(|| format!("{} ({})", self.name, self.summary)),
            image: choice.image.clone().filter(|i| !i.trim().is_empty()),
            command: Some(s.command.clone()),
            args: s.args.clone(),
            env: s.env.clone(),
            policy: choice.policy.clone().filter(|p| !p.trim().is_empty()),
            follow_up_args: s.follow_up_args.clone(),
            tty: s.tty,
            files: s.files.clone(),
            provider: (!direct).then(|| choice.provider.clone()),
            protocol: None,
            model: Some(choice.model.trim().to_string()).filter(|m| !m.is_empty()),
            catalog: Some(self.id.clone()),
            options: self.options(choice),
        }
    }
}

/// What a person chooses when adding an agent.
#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
    pub name: String,
    /// Model provider (by name, from Settings → Model providers).
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub policy: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Sandbox image override (an image with this agent installed).
    #[serde(default)]
    pub image: Option<String>,
    /// Values for the entry's `options`, by key.
    #[serde(default)]
    pub options: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AgentCatalog {
    pub agents: Vec<CatalogEntry>,
}

impl AgentCatalog {
    /// Load `dir/*.toml` (one entry per file). A missing directory gives an
    /// empty catalogue.
    pub fn load_dir(dir: &Path) -> anyhow::Result<Self> {
        let mut agents = Vec::new();
        if dir.exists() {
            let mut files: Vec<_> = std::fs::read_dir(dir)
                .with_context(|| format!("reading {}", dir.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .collect();
            files.sort();
            for file in files {
                let src = std::fs::read_to_string(&file)?;
                let entry: CatalogEntry =
                    toml::from_str(&src).with_context(|| format!("parsing {}", file.display()))?;
                agents.push(entry);
            }
        }
        let catalog = Self { agents };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut ids = std::collections::HashSet::new();
        for e in &self.agents {
            anyhow::ensure!(ids.insert(e.id.as_str()), "duplicate agent `{}`", e.id);
            if e.bring_your_own {
                anyhow::ensure!(
                    e.terms_url
                        .as_deref()
                        .is_some_and(|u| u.starts_with("https://")),
                    "agent `{}`: bring-your-own agents need a `terms_url`",
                    e.id
                );
            } else {
                anyhow::ensure!(
                    ALLOWED_LICENSES.contains(&e.license.as_str()),
                    "agent `{}`: license `{}` is not on the allowed list ({})",
                    e.id,
                    e.license,
                    ALLOWED_LICENSES.join(", ")
                );
            }
            for o in &e.options {
                anyhow::ensure!(
                    o.values.iter().any(|v| v.value == o.default),
                    "agent `{}`: option `{}` defaults to `{}`, which is not one of its values",
                    e.id,
                    o.key,
                    o.default
                );
            }
            anyhow::ensure!(!e.protocols.is_empty(), "agent `{}`: no protocols", e.id);
            anyhow::ensure!(
                e.conversation || e.spec.follow_up_args.is_empty(),
                "agent `{}`: follow_up_args given but `conversation = false`",
                e.id
            );
            let probe = e.instantiate(&Choice {
                name: e.id.clone(),
                provider: "p".into(),
                model: "m".into(),
                policy: None,
                description: None,
                image: None,
                options: Default::default(),
            });
            probe
                .validate()
                .map_err(|err| anyhow::anyhow!("agent `{}`: {err}", e.id))?;
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&CatalogEntry> {
        self.agents.iter().find(|a| a.id == id)
    }
}

impl AppState {
    /// Load the installed agents from PostgreSQL into the session manager.
    pub async fn reload_agents(&self) {
        match self.store.list_installed_agents().await {
            Ok(list) => {
                let specs = list.into_iter().filter(|a| a.enabled).map(|a| a.spec);
                for (name, err) in self.manager.set_installed(specs.collect()) {
                    tracing::warn!(agent = %name, error = %err, "installed agent skipped");
                }
            }
            Err(err) => tracing::error!(error = %err, "cannot load installed agents"),
        }
    }

    async fn agents_changed(&self) {
        self.reload_agents().await;
        crate::org::notify(&self.store, json!({ "kind": "agents_installed" })).await;
    }
}

// ---- API ------------------------------------------------------------------------

/// The catalogue, with what is already added.
pub async fn list_catalog(
    State(state): State<AppState>,
    _caller: Caller,
) -> ApiResult<Json<Value>> {
    let installed = state.store.list_installed_agents().await?;
    let entries: Vec<Value> = state
        .agent_catalog
        .agents
        .iter()
        .map(|e| {
            let mut v = json!(e);
            v["installed"] = json!(
                installed
                    .iter()
                    .filter(|a| a.catalog == e.id)
                    .map(|a| &a.name)
                    .collect::<Vec<_>>()
            );
            v
        })
        .collect();
    Ok(Json(
        json!({ "agents": entries, "allowed_licenses": ALLOWED_LICENSES }),
    ))
}

/// Every agent: from the configuration file and added from the catalogue.
pub async fn list_agents(State(state): State<AppState>, _caller: Caller) -> ApiResult<Json<Value>> {
    let installed = state.store.list_installed_agents().await?;
    let mut list: Vec<Value> = state
        .manager
        .agents()
        .into_iter()
        .filter(|a| state.manager.is_configured(&a.name))
        .map(|a| json!({ "source": "config", "enabled": true, "spec": public_spec(&a) }))
        .collect();
    for a in installed {
        let shadowed = state.manager.is_configured(&a.name);
        list.push(json!({
            "source": "catalog",
            "catalog": a.catalog,
            "enabled": a.enabled,
            "shadowed_by_config": shadowed,
            "updated_at": a.updated_at,
            "updated_by": a.updated_by,
            "spec": public_spec(&a.spec),
        }));
    }
    Ok(Json(json!(list)))
}

/// What people see of a spec (files and env may contain templates for
/// secrets, never secrets themselves, but they are long and noisy).
fn public_spec(spec: &AgentSpec) -> Value {
    json!({
        "name": spec.name,
        "adapter": spec.adapter,
        "description": spec.description,
        "command": spec.command,
        "policy": spec.policy,
        "provider": spec.provider,
        "model": spec.model,
        "image": spec.image,
        "catalog": spec.catalog,
        "conversation": !spec.follow_up_args.is_empty()
            || matches!(spec.adapter.as_str(), "opencode" | "claude-code"),
        "options": spec.options,
    })
}

/// Check the provider and model a person chose for a catalogue agent.
async fn check_choice(state: &AppState, entry: &CatalogEntry, choice: &Choice) -> ApiResult<()> {
    let bad = |m: String| ApiError::new(StatusCode::BAD_REQUEST, m);
    for (key, value) in &choice.options {
        let option = entry
            .options
            .iter()
            .find(|o| &o.key == key)
            .ok_or_else(|| bad(format!("{} has no option `{key}`", entry.name)))?;
        if !value.trim().is_empty() && !option.values.iter().any(|v| &v.value == value) {
            return Err(bad(format!(
                "{}: `{value}` is not a choice for {} ({})",
                entry.name,
                option.label,
                option
                    .values
                    .iter()
                    .map(|v| v.value.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    // Signed in by other means: no provider of agentcore's, model optional.
    if entry.is_direct(choice) {
        return Ok(());
    }
    let providers = state.store.list_providers().await?;
    let provider = providers
        .iter()
        .find(|p| p.name == choice.provider)
        .ok_or_else(|| {
            bad(format!(
                "unknown model provider `{}`: add it under Settings → Model providers",
                choice.provider
            ))
        })?;
    if !entry.protocols.contains(&provider.kind) {
        return Err(bad(format!(
            "{} speaks {}; provider `{}` is {}",
            entry.name,
            entry
                .protocols
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(" or "),
            provider.name,
            provider.kind.as_str()
        )));
    }
    let model = choice.model.trim();
    if model.is_empty() {
        return Err(bad("choose a model".into()));
    }
    if !provider.allowed_models.is_empty()
        && !provider.allowed_models.iter().any(|glob| {
            globset::Glob::new(glob)
                .map(|g| g.compile_matcher().is_match(model))
                .unwrap_or(false)
        })
    {
        return Err(bad(format!(
            "provider `{}` only allows the models {}",
            provider.name,
            provider.allowed_models.join(", ")
        )));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct InstallRequest {
    pub catalog: String,
    #[serde(flatten)]
    pub choice: Choice,
}

pub async fn install(
    State(state): State<AppState>,
    caller: Caller,
    Json(req): Json<InstallRequest>,
) -> ApiResult<impl IntoResponse> {
    caller.require_admin()?;
    let entry = state.agent_catalog.get(&req.catalog).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            format!("`{}` is not in the agent catalogue", req.catalog),
        )
    })?;
    let mut choice = req.choice;
    choice.name = choice.name.trim().to_ascii_lowercase();
    if state.manager.agent(&choice.name).is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("an agent named `{}` already exists", choice.name),
        ));
    }
    check_choice(&state, entry, &choice).await?;
    let spec = entry.instantiate(&choice);
    state.manager.check_agent(&spec)?;
    let installed = state
        .store
        .install_agent(&entry.id, &spec, &caller.name)
        .await?;
    state.agents_changed().await;
    Ok((StatusCode::CREATED, Json(json!(installed))))
}

#[derive(Debug, Deserialize)]
pub struct UpdateRequest {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub policy: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub options: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

pub async fn update(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(name): UrlPath<String>,
    Json(req): Json<UpdateRequest>,
) -> ApiResult<Json<Value>> {
    caller.require_admin()?;
    let current = state.store.installed_agent(&name).await?;
    let entry = state.agent_catalog.get(&current.catalog).ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            format!("`{}` is no longer in the agent catalogue", current.catalog),
        )
    })?;
    let old = &current.spec;
    let choice = Choice {
        name: name.clone(),
        provider: req
            .provider
            .or_else(|| old.provider.clone())
            .unwrap_or_default(),
        model: req.model.or_else(|| old.model.clone()).unwrap_or_default(),
        policy: req.policy.or_else(|| old.policy.clone()),
        description: req.description.or_else(|| Some(old.description.clone())),
        image: req.image.or_else(|| old.image.clone()),
        options: req.options.unwrap_or_else(|| old.options.clone()),
    };
    check_choice(&state, entry, &choice).await?;
    // Re-created from the current catalogue entry: updating an agent also
    // picks up a newer integration.
    let spec = entry.instantiate(&choice);
    state.manager.check_agent(&spec)?;
    let saved = state
        .store
        .update_installed_agent(&spec, req.enabled.unwrap_or(current.enabled), &caller.name)
        .await?;
    state.agents_changed().await;
    Ok(Json(json!(saved)))
}

pub async fn uninstall(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(name): UrlPath<String>,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    state.store.uninstall_agent(&name, &caller.name).await?;
    state.agents_changed().await;
    Ok(StatusCode::NO_CONTENT)
}

/// Start a throwaway sandbox and look for the agent's program in it: is the
/// agent installed in the image sessions would use?
pub async fn check(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(name): UrlPath<String>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let spec = state
        .manager
        .agent(&name)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("unknown agent `{name}`")))?;
    let program = spec.command.clone().unwrap_or_else(|| spec.adapter.clone());
    let id = uuid::Uuid::now_v7();
    let dir = state
        .config
        .storage
        .data_dir
        .join("checks")
        .join(id.to_string());
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let provider = state.manager.sandbox_provider();
    let result = async {
        let sandbox = provider
            .create(&SandboxRequest {
                session_id: id,
                workspace_dir: dir.clone(),
                image: spec.image.clone(),
                egress: false,
                shared_login_dir: None,
            })
            .await?;
        let out = sandbox
            .exec(ExecRequest {
                command: "sh".into(),
                args: vec![
                    "-c".into(),
                    "command -v \"$1\" || exit 127; \"$1\" --version 2>&1 | head -n 3; exit 0"
                        .into(),
                    "check".into(),
                    program.clone(),
                ],
                cwd: agentcore_sandbox::WORKSPACE.into(),
                env: Default::default(),
                timeout: Duration::from_secs(60),
                max_output_bytes: 8 * 1024,
                live: None,
            })
            .await;
        let _ = sandbox.destroy().await;
        out
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    let out = result.map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let available = out.exit_code == Some(0);
    let mut lines = out.stdout.lines();
    Ok(Json(json!({
        "agent": name,
        "program": program,
        "available": available,
        "path": if available { lines.next().map(str::to_string) } else { None },
        "version": if available { Some(lines.collect::<Vec<_>>().join("\n")) } else { None },
        "image": spec.image,
        "backend": state.manager.sandbox_backend(),
        "hint": (!available).then(|| format!(
            "`{program}` is not installed in the sandbox image. Build the image with this agent \
             (see docs/AGENT_CATALOG.md) or set an image that has it."
        )),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> AgentCatalog {
        AgentCatalog::load_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates/agents"),
        )
        .unwrap()
    }

    #[test]
    fn bundled_catalog_is_valid_permissive_and_keeps_secrets_out_of_arguments() {
        let c = catalog();
        assert!(c.agents.len() >= 7, "{}", c.agents.len());
        for e in &c.agents {
            assert!(
                e.bring_your_own || ALLOWED_LICENSES.contains(&e.license.as_str()),
                "{}",
                e.id
            );
            assert!(e.license_url.starts_with("https://"), "{}", e.id);
            let all_args = e.spec.args.iter().chain(&e.spec.follow_up_args);
            for a in all_args {
                assert!(!a.contains("{gateway_token}"), "{}: {a}", e.id);
            }
            // Every agent reaches models only through the gateway.
            let text = format!("{:?}{:?}{:?}", e.spec.args, e.spec.env, e.spec.files);
            assert!(
                text.contains("{model_base_url}")
                    || text.contains("{openai_base_url}")
                    || matches!(e.spec.adapter.as_str(), "opencode" | "claude-code"),
                "{}",
                e.id
            );
            // Agents with full guardrails are connected to the tool gateway.
            if e.guardrails == Guardrails::Full
                && !matches!(e.spec.adapter.as_str(), "opencode" | "claude-code")
            {
                assert!(text.contains("{gateway_url}"), "{}", e.id);
            }
        }
    }

    #[test]
    fn rejects_copyleft_and_secret_arguments() {
        let mut c = catalog();
        c.agents[0].license = "AGPL-3.0".into();
        assert!(c.validate().is_err());
        let mut c = catalog();
        c.agents[0].spec.args.push("--key={gateway_token}".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn instantiates_a_spec_for_a_provider_and_model() {
        let c = catalog();
        let codex = c.get("codex").unwrap();
        let spec = codex.instantiate(&Choice {
            name: "codex".into(),
            provider: "openai".into(),
            model: "gpt-5.1".into(),
            policy: Some("default".into()),
            description: None,
            image: None,
            options: Default::default(),
        });
        assert_eq!(spec.catalog.as_deref(), Some("codex"));
        assert_eq!(spec.model.as_deref(), Some("gpt-5.1"));
        assert_eq!(spec.provider.as_deref(), Some("openai"));
        assert!(spec.files.contains_key(".codex/config.toml"));
        assert!(spec.description.starts_with("Codex CLI"));
    }

    #[test]
    fn claude_code_is_bring_your_own_with_sign_in_choices() {
        let c = catalog();
        let entry = c.get("claude-code").unwrap();
        assert!(entry.bring_your_own);
        assert!(entry.protocols.contains(&ProviderKind::ClaudeSubscription));
        let mut choice = Choice {
            name: "claude".into(),
            provider: "max".into(),
            model: "claude-sonnet-4-5".into(),
            policy: None,
            description: None,
            image: None,
            options: Default::default(),
        };
        let spec = entry.instantiate(&choice);
        assert_eq!(spec.adapter, "claude-code");
        assert_eq!(spec.options["auth"], "gateway");
        assert_eq!(spec.provider.as_deref(), Some("max"));

        choice.options.insert("auth".into(), "login".into());
        choice.options.insert("bogus".into(), "x".into());
        choice.model.clear();
        assert!(entry.is_direct(&choice));
        let spec = entry.instantiate(&choice);
        assert_eq!(spec.options["auth"], "login");
        assert!(!spec.options.contains_key("bogus"));
        assert_eq!(spec.provider, None);
        assert_eq!(spec.model, None);
    }
}
