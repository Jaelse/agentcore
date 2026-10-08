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
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
            Self::OpencodeZen => "opencode_zen",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::Openai => "https://api.openai.com",
            Self::OpencodeZen => "https://opencode.ai/zen",
        }
    }
}

impl ProviderKind {
    /// API key used when none is given: OpenCode Zen's free tier is keyless.
    pub fn default_api_key(self) -> Option<&'static str> {
        match self {
            Self::OpencodeZen => Some("public"),
            Self::Anthropic | Self::Openai => None,
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
