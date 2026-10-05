use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentcore_audit::AuditLog;
use agentcore_core::{
    AI_GENERATED_MARKER, Action, ActionOutcome, AgentAdapter, AgentSpec, Event, EventKind,
    LaunchContext, ModelEndpoint, OutputStream, Principal, SessionId, SessionInfo, SessionStatus,
    Verdict,
};
use agentcore_policy::{CompiledPolicy, normalize_action};
use agentcore_sandbox::{ExecRequest, Sandbox, SandboxProvider, SandboxRequest, WORKSPACE};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

use crate::approvals::{ApprovalBroker, ApprovalDecision, PendingApproval};
use crate::{RuntimeError, manager::RuntimeConfig};

/// Events kept in memory for late subscribers. The audit log has all of them.
const HISTORY_LIMIT: usize = 10_000;
const MAX_LINE: usize = 8 * 1024;
const EXEC_TIMEOUT: Duration = Duration::from_secs(300);

struct State {
    seq: u64,
    status: SessionStatus,
    ended_at: Option<DateTime<Utc>>,
    history: VecDeque<Event>,
}

pub struct Session {
    id: SessionId,
    spec: AgentSpec,
    task: String,
    created_by: Principal,
    created_at: DateTime<Utc>,
    policy: Arc<CompiledPolicy>,
    gateway_token: String,
    audit: AuditLog,
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
    approvals: ApprovalBroker,
    sandbox: Mutex<Option<Arc<dyn Sandbox>>>,
    cancel: CancellationToken,
    actions: AtomicU64,
    model_calls: AtomicU64,
    models: Vec<ModelEndpoint>,
}

pub(crate) struct SessionParams {
    pub id: SessionId,
    pub spec: AgentSpec,
    pub task: String,
    pub created_by: Principal,
    pub policy: Arc<CompiledPolicy>,
    pub audit: AuditLog,
    pub models: Vec<ModelEndpoint>,
}

