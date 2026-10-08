//! Agent adapters translate "run agent X on task Y" into a concrete process
//! launch inside a sandbox. Adding support for a new coding agent (or any AI
//! agent) means implementing [`AgentAdapter`]; nothing else in agentcore needs
//! to change.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ModelEndpoint, ProviderKind, SessionId};

/// Declarative agent definition, loaded from configuration or created from
/// the agent catalogue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpec {
    /// Unique name users refer to, e.g. `opencode`.
    pub name: String,
    /// Which adapter launches it, e.g. `command` or `opencode`.
    pub adapter: String,
    /// Human readable description shown in the UI.
    #[serde(default)]
    pub description: String,
    /// Container image override for this agent.
    #[serde(default)]
    pub image: Option<String>,
    /// Program to run (adapter specific default if omitted).
    #[serde(default)]
    pub command: Option<String>,
    /// Arguments; `{task}`, `{workspace}` and `{gateway_url}` are substituted.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables. Values may use the same placeholders.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Name of the policy applied when the session does not choose one.
    #[serde(default)]
    pub policy: Option<String>,
    /// Arguments for follow-up turns (`{task}` is the human's message), for
    /// CLI agents that can continue a conversation. Empty = single run.
    #[serde(default)]
    pub follow_up_args: Vec<String>,
    /// Run the agent in a pseudo-terminal, so its screen (colours, progress,
    /// interactive output) can be watched live. Default: true.
    #[serde(default)]
    pub tty: Option<bool>,
    /// Files written before the agent starts (path relative to its HOME, or
    /// absolute → content). Contents may use every placeholder, including
    /// `{gateway_token}`; they are not audited.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// Model provider (by name) the agent uses; see `{model_base_url}`.
    #[serde(default)]
    pub provider: Option<String>,
    /// Wire protocol to pick a provider by when `provider` is not set.
    #[serde(default)]
    pub protocol: Option<ProviderKind>,
    /// Model name, for `{model}`.
    #[serde(default)]
    pub model: Option<String>,
    /// Catalogue entry the agent was created from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<String>,
}

/// Placeholders that expand to secrets: allowed in `env` and `files` only,
/// never in arguments (those are recorded in the audit log).
pub const SECRET_PLACEHOLDERS: [&str; 1] = ["{gateway_token}"];

impl AgentSpec {
    /// Refuse specs that would put a secret into audited arguments.
    pub fn validate(&self) -> Result<(), AdapterError> {
        for arg in self
            .args
            .iter()
            .chain(&self.follow_up_args)
            .chain(&self.command)
        {
            if SECRET_PLACEHOLDERS.iter().any(|s| arg.contains(s)) {
                return Err(AdapterError::InvalidSpec(
                    self.name.clone(),
                    "`{gateway_token}` may only be used in `env` and `files`, never in arguments"
                        .into(),
                ));
            }
        }
        Ok(())
    }
}

/// Everything an adapter needs to know to build a launch plan.
#[derive(Debug, Clone)]
pub struct LaunchContext {
    pub session_id: SessionId,
    pub task: String,
    /// Workspace path as seen from inside the sandbox.
    pub workspace: String,
    /// The agent's `HOME` as seen from inside the sandbox.
    pub home: String,
    /// MCP tool gateway URL as seen from inside the sandbox.
    pub gateway_url: String,
    /// Bearer token scoped to this session's gateways (tools and models).
    pub gateway_token: String,
    /// Model gateway base URL as seen from inside the sandbox
    /// (`{base}/llm/{session}`); providers live below it.
    pub model_gateway_url: String,
    /// Model providers the agent may use through the gateway.
    pub models: Vec<ModelEndpoint>,
}

impl LaunchContext {
    /// Base URL for a provider, *without* an API version suffix
    /// (e.g. what the Anthropic SDKs expect in `ANTHROPIC_BASE_URL`).
    pub fn model_base_url(&self, provider: &str) -> String {
        format!("{}/{provider}", self.model_gateway_url)
    }

