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
use crate::{AdapterRegistry, RuntimeError};

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
    agents: HashMap<String, AgentSpec>,
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
            if adapters.get(&spec.adapter).is_none() {
                return Err(RuntimeError::UnknownAdapter(spec.adapter.clone()));
            }
            if let Some(policy) = &spec.policy
                && policies.get(policy).is_none()
            {
                return Err(RuntimeError::UnknownPolicy(policy.clone()));
            }
        }
        Ok(Self {
            config: Arc::new(config),
            policies,
            agents: agents.into_iter().map(|a| (a.name.clone(), a)).collect(),
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

    pub fn agents(&self) -> impl Iterator<Item = &AgentSpec> {
        self.agents.values()
    }

    pub fn sandbox_backend(&self) -> &'static str {
        self.provider.name()
    }

    /// Create a session and start it in the background.
    pub fn create(
        &self,
        request: CreateSession,
        by: Principal,
    ) -> Result<Arc<Session>, RuntimeError> {
        let spec = self
            .agents
            .get(&request.agent)
            .cloned()
            .ok_or_else(|| RuntimeError::UnknownAgent(request.agent.clone()))?;
        let policy_name = request
            .policy
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
