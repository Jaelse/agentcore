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
use agentcore_core::{AgentAdapter, AgentSpec, LaunchContext, LaunchPlan, ProviderKind};

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

/// Expand `{env:NAME}` references and every placeholder, secrets included.
/// Only used for environment values and files: arguments are recorded in the
/// audit log, so secrets must never be expanded into them.
pub fn expand(spec: &AgentSpec, template: &str, ctx: &LaunchContext) -> String {
    let mut out = ctx.render(spec, template, true);
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
        .map(|(k, v)| (k.clone(), expand(spec, v, ctx)))
        .collect()
}

/// Writes the files handed over in `AGENTCORE_FILE_<n>_PATH/_CONTENT`
/// (relative paths below `$HOME`), then runs the agent. Contents travel in
/// the environment, which is not audited, so they may hold the session token.
const WRITE_FILES: &str = r#"set -e
i=0
while [ "$i" -lt "${AGENTCORE_FILE_COUNT:-0}" ]; do
  eval "p=\${AGENTCORE_FILE_${i}_PATH}; c=\${AGENTCORE_FILE_${i}_CONTENT}"
  unset "AGENTCORE_FILE_${i}_PATH" "AGENTCORE_FILE_${i}_CONTENT"
  case "$p" in /*) ;; *) p="$HOME/$p" ;; esac
  mkdir -p "$(dirname "$p")"
  printf '%s' "$c" > "$p"
  i=$((i + 1))
done
unset AGENTCORE_FILE_COUNT
exec "$@""#;

/// A plan for `spec` with the given argument templates (first turn or
/// follow-up), writing the spec's files first.
fn command_plan(
    spec: &AgentSpec,
    ctx: &LaunchContext,
    args: &[String],
) -> Result<LaunchPlan, AdapterError> {
    spec.validate()?;
    let program = spec.command.clone().ok_or_else(|| {
        AdapterError::InvalidSpec(spec.name.clone(), "`command` is required".into())
    })?;
    let args: Vec<String> = args.iter().map(|a| ctx.render(spec, a, false)).collect();
    let mut env = expand_env(spec, ctx);
    if spec.files.is_empty() {
        return Ok(LaunchPlan {
            program: ctx.render(spec, &program, false),
            args,
            env,
            tty: spec.tty.unwrap_or(true),
        });
    }
    for (i, (path, content)) in spec.files.iter().enumerate() {
        env.insert(
            format!("AGENTCORE_FILE_{i}_PATH"),
            ctx.render(spec, path, false),
        );
        env.insert(
            format!("AGENTCORE_FILE_{i}_CONTENT"),
            expand(spec, content, ctx),
        );
    }
    env.insert("AGENTCORE_FILE_COUNT".into(), spec.files.len().to_string());
    let mut wrapped = vec![
        "-c".to_string(),
        WRITE_FILES.to_string(),
        "agentcore-files".to_string(),
        ctx.render(spec, &program, false),
    ];
    wrapped.extend(args);
    Ok(LaunchPlan {
        program: "sh".into(),
        args: wrapped,
        env,
        tty: spec.tty.unwrap_or(true),
    })
}

pub struct CommandAdapter;

impl AgentAdapter for CommandAdapter {
    fn id(&self) -> &'static str {
        "command"
    }

    fn follow_up(
        &self,
        spec: &AgentSpec,
        ctx: &LaunchContext,
        message: &str,
    ) -> Result<Option<LaunchPlan>, AdapterError> {
        if spec.follow_up_args.is_empty() {
            return Ok(None);
        }
        let follow_ctx = LaunchContext {
            task: message.to_string(),
            ..ctx.clone()
        };
        command_plan(spec, &follow_ctx, &spec.follow_up_args).map(Some)
    }

    fn plan(&self, spec: &AgentSpec, ctx: &LaunchContext) -> Result<LaunchPlan, AdapterError> {
        command_plan(spec, ctx, &spec.args)
    }
}

pub struct OpenCodeAdapter;

impl OpenCodeAdapter {
    /// Inline opencode configuration. Every native tool that touches files,
    /// runs commands or reaches the network is denied, so the model has to use
    /// the policy-checked gateway tools. Read-only tools are included: left
    /// enabled they would bypass path rules such as `deny-secrets`. Verified
    /// against opencode 1.18 with `opencode debug agent build`.
    ///
    /// opencode's `anthropic` and `openai` providers are pointed at the model
    /// gateway with the session token as API key, so the real keys stay in
    /// agentcore and every model call is logged.
    ///
    /// opencode's own `opencode` provider (OpenCode Zen, e.g. Big Pickle) is
    /// pointed at the gateway too, but *without* an `apiKey`: opencode then
    /// stays on Zen's free tier, sends `Bearer public`, and picks a free
    /// default model. The session token travels in `x-agentcore-token`.
    /// Verified against opencode 1.18.
    pub fn config(ctx: &LaunchContext) -> serde_json::Value {
        let mut providers = serde_json::Map::new();
        if let Some(zen) = ctx.model(ProviderKind::OpencodeZen) {
            providers.insert(
                "opencode".into(),
                serde_json::json!({ "options": {
                    "baseURL": format!("{}/v1", ctx.model_base_url(&zen.name)),
                    "headers": { "x-agentcore-token": ctx.gateway_token },
                }}),
            );
        }
        for kind in [ProviderKind::Anthropic, ProviderKind::Openai] {
            if let Some(endpoint) = ctx.model(kind) {
                providers.insert(
                    kind.as_str().into(),
                    serde_json::json!({ "options": {
                        // The AI SDK expects the version prefix in baseURL.
                        "baseURL": format!("{}/v1", ctx.model_base_url(&endpoint.name)),
                        "apiKey": ctx.gateway_token,
                    }}),
                );
            }
        }
        serde_json::json!({
            "provider": providers,
            "$schema": "https://opencode.ai/config.json",
            "autoupdate": false,
            "share": "disabled",
            "permission": {
                "read": "deny",
                "glob": "deny",
                "grep": "deny",
                "list": "deny",
                "edit": "deny",
                "bash": "deny",
                "webfetch": "deny",
                "websearch": "deny",
                "codesearch": "deny",
                "external_directory": "deny"
            },
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

    /// `opencode run --continue <message>` resumes the last opencode session
    /// in the sandbox (its state lives in the agent's HOME, which survives
    /// between turns because the sandbox does).
    fn follow_up(
        &self,
        spec: &AgentSpec,
        ctx: &LaunchContext,
        message: &str,
    ) -> Result<Option<LaunchPlan>, AdapterError> {
        let follow_ctx = LaunchContext {
            task: message.to_string(),
            ..ctx.clone()
        };
        let mut plan = self.plan(spec, &follow_ctx)?;
        match plan.args.iter().position(|a| a == "run") {
            Some(i) if !plan.args.iter().any(|a| a == "--continue" || a == "-c") => {
                plan.args.insert(i + 1, "--continue".into());
            }
            Some(_) => {}
            None => return Ok(None),
        }
        Ok(Some(plan))
    }

    fn plan(&self, spec: &AgentSpec, ctx: &LaunchContext) -> Result<LaunchPlan, AdapterError> {
        let args = if spec.args.is_empty() {
            vec!["run".into(), ctx.task.clone()]
        } else {
            spec.args
                .iter()
                .map(|a| ctx.render(spec, a, false))
                .collect()
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
            tty: spec.tty.unwrap_or(true),
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
            home: "/home/agent".into(),
            gateway_url: "http://gw/mcp/x".into(),
            gateway_token: "secret".into(),
            model_gateway_url: "http://gw/llm/x".into(),
            models: vec![agentcore_core::ModelEndpoint {
                name: "claude".into(),
                kind: ProviderKind::Anthropic,
            }],
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
            tty: None,
            follow_up_args: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn opencode_follow_up_continues_the_conversation() {
        let mut s = spec("opencode");
        s.command = None;
        s.args = vec![
            "run".into(),
            "--model".into(),
            "opencode/big-pickle".into(),
            "{task}".into(),
        ];
        let plan = OpenCodeAdapter
            .follow_up(&s, &ctx(), "now add tests")
            .unwrap()
            .unwrap();
        assert_eq!(
            plan.args,
            [
                "run",
                "--continue",
                "--model",
                "opencode/big-pickle",
                "now add tests"
            ]
        );
        s.args.clear();
        let plan = OpenCodeAdapter
            .follow_up(&s, &ctx(), "again")
            .unwrap()
            .unwrap();
        assert_eq!(plan.args, ["run", "--continue", "again"]);
        assert!(
            CommandAdapter
                .follow_up(&spec("command"), &ctx(), "x")
                .unwrap()
                .is_none()
        );
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
        for tool in ["read", "glob", "grep", "edit", "bash", "webfetch"] {
            assert_eq!(config["permission"][tool], "deny", "{tool}");
        }
        assert_eq!(config["mcp"]["agentcore"]["url"], "http://gw/mcp/x");
        let anthropic = &config["provider"]["anthropic"]["options"];
        assert_eq!(anthropic["baseURL"], "http://gw/llm/x/claude/v1");
        assert_eq!(anthropic["apiKey"], "secret");
        assert!(config["provider"].get("openai").is_none());
        assert!(config["provider"].get("opencode").is_none());

        let mut zen_ctx = ctx();
        zen_ctx.models = vec![agentcore_core::ModelEndpoint {
            name: "zen".into(),
            kind: ProviderKind::OpencodeZen,
        }];
        let zen = &OpenCodeAdapter::config(&zen_ctx)["provider"]["opencode"]["options"];
        assert_eq!(zen["baseURL"], "http://gw/llm/x/zen/v1");
        assert_eq!(zen["headers"]["x-agentcore-token"], "secret");
        assert!(
            zen.get("apiKey").is_none(),
            "an apiKey would unlock paid models"
        );

        let env = ctx().model_env();
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://gw/llm/x/claude");
        assert_eq!(env["ANTHROPIC_API_KEY"], "secret");
        assert!(!env.contains_key("OPENAI_API_KEY"));
    }
}