    /// The first enabled provider of a kind, used for standard SDK variables.
    pub fn model(&self, kind: ProviderKind) -> Option<&ModelEndpoint> {
        self.models.iter().find(|m| m.kind == kind)
    }

    /// Environment variables pointing standard SDKs at the model gateway.
    /// The "API key" is the session token: real keys never enter the sandbox.
    pub fn model_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert(
            "AGENTCORE_MODEL_GATEWAY_URL".into(),
            self.model_gateway_url.clone(),
        );
        if let Some(m) = self.model(ProviderKind::Anthropic) {
            env.insert("ANTHROPIC_BASE_URL".into(), self.model_base_url(&m.name));
            env.insert("ANTHROPIC_API_KEY".into(), self.gateway_token.clone());
        }
        if let Some(m) = self.model(ProviderKind::Openai) {
            env.insert(
                "OPENAI_BASE_URL".into(),
                format!("{}/v1", self.model_base_url(&m.name)),
            );
            env.insert("OPENAI_API_KEY".into(), self.gateway_token.clone());
        }
        env
    }

    pub fn substitute(&self, template: &str) -> String {
        // `{task}` last, so placeholders written in the task stay literal.
        template
            .replace("{workspace}", &self.workspace)
            .replace("{home}", &self.home)
            .replace("{gateway_url}", &self.gateway_url)
            .replace("{session_id}", &self.session_id.to_string())
            .replace("{task}", &self.task)
    }

    /// The model provider an agent uses: the one it names, else the first of
    /// its protocol, else the first one.
    pub fn endpoint(&self, spec: &AgentSpec) -> Option<&ModelEndpoint> {
        match (&spec.provider, spec.protocol) {
            (Some(name), _) => self.models.iter().find(|m| &m.name == name),
            (None, Some(kind)) => self.model(kind),
            (None, None) => self.models.first(),
        }
    }

    /// Expand every placeholder for an agent:
    ///
    /// * `{workspace}`, `{home}`, `{gateway_url}`, `{session_id}`, `{task}`;
    /// * `{model}`, `{provider}`: the agent's model and provider name;
    /// * `{protocol}`: the provider's kind (`anthropic`, `openai`);
    /// * `{model_base_url}`: the provider's base URL on the model gateway
    ///   (no version suffix), `{openai_base_url}`: the same with `/v1`;
    /// * with `secrets`: `{gateway_token}`, the session's token for both
    ///   gateways (it is the "API key" agents present to the model gateway).
    pub fn render(&self, spec: &AgentSpec, template: &str, secrets: bool) -> String {
        let endpoint = self.endpoint(spec);
        let base = endpoint
            .map(|e| self.model_base_url(&e.name))
            .unwrap_or_default();
        let mut out = template
            .replace("{model_base_url}", &base)
            .replace("{openai_base_url}", &format!("{base}/v1"))
            .replace("{provider}", endpoint.map_or("", |e| e.name.as_str()))
            .replace("{protocol}", endpoint.map_or("", |e| e.kind.as_str()))
            .replace("{model}", spec.model.as_deref().unwrap_or_default());
        if secrets {
            out = out.replace("{gateway_token}", &self.gateway_token);
        }
        self.substitute(&out)
    }
}

/// A concrete process to start inside the sandbox.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchPlan {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Allocate a pseudo-terminal (stdout and stderr merged).
    #[serde(default)]
    pub tty: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("agent `{0}` has invalid configuration: {1}")]
    InvalidSpec(String, String),
}

pub trait AgentAdapter: Send + Sync {
    /// Adapter identifier referenced by [`AgentSpec::adapter`].
    fn id(&self) -> &'static str;

    fn plan(&self, spec: &AgentSpec, ctx: &LaunchContext) -> Result<LaunchPlan, AdapterError>;

    /// Plan for a follow-up turn that continues the same conversation with a
    /// new message from a human. `None` means the agent cannot continue and
    /// the session ends after the first run.
    fn follow_up(
        &self,
        _spec: &AgentSpec,
        _ctx: &LaunchContext,
        _message: &str,
    ) -> Result<Option<LaunchPlan>, AdapterError> {
        Ok(None)
    }
}
