use serde::{Deserialize, Serialize};

/// A side effect an agent wants to perform. Every action is evaluated against
/// the session policy before it is executed in the sandbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// Run a program inside the sandbox.
    Exec {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Read a file inside the sandbox workspace.
    FileRead { path: String },
    /// Create or overwrite a file inside the sandbox workspace.
    FileWrite { path: String, bytes: usize },
    /// Open an outbound network connection.
    Network { host: String, port: Option<u16> },
    /// Any other tool, e.g. one exposed by a third-party MCP server.
    ToolCall {
        tool: String,
        #[serde(default)]
        arguments: serde_json::Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Exec,
    FileRead,
    FileWrite,
    Network,
    ToolCall,
}

impl Action {
    pub fn kind(&self) -> ActionKind {
        match self {
            Self::Exec { .. } => ActionKind::Exec,
            Self::FileRead { .. } => ActionKind::FileRead,
            Self::FileWrite { .. } => ActionKind::FileWrite,
            Self::Network { .. } => ActionKind::Network,
            Self::ToolCall { .. } => ActionKind::ToolCall,
        }
    }

    /// Full command line for an exec action (`command arg1 arg2 ...`).
    pub fn command_line(&self) -> Option<String> {
        match self {
            Self::Exec { command, args, .. } if args.is_empty() => Some(command.clone()),
            Self::Exec { command, args, .. } => Some(format!("{command} {}", args.join(" "))),
            _ => None,
        }
    }

    /// One-line human readable summary used in the UI and approval prompts.
    pub fn summary(&self) -> String {
        match self {
            Self::Exec { .. } => format!("exec `{}`", self.command_line().unwrap_or_default()),
            Self::FileRead { path } => format!("read {path}"),
            Self::FileWrite { path, bytes } => format!("write {path} ({bytes} bytes)"),
            Self::Network {
                host,
                port: Some(p),
            } => format!("connect {host}:{p}"),
            Self::Network { host, port: None } => format!("connect {host}"),
            Self::ToolCall { tool, .. } => format!("tool {tool}"),
        }
    }
}

impl std::fmt::Display for ActionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Exec => "exec",
            Self::FileRead => "file_read",
            Self::FileWrite => "file_write",
            Self::Network => "network",
            Self::ToolCall => "tool_call",
        })
    }
}

/// The result of evaluating an action against a policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Allow {
        rule: Option<String>,
    },
    Deny {
        rule: Option<String>,
        reason: String,
    },
    RequireApproval {
        rule: Option<String>,
        reason: String,
    },
}

impl Verdict {
    pub fn rule(&self) -> Option<&str> {
        match self {
            Self::Allow { rule } | Self::Deny { rule, .. } | Self::RequireApproval { rule, .. } => {
                rule.as_deref()
            }
        }
    }
}

/// What happened when an action was (or was not) carried out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ActionOutcome {
    Succeeded {
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        output: serde_json::Value,
    },
    Failed {
        error: String,
    },
    Denied {
        reason: String,
    },
}
