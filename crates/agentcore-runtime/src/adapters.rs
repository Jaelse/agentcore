//! Built-in agent adapters.
//!
//! * `command`: run any program. Works for every CLI agent; the task is passed
//!   through `{task}` placeholders in `args`.
//! * `opencode`: runs [opencode](https://opencode.ai) headless with its native
//!   side-effecting tools disabled and the agentcore MCP gateway registered,
//!   so every edit and command goes through policy and approval.
//! * `claude-code`: runs [Claude Code](https://code.claude.com) headless or
//!   interactively, signed in with an API key or a Claude subscription
//!   through the model gateway, or logged in by a person in the live
//!   terminal, or on Amazon Bedrock / Google Vertex AI / Microsoft Foundry.
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
        registry.register(Arc::new(ClaudeCodeAdapter));
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
/// With `AGENTCORE_FILE_<n>_KEEP` set, an existing file is left as it is
/// (state the agent keeps between turns, such as a login).
const WRITE_FILES: &str = r#"set -e
i=0
while [ "$i" -lt "${AGENTCORE_FILE_COUNT:-0}" ]; do
  eval "p=\${AGENTCORE_FILE_${i}_PATH}; c=\${AGENTCORE_FILE_${i}_CONTENT}; k=\${AGENTCORE_FILE_${i}_KEEP:-}"
  unset "AGENTCORE_FILE_${i}_PATH" "AGENTCORE_FILE_${i}_CONTENT" "AGENTCORE_FILE_${i}_KEEP"
  case "$p" in /*) ;; *) p="$HOME/$p" ;; esac
  mkdir -p "$(dirname "$p")"
  if [ -z "$k" ] || [ ! -e "$p" ]; then printf '%s' "$c" > "$p"; fi
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
            direct_model_access: false,
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
        direct_model_access: false,
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
        Self::config_with_model(ctx, None)
    }

    /// [`Self::config`] that also lists `model` under the `qwen` provider:
    /// opencode only knows the models a custom provider declares, and the
    /// agent's model may be one agentcore has no entry for.
    pub fn config_with_model(ctx: &LaunchContext, model: Option<&str>) -> serde_json::Value {
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
        // Qwen (DashScope) speaks the OpenAI chat-completions protocol only,
        // so it goes through opencode's OpenAI-compatible provider, not the
        // built-in `openai` one (which uses the Responses API).
        if let Some(qwen) = ctx.model(ProviderKind::Qwen) {
            let models: serde_json::Map<String, serde_json::Value> = ProviderKind::Qwen
                .known_models()
                .iter()
                .map(|m| m.id)
                .chain(model)
                .map(|id| (id.into(), serde_json::json!({ "name": id })))
                .collect();
            providers.insert(
                "qwen".into(),
                serde_json::json!({
                    "npm": "@ai-sdk/openai-compatible",
                    "name": "Qwen",
                    "options": {
                        "baseURL": format!("{}/v1", ctx.model_base_url(&qwen.name)),
                        "apiKey": ctx.gateway_token,
                    },
                    "models": models,
                }),
            );
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
            Self::config_with_model(ctx, spec.model.as_deref()).to_string(),
        );
        Ok(LaunchPlan {
            program: spec.command.clone().unwrap_or_else(|| "opencode".into()),
            args,
            env,
            tty: spec.tty.unwrap_or(true),
            direct_model_access: false,
        })
    }
}

/// How Claude Code signs in (`options.auth`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeAuth {
    /// Through the model gateway, with the agent's provider: an Anthropic
    /// API key (`anthropic`) or a Claude subscription token from
    /// `claude setup-token` (`claude_subscription`). Real credentials stay
    /// in agentcore and every model call is recorded.
    Gateway,
    /// A person logs Claude Code in from the live terminal (`/login`: Claude
    /// subscription, Anthropic Console or SSO). The login lives in the
    /// sandbox for the session; model calls go straight to Anthropic.
    Login,
    /// Amazon Bedrock, with credentials from the agent's `env`.
    Bedrock,
    /// Google Vertex AI, with credentials from the agent's `env`.
    Vertex,
    /// Microsoft Foundry, with credentials from the agent's `env`.
    Foundry,
}

impl ClaudeAuth {
    pub const ALL: [&'static str; 5] = ["gateway", "login", "bedrock", "vertex", "foundry"];

    pub fn parse(spec: &AgentSpec) -> Result<Self, AdapterError> {
        Ok(match spec.options.get("auth").map(String::as_str) {
            None | Some("gateway") => Self::Gateway,
            Some("login") => Self::Login,
            Some("bedrock") => Self::Bedrock,
            Some("vertex") => Self::Vertex,
            Some("foundry") => Self::Foundry,
            Some(other) => {
                return Err(AdapterError::InvalidSpec(
                    spec.name.clone(),
                    format!(
                        "unknown Claude Code auth `{other}` (one of: {})",
                        Self::ALL.join(", ")
                    ),
                ));
            }
        })
    }

    /// Claude Code reaches its model provider itself.
    pub fn is_direct(self) -> bool {
        self != Self::Gateway
    }

    /// The variable switching Claude Code to a cloud provider.
    fn provider_switch(self) -> Option<&'static str> {
        match self {
            Self::Bedrock => Some("CLAUDE_CODE_USE_BEDROCK"),
            Self::Vertex => Some("CLAUDE_CODE_USE_VERTEX"),
            Self::Foundry => Some("CLAUDE_CODE_USE_FOUNDRY"),
            Self::Gateway | Self::Login => None,
        }
    }
}

/// Built-in tools that touch files, run code or reach the network: denied
/// when Claude Code works through the gateway tools, in case `--tools ""`
/// ever lets one through.
const CLAUDE_NATIVE_TOOLS: [&str; 11] = [
    "Bash",
    "BashOutput",
    "KillShell",
    "Read",
    "Write",
    "Edit",
    "NotebookEdit",
    "Glob",
    "Grep",
    "WebFetch",
    "WebSearch",
];

pub struct ClaudeCodeAdapter;

impl ClaudeCodeAdapter {
    /// `options.mode`: `headless` (`claude -p`, one run per turn) or
    /// `interactive` (Claude Code's own screen, which people can type into
    /// from the live view; the turn ends when they exit it). Logging in
    /// needs the screen, so `auth = "login"` is always interactive.
    fn interactive(spec: &AgentSpec, auth: ClaudeAuth) -> Result<bool, AdapterError> {
        match spec.options.get("mode").map(String::as_str) {
            _ if auth == ClaudeAuth::Login => Ok(true),
            None | Some("headless") => Ok(false),
            Some("interactive") => Ok(true),
            Some(other) => Err(AdapterError::InvalidSpec(
                spec.name.clone(),
                format!("unknown Claude Code mode `{other}` (headless or interactive)"),
            )),
        }
    }

    /// `options.tools`: `gateway` (default; Claude Code's own tools are off
    /// and it works through agentcore's policy-checked tools) or `native`
    /// (its own tools, inside the sandbox).
    fn native_tools(spec: &AgentSpec) -> Result<bool, AdapterError> {
        match spec.options.get("tools").map(String::as_str) {
            None | Some("gateway") => Ok(false),
            Some("native") => Ok(true),
            Some(other) => Err(AdapterError::InvalidSpec(
                spec.name.clone(),
                format!("unknown Claude Code tools `{other}` (gateway or native)"),
            )),
        }
    }

    /// User settings (`~/.claude/settings.json`). Project settings in the
    /// repository are not loaded: their hooks would run commands outside
    /// the policy.
    pub fn settings(native: bool) -> serde_json::Value {
        let mut permissions = serde_json::json!({ "allow": ["mcp__agentcore"] });
        if !native {
            permissions["deny"] = serde_json::json!(CLAUDE_NATIVE_TOOLS);
        }
        serde_json::json!({
            "permissions": permissions,
            "disableAllHooks": true,
            "includeCoAuthoredBy": false,
            "env": {
                "DISABLE_AUTOUPDATER": "1",
                "DISABLE_TELEMETRY": "1",
                "DISABLE_ERROR_REPORTING": "1",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"
            }
        })
    }

    /// `~/.claude.json`: onboarding done and the workspace trusted, so the
    /// interactive screen starts with the task, not with setup questions.
    /// The session token is pre-approved when it is the "API key".
    ///
    /// With `auth = "login"` onboarding is left on: it is where Claude Code
    /// asks how to log in.
    pub fn state(ctx: &LaunchContext, workspace: &str, auth: ClaudeAuth) -> serde_json::Value {
        let tail: String = {
            let chars: Vec<char> = ctx.gateway_token.chars().collect();
            chars[chars.len().saturating_sub(20)..].iter().collect()
        };
        serde_json::json!({
            "hasCompletedOnboarding": auth != ClaudeAuth::Login,
            "bypassPermissionsModeAccepted": true,
            "autoUpdates": false,
            "customApiKeyResponses": { "approved": [tail], "rejected": [] },
            "projects": {
                workspace: {
                    "hasTrustDialogAccepted": true,
                    "hasCompletedProjectOnboarding": true
                }
            }
        })
    }

    /// The agentcore tool gateway as Claude Code's only MCP server.
    pub fn mcp_config(ctx: &LaunchContext) -> serde_json::Value {
        serde_json::json!({ "mcpServers": { "agentcore": {
            "type": "http",
            "url": ctx.gateway_url,
            "headers": { "Authorization": format!("Bearer {}", ctx.gateway_token) }
        }}})
    }

    fn build(
        &self,
        spec: &AgentSpec,
        ctx: &LaunchContext,
        follow_up: bool,
    ) -> Result<LaunchPlan, AdapterError> {
        spec.validate()?;
        let auth = ClaudeAuth::parse(spec)?;
        let interactive = Self::interactive(spec, auth)?;
        let native = Self::native_tools(spec)?;
        let mcp_path = format!("{}/.agentcore/claude-mcp.json", ctx.home);

        let mut args: Vec<String> = Vec::new();
        if !interactive {
            args.push("-p".into());
        }
        if follow_up {
            args.push("--continue".into());
        }
        if let Some(model) = spec.model.as_deref().filter(|m| !m.trim().is_empty()) {
            args.extend(["--model".into(), ctx.render(spec, model, false)]);
        }
        args.extend([
            "--mcp-config".into(),
            mcp_path.clone(),
            "--strict-mcp-config".into(),
            "--setting-sources".into(),
            "user".into(),
        ]);
        if native {
            args.extend(["--permission-mode".into(), "bypassPermissions".into()]);
        } else {
            args.extend([
                "--tools".into(),
                String::new(),
                "--permission-mode".into(),
                "dontAsk".into(),
                "--allowedTools".into(),
                "mcp__agentcore".into(),
            ]);
        }
        // Extra arguments from the agent definition, before the prompt.
        args.extend(spec.args.iter().map(|a| ctx.render(spec, a, false)));
        if !ctx.task.is_empty() {
            // `--mcp-config` and `--allowedTools` take several values: end
            // the options so they do not swallow the prompt (or a prompt
            // starting with `-` is not read as an option).
            args.push("--".into());
            args.push(ctx.task.clone());
        }

        let mut env = expand_env(spec, ctx);
        if let Some(switch) = auth.provider_switch() {
            env.insert(switch.into(), "1".into());
        }
        let files: Vec<(String, String, bool)> = vec![
            (
                ".claude/settings.json".into(),
                Self::settings(native).to_string(),
                false,
            ),
            // Kept once written: Claude Code records a login here.
            (
                ".claude.json".into(),
                Self::state(ctx, &ctx.workdir, auth).to_string(),
                true,
            ),
            (mcp_path, Self::mcp_config(ctx).to_string(), false),
        ];
        for (i, (path, content, keep)) in files.iter().enumerate() {
            env.insert(format!("AGENTCORE_FILE_{i}_PATH"), path.clone());
            env.insert(format!("AGENTCORE_FILE_{i}_CONTENT"), content.clone());
            if *keep {
                env.insert(format!("AGENTCORE_FILE_{i}_KEEP"), "1".into());
            }
        }
        env.insert("AGENTCORE_FILE_COUNT".into(), files.len().to_string());

        let mut wrapped = vec![
            "-c".to_string(),
            WRITE_FILES.to_string(),
            "agentcore-files".to_string(),
            spec.command.clone().unwrap_or_else(|| "claude".into()),
        ];
        wrapped.extend(args);
        Ok(LaunchPlan {
            program: "sh".into(),
            args: wrapped,
            env,
            // The interactive screen needs a terminal; headless output reads
            // better in one too.
            tty: if interactive {
                true
            } else {
                spec.tty.unwrap_or(true)
            },
            direct_model_access: auth.is_direct(),
        })
    }
}

impl AgentAdapter for ClaudeCodeAdapter {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn direct_model_access(&self, spec: &AgentSpec) -> bool {
        ClaudeAuth::parse(spec).is_ok_and(ClaudeAuth::is_direct)
    }

    /// `claude --continue <message>` resumes the last conversation in the
    /// workspace (kept in the agent's HOME, which lives as long as the
    /// sandbox).
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
        self.build(spec, &follow_ctx, true).map(Some)
    }

    fn plan(&self, spec: &AgentSpec, ctx: &LaunchContext) -> Result<LaunchPlan, AdapterError> {
        self.build(spec, ctx, false)
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
            workdir: "/workspace".into(),
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

    fn claude(options: &[(&str, &str)]) -> AgentSpec {
        let mut s = spec("claude-code");
        s.command = None;
        s.args.clear();
        s.env.clear();
        s.model = Some("claude-sonnet-4-5".into());
        s.options = options
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        s
    }

    fn file(plan: &LaunchPlan, path: &str) -> serde_json::Value {
        let i = (0..)
            .find(|i| {
                plan.env
                    .get(&format!("AGENTCORE_FILE_{i}_PATH"))
                    .is_none_or(|p| p.ends_with(path))
            })
            .unwrap();
        serde_json::from_str(&plan.env[&format!("AGENTCORE_FILE_{i}_CONTENT")]).unwrap()
    }

    #[test]
    fn claude_code_works_through_the_gateway_tools_by_default() {
        let plan = ClaudeCodeAdapter.plan(&claude(&[]), &ctx()).unwrap();
        assert_eq!(plan.program, "sh");
        let args = &plan.args[3..];
        assert_eq!(args[0], "claude");
        assert_eq!(args[1], "-p");
        assert!(
            args.windows(2)
                .any(|w| w == ["--model", "claude-sonnet-4-5"])
        );
        assert!(args.windows(2).any(|w| w == ["--tools", ""]));
        assert!(args.windows(2).any(|w| w == ["--setting-sources", "user"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--permission-mode", "dontAsk"])
        );
        assert!(args.contains(&"--strict-mcp-config".to_string()));
        assert_eq!(&args[args.len() - 2..], ["--", "fix the bug"]);
        // The session token never reaches the (audited) arguments.
        assert!(!plan.args.iter().any(|a| a.contains("secret")));
        assert!(!plan.direct_model_access);

        let mcp = file(&plan, "claude-mcp.json");
        assert_eq!(mcp["mcpServers"]["agentcore"]["url"], "http://gw/mcp/x");
        assert_eq!(
            mcp["mcpServers"]["agentcore"]["headers"]["Authorization"],
            "Bearer secret"
        );
        let settings = file(&plan, ".claude/settings.json");
        assert_eq!(settings["disableAllHooks"], true);
        for tool in ["Bash", "Edit", "Write", "Read", "WebFetch"] {
            assert!(
                settings["permissions"]["deny"]
                    .as_array()
                    .unwrap()
                    .contains(&tool.into()),
                "{tool}"
            );
        }
        let state = file(&plan, ".claude.json");
        assert_eq!(
            state["projects"]["/workspace"]["hasTrustDialogAccepted"],
            true
        );
        assert_eq!(state["customApiKeyResponses"]["approved"][0], "secret");

        let next = ClaudeCodeAdapter
            .follow_up(&claude(&[]), &ctx(), "now add tests")
            .unwrap()
            .unwrap();
        let args = &next.args[3..];
        assert_eq!(&args[..3], ["claude", "-p", "--continue"]);
        assert_eq!(&args[args.len() - 2..], ["--", "now add tests"]);
    }

    #[test]
    fn claude_code_native_tools_and_interactive_mode() {
        let plan = ClaudeCodeAdapter
            .plan(
                &claude(&[("tools", "native"), ("mode", "interactive")]),
                &ctx(),
            )
            .unwrap();
        let args = &plan.args[3..];
        assert!(!args.contains(&"-p".to_string()));
        assert!(!args.contains(&"--tools".to_string()));
        assert!(
            args.windows(2)
                .any(|w| w == ["--permission-mode", "bypassPermissions"])
        );
        assert!(plan.tty);
        let settings = file(&plan, ".claude/settings.json");
        assert!(settings["permissions"].get("deny").is_none());
    }

    #[test]
    fn claude_code_login_and_cloud_providers_bring_their_own_credentials() {
        let login = claude(&[("auth", "login")]);
        assert!(ClaudeCodeAdapter.direct_model_access(&login));
        let plan = ClaudeCodeAdapter.plan(&login, &ctx()).unwrap();
        assert!(plan.direct_model_access);
        assert!(plan.tty);
        assert!(
            !plan.args.contains(&"-p".to_string()),
            "login needs the screen"
        );
        // The login Claude Code stores in ~/.claude.json survives later turns.
        let i = (0..)
            .find(|i| plan.env[&format!("AGENTCORE_FILE_{i}_PATH")] == ".claude.json")
            .unwrap();
        assert_eq!(plan.env[&format!("AGENTCORE_FILE_{i}_KEEP")], "1");
        assert_eq!(file(&plan, ".claude.json")["hasCompletedOnboarding"], false);
        // The screen is needed to log in, whatever the mode says.
        let plan = ClaudeCodeAdapter
            .plan(&claude(&[("auth", "login"), ("mode", "headless")]), &ctx())
            .unwrap();
        assert!(!plan.args.contains(&"-p".to_string()));

        let plan = ClaudeCodeAdapter
            .plan(&claude(&[("auth", "bedrock")]), &ctx())
            .unwrap();
        assert_eq!(plan.env["CLAUDE_CODE_USE_BEDROCK"], "1");
        assert!(plan.direct_model_access);
        assert!(plan.args.contains(&"-p".to_string()));
        for (auth, var) in [
            ("vertex", "CLAUDE_CODE_USE_VERTEX"),
            ("foundry", "CLAUDE_CODE_USE_FOUNDRY"),
        ] {
            let plan = ClaudeCodeAdapter
                .plan(&claude(&[("auth", auth)]), &ctx())
                .unwrap();
            assert_eq!(plan.env[var], "1");
        }
        assert!(
            ClaudeCodeAdapter
                .plan(&claude(&[("auth", "magic")]), &ctx())
                .is_err()
        );
        assert!(!ClaudeCodeAdapter.direct_model_access(&claude(&[])));
    }

    #[test]
    fn claude_subscription_is_a_bearer_token_for_its_agent_only() {
        let mut c = ctx();
        c.models.push(agentcore_core::ModelEndpoint {
            name: "max".into(),
            kind: ProviderKind::ClaudeSubscription,
        });
        let mut s = claude(&[]);
        s.provider = Some("max".into());
        let env = c.model_env_for(Some(&s));
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://gw/llm/x/max");
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "secret");
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
        // Other agents keep the API-key provider.
        let env = c.model_env();
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://gw/llm/x/claude");
        assert_eq!(env["ANTHROPIC_API_KEY"], "secret");
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

        let mut qwen_ctx = ctx();
        qwen_ctx.models = vec![agentcore_core::ModelEndpoint {
            name: "dashscope".into(),
            kind: ProviderKind::Qwen,
        }];
        let qwen = &OpenCodeAdapter::config(&qwen_ctx)["provider"]["qwen"];
        assert_eq!(qwen["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(qwen["options"]["baseURL"], "http://gw/llm/x/dashscope/v1");
        assert_eq!(qwen["options"]["apiKey"], "secret");
        assert!(qwen["models"].get("qwen3-coder-plus").is_some());
        let custom = OpenCodeAdapter::config_with_model(&qwen_ctx, Some("qwen-turbo-latest"));
        assert!(
            custom["provider"]["qwen"]["models"]
                .get("qwen-turbo-latest")
                .is_some()
        );

        let env = ctx().model_env();
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://gw/llm/x/claude");
        assert_eq!(env["ANTHROPIC_API_KEY"], "secret");
        assert!(!env.contains_key("OPENAI_API_KEY"));
    }
}
