//! Agent adapters translate "run agent X on task Y" into a concrete process
//! launch inside a sandbox. Adding support for a new coding agent (or any AI
//! agent) means implementing [`AgentAdapter`]; nothing else in agentcore needs
//! to change.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::SessionId;

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
    /// Bearer token scoped to this session's gateway.
    pub gateway_token: String,
}

impl LaunchContext {
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
