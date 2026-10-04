//! Built-in agent adapters.
//!
//! * `command`: run any program. Works for every CLI agent; the task is passed
//!   through `{task}` placeholders in `args`.
//! * `opencode`: runs [opencode](https://opencode.ai) headless with its native
//!   side-effecting tools disabled and the agentcore MCP gateway registered,
//!   so every edit and command goes through policy and approval.
//!
//! Environment values may reference variables of the agentcore process with
//! `{env:NAME}`; nothing from the host environment is passed implicitly.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use agentcore_core::agent::AdapterError;
use agentcore_core::{AgentAdapter, AgentSpec, LaunchContext, LaunchPlan};

pub struct AdapterRegistry {
    adapters: HashMap<&'static str, Arc<dyn AgentAdapter>>,
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        let mut registry = Self {
            adapters: HashMap::new(),
        };
        registry.register(Arc::new(CommandAdapter));
        registry.register(Arc::new(OpenCodeAdapter));
        registry
    }
}

impl AdapterRegistry {
    pub fn register(&mut self, adapter: Arc<dyn AgentAdapter>) {
        self.adapters.insert(adapter.id(), adapter);
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn AgentAdapter>> {
        self.adapters.get(id).cloned()
    }
}

/// Expand `{env:NAME}` references and the standard placeholders. Only used for
/// environment values: arguments are recorded in the audit log, so secrets
/// must never be expanded into them.
pub fn expand(template: &str, ctx: &LaunchContext) -> String {
    let mut out = ctx.substitute(template);
    while let Some(start) = out.find("{env:") {
        let Some(len) = out[start..].find('}') else {
            break;
        };
        let name = &out[start + 5..start + len];
        let value = std::env::var(name).unwrap_or_default();
        out.replace_range(start..=start + len, &value);
    }
    out
}

fn expand_env(spec: &AgentSpec, ctx: &LaunchContext) -> BTreeMap<String, String> {
    spec.env
        .iter()
        .map(|(k, v)| (k.clone(), expand(v, ctx)))
        .collect()
}

pub struct CommandAdapter;

impl AgentAdapter for CommandAdapter {
    fn id(&self) -> &'static str {
        "command"
    }

    fn plan(&self, spec: &AgentSpec, ctx: &LaunchContext) -> Result<LaunchPlan, AdapterError> {
        let program = spec.command.clone().ok_or_else(|| {
            AdapterError::InvalidSpec(spec.name.clone(), "`command` is required".into())
        })?;
        Ok(LaunchPlan {
            program,
            args: spec.args.iter().map(|a| ctx.substitute(a)).collect(),
            env: expand_env(spec, ctx),
        })
    }
}

pub struct OpenCodeAdapter;

impl OpenCodeAdapter {
    /// Inline opencode configuration. Native tools that cause side effects are
    /// denied so the model has to use the policy-checked gateway tools.
    pub fn config(ctx: &LaunchContext) -> serde_json::Value {
        serde_json::json!({
            "$schema": "https://opencode.ai/config.json",
            "autoupdate": false,
            "share": "disabled",
            "permission": { "edit": "deny", "bash": "deny", "webfetch": "deny" },
            "mcp": {
                "agentcore": {
                    "type": "remote",
                    "url": ctx.gateway_url,
                    "enabled": true,
                    "headers": { "Authorization": format!("Bearer {}", ctx.gateway_token) }
                }
            }
        })
    }
}

impl AgentAdapter for OpenCodeAdapter {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn plan(&self, spec: &AgentSpec, ctx: &LaunchContext) -> Result<LaunchPlan, AdapterError> {
        let args = if spec.args.is_empty() {
            vec!["run".into(), ctx.task.clone()]
        } else {
            spec.args.iter().map(|a| ctx.substitute(a)).collect()
        };
        let mut env = expand_env(spec, ctx);
        env.insert(
            "OPENCODE_CONFIG_CONTENT".into(),
            Self::config(ctx).to_string(),
        );
        Ok(LaunchPlan {
            program: spec.command.clone().unwrap_or_else(|| "opencode".into()),
            args,
            env,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> LaunchContext {
        LaunchContext {
            session_id: uuid::Uuid::nil(),
            task: "fix the bug".into(),
            workspace: "/workspace".into(),
            gateway_url: "http://gw/mcp/x".into(),
            gateway_token: "secret".into(),
        }
    }

    fn spec(adapter: &str) -> AgentSpec {
        AgentSpec {
            name: "a".into(),
            adapter: adapter.into(),
            description: String::new(),
            image: None,
            command: Some("my-agent".into()),
            args: vec!["--task".into(), "{task}".into()],
            env: [("KEY".to_string(), "{env:AGENTCORE_TEST_VAR}-x".to_string())].into(),
            policy: None,
        }
    }

    #[test]
    fn command_adapter_substitutes() {
        let plan = CommandAdapter.plan(&spec("command"), &ctx()).unwrap();
        assert_eq!(plan.program, "my-agent");
        assert_eq!(plan.args, ["--task", "fix the bug"]);
        assert_eq!(plan.env["KEY"], "-x");
    }

    #[test]
    fn opencode_routes_tools_through_gateway() {
        let mut s = spec("opencode");
        s.args.clear();
        s.command = None;
        let plan = OpenCodeAdapter.plan(&s, &ctx()).unwrap();
        assert_eq!(plan.program, "opencode");
        assert_eq!(plan.args, ["run", "fix the bug"]);
        let config: serde_json::Value =
            serde_json::from_str(&plan.env["OPENCODE_CONFIG_CONTENT"]).unwrap();
        assert_eq!(config["permission"]["bash"], "deny");
        assert_eq!(config["mcp"]["agentcore"]["url"], "http://gw/mcp/x");
    }
}
