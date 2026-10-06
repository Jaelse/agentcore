//! `agentcore.toml` configuration file.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use agentcore_core::AgentSpec;
use agentcore_sandbox::SandboxConfig;
use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub model_gateway: ModelGatewayConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub policies: PoliciesConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub transparency: Transparency,
    #[serde(default)]
    pub agents: Vec<AgentSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    /// Base URL of this server as reachable from *inside* the sandbox, used
    /// by agents to reach the MCP gateway. Defaults to `http://<bind>`.
    pub gateway_url: Option<String>,
    /// Directory with the built web UI (`web/dist`).
    pub ui_dir: PathBuf,
    /// Authenticated operators. When empty, the server only accepts
    /// connections on a loopback address and treats callers as `local`.
    pub operators: Vec<Operator>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".parse().expect("valid address"),
            gateway_url: None,
            ui_dir: "web/dist".into(),
            operators: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// May watch sessions and read audit logs.
    Viewer,
    /// May also start sessions, approve/reject actions and stop agents.
    #[default]
    Operator,
    /// May also manage configuration: model providers and their API keys.
    Admin,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    /// PostgreSQL URL. `$AGENTCORE_DATABASE_URL` or `$DATABASE_URL` take
    /// precedence, so the password need not live in the file.
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecretsConfig {
    /// File holding the base64 master key used to encrypt provider API keys.
    /// Generated on first start. `$AGENTCORE_MASTER_KEY` takes precedence.
    /// Defaults to `<data_dir>/master.key`.
    pub master_key_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelGatewayConfig {
    /// Store full request and response bodies of model calls (EU AI Act
    /// Art. 12 record-keeping). Hashes are always recorded.
    pub log_bodies: bool,
    /// Bodies are stored up to this size; the rest is cut (hashes cover all).
    pub max_logged_body_bytes: usize,
    /// Give up on an upstream response that sends nothing for this long.
    pub upstream_read_timeout_secs: u64,
}

impl Default for ModelGatewayConfig {
    fn default() -> Self {
        Self {
            log_bodies: true,
            max_logged_body_bytes: 1024 * 1024,
            upstream_read_timeout_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operator {
    pub name: String,
    /// Hex SHA-256 of the operator's bearer token (`agentcore hash-token`).
    pub token_sha256: String,
    #[serde(default)]
    pub role: Role,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub data_dir: PathBuf,
    pub audit_fsync: bool,
    /// Minimum retention promised for audit logs. agentcore never deletes
    /// them itself; this value is published in the system card.
    pub audit_retention_days: u32,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: "data".into(),
            audit_fsync: true,
            // EU AI Act Art. 19 / 26(6): at least six months.
            audit_retention_days: 183,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoliciesConfig {
    pub dir: PathBuf,
    pub default: String,
}

impl Default for PoliciesConfig {
    fn default() -> Self {
        Self {
            dir: "policies".into(),
            default: "default".into(),
        }
    }
}

/// Information published to users and affected persons (EU AI Act Art. 13
/// and Art. 50). Shown in the UI and served at `/api/v1/system-card`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Transparency {
    pub system_name: String,
    pub provider: String,
    pub contact: String,
    pub intended_purpose: String,
    pub limitations: Vec<String>,
}

impl Default for Transparency {
    fn default() -> Self {
        Self {
            system_name: "agentcore".into(),
            provider: "unspecified".into(),
            contact: "unspecified".into(),
            intended_purpose: "Running AI coding agents as supervised collaborators on \
                               software repositories inside an isolated sandbox."
                .into(),
            limitations: vec![
                "AI agents can produce incorrect, insecure or incomplete code; all output \
                 must be reviewed by a human before use."
                    .into(),
                "Policies restrict actions taken through agentcore; they do not make the \
                 underlying model's output correct."
                    .into(),
            ],
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let src = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut config: Self =
            toml::from_str(&src).with_context(|| format!("parsing config {}", path.display()))?;
        // `$AGENTCORE_DATA` wins over the file. docker-compose sets it to the
        // directory it shares with the host, so the two cannot drift apart.
        if let Ok(dir) = std::env::var("AGENTCORE_DATA")
            && !dir.trim().is_empty()
        {
            config.storage.data_dir = dir.trim().into();
        }
        // Relative paths are relative to the config file.
        let base = path.parent().unwrap_or(Path::new("."));
        if let Some(p) = &mut config.secrets.master_key_file
            && p.is_relative()
        {
            *p = base.join(&*p);
        }
        for p in [
            &mut config.storage.data_dir,
            &mut config.policies.dir,
            &mut config.server.ui_dir,
        ] {
            if p.is_relative() {
                *p = base.join(&*p);
            }
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.server.operators.is_empty() && !self.server.bind.ip().is_loopback() {
            anyhow::bail!(
                "refusing to listen on {} without authentication: configure \
                 [[server.operators]] or bind to a loopback address",
                self.server.bind
            );
        }
        for op in &self.server.operators {
            if op.token_sha256.len() != 64 || hex::decode(&op.token_sha256).is_err() {
                anyhow::bail!(
                    "operator `{}`: token_sha256 must be 64 hex characters",
                    op.name
                );
            }
        }
        Ok(())
    }

    pub fn database_url(&self) -> anyhow::Result<String> {
        ["AGENTCORE_DATABASE_URL", "DATABASE_URL"]
            .iter()
            .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()))
            .or_else(|| self.database.url.clone())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no database configured: set [database].url or $AGENTCORE_DATABASE_URL \
                     (e.g. postgres://agentcore:secret@localhost/agentcore)"
                )
            })
    }

    pub fn master_key_file(&self) -> PathBuf {
        self.secrets
            .master_key_file
            .clone()
            .unwrap_or_else(|| self.storage.data_dir.join("master.key"))
    }

    pub fn gateway_url(&self) -> String {
        self.server
            .gateway_url
            .clone()
            .unwrap_or_else(|| format!("http://{}", self.server.bind))
    }
}
