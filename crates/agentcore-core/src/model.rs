//! Types shared by the model gateway, the runtime and the store.

use serde::{Deserialize, Serialize};

/// Wire protocol of an upstream model provider. Decides how the real API key
/// is attached and how token usage is read from responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Anthropic Messages API (`x-api-key` header).
    Anthropic,
    /// OpenAI and OpenAI-compatible APIs (`Authorization: Bearer`).
    Openai,
    /// OpenCode Zen, opencode's hosted model service (OpenAI-compatible).
    /// Its free models (e.g. Big Pickle) accept the API key `public`.
    OpencodeZen,
    /// Alibaba Cloud Model Studio (DashScope) serving Qwen models through its
    /// OpenAI-compatible endpoint (`Authorization: Bearer`).
    Qwen,
    /// A Claude Pro/Max/Team/Enterprise subscription, used by Claude Code
    /// only. The "API key" is a long-lived OAuth token from
    /// `claude setup-token`; it is sent as `Authorization: Bearer` with the
    /// OAuth beta header, on the Anthropic Messages API.
    ClaudeSubscription,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
            Self::OpencodeZen => "opencode_zen",
            Self::Qwen => "qwen",
            Self::ClaudeSubscription => "claude_subscription",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Anthropic | Self::ClaudeSubscription => "https://api.anthropic.com",
            Self::Openai => "https://api.openai.com",
            Self::OpencodeZen => "https://opencode.ai/zen",
            Self::Qwen => "https://dashscope-intl.aliyuncs.com/compatible-mode",
        }
    }
}

impl ProviderKind {
    /// Speaks the Anthropic Messages API (usage parsing, streaming).
    pub fn is_anthropic(self) -> bool {
        matches!(self, Self::Anthropic | Self::ClaudeSubscription)
    }

    /// Only the given adapter may use providers of this kind: a Claude
    /// subscription is licensed for Claude Code, not for other agents.
    pub fn restricted_to_adapter(self) -> Option<&'static str> {
        match self {
            Self::ClaudeSubscription => Some("claude-code"),
            _ => None,
        }
    }

    /// API key used when none is given: OpenCode Zen's free tier is keyless.
    pub fn default_api_key(self) -> Option<&'static str> {
        match self {
            Self::OpencodeZen => Some("public"),
            Self::Anthropic | Self::Openai | Self::Qwen | Self::ClaudeSubscription => None,
        }
    }
}

/// A model a provider is known to serve, offered when adding an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct KnownModel {
    pub id: &'static str,
    pub description: &'static str,
}

impl ProviderKind {
    /// Models offered in the "add agent" dialog. Empty for providers whose
    /// models change too fast to list (any model name can still be typed).
    /// Which of them an account can call depends on its region and plan.
    pub fn known_models(self) -> &'static [KnownModel] {
        match self {
            Self::Qwen => &[
                KnownModel {
                    id: "qwen3-coder-plus",
                    description: "Qwen's flagship coding model: agentic coding and tool use, very long context. The best default for opencode.",
                },
                KnownModel {
                    id: "qwen3-coder-flash",
                    description: "Faster, cheaper coding model for simple edits and quick iterations.",
                },
                KnownModel {
                    id: "qwen3-max",
                    description: "Qwen's most capable general model: hard reasoning, planning, long documents. Slower and more expensive.",
                },
                KnownModel {
                    id: "qwen-plus",
                    description: "Balanced general model: good quality for its price, long context.",
                },
                KnownModel {
                    id: "qwen-flash",
                    description: "Fastest and cheapest general model: simple tasks, summaries, classification.",
                },
                KnownModel {
                    id: "qwen-max",
                    description: "Previous-generation top general model: strong reasoning, shorter context than qwen3-max.",
                },
            ],
            Self::Anthropic | Self::Openai | Self::OpencodeZen | Self::ClaudeSubscription => &[],
        }
    }
}

impl std::str::FromStr for ProviderKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "anthropic" => Ok(Self::Anthropic),
            "openai" => Ok(Self::Openai),
            "opencode_zen" => Ok(Self::OpencodeZen),
            "qwen" => Ok(Self::Qwen),
            "claude_subscription" => Ok(Self::ClaudeSubscription),
            other => Err(format!("unknown provider kind `{other}`")),
        }
    }
}

/// A model provider reachable through the gateway, as handed to agents.
/// Contains no credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEndpoint {
    /// Provider name, used in the gateway URL (`/llm/{session}/{name}/...`).
    pub name: String,
    pub kind: ProviderKind,
}

/// How a model call through the gateway ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCallOutcome {
    /// Upstream answered (any HTTP status) and the response was relayed.
    Completed,
    /// Refused by agentcore (model not allowed, limit reached, ...).
    Rejected,
    /// The upstream request failed (network error, timeout).
    UpstreamError,
    /// The session was stopped while the response was streaming.
    Aborted,
}
