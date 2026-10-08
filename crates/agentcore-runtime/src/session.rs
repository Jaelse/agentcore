use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentcore_audit::AuditLog;
use agentcore_core::{
    AI_GENERATED_MARKER, Action, ActionOutcome, AgentAdapter, AgentSpec, Changes, CheckResult,
    DeliveredMessage, Event, EventKind, LaunchContext, LaunchPlan, LiveFrame, ModelEndpoint,
    OutputStream, Principal, PullRequestProposal, SessionContext, SessionId, SessionInfo,
    SessionStatus, Verdict,
};
use agentcore_policy::{CompiledPolicy, normalize_action};
use agentcore_roles::{CheckKind, RepoDoc, Role, compose_prompt};
use agentcore_sandbox::{
    AgentProcess, ExecRequest, Sandbox, SandboxProvider, SandboxRequest, WORKSPACE,
};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

use crate::approvals::{ApprovalBroker, ApprovalDecision, PendingApproval};
use crate::live::{LiveHub, PlainLines, Utf8Stream, watch_files};
use crate::work::{
    GIT, SessionOptions, ToolHandler, WorkspaceSetup, changes_script, parse_changes, patch_script,
};
use crate::{RuntimeError, manager::RuntimeConfig};

/// Events kept in memory for late subscribers. The audit log has all of them.
const HISTORY_LIMIT: usize = 10_000;
const MAX_LINE: usize = 8 * 1024;
const EXEC_TIMEOUT: Duration = Duration::from_secs(300);
const GIT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_PATCH: usize = 2 * 1024 * 1024;
const MAX_BUNDLE: usize = 512 * 1024 * 1024;
const BUNDLE_REL: &str = ".git/agentcore-delivery.bundle";
const BUNDLE_PATH: &str = "/workspace/.git/agentcore-delivery.bundle";
/// How often the process list is refreshed while someone is watching.
const PROCESS_POLL: Duration = Duration::from_secs(2);

struct State {
    seq: u64,
    status: SessionStatus,
    ended_at: Option<DateTime<Utc>>,
    history: VecDeque<Event>,
}

enum Inbox {
    Message(String),
    Finish(Principal),
}

enum TurnOutcome {
    Exited { code: Option<i32>, success: bool },
    Stopped(Option<String>),
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
    role: Option<Arc<Role>>,
    workspace: Option<Arc<dyn WorkspaceSetup>>,
    tools: Option<Arc<dyn ToolHandler>>,
    work_item: Option<agentcore_roles::WorkItem>,
    hide_sandbox_tools: bool,
    agent_label: String,
    context: Mutex<SessionContext>,
    base_commit: Mutex<Option<String>>,
    last_changes: Mutex<Option<Changes>>,
    proposal: Mutex<Option<PullRequestProposal>>,
    /// True while the agent process runs (between TurnStarted and TurnEnded).
    turn_active: AtomicBool,
    /// True while agentcore itself runs git/checks in the sandbox.
    busy: tokio::sync::Mutex<()>,
    inbox: mpsc::UnboundedSender<Inbox>,
    inbox_rx: Mutex<Option<mpsc::UnboundedReceiver<Inbox>>>,
    live: Arc<LiveHub>,
    paused: watch::Sender<bool>,
    /// Status to return to on resume (updated by changes made while paused).
    resume_status: Mutex<Option<SessionStatus>>,
}