impl Session {
    pub(crate) fn new(params: SessionParams) -> Result<Arc<Self>, RuntimeError> {
        let (events, _) = broadcast::channel(1024);
        let session = Arc::new(Self {
            id: params.id,
            spec: params.spec,
            task: params.task,
            created_by: params.created_by,
            created_at: Utc::now(),
            policy: params.policy,
            gateway_token: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
            audit: params.audit,
            state: Mutex::new(State {
                seq: 0,
                status: SessionStatus::Pending,
                ended_at: None,
                history: VecDeque::new(),
            }),
            events,
            approvals: ApprovalBroker::default(),
            sandbox: Mutex::new(None),
            cancel: CancellationToken::new(),
            actions: AtomicU64::new(0),
            model_calls: AtomicU64::new(0),
            models: params.models,
        });
        session.emit(EventKind::SessionCreated {
            agent: session.spec.name.clone(),
            task: session.task.clone(),
            policy: session.policy.name().to_string(),
            policy_digest: session.policy.digest().to_string(),
            created_by: session.created_by.clone(),
        })?;
        Ok(session)
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn policy(&self) -> &CompiledPolicy {
        &self.policy
    }

    pub fn audit_path(&self) -> &std::path::Path {
        self.audit.path()
    }

    /// Constant-time comparison of a presented gateway token.
    pub fn check_gateway_token(&self, presented: &str) -> bool {
        let a = Sha256::digest(presented.as_bytes());
        let b = Sha256::digest(self.gateway_token.as_bytes());
        a.iter()
            .zip(b.iter())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
    }

    pub fn status(&self) -> SessionStatus {
        self.lock().status
    }

    pub fn info(&self) -> SessionInfo {
        let state = self.lock();
        SessionInfo {
            id: self.id,
            agent: self.spec.name.clone(),
            task: self.task.clone(),
            policy: self.policy.name().to_string(),
            status: state.status,
            created_by: self.created_by.clone(),
            created_at: self.created_at,
            ended_at: state.ended_at,
            pending_approvals: self.approvals.len(),
            actions: self.actions.load(Ordering::SeqCst),
            model_calls: self.model_calls.load(Ordering::SeqCst),
        }
    }

    /// Snapshot of recent events plus a live receiver, taken atomically so a
    /// subscriber never misses or duplicates an event.
    pub fn subscribe(&self) -> (Vec<Event>, broadcast::Receiver<Event>) {
        let state = self.lock();
        (
            state.history.iter().cloned().collect(),
            self.events.subscribe(),
        )
    }

    pub fn policy_digest(&self) -> &str {
        self.policy.digest()
    }

    /// Model providers this session may use.
    pub fn models(&self) -> &[ModelEndpoint] {
        &self.models
    }

    /// Fires when the session is stopped; used to abort in-flight model calls.
    pub fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Admit one model call: the session must be live and under its
    /// `max_model_calls` limit. Every admitted or refused call must then be
    /// reported with [`Session::record_model_call`].
    pub fn admit_model_call(&self) -> Result<(), String> {
        if self.cancel.is_cancelled() || self.status().is_terminal() {
            return Err("the session has been stopped".into());
        }
        let limit = self.policy.limits().max_model_calls;
        let n = self.model_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n > limit {
            return Err(format!("model call limit of {limit} reached"));
        }
        Ok(())
    }

    /// Record a completed (or refused) model call in the audit log.
    pub fn record_model_call(&self, event: EventKind) -> Result<(), RuntimeError> {
        debug_assert!(matches!(event, EventKind::ModelCall { .. }));
        self.emit(event).map(|_| ())
    }

    pub fn pending_approvals(&self) -> Vec<PendingApproval> {
        self.approvals.list()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Record an event: audit log first (fail closed), then history, tracing
    /// and live subscribers.
    pub(crate) fn emit(&self, kind: EventKind) -> Result<Event, RuntimeError> {
        let mut state = self.lock();
        let event = Event {
            id: Uuid::now_v7(),
            session_id: self.id,
            seq: state.seq,
            timestamp: Utc::now(),
            kind,
        };
        if let Err(err) = self.audit.append(&event) {
            drop(state);
            tracing::error!(session = %self.id, error = %err, "audit write failed; stopping session");
            self.cancel.cancel();
            self.approvals.cancel_all();
            return Err(err.into());
        }
        state.seq += 1;
        if let EventKind::StatusChanged { status } | EventKind::SessionEnded { status, .. } =
            &event.kind
        {
            state.status = *status;
            if status.is_terminal() {
                state.ended_at.get_or_insert(event.timestamp);
            }
        }
        if state.history.len() >= HISTORY_LIMIT {
            state.history.pop_front();
        }
        state.history.push_back(event.clone());
        tracing::info!(
            target: "agentcore::event",
            session = %self.id,
            seq = event.seq,
            event = event.kind.name(),
        );
        let _ = self.events.send(event.clone());
        Ok(event)
    }

    fn set_status(&self, status: SessionStatus) {
        let current = self.status();
        if current != status && !current.is_terminal() {
            let _ = self.emit(EventKind::StatusChanged { status });
        }
    }

    fn sandbox(&self) -> Option<Arc<dyn Sandbox>> {
        self.sandbox
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Stop the session immediately: kill everything in the sandbox and deny
    /// every pending approval. This is the "big red button".
    pub async fn stop(&self, by: Principal, reason: impl Into<String>) {
        if self.cancel.is_cancelled() || self.status().is_terminal() {
            return;
        }
        let _ = self.emit(EventKind::StopRequested {
            by,
            reason: reason.into(),
        });
        self.cancel.cancel();
        self.approvals.cancel_all();
        if let Some(sandbox) = self.sandbox()
            && let Err(err) = sandbox.kill().await
        {
            tracing::error!(session = %self.id, error = %err, "failed to kill sandbox");
        }
    }

    pub fn resolve_approval(
        &self,
        approval_id: Uuid,
        decision: ApprovalDecision,
    ) -> Result<(), RuntimeError> {
        if self.approvals.resolve(approval_id, decision) {
            Ok(())
        } else {
            Err(RuntimeError::ApprovalNotFound(approval_id))
        }
    }

    /// Main loop: create the sandbox, launch the agent, stream its output and
    /// wait for it to exit, be stopped or exceed its time budget.
    pub(crate) async fn run(
        self: Arc<Self>,
        provider: Arc<dyn SandboxProvider>,
        adapter: Arc<dyn AgentAdapter>,
        config: Arc<RuntimeConfig>,
    ) {
        let span = tracing::info_span!("session", id = %self.id, agent = %self.spec.name);
        async move {
            let (status, exit_code, reason) = match self.drive(provider, adapter, &config).await {
                Ok(result) => result,
                Err(err) => (SessionStatus::Failed, None, Some(err.to_string())),
            };
            if let Some(sandbox) = self.sandbox()
                && let Err(err) = sandbox.destroy().await
            {
                tracing::warn!(error = %err, "failed to destroy sandbox");
            }
            let status = if self.cancel.is_cancelled() {
                SessionStatus::Stopped
            } else {
                status
            };
            self.cancel.cancel();
            self.approvals.cancel_all();
            let _ = self.emit(EventKind::SessionEnded {
                status,
                exit_code,
                reason,
            });
        }
        .instrument(span)
        .await
    }

    async fn drive(
        &self,
        provider: Arc<dyn SandboxProvider>,
        adapter: Arc<dyn AgentAdapter>,
        config: &RuntimeConfig,
    ) -> Result<(SessionStatus, Option<i32>, Option<String>), RuntimeError> {
        let request = SandboxRequest {
            session_id: self.id,
            workspace_dir: config.data_dir.join("workspaces").join(self.id.to_string()),
            image: self.spec.image.clone(),
        };
        let sandbox = tokio::select! {
            sandbox = provider.create(&request) => sandbox?,
            () = self.cancel.cancelled() => return Ok((SessionStatus::Stopped, None, None)),
        };
        *self.sandbox.lock().unwrap_or_else(|p| p.into_inner()) = Some(sandbox.clone());
        if self.cancel.is_cancelled() {
            // Stop raced with sandbox creation; `run` destroys it.
            return Ok((SessionStatus::Stopped, None, None));
        }
        self.emit(EventKind::SandboxStarted {
            backend: sandbox.backend().into(),
            details: sandbox.describe(),
        })?;

        let ctx = LaunchContext {
            session_id: self.id,
            task: self.task.clone(),
            workspace: WORKSPACE.into(),
            gateway_url: format!(
                "{}/mcp/{}",
                config.gateway_url.trim_end_matches('/'),
                self.id
            ),
            gateway_token: self.gateway_token.clone(),
            model_gateway_url: format!(
                "{}/llm/{}",
                config.gateway_url.trim_end_matches('/'),
                self.id
            ),
            models: self.models.clone(),
        };
        let mut plan = adapter.plan(&self.spec, &ctx)?;
        // Gateway variables win over anything the agent spec sets, so real
        // provider keys configured the old way can never leak in.
        plan.env.extend(ctx.model_env());
        plan.env.extend([
            ("AGENTCORE_SESSION_ID".into(), self.id.to_string()),
            ("AGENTCORE_GATEWAY_URL".into(), ctx.gateway_url.clone()),
            ("AGENTCORE_GATEWAY_TOKEN".into(), self.gateway_token.clone()),
            ("AGENTCORE_WORKSPACE".into(), WORKSPACE.into()),
            ("AGENTCORE_AI_GENERATED".into(), AI_GENERATED_MARKER.into()),
        ]);
        // Arguments are audited; the environment is not (it carries secrets).
        self.emit(EventKind::AgentStarted {
            program: plan.program.clone(),
            args: plan.args.clone(),
        })?;
        self.set_status(SessionStatus::Running);

        let mut child = sandbox.spawn(&plan).await?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let pumps = async {
            tokio::join!(
                async {
                    if let Some(s) = stdout {
                        self.pump(s, OutputStream::Stdout).await;
                    }
                },
                async {
                    if let Some(s) = stderr {
                        self.pump(s, OutputStream::Stderr).await;
                    }
                },
            );
        };
        tokio::pin!(pumps);
        let mut pumps_done = false;
        let deadline =
            tokio::time::sleep(Duration::from_secs(self.policy.limits().max_session_secs));
        tokio::pin!(deadline);

        let result = loop {
            tokio::select! {
                () = &mut pumps, if !pumps_done => pumps_done = true,
                status = child.wait() => {
                    let status = status.map_err(|e| agentcore_sandbox::SandboxError::Io {
                        context: "wait for agent".into(),
                        source: e,
                    })?;
                    let code = status.code();
                    if self.cancel.is_cancelled() {
                        // Killed by a stop that raced with the exit.
                        break (SessionStatus::Stopped, code, None);
                    }
                    break if status.success() {
                        (SessionStatus::Completed, code, None)
                    } else {
                        (SessionStatus::Failed, code, Some(format!("agent exited with {status}")))
                    };
                }
                () = self.cancel.cancelled() => break (SessionStatus::Stopped, None, None),
                () = &mut deadline => {
                    let reason = "maximum session duration exceeded";
                    self.stop(Principal::System, reason).await;
                    break (SessionStatus::Stopped, None, Some(reason.into()));
                }
            }
        };
        let _ = child.start_kill();
        if !pumps_done {
            // Give the readers a moment to flush the tail of the output.
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut pumps).await;
        }
        Ok(result)
    }

    async fn pump(&self, reader: impl AsyncRead + Unpin, stream: OutputStream) {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(mut line)) = lines.next_line().await {
            if line.len() > MAX_LINE {
                let mut cut = MAX_LINE;
                while !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                line.truncate(cut);
                line.push_str(" …[truncated]");
            }
            if self.emit(EventKind::Output { stream, line }).is_err() {
                break;
            }
        }
    }

    /// Gate an action through policy and (if required) a human, then execute
    /// it in the sandbox. Called by the tool gateway on behalf of the agent.
    pub async fn request_action(
        &self,
        action: Action,
        contents: Option<Vec<u8>>,
    ) -> Result<ActionOutcome, RuntimeError> {
        if self.cancel.is_cancelled() || self.status().is_terminal() {
            return Err(RuntimeError::NotRunning);
        }
        let action = normalize_action(&action, WORKSPACE);
        let action_id = Uuid::now_v7();
        let count = self.actions.fetch_add(1, Ordering::SeqCst) + 1;
        self.emit(EventKind::ActionRequested {
            action_id,
            action: action.clone(),
            requested_by: Principal::Agent(self.spec.name.clone()),
        })?;

        let limits = self.policy.limits().clone();
        let verdict = if count > limits.max_actions {
            Verdict::Deny {
                rule: None,
                reason: format!("action limit of {} reached", limits.max_actions),
            }
        } else {
            self.policy.evaluate(&action)
        };
        self.emit(EventKind::PolicyEvaluated {
            action_id,
            verdict: verdict.clone(),
        })?;

        let outcome = match verdict {
            Verdict::Deny { reason, .. } => ActionOutcome::Denied { reason },
            Verdict::RequireApproval { reason, .. } => {
                let timeout = Duration::from_secs(limits.approval_timeout_secs);
                match self
                    .await_approval(action_id, &action, reason, timeout)
                    .await?
                {
                    None => {
                        self.execute(&action, contents, limits.max_output_bytes)
                            .await
                    }
                    Some(denied) => denied,
                }
            }
            Verdict::Allow { .. } => {
                self.execute(&action, contents, limits.max_output_bytes)
                    .await
            }
        };
        self.emit(EventKind::ActionCompleted {
            action_id,
            outcome: outcome.clone(),
        })?;
        Ok(outcome)
    }

    /// Returns `None` if approved, or the denial outcome.
    async fn await_approval(
        &self,
        action_id: Uuid,
        action: &Action,
        reason: String,
        timeout: Duration,
    ) -> Result<Option<ActionOutcome>, RuntimeError> {
        let approval_id = Uuid::now_v7();
        let now = Utc::now();
        let rx = self.approvals.open(PendingApproval {
            approval_id,
            action_id,
            action: action.clone(),
            reason: reason.clone(),
            requested_at: now,
            expires_at: now + chrono::Duration::from_std(timeout).unwrap_or(chrono::Duration::MAX),
        });
        self.emit(EventKind::ApprovalRequested {
            approval_id,
            action_id,
            action: action.clone(),
            reason,
        })?;
        self.set_status(SessionStatus::AwaitingApproval);

        let decision = tokio::select! {
            decision = rx => decision.unwrap_or(ApprovalDecision {
                approved: false,
                by: Principal::System,
                comment: Some("session stopped".into()),
            }),
            () = tokio::time::sleep(timeout) => {
                self.approvals.withdraw(approval_id);
                ApprovalDecision {
                    approved: false,
                    by: Principal::System,
                    comment: Some("approval timed out".into()),
                }
            }
        };
        self.emit(EventKind::ApprovalResolved {
            approval_id,
            action_id,
            approved: decision.approved,
            by: decision.by.clone(),
            comment: decision.comment.clone(),
        })?;
        if self.approvals.len() == 0 && !self.cancel.is_cancelled() {
            self.set_status(SessionStatus::Running);
        }
        if decision.approved && !self.cancel.is_cancelled() {
            Ok(None)
        } else {
            Ok(Some(ActionOutcome::Denied {
                reason: match decision.comment {
                    Some(comment) => format!("rejected by {}: {comment}", decision.by),
                    None => format!("rejected by {}", decision.by),
                },
            }))
        }
    }

    async fn execute(
        &self,
        action: &Action,
        contents: Option<Vec<u8>>,
        max_output: usize,
    ) -> ActionOutcome {
        let Some(sandbox) = self.sandbox() else {
            return ActionOutcome::Failed {
                error: "sandbox is not running".into(),
            };
        };
        let result = match action {
            Action::Exec { command, args, cwd } => sandbox
                .exec(ExecRequest {
                    command: command.clone(),
                    args: args.clone(),
                    cwd: cwd.clone().unwrap_or_else(|| WORKSPACE.into()),
                    env: [(
                        "AGENTCORE_AI_GENERATED".to_string(),
                        AI_GENERATED_MARKER.to_string(),
                    )]
                    .into(),
                    timeout: EXEC_TIMEOUT,
                    max_output_bytes: max_output,
                })
                .await
                .map(|out| serde_json::to_value(out).unwrap_or_default()),
            Action::FileRead { path } => {
                sandbox
                    .read_file(path, max_output)
                    .await
                    .map(|(data, truncated)| {
                        serde_json::json!({
                            "content": String::from_utf8_lossy(&data),
                            "truncated": truncated,
                        })
                    })
            }
            Action::FileWrite { path, .. } => {
                let data = contents.unwrap_or_default();
                let digest = hex::encode(Sha256::digest(&data));
                sandbox
                    .write_file(path, &data)
                    .await
                    .map(|()| serde_json::json!({ "bytes": data.len(), "sha256": digest }))
            }
            Action::Network { .. } => {
                return ActionOutcome::Failed {
                    error: "direct network actions are not supported yet".into(),
                };
            }
            Action::ToolCall { tool, .. } => {
                return ActionOutcome::Failed {
                    error: format!("no handler registered for tool `{tool}`"),
                };
            }
        };
        match result {
            Ok(output) => ActionOutcome::Succeeded { output },
            Err(err) => ActionOutcome::Failed {
                error: err.to_string(),
            },
        }
    }
}
