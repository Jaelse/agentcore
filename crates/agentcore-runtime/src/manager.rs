use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use agentcore_audit::AuditLog;
use agentcore_core::{AgentSpec, Principal, SessionId, SessionInfo};
use agentcore_policy::PolicySet;
use agentcore_sandbox::SandboxProvider;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::recover::{Recovered, SessionPaths};
use crate::session::{Session, SessionParams};
use crate::{AdapterRegistry, RuntimeError, SessionOptions};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// The data volume: every session keeps its audit log, recording,
    /// workspace and agent home in one directory here (see
    /// [`SessionPaths`]). Shared by every node, so any node can carry a
    /// session on.
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
    /// Model provider to use instead of the agent's own.
    #[serde(default)]
    pub provider: Option<String>,
    /// Model to use instead of the agent's own.
    #[serde(default)]
    pub model: Option<String>,
}

/// What a session runs with: its agent, policy and adapter.
type Prepared = (
    AgentSpec,
    Arc<agentcore_policy::CompiledPolicy>,
    Arc<dyn agentcore_core::AgentAdapter>,
);

/// A session to take over from the node that ran it.
#[derive(Debug, Clone)]
pub struct Resume {
    pub id: SessionId,
    /// Its audit log, as recorded in the session index.
    pub audit_path: PathBuf,
    /// Why it is carried on (recorded in the log and shown in its terminal).
    pub reason: String,
    /// The node taking it over.
    pub node: String,
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
        let task = request.task.clone();
        let (spec, policy, adapter) = self.prepare(request, &options)?;
        let id = Uuid::now_v7();
        let mut paths = SessionPaths::new(&self.config.data_dir, id, options.context.department_id);
        if let Some(dir) = &options.agent_dir {
            paths = paths.kept_in(dir);
        }
        let audit = AuditLog::open(&paths.audit, self.config.audit_fsync)?;
        let session = Session::new(SessionParams {
            id,
            spec,
            task,
            created_by: by,
            policy,
            audit,
            paths,
            options,
            recovered: None,
        })?;
        self.start(session.clone(), adapter);
        Ok(session)
    }

    /// Carry on a session whose node went away (or restarted): same id, same
    /// audit log (continued), same workspace and agent home, a new sandbox.
    /// `request` and `options` are built as for a new session; the task,
    /// policy and creator come from the log. Fails with
    /// [`RuntimeError::Ended`] when the log already records the end.
    pub fn resume(
        &self,
        request: CreateSession,
        resume: Resume,
        options: SessionOptions,
    ) -> Result<Arc<Session>, RuntimeError> {
        let mut paths = SessionPaths::of(&self.config.data_dir, resume.id, &resume.audit_path);
        if let Some(dir) = &options.agent_dir {
            paths = paths.kept_in(dir);
        }
        let log = Recovered::read(&paths.audit)?;
        let mut request = request;
        request.policy = Some(log.policy.clone());
        let (spec, policy, adapter) = self.prepare(request, &options)?;
        let audit = AuditLog::open(&paths.audit, self.config.audit_fsync)?;
        let session = Session::new(SessionParams {
            id: resume.id,
            spec,
            task: String::new(),
            created_by: Principal::System,
            policy,
            audit,
            paths,
            options,
            recovered: Some((log, resume.reason.clone())),
        })?;
        session.emit(agentcore_core::EventKind::SessionRecovered {
            node: resume.node,
            reason: resume.reason,
        })?;
        self.start(session.clone(), adapter);
        Ok(session)
    }

    /// The agent, policy and adapter a session runs with.
    fn prepare(
        &self,
        request: CreateSession,
        options: &SessionOptions,
    ) -> Result<Prepared, RuntimeError> {
        let mut spec = self
            .agent(&request.agent)
            .ok_or_else(|| RuntimeError::UnknownAgent(request.agent.clone()))?;
        if let Some(provider) = request.provider.filter(|p| !p.trim().is_empty()) {
            spec.provider = Some(provider);
        }
        if let Some(model) = request.model.filter(|m| !m.trim().is_empty()) {
            spec.model = Some(model);
        }
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

        Ok((spec, policy, adapter))
    }

    fn start(&self, session: Arc<Session>, adapter: Arc<dyn agentcore_core::AgentAdapter>) {
        self.sessions
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(session.id(), session.clone());
        tokio::spawn(session.run(self.provider.clone(), adapter, self.config.clone()));
    }

    /// Let go of every session without ending them ([`Session::fence`]):
    /// this node shuts down or lost the database, and other nodes carry the
    /// sessions on. Returns how many were let go.
    pub async fn fence_all(&self) -> usize {
        let live: Vec<_> = self
            .sessions
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .drain()
            .map(|(_, s)| s)
            .filter(|s| !s.status().is_terminal())
            .collect();
        for session in &live {
            session.fence().await;
        }
        live.len()
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
