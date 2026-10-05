//! Agent adapters translate "run agent X on task Y" into a concrete process
//! launch inside a sandbox. Adding support for a new coding agent (or any AI
//! agent) means implementing [`AgentAdapter`]; nothing else in agentcore needs
//! to change.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ModelEndpoint, ProviderKind, SessionId};

/// Declarative agent definition, loaded from configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

/// Everything an adapter needs to know to build a launch plan.
#[derive(Debug, Clone)]
pub struct LaunchContext {
    pub session_id: SessionId,
    pub task: String,
    /// Workspace path as seen from inside the sandbox.
    pub workspace: String,
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
        template
            .replace("{task}", &self.task)
            .replace("{workspace}", &self.workspace)
            .replace("{gateway_url}", &self.gateway_url)
            .replace("{session_id}", &self.session_id.to_string())
    }
}

/// A concrete process to start inside the sandbox.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchPlan {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
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
}