pub(crate) struct SessionParams {
    pub id: SessionId,
    pub spec: AgentSpec,
    pub task: String,
    pub created_by: Principal,
    pub policy: Arc<CompiledPolicy>,
    pub audit: AuditLog,
    pub options: SessionOptions,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Session {
    pub(crate) fn new(params: SessionParams) -> Result<Arc<Self>, RuntimeError> {
        let (events, _) = broadcast::channel(1024);
        let (inbox, inbox_rx) = mpsc::unbounded_channel();
        let options = params.options;
        let agent_label = options
            .agent_label
            .clone()
            .unwrap_or_else(|| params.spec.name.clone());
        let mut context = options.context;
        if let Some(role) = &options.role {
            context.role.get_or_insert_with(|| role.name.clone());
        }
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
            models: options.models,
            role: options.role,
            workspace: options.workspace,
            tools: options.tools,
            work_item: options.work_item,
            hide_sandbox_tools: options.hide_sandbox_tools,
            agent_label,
            context: Mutex::new(context),
            base_commit: Mutex::new(None),
            last_changes: Mutex::new(None),
            proposal: Mutex::new(None),
            turn_active: AtomicBool::new(false),
            busy: tokio::sync::Mutex::new(()),
            inbox,
            inbox_rx: Mutex::new(Some(inbox_rx)),
            live: Arc::new(LiveHub::default()),
            paused: watch::channel(false).0,
            resume_status: Mutex::new(None),
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

    pub fn role(&self) -> Option<&Arc<Role>> {
        self.role.as_ref()
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
            context: lock(&self.context).clone(),
        }
    }

    pub fn context(&self) -> SessionContext {
        lock(&self.context).clone()
    }

    /// The latest snapshot of what the agent changed (repository sessions).
    pub fn last_changes(&self) -> Option<Changes> {
        lock(&self.last_changes).clone()
    }

    pub fn proposal(&self) -> Option<PullRequestProposal> {
        lock(&self.proposal).clone()
    }

    pub fn base_commit(&self) -> Option<String> {
        lock(&self.base_commit).clone()
    }

    /// Whether the built-in sandbox tools are offered to the agent.
    pub fn sandbox_tools_enabled(&self) -> bool {
        !self.hide_sandbox_tools
    }

    /// Tool definitions from the external tool handler (e.g. GitHub).
    pub fn external_tools(&self) -> Vec<serde_json::Value> {
        self.tools
            .as_ref()
            .map(|t| t.definitions())
            .unwrap_or_default()
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

    /// The live view of this session.
    pub fn live(&self) -> &LiveHub {
        &self.live
    }

    pub fn is_paused(&self) -> bool {
        *self.paused.borrow()
    }

    /// Wait while the session is paused. Returns `false` if it was stopped.
    pub async fn wait_unpaused(&self) -> bool {
        let mut rx = self.paused.subscribe();
        tokio::select! {
            r = rx.wait_for(|paused| !paused) => r.is_ok() && !self.cancel.is_cancelled(),
            () = self.cancel.cancelled() => false,
        }
    }

    /// Freeze the agent and everything it started. Model and tool calls that
    /// arrive while paused wait; the time budget keeps running.
    pub async fn pause(&self, by: Principal) -> Result<(), RuntimeError> {
        if self.cancel.is_cancelled() || self.status().is_terminal() {
            return Err(RuntimeError::NotRunning);
        }
        if self.paused.send_replace(true) {
            return Ok(());
        }
        if let Some(sandbox) = self.sandbox()
            && let Err(err) = sandbox.pause().await
        {
            self.paused.send_replace(false);
            return Err(err.into());
        }
        *lock(&self.resume_status) = Some(self.status());
        let banner = format!("\r\n\x1b[43;30m paused by {} \x1b[0m\r\n", who(&by));
        self.emit(EventKind::Paused { by })?;
        let current = self.status();
        if current != SessionStatus::Paused && !current.is_terminal() {
            self.emit(EventKind::StatusChanged {
                status: SessionStatus::Paused,
            })?;
        }
        self.live.terminal(&banner);
        Ok(())
    }

    /// Continue after [`Session::pause`].
    pub async fn resume(&self, by: Principal) -> Result<(), RuntimeError> {
        if !*self.paused.borrow() {
            return Ok(());
        }
        if let Some(sandbox) = self.sandbox() {
            sandbox.resume().await?;
        }
        let status = lock(&self.resume_status).take();
        let banner = format!("\x1b[42;30m resumed by {} \x1b[0m\r\n", who(&by));
        self.emit(EventKind::Resumed { by })?;
        self.paused.send_replace(false);
        self.live.terminal(&banner);
        if let Some(status) = status {
            self.set_status(status);
        } else {
            self.set_status(self.idle_status());
        }
        Ok(())
    }

    pub fn pending_approvals(&self) -> Vec<PendingApproval> {
        self.approvals.list()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        lock(&self.state)
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
        if *self.paused.borrow() && !status.is_terminal() {
            // Applied on resume.
            *lock(&self.resume_status) = Some(status);
            return;
        }
        let current = self.status();
        if current != status && !current.is_terminal() {
            let _ = self.emit(EventKind::StatusChanged { status });
        }
    }

    /// Status to return to after an approval is resolved.
    fn idle_status(&self) -> SessionStatus {
        if self.turn_active.load(Ordering::SeqCst) {
            SessionStatus::Running
        } else {
            SessionStatus::AwaitingInput
        }
    }

    /// A sink that turns command output into live frames.
    fn tool_output(&self, action_id: Uuid) -> agentcore_sandbox::OutputSink {
        let (tx, mut rx) = mpsc::unbounded_channel::<(OutputStream, Vec<u8>)>();
        let frames = self.live.sender();
        tokio::spawn(async move {
            let mut out = Utf8Stream::default();
            let mut err = Utf8Stream::default();
            while let Some((stream, bytes)) = rx.recv().await {
                let decoder = match stream {
                    OutputStream::Stdout => &mut out,
                    OutputStream::Stderr => &mut err,
                };
                let data = decoder.push(&bytes);
                if !data.is_empty() {
                    let _ = frames.send(LiveFrame::ToolOutput {
                        action_id,
                        stream,
                        data,
                    });
                }
            }
        });
        tx
    }

    fn sandbox(&self) -> Option<Arc<dyn Sandbox>> {
        lock(&self.sandbox).clone()
    }

    /// Stop the session immediately: kill everything in the sandbox and deny
    /// every pending approval. This is the "big red button".
    pub async fn stop(&self, by: Principal, reason: impl Into<String>) {
        if self.cancel.is_cancelled() || self.status().is_terminal() {
            return;
        }
        let reason = reason.into();
        self.live.terminal(&format!(
            "\r\n\x1b[41;97m stopped by {}: {} \x1b[0m\r\n",
            who(&by),
            one_line(&reason, 100)
        ));
        let _ = self.emit(EventKind::StopRequested { by, reason });
        self.cancel.cancel();
        self.approvals.cancel_all();
        let was_paused = self.paused.send_replace(false);
        if let Some(sandbox) = self.sandbox() {
            if was_paused {
                // Some runtimes refuse to remove a frozen container.
                let _ = sandbox.resume().await;
            }
            if let Err(err) = sandbox.kill().await {
                tracing::error!(session = %self.id, error = %err, "failed to kill sandbox");
            }
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

    /// Send a follow-up message to an agent that is waiting for input.
    pub fn send_message(&self, by: Principal, text: String) -> Result<(), RuntimeError> {
        if self.status() != SessionStatus::AwaitingInput
            || self.turn_active.swap(true, Ordering::SeqCst)
        {
            return Err(RuntimeError::NotAwaitingInput);
        }
        if let Err(err) = self.emit(EventKind::UserMessage {
            by,
            text: text.clone(),
        }) {
            self.turn_active.store(false, Ordering::SeqCst);
            return Err(err);
        }
        self.inbox
            .send(Inbox::Message(text))
            .map_err(|_| RuntimeError::NotRunning)
    }

    /// Hand messages from the agent's organisation (colleagues, its
    /// communicator, people) to an agent that is waiting for input; they
    /// start its next turn. Fails with [`RuntimeError::NotAwaitingInput`]
    /// while the agent is busy or paused, so the caller keeps them queued.
    pub fn deliver_messages(&self, messages: Vec<DeliveredMessage>) -> Result<(), RuntimeError> {
        if messages.is_empty() {
            return Ok(());
        }
        if self.status() != SessionStatus::AwaitingInput
            || self.is_paused()
            || self.turn_active.swap(true, Ordering::SeqCst)
        {
            return Err(RuntimeError::NotAwaitingInput);
        }
        let text = format!(
            "You have {} new message(s):\n\n{}",
            messages.len(),
            messages
                .iter()
                .map(DeliveredMessage::render)
                .collect::<Vec<_>>()
                .join("\n\n")
        );
        if let Err(err) = self.emit(EventKind::MessagesDelivered { messages }) {
            self.turn_active.store(false, Ordering::SeqCst);
            return Err(err);
        }
        self.inbox
            .send(Inbox::Message(text))
            .map_err(|_| RuntimeError::NotRunning)
    }

    /// End a session that is waiting for input, as completed.
    pub fn finish(&self, by: Principal) -> Result<(), RuntimeError> {
        if self.status() != SessionStatus::AwaitingInput || self.turn_active.load(Ordering::SeqCst)
        {
            return Err(RuntimeError::NotAwaitingInput);
        }
        self.inbox
            .send(Inbox::Finish(by))
            .map_err(|_| RuntimeError::NotRunning)
    }

    /// Record that the work was delivered (branch pushed, PR opened).
    pub fn record_delivery(
        &self,
        by: Principal,
        branch: String,
        commit: String,
        pull_request_url: Option<String>,
    ) -> Result<(), RuntimeError> {
        {
            let mut ctx = lock(&self.context);
            ctx.delivery_branch = Some(branch.clone());
            if pull_request_url.is_some() {
                ctx.pull_request_url = pull_request_url.clone();
            }
        }
        self.emit(EventKind::Delivered {
            by,
            branch,
            commit,
            pull_request_url,
        })
        .map(|_| ())
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
            if !self.cancel.is_cancelled() {
                // Final snapshot of the work before the sandbox goes away.
                self.snapshot_changes().await;
            }
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
            match self.live.finish_recording() {
                Some(Ok(rec)) => {
                    let _ = self.emit(EventKind::RecordingClosed {
                        file: rec.file,
                        bytes: rec.bytes,
                        sha256: rec.sha256,
                    });
                }
                Some(Err(err)) => {
                    tracing::error!(error = %err, "failed to close the terminal recording");
                }
                None => {}
            }
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
        let started = Instant::now();
        let limits = self.policy.limits().clone();
        let deadline = started + Duration::from_secs(limits.max_session_secs);
        let workspace_dir = config.data_dir.join("workspaces").join(self.id.to_string());

        // 1. Check out the repository on the host, before any agent code runs.
        if let Some(setup) = self.workspace.clone() {
            let prepared = tokio::select! {
                r = setup.prepare(&workspace_dir) => r.map_err(RuntimeError::Workspace)?,
                () = self.cancel.cancelled() => return Ok((SessionStatus::Stopped, None, None)),
            };
            *lock(&self.base_commit) = Some(prepared.base_commit.clone());
            {
                let mut ctx = lock(&self.context);
                ctx.repository.get_or_insert(prepared.repository.clone());
                ctx.base_branch.get_or_insert(prepared.branch.clone());
            }
            self.emit(EventKind::WorkspacePrepared {
                repository: prepared.repository,
                branch: prepared.branch,
                base_commit: prepared.base_commit,
            })?;
        }

        // 2. Start the sandbox.
        let request = SandboxRequest {
            session_id: self.id,
            workspace_dir: workspace_dir.clone(),
            image: self.spec.image.clone(),
        };
        let sandbox = tokio::select! {
            sandbox = provider.create(&request) => sandbox?,
            () = self.cancel.cancelled() => return Ok((SessionStatus::Stopped, None, None)),
        };
        *lock(&self.sandbox) = Some(sandbox.clone());
        if self.cancel.is_cancelled() {
            // Stop raced with sandbox creation; `run` destroys it.
            return Ok((SessionStatus::Stopped, None, None));
        }
        self.emit(EventKind::SandboxStarted {
            backend: sandbox.backend().into(),
            details: sandbox.describe(),
        })?;

        // The live view: terminal recording, file changes, processes.
        let recording = config
            .data_dir
            .join("recordings")
            .join(format!("{}.cast", self.id));
        if let Err(err) = self
            .live
            .start_recording(&recording, &format!("{} · {}", self.spec.name, self.id))
        {
            tracing::warn!(error = %err, "cannot record the terminal");
        }
        let _watcher = {
            let live = self.live.clone();
            watch_files(workspace_dir.clone(), move |changes| live.files(changes))
                .map_err(|err| tracing::warn!(error = %err, "cannot watch the workspace"))
                .ok()
        };
        let poller = self.cancel.child_token();
        let _stop_poller = poller.clone().drop_guard();
        {
            let live = self.live.clone();
            let sandbox = sandbox.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(PROCESS_POLL);
                loop {
                    tokio::select! {
                        _ = tick.tick() => {}
                        () = poller.cancelled() => break,
                    }
                    if live.viewers() == 0 {
                        continue;
                    }
                    if let Ok(processes) = sandbox.processes().await {
                        live.processes(processes);
                    }
                }
            });
        }

        // 3. What the agent is asked to do: the role playbook + team
        //    conventions + work item, or just the task.
        let task = self.first_prompt(sandbox.as_ref()).await?;
        let ctx = LaunchContext {
            session_id: self.id,
            task,
            workspace: WORKSPACE.into(),
            home: sandbox.home(),
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
        let can_continue = adapter.follow_up(&self.spec, &ctx, "")?.is_some();
        let mut inbox = lock(&self.inbox_rx)
            .take()
            .ok_or(RuntimeError::NotRunning)?;

        // 4. Turns: run the agent; if it can continue, wait for a human.
        let mut turn = 1u32;
        let mut said: Option<String> = None;
        loop {
            if !self.wait_unpaused().await {
                return Ok((SessionStatus::Stopped, None, None));
            }
            self.decorate(&mut plan, &ctx);
            let label = match said.take() {
                Some(text) => format!("turn {turn} · {}", one_line(&text, 80)),
                None => format!("turn {turn}"),
            };
            self.live.marker(&label);
            self.live
                .terminal(&format!("\x1b[2m── {label} ──\x1b[0m\r\n"));
            if turn == 1 {
                // Arguments are audited; the environment is not (it carries secrets).
                self.emit(EventKind::AgentStarted {
                    program: plan.program.clone(),
                    args: plan.args.iter().map(|a| truncate(a, 2048)).collect(),
                })?;
            }
            self.turn_active.store(true, Ordering::SeqCst);
            self.emit(EventKind::TurnStarted { turn })?;
            self.set_status(SessionStatus::Running);
            let outcome = self.run_turn(sandbox.as_ref(), &plan, deadline).await;
            self.turn_active.store(false, Ordering::SeqCst);
            let (code, success) = match outcome? {
                TurnOutcome::Stopped(reason) => return Ok((SessionStatus::Stopped, None, reason)),
                TurnOutcome::Exited { code, success } => (code, success),
            };
            self.emit(EventKind::TurnEnded {
                turn,
                exit_code: code,
            })?;
            self.snapshot_changes().await;

            if !can_continue {
                return Ok(if success {
                    (SessionStatus::Completed, code, None)
                } else {
                    (
                        SessionStatus::Failed,
                        code,
                        Some(format!("agent exited with code {code:?}")),
                    )
                });
            }

            self.set_status(SessionStatus::AwaitingInput);
            let idle = tokio::time::sleep(Duration::from_secs(limits.idle_timeout_secs));
            let overall = tokio::time::sleep_until(deadline.into());
            let next = tokio::select! {
                msg = inbox.recv() => msg,
                () = self.cancel.cancelled() => return Ok((SessionStatus::Stopped, None, None)),
                () = idle => {
                    let reason = "no input from a human within the idle timeout";
                    self.stop(Principal::System, reason).await;
                    return Ok((SessionStatus::Stopped, None, Some(reason.into())));
                }
                () = overall => {
                    let reason = "maximum session duration exceeded";
                    self.stop(Principal::System, reason).await;
                    return Ok((SessionStatus::Stopped, None, Some(reason.into())));
                }
            };
            match next {
                Some(Inbox::Message(text)) => {
                    // Serialise with checks / exports that may be running.
                    let _guard = self.busy.lock().await;
                    plan = adapter
                        .follow_up(&self.spec, &ctx, &text)?
                        .ok_or(RuntimeError::NotAwaitingInput)?;
                    said = Some(text);
                    turn += 1;
                }
                Some(Inbox::Finish(by)) => {
                    return Ok((
                        SessionStatus::Completed,
                        code,
                        Some(format!("finished by {by}")),
                    ));
                }
                None => return Ok((SessionStatus::Stopped, None, None)),
            }
        }
    }

    fn decorate(&self, plan: &mut LaunchPlan, ctx: &LaunchContext) {
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
    }

    /// Build the first message: role playbook + the team's convention files
    /// (read inside the sandbox, so symlinks cannot reach the host) + task.
    async fn first_prompt(&self, sandbox: &dyn Sandbox) -> Result<String, RuntimeError> {
        let Some(role) = self.role.clone() else {
            return Ok(self.task.clone());
        };
        let mut docs = Vec::new();
        let mut budget = role.max_repo_doc_bytes;
        for path in &role.repo_docs {
            if budget == 0 {
                break;
            }
            let full = format!("{WORKSPACE}/{}", path.trim_start_matches('/'));
            if let Ok((data, truncated)) = sandbox.read_file(&full, budget).await {
                if data.is_empty() {
                    continue;
                }
                budget = budget.saturating_sub(data.len());
                docs.push(RepoDoc {
                    path: path.clone(),
                    content: String::from_utf8_lossy(&data).into_owned(),
                    truncated,
                });
            }
        }
        let mut item = self.work_item.clone().unwrap_or_default();
        if item.task.is_empty() {
            item.task = self.task.clone();
        }
        let prompt = compose_prompt(&role, &docs, &item);
        self.emit(EventKind::RoleApplied {
            role: role.name.clone(),
            role_digest: role.digest(),
            repo_docs: docs.iter().map(|d| d.path.clone()).collect(),
            prompt_sha256: hex::encode(Sha256::digest(prompt.as_bytes())),
        })?;
        Ok(prompt)
    }

    async fn run_turn(
        &self,
        sandbox: &dyn Sandbox,
        plan: &LaunchPlan,
        deadline: Instant,
    ) -> Result<TurnOutcome, RuntimeError> {
        let AgentProcess {
            mut child,
            stdout,
            stderr,
            tty,
        } = sandbox.spawn(plan).await?;
        let pumps = async {
            tokio::join!(
                async {
                    if let Some(s) = stdout {
                        self.pump(s, OutputStream::Stdout, tty).await;
                    }
                },
                async {
                    if let Some(s) = stderr {
                        self.pump(s, OutputStream::Stderr, tty).await;
                    }
                },
            );
        };
        tokio::pin!(pumps);
        let mut pumps_done = false;
        let deadline = tokio::time::sleep_until(deadline.into());
        tokio::pin!(deadline);

        let result = loop {
            tokio::select! {
                () = &mut pumps, if !pumps_done => pumps_done = true,
                status = child.wait() => {
                    let status = status.map_err(|e| agentcore_sandbox::SandboxError::Io {
                        context: "wait for agent".into(),
                        source: e,
                    })?;
                    if self.cancel.is_cancelled() {
                        // Killed by a stop that raced with the exit.
                        break TurnOutcome::Stopped(None);
                    }
                    break TurnOutcome::Exited { code: status.code(), success: status.success() };
                }
                () = self.cancel.cancelled() => break TurnOutcome::Stopped(None),
                () = &mut deadline => {
                    let reason = "maximum session duration exceeded";
                    self.stop(Principal::System, reason).await;
                    break TurnOutcome::Stopped(Some(reason.into()));
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

    /// Copy agent output to the live terminal (as is) and to the audit log
    /// (as plain text lines).
    async fn pump(&self, mut reader: impl AsyncRead + Unpin, stream: OutputStream, tty: bool) {
        let mut decoder = Utf8Stream::default();
        let mut lines = PlainLines::default();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            // A PTY master reports EIO once the agent has exited.
            let n = match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let text = decoder.push(&buf[..n]);
            if tty {
                self.live.terminal(&text);
            } else {
                // Without a terminal, a bare newline does not return the cursor.
                let shown = text.replace('\n', "\r\n");
                if stream == OutputStream::Stderr {
                    self.live.terminal(&format!("\x1b[31m{shown}\x1b[0m"));
                } else {
                    self.live.terminal(&shown);
                }
            }
            for line in lines.push(&text) {
                let line = truncate(&line, MAX_LINE);
                if self.emit(EventKind::Output { stream, line }).is_err() {
                    return;
                }
            }
        }
        if let Some(line) = lines.finish() {
            let _ = self.emit(EventKind::Output {
                stream,
                line: truncate(&line, MAX_LINE),
            });
        }
    }

    // ---- git inside the sandbox ------------------------------------------------

    async fn sh(
        &self,
        script: &str,
        args: &[&str],
        timeout: Duration,
        max: usize,
    ) -> Result<agentcore_sandbox::ExecOutput, RuntimeError> {
        let sandbox = self.sandbox().ok_or(RuntimeError::NotRunning)?;
        let mut argv = vec![
            "-c".to_string(),
            script.to_string(),
            "agentcore".to_string(),
        ];
        argv.extend(args.iter().map(|a| a.to_string()));
        Ok(sandbox
            .exec(ExecRequest {
                command: "sh".into(),
                args: argv,
                cwd: WORKSPACE.into(),
                env: Default::default(),
                timeout,
                max_output_bytes: max,
                live: None,
            })
            .await?)
    }

    /// What the agent changed since the base commit (repository sessions).
    pub async fn changes(&self) -> Result<Option<Changes>, RuntimeError> {
        let Some(base) = self.base_commit() else {
            return Ok(None);
        };
        if self.sandbox().is_none() || self.status().is_terminal() {
            return Ok(self.last_changes());
        }
        let summary = self
            .sh(&changes_script(), &[&base], GIT_TIMEOUT, 256 * 1024)
            .await?;
        let patch = self
            .sh(&patch_script(), &[&base], GIT_TIMEOUT, MAX_PATCH)
            .await?;
        let changes = parse_changes(&base, &summary.stdout, patch.stdout, patch.truncated);
        *lock(&self.last_changes) = Some(changes.clone());
        Ok(Some(changes))
    }

    async fn snapshot_changes(&self) {
        if self.base_commit().is_some()
            && let Err(err) = self.changes().await
        {
            tracing::warn!(session = %self.id, error = %err, "could not capture changes");
        }
    }

    /// Run the role's checks inside the sandbox. Only while the agent is
    /// waiting for input, so the workspace does not change underneath.
    pub async fn run_checks(
        &self,
        by: Principal,
    ) -> Result<(bool, Vec<CheckResult>), RuntimeError> {
        let _guard = self.busy.lock().await;
        if self.status() != SessionStatus::AwaitingInput || self.turn_active.load(Ordering::SeqCst)
        {
            return Err(RuntimeError::NotAwaitingInput);
        }
        let mut results = Vec::new();
        let checks = self
            .role
            .as_ref()
            .map(|r| r.checks.clone())
            .unwrap_or_default();
        let base = self.base_commit();
        for check in checks {
            let mut result = CheckResult {
                name: check.name.clone(),
                passed: false,
                optional: check.optional,
                skipped: false,
                detail: String::new(),
            };
            match &check.kind {
                CheckKind::CommitMessage { pattern } => {
                    let re = regex::Regex::new(pattern)
                        .map_err(|e| RuntimeError::Workspace(e.to_string()))?;
                    let Some(base) = &base else {
                        result.skipped = true;
                        result.passed = true;
                        result.detail = "not a repository session".into();
                        results.push(result);
                        continue;
                    };
                    let out = self
                        .sh(
                            &format!("{GIT} log --format=%s \"$1..HEAD\""),
                            &[base],
                            GIT_TIMEOUT,
                            256 * 1024,
                        )
                        .await?;
                    let subjects: Vec<&str> =
                        out.stdout.lines().filter(|l| !l.is_empty()).collect();
                    let bad: Vec<&str> = subjects
                        .iter()
                        .copied()
                        .filter(|s| !re.is_match(s))
                        .collect();
                    result.passed = !subjects.is_empty() && bad.is_empty();
                    result.detail = if subjects.is_empty() {
                        "no commits yet".into()
                    } else if bad.is_empty() {
                        format!("{} commit(s) ok", subjects.len())
                    } else {
                        format!("not matching `{pattern}`:\n{}", bad.join("\n"))
                    };
                }
                CheckKind::CleanWorktree => {
                    let out = self
                        .sh(
                            &format!("{GIT} status --porcelain"),
                            &[],
                            GIT_TIMEOUT,
                            64 * 1024,
                        )
                        .await?;
                    result.passed = out.exit_code == Some(0) && out.stdout.trim().is_empty();
                    result.detail = if result.passed {
                        "clean".into()
                    } else {
                        format!("uncommitted changes:\n{}", out.stdout.trim())
                    };
                }
                CheckKind::Command {
                    run,
                    when_exists,
                    timeout_secs,
                } => {
                    if let Some(path) = when_exists {
                        let exists = self
                            .sh("test -e \"$1\"", &[path], Duration::from_secs(10), 1024)
                            .await?;
                        if exists.exit_code != Some(0) {
                            result.skipped = true;
                            result.passed = true;
                            result.detail = format!("skipped: {path} not found");
                            results.push(result);
                            continue;
                        }
                    }
                    let out = self
                        .sh(run, &[], Duration::from_secs(*timeout_secs), 64 * 1024)
                        .await?;
                    result.passed = out.exit_code == Some(0) && !out.timed_out;
                    let combined = format!("{}{}", out.stdout, out.stderr);
                    let tail: String = combined
                        .chars()
                        .rev()
                        .take(4000)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    result.detail = if out.timed_out {
                        format!("timed out after {timeout_secs}s\n{tail}")
                    } else {
                        format!("exit code {:?}\n{tail}", out.exit_code)
                    };
                }
            }
            results.push(result);
        }
        let passed = results.iter().all(|r| r.passed || r.optional);
        self.emit(EventKind::ChecksCompleted {
            requested_by: by,
            passed,
            results: results.clone(),
        })?;
        Ok((passed, results))
    }

    /// Export the agent's commits as a git bundle (`base..HEAD`), created
    /// inside the sandbox. agentcore fetches from the bundle into its own
    /// repository on the host and never runs git in the agent's checkout.
    pub async fn export_bundle(&self) -> Result<(Vec<u8>, String), RuntimeError> {
        let _guard = self.busy.lock().await;
        if self.status() != SessionStatus::AwaitingInput || self.turn_active.load(Ordering::SeqCst)
        {
            return Err(RuntimeError::NotAwaitingInput);
        }
        let base = self
            .base_commit()
            .ok_or_else(|| RuntimeError::Workspace("not a repository session".into()))?;
        let head = self
            .sh(&format!("{GIT} rev-parse HEAD"), &[], GIT_TIMEOUT, 1024)
            .await?;
        let head = head.stdout.trim().to_string();
        if head.is_empty() || head == base {
            return Err(RuntimeError::Workspace(
                "the agent has not committed anything yet".into(),
            ));
        }
        // Relative path: commands run in the workspace on every backend.
        let script = format!("rm -f \"$2\" && {GIT} bundle create \"$2\" \"$1..HEAD\" 2>&1");
        let out = self
            .sh(&script, &[&base, BUNDLE_REL], GIT_TIMEOUT, 16 * 1024)
            .await?;
        if out.exit_code != Some(0) {
            return Err(RuntimeError::Workspace(format!(
                "git bundle failed: {}",
                out.stdout.trim()
            )));
        }
        let sandbox = self.sandbox().ok_or(RuntimeError::NotRunning)?;
        let (bytes, truncated) = sandbox.read_file(BUNDLE_PATH, MAX_BUNDLE).await?;
        let _ = self
            .sh("rm -f \"$1\"", &[BUNDLE_REL], Duration::from_secs(10), 1024)
            .await;
        if truncated {
            return Err(RuntimeError::Workspace(
                "the change set is too large to deliver".into(),
            ));
        }
        Ok((bytes, head))
    }

    async fn call_tool(&self, tool: &str, arguments: &serde_json::Value) -> ActionOutcome {
        if tool == "propose_pull_request" {
            let get = |k: &str| {
                arguments
                    .get(k)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            };
            let proposal = PullRequestProposal {
                title: get("title"),
                body: get("body"),
            };
            if proposal.title.is_empty() {
                return ActionOutcome::Failed {
                    error: "`title` is required".into(),
                };
            }
            *lock(&self.proposal) = Some(proposal.clone());
            let _ = self.emit(EventKind::PullRequestProposed { proposal });
            return ActionOutcome::Succeeded {
                output: serde_json::json!({
                    "status": "recorded",
                    "next": "A human reviews your changes and delivers the pull request. You are done unless they ask for more."
                }),
            };
        }
        let Some(handler) = &self.tools else {
            return ActionOutcome::Failed {
                error: format!("no handler registered for tool `{tool}`"),
            };
        };
        match handler.call(tool, arguments).await {
            Ok(output) => ActionOutcome::Succeeded { output },
            Err(error) => ActionOutcome::Failed { error },
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
        if !self.wait_unpaused().await {
            return Err(RuntimeError::NotRunning);
        }
        let action = normalize_action(&action, WORKSPACE);
        // Track in-flight actions so status changes after approvals stay correct.
        let action_id = Uuid::now_v7();
        let count = self.actions.fetch_add(1, Ordering::SeqCst) + 1;
        self.emit(EventKind::ActionRequested {
            action_id,
            action: action.clone(),
            requested_by: Principal::Agent(self.agent_label.clone()),
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
                        self.execute(action_id, &action, contents, limits.max_output_bytes)
                            .await
                    }
                    Some(denied) => denied,
                }
            }
            Verdict::Allow { .. } => {
                self.execute(action_id, &action, contents, limits.max_output_bytes)
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
            self.set_status(self.idle_status());
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
        action_id: Uuid,
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
                    live: Some(self.tool_output(action_id)),
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
            Action::ToolCall { tool, arguments } => {
                return self.call_tool(tool, arguments).await;
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

/// How a principal is named in terminal banners.
fn who(p: &Principal) -> String {
    match p {
        Principal::Human(name) => name.clone(),
        Principal::Agent(name) => format!("agent {name}"),
        Principal::System => "agentcore".into(),
    }
}

fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{} …[truncated]", &s[..cut])
}
