use serde::{Deserialize, Serialize};

/// Who caused something to happen. Recorded on every human-relevant event so
/// the audit trail can answer "who approved / stopped this?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Principal {
    /// An authenticated human operator.
    Human(String),
    /// The AI agent running inside a session.
    Agent(String),
    /// agentcore itself (timeouts, limits, policy enforcement).
    System,
}

impl Principal {
    pub fn human(name: impl Into<String>) -> Self {
        Self::Human(name.into())
    }
}

impl std::fmt::Display for Principal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Human(id) => write!(f, "human:{id}"),
            Self::Agent(id) => write!(f, "agent:{id}"),
            Self::System => f.write_str("system"),
        }
    }
}
