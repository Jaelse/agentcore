use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use agentcore_audit::AuditLog;
use agentcore_core::{AgentSpec, Principal, SessionId, SessionInfo};
use agentcore_policy::PolicySet;
use agentcore_sandbox::SandboxProvider;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::session::{Session, SessionParams};
use crate::{AdapterRegistry, RuntimeError, SessionOptions};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Root for audit logs (`audit/`) and workspaces (`workspaces/`).
    pub data_dir: PathBuf,
    /// Base URL of the tool gateway as reachable from inside the sandbox.
    pub gateway_url: String,
    /// Policy used when neither the request nor the agent names one.
    pub default_policy: String,
    /// `fsync` every audit record (durable, slower).
    pub audit_fsync: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSession {
    pub agent: String,
    pub task: String,
    #[serde(default)]
    pub policy: Option<String>,
}

pub struct SessionManager {
    config: Arc<RuntimeConfig>,
    policies: PolicySet,
    /// `[[agents]]` from the configuration file.
    configured: HashMap<String, AgentSpec>,
    /// Agents added from the catalogue (from the database; replaced as a whole).
    installed: RwLock<HashMap<String, AgentSpec>>,
    adapters: AdapterRegistry,
    provider: Arc<dyn SandboxProvider>,
    sessions: RwLock<HashMap<SessionId, Arc<Session>>>,
}

impl SessionManager {
    pub fn new(
        config: RuntimeConfig,
        policies: PolicySet,
        agents: Vec<AgentSpec>,
        adapters: AdapterRegistry,
        provider: Arc<dyn SandboxProvider>,
    ) -> Result<Self, RuntimeError> {
        if policies.get(&config.default_policy).is_none() {
            return Err(RuntimeError::UnknownPolicy(config.default_policy.clone()));
        }
        for spec in &agents {
            check_spec(spec, &policies, &adapters)?;
        }
        Ok(Self {
            config: Arc::new(config),
            policies,
            configured: agents.into_iter().map(|a| (a.name.clone(), a)).collect(),
            installed: RwLock::default(),
            adapters,
            provider,
            sessions: RwLock::new(HashMap::new()),
        })
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    pub fn policies(&self) -> &PolicySet {
        &self.policies
    }

    /// Every agent that can be started: configured ones, then installed
    /// ones (a configured agent wins over an installed one of the same name).
    pub fn agents(&self) -> Vec<AgentSpec> {
        let installed = self.installed.read().unwrap_or_else(|p| p.into_inner());
        let mut list: Vec<AgentSpec> = self
            .configured
            .values()
            .cloned()
            .chain(
                installed
                    .values()
                    .filter(|a| !self.configured.contains_key(&a.name))
                    .cloned(),
            )
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    pub fn agent(&self, name: &str) -> Option<AgentSpec> {
        self.configured.get(name).cloned().or_else(|| {
            self.installed
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .get(name)
                .cloned()
        })
    }

    /// Whether an agent comes from the configuration file.
    pub fn is_configured(&self, name: &str) -> bool {
        self.configured.contains_key(name)
    }

    /// Check an agent definition against the known adapters and policies.
    pub fn check_agent(&self, spec: &AgentSpec) -> Result<(), RuntimeError> {
        check_spec(spec, &self.policies, &self.adapters)
    }

    /// Replace the installed agents. Invalid ones are skipped and returned
    /// with the reason; sessions already running keep their definition.
    pub fn set_installed(&self, specs: Vec<AgentSpec>) -> Vec<(String, RuntimeError)> {
        let mut errors = Vec::new();
        let mut map = HashMap::new();
        for spec in specs {
            match self.check_agent(&spec) {
                Ok(()) => {
                    map.insert(spec.name.clone(), spec);
                }
                Err(err) => errors.push((spec.name.clone(), err)),
            }
        }
        *self.installed.write().unwrap_or_else(|p| p.into_inner()) = map;
        errors
    }

    pub fn sandbox_provider(&self) -> Arc<dyn SandboxProvider> {
        self.provider.clone()
    }

    pub fn sandbox_backend(&self) -> &'static str {
        self.provider.name()
    }

    /// Create a session and start it in the background.
    pub fn create(
        &self,
        request: CreateSession,
        by: Principal,
        options: SessionOptions,
    ) -> Result<Arc<Session>, RuntimeError> {
        let spec = self
            .agent(&request.agent)
            .ok_or_else(|| RuntimeError::UnknownAgent(request.agent.clone()))?;
        let policy_name = request
            .policy
            .or_else(|| options.role.as_ref().and_then(|r| r.policy.clone()))
            .or_else(|| spec.policy.clone())
            .unwrap_or_else(|| self.config.default_policy.clone());
        let policy = self
            .policies
            .get(&policy_name)
            .ok_or(RuntimeError::UnknownPolicy(policy_name))?;
        let adapter = self
            .adapters
            .get(&spec.adapter)
            .ok_or_else(|| RuntimeError::UnknownAdapter(spec.adapter.clone()))?;

        let id = Uuid::now_v7();
        let audit = AuditLog::open(
            self.config
                .data_dir
                .join("audit")
                .join(format!("{id}.jsonl")),
            self.config.audit_fsync,
        )?;
        let session = Session::new(SessionParams {
            id,
            spec,
            task: request.task,
            created_by: by,
            policy,
            audit,
            options,
        })?;
        self.sessions
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, session.clone());
        tokio::spawn(
            session
                .clone()
                .run(self.provider.clone(), adapter, self.config.clone()),
        );
        Ok(session)
    }

    pub fn get(&self, id: SessionId) -> Result<Arc<Session>, RuntimeError> {
        self.sessions
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id)
            .cloned()
            .ok_or(RuntimeError::SessionNotFound(id))
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let mut list: Vec<_> = self
            .sessions
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .map(|s| s.info())
            .collect();
        list.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        list
    }

    /// Emergency stop for every live session.
    pub async fn stop_all(&self, by: Principal, reason: &str) -> usize {
        let live: Vec<_> = self
            .sessions
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .filter(|s| !s.status().is_terminal())
            .cloned()
            .collect();
        let stops = live.iter().map(|s| s.stop(by.clone(), reason.to_string()));
        let count = live.len();
        for stop in stops {
            stop.await;
        }
        count
    }
}

fn check_spec(
    spec: &AgentSpec,
    policies: &PolicySet,
    adapters: &AdapterRegistry,
) -> Result<(), RuntimeError> {
    if adapters.get(&spec.adapter).is_none() {
        return Err(RuntimeError::UnknownAdapter(spec.adapter.clone()));
    }
    if let Some(policy) = &spec.policy
        && policies.get(policy).is_none()
    {
        return Err(RuntimeError::UnknownPolicy(policy.clone()));
    }
    spec.validate()?;
    Ok(())
}
