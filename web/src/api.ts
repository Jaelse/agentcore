import type {
  Board,
  BoardConfig,
  Changes,
  CheckResult,
  GitHubConnection,
  IssueSummary,
  Project,
  PullRequestProposal,
  TeamRole,
  AdminEvent,
  AgentEvent,
  Department,
  DepartmentFile,
  LiveFrame,
  MessageTo,
  OrgAgent,
  OrgMessage,
  OrgOverview,
  OrgSettings,
  BuildPlan,
  OrgProfile,
  Suggestion,
  TemplateCatalog,
  AgentCheck,
  AgentEntry,
  CatalogAgent,
  Me,
  ModelCallRecord,
  PendingApproval,
  PoolToken,
  ProviderInfo,
  KnownModel,
  ProviderKind,
  SessionInfo,
  SystemCard,
  OrgGoal,
  CheckIn,
  DataSource,
  DataSourceKind,
  OrgMetrics,
  Proposal,
  ProposalAction,
  ProposalStatus,
  Spending,
  ModelPrice,
  BudgetPeriod,
  BudgetAction,
  BudgetUnit,
  HoursWindow,
  DepartmentHours,
  Channel,
  ChannelKind,
  OutboxItem,
  Product,
  ProductInput,
  ProductSummary,
  Release,
  ReleaseDetail,
  LaunchStats,
  FeedKind,
  FeedPage,
  WorkItem,
} from "./types";

const TOKEN_KEY = "agentcore.token";

export function getToken(): string | null {
  try {
    return sessionStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

export function setToken(token: string | null) {
  try {
    if (token) sessionStorage.setItem(TOKEN_KEY, token);
    else sessionStorage.removeItem(TOKEN_KEY);
  } catch {
    // Storage unavailable: the token lives only for this page load.
  }
}

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
    public body?: Record<string, unknown>,
  ) {
    super(message);
  }
}

function headers(extra?: HeadersInit): HeadersInit {
  const token = getToken();
  return { ...(token ? { Authorization: `Bearer ${token}` } : {}), ...extra };
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(`/api/v1${path}`, {
    method,
    headers: headers(body === undefined ? undefined : { "Content-Type": "application/json" }),
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) {
    let message = res.statusText;
    let body: Record<string, unknown> | undefined;
    try {
      body = await res.json();
      message = (body?.error as string) ?? message;
    } catch {
      // Non-JSON error body.
    }
    throw new ApiError(res.status, message, body);
  }
  // 202/204 and other empty bodies carry no JSON.
  const text = await res.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

export const api = {
  whoami: () => request<Me>("GET", "/whoami"),
  systemCard: () => request<SystemCard>("GET", "/system-card"),
  sessions: (archived = false) => request<SessionInfo[]>("GET", `/sessions${archived ? "?archived=true" : ""}`),
  archiveSession: (id: string) => request<void>("POST", `/sessions/${id}/archive`),
  unarchiveSession: (id: string) => request<void>("POST", `/sessions/${id}/unarchive`),
  archiveFinished: (olderThanHours?: number) =>
    request<{ archived: number }>("POST", "/sessions/archive-finished", { older_than_hours: olderThanHours ?? null }),
  session: (id: string) =>
    request<{
      session: SessionInfo;
      live: boolean;
      policy: { name: string; digest: string };
      approvals: PendingApproval[];
      proposal: PullRequestProposal | null;
      role: { name: string; title: string; delivery: "pull_request" | "none"; checks: string[] } | null;
    }>("GET", `/sessions/${id}`),
  sendMessage: (id: string, text: string) => request<void>("POST", `/sessions/${id}/messages`, { text }),
  finish: (id: string) => request<void>("POST", `/sessions/${id}/finish`),
  changes: (id: string) => request<Changes | null>("GET", `/sessions/${id}/changes`),
  runChecks: (id: string) => request<{ passed: boolean; checks: CheckResult[] }>("POST", `/sessions/${id}/checks`),
  deliver: (id: string, body: { title?: string; body?: string; draft?: boolean }) =>
    request<{ branch: string; commit: string; pull_request: { number: number; url: string }; checks: CheckResult[] }>(
      "POST",
      `/sessions/${id}/deliver`,
      body,
    ),
  roles: () => request<TeamRole[]>("GET", "/roles"),
  github: () => request<GitHubConnection | null>("GET", "/integrations/github"),
  saveGithub: (body: {
    token?: string;
    api_url: string;
    web_url: string;
    commit_name: string;
    commit_email: string;
  }) => request<GitHubConnection>("PUT", "/integrations/github", body),
  testGithub: () => request<{ login: string }>("POST", "/integrations/github/test"),
  deleteGithub: () => request<void>("DELETE", "/integrations/github"),
  projects: () => request<Project[]>("GET", "/projects"),
  saveProject: (
    id: string | null,
    body: {
      name: string;
      repository: string;
      default_branch: string;
      agent: string;
      role: string;
      board: BoardConfig | null;
      notes: string;
    },
  ) => (id ? request<Project>("PUT", `/projects/${id}`, body) : request<Project>("POST", "/projects", body)),
  deleteProject: (id: string) => request<void>("DELETE", `/projects/${id}`),
  board: (id: string) => request<{ board: Board; config: BoardConfig }>("GET", `/projects/${id}/board`),
  issues: (id: string, state = "open") => request<IssueSummary[]>("GET", `/projects/${id}/issues?state=${state}`),
  startProjectSession: (
    id: string,
    body: { issue_number?: number; task?: string; role?: string; agent?: string; policy?: string },
  ) => request<SessionInfo>("POST", `/projects/${id}/sessions`, body),
  createSession: (agent: string, task: string, policy?: string) =>
    request<SessionInfo>("POST", "/sessions", { agent, task, policy: policy || undefined }),
  pause: (id: string) => request<SessionInfo>("POST", `/sessions/${id}/pause`),
  resume: (id: string) => request<SessionInfo>("POST", `/sessions/${id}/resume`),
  /** Type into the agent's terminal (keystrokes as a terminal sends them). */
  typeInput: (id: string, data: string) => request<void>("POST", `/sessions/${id}/input`, { data }),
  /** The terminal recording (asciicast v2), or null if there is none. */
  async recording(id: string): Promise<string | null> {
    const res = await fetch(`/api/v1/sessions/${id}/recording`, { headers: headers() });
    if (res.status === 404) return null;
    if (!res.ok) throw new ApiError(res.status, res.statusText);
    return res.text();
  },
  async downloadRecording(id: string) {
    const res = await fetch(`/api/v1/sessions/${id}/recording`, { headers: headers() });
    if (!res.ok) throw new ApiError(res.status, res.statusText);
    const url = URL.createObjectURL(await res.blob());
    const a = Object.assign(document.createElement("a"), { href: url, download: `agentcore-${id}.cast` });
    a.click();
    URL.revokeObjectURL(url);
  },
  stop: (id: string, reason?: string) => request<SessionInfo>("POST", `/sessions/${id}/stop`, { reason }),
  stopAll: (reason?: string) => request<{ stopped: number }>("POST", "/stop-all", { reason }),
  decide: (id: string, approvalId: string, approved: boolean, comment?: string) =>
    request<void>("POST", `/sessions/${id}/approvals/${approvalId}`, { approved, comment }),
  verifyAudit: (id: string) =>
    request<{ valid: boolean; records?: number; head?: string; error?: string }>(
      "GET",
      `/sessions/${id}/audit/verify`,
    ),
  modelCall: (sessionId: string, callId: string) =>
    request<ModelCallRecord>("GET", `/sessions/${sessionId}/model-calls/${callId}`),
  providers: () => request<ProviderInfo[]>("GET", "/providers"),
  createProvider: (p: {
    name: string;
    kind: ProviderKind;
    base_url?: string;
    api_key: string;
    allowed_models: string[];
  }) => request<ProviderInfo>("POST", "/providers", p),
  updateProvider: (
    name: string,
    update: Partial<{ base_url: string; api_key: string; allowed_models: string[]; enabled: boolean }>,
  ) => request<ProviderInfo>("PATCH", `/providers/${encodeURIComponent(name)}`, update),
  deleteProvider: (name: string) => request<void>("DELETE", `/providers/${encodeURIComponent(name)}`),
  poolTokens: (provider: string) =>
    request<PoolToken[]>("GET", `/providers/${encodeURIComponent(provider)}/tokens`),
  addPoolToken: (provider: string, label: string, token: string) =>
    request<PoolToken>("POST", `/providers/${encodeURIComponent(provider)}/tokens`, { label, token }),
  setPoolTokenEnabled: (provider: string, id: string, enabled: boolean) =>
    request<void>("PATCH", `/providers/${encodeURIComponent(provider)}/tokens/${id}`, { enabled }),
  removePoolToken: (provider: string, id: string) =>
    request<void>("DELETE", `/providers/${encodeURIComponent(provider)}/tokens/${id}`),
  testPoolToken: (provider: string, id: string) =>
    request<{ ok: boolean; status: number | null; message: string }>(
      "POST",
      `/providers/${encodeURIComponent(provider)}/tokens/${id}/test`,
    ),
  testProvider: (provider: string, model?: string) =>
    request<{ ok: boolean; status: number | null; message: string }>(
      "POST",
      `/providers/${encodeURIComponent(provider)}/test${model ? `?model=${encodeURIComponent(model)}` : ""}`,
    ),
  attachAgents: (provider: string) =>
    request<{ provider: string; updated: string[]; skipped: { name: string; reason: string }[] }>(
      "POST",
      `/providers/${encodeURIComponent(provider)}/agents`,
    ),
  adminEvents: () => request<AdminEvent[]>("GET", "/admin-events"),
  org: () => request<OrgOverview>("GET", "/org"),
  saveOrgSettings: (s: OrgSettings) => request<OrgSettings>("PUT", "/org/settings", s),
  saveDepartment: (
    id: string | null,
    body: {
      name: string;
      description: string;
      mission: string;
      policy: string;
      tools: string[];
      communicator_agent: string;
      project_id: string | null;
      role: string | null;
      requires_repository: boolean;
    },
  ) =>
    id ? request<Department>("PUT", `/org/departments/${id}`, body) : request<Department>("POST", "/org/departments", body),
  deleteDepartment: (id: string) => request<void>("DELETE", `/org/departments/${id}`),
  controlDepartment: (id: string, control: "start" | "pause" | "resume" | "stop") =>
    request<{ changed: number; failed: { agent: string; error: string }[] }>("POST", `/org/departments/${id}/${control}`),
  addAgent: (
    department: string,
    body: { name: string; agent: string; instructions: string; provider?: string; model?: string },
  ) =>
    request<OrgAgent>("POST", `/org/departments/${department}/agents`, body),
  updateAgent: (id: string, body: { agent?: string; instructions?: string; provider?: string; model?: string }) =>
    request<OrgAgent>("PUT", `/org/agents/${id}`, body),
  deleteAgent: (id: string) => request<void>("DELETE", `/org/agents/${id}`),
  controlAgent: (id: string, control: "start" | "pause" | "resume" | "stop") =>
    request<OrgAgent>("POST", `/org/agents/${id}/${control}`),
  agentCatalog: () => request<{ agents: CatalogAgent[]; allowed_licenses: string[]; provider_models: Partial<Record<ProviderKind, KnownModel[]>> }>("GET", "/agents/catalog"),
  agentList: () => request<AgentEntry[]>("GET", "/agents"),
  installAgent: (body: {
    catalog: string;
    name: string;
    provider: string;
    model: string;
    policy?: string;
    image?: string;
    description?: string;
    options?: Record<string, string>;
  }) => request<unknown>("POST", "/agents", body),
  updateAgentInstall: (
    name: string,
    body: Partial<{ provider: string; model: string; policy: string; image: string; enabled: boolean }>,
  ) => request<unknown>("PUT", `/agents/${encodeURIComponent(name)}`, body),
  uninstallAgent: (name: string) => request<void>("DELETE", `/agents/${encodeURIComponent(name)}`),
  checkAgent: (name: string) => request<AgentCheck>("POST", `/agents/${encodeURIComponent(name)}/check`),
  templates: () => request<TemplateCatalog>("GET", "/org/templates"),
  suggestions: () => request<Suggestion[]>("GET", "/org/suggestions"),
  saveProfile: (p: OrgProfile) => request<OrgProfile>("PUT", "/org/profile", p),
  build: (body: {
    profile?: OrgProfile;
    departments: string[];
    size: "lean" | "full";
    agent?: string;
    communicator_agent?: string;
    project_id?: string | null;
    start?: boolean;
    goal?: string;
    check_ins?: boolean;
    raise_limits?: boolean;
    dry_run?: boolean;
  }) =>
    request<{ plan: BuildPlan; created?: Department[]; errors?: { department: string; agent?: string; error: string }[] }>(
      "POST",
      "/org/build",
      body,
    ),
  createGoal: (body: { title: string; description: string; department_id: string | null }) =>
    request<OrgGoal>("POST", "/org/goals", body),
  updateGoal: (
    id: string,
    body: Partial<{ title: string; description: string; department_id: string | null; status: OrgGoal["status"] }>,
  ) => request<OrgGoal>("PUT", `/org/goals/${id}`, body),
  deleteGoal: (id: string) => request<void>("DELETE", `/org/goals/${id}`),
  goalProgress: (id: string, text: string) => request<OrgGoal>("POST", `/org/goals/${id}/progress`, { text }),
  createCheckIn: (
    department: string,
    body: { name: string; message: string; every_minutes: number; agent_id: string | null },
  ) => request<CheckIn>("POST", `/org/departments/${department}/checkins`, body),
  updateCheckIn: (id: string, body: Partial<{ name: string; message: string; every_minutes: number; enabled: boolean }>) =>
    request<CheckIn>("PUT", `/org/checkins/${id}`, body),
  deleteCheckIn: (id: string) => request<void>("DELETE", `/org/checkins/${id}`),
  runCheckIn: (id: string) => request<CheckIn>("POST", `/org/checkins/${id}/run`),
  feed: (opts: { kind?: FeedKind; before?: string; limit?: number } = {}) => {
    const q = new URLSearchParams();
    if (opts.kind) q.set("kind", opts.kind);
    if (opts.before) q.set("before", opts.before);
    if (opts.limit) q.set("limit", String(opts.limit));
    return request<FeedPage>("GET", `/feed?${q}`);
  },
  metrics: (days: number) => request<OrgMetrics>("GET", `/org/metrics?days=${days}`),
  proposals: (status?: ProposalStatus) =>
    request<Proposal[]>("GET", `/org/proposals${status ? `?status=${status}` : ""}`),
  updateProposal: (
    id: string,
    body: Partial<{ title: string; problem: string; evidence: string; solution: string; actions: ProposalAction[]; note: string }>,
  ) => request<Proposal>("PUT", `/org/proposals/${id}`, body),
  applyProposal: (id: string, revision: number) => request<Proposal>("POST", `/org/proposals/${id}/apply`, { revision }),
  requestChanges: (id: string, text: string) => request<Proposal>("POST", `/org/proposals/${id}/changes`, { text }),
  rejectProposal: (id: string, text: string) => request<Proposal>("POST", `/org/proposals/${id}/reject`, { text }),
  autoApply: () => request<{ kinds: string[]; by: string | null; available: string[] }>("GET", "/org/auto-apply"),
  setAutoApply: (kinds: string[]) => request<{ kinds: string[] }>("PUT", "/org/auto-apply", { kinds }),
  dataSources: () => request<DataSource[]>("GET", "/org/data-sources"),
  createDataSource: (body: {
    name: string;
    kind: DataSourceKind;
    description: string;
    config: Record<string, unknown>;
    secret?: string;
    content?: string;
    departments: string[];
  }) => request<DataSource>("POST", "/org/data-sources", body),
  updateDataSource: (
    id: string,
    body: Partial<{ description: string; config: Record<string, unknown>; secret: string; content: string; departments: string[]; enabled: boolean }>,
  ) => request<DataSource>("PUT", `/org/data-sources/${id}`, body),
  deleteDataSource: (id: string) => request<void>("DELETE", `/org/data-sources/${id}`),
  testDataSource: (id: string, args?: Record<string, unknown>) =>
    request<{ ok: boolean; result?: unknown; error?: string }>("POST", `/org/data-sources/${id}/test`, args ?? {}),
  spending: (days = 30) => request<Spending>("GET", `/org/spending?days=${days}`),
  setPrice: (body: { provider: string; model: string; input_per_mtok: number; output_per_mtok: number }) =>
    request<ModelPrice>("PUT", "/org/prices", body),
  deletePrice: (provider: string, model: string) =>
    request<void>("DELETE", `/org/prices?provider=${encodeURIComponent(provider)}&model=${encodeURIComponent(model)}`),
  pricePastCalls: () => request<{ priced: number }>("POST", "/org/prices/backfill"),
  setCurrency: (currency: string) => request<{ currency: string }>("PUT", "/org/currency", { currency }),
  createBudget: (body: { department_id: string | null; period: BudgetPeriod; unit: BudgetUnit; limit_micros: number; action: BudgetAction; warn_percent: number }) =>
    request<unknown>("POST", "/org/budgets", body),
  updateBudget: (id: string, body: Partial<{ limit_micros: number; action: BudgetAction; warn_percent: number }>) =>
    request<unknown>("PUT", `/org/budgets/${id}`, body),
  deleteBudget: (id: string) => request<void>("DELETE", `/org/budgets/${id}`),
  setHours: (department: string, body: { enabled: boolean; timezone: string; windows: HoursWindow[] }) =>
    request<DepartmentHours>("PUT", `/org/departments/${department}/hours`, body),
  deleteHours: (department: string) => request<void>("DELETE", `/org/departments/${department}/hours`),
  outbox: () => request<OutboxItem[]>("GET", "/org/outbox"),
  editOutbox: (id: string, body: Partial<{ recipients: string[]; subject: string; body: string; note: string }>) =>
    request<OutboxItem>("PUT", `/org/outbox/${id}`, body),
  sendOutbox: (id: string, revision: number) => request<OutboxItem>("POST", `/org/outbox/${id}/send`, { revision }),
  outboxChanges: (id: string, text: string) => request<OutboxItem>("POST", `/org/outbox/${id}/changes`, { text }),
  rejectOutbox: (id: string, text: string) => request<OutboxItem>("POST", `/org/outbox/${id}/reject`, { text }),
  createChannel: (body: {
    name: string;
    kind: ChannelKind;
    description: string;
    config: Record<string, unknown>;
    secret?: string;
    departments: string[];
    requires_approval: boolean;
    max_per_day: number;
    disclosure?: string;
  }) => request<Channel>("POST", "/org/channels", body),
  updateChannel: (
    id: string,
    body: Partial<{ description: string; config: Record<string, unknown>; secret: string; departments: string[]; requires_approval: boolean; max_per_day: number; disclosure: string; enabled: boolean }>,
  ) => request<Channel>("PUT", `/org/channels/${id}`, body),
  deleteChannel: (id: string) => request<void>("DELETE", `/org/channels/${id}`),
  products: () => request<ProductSummary[]>("GET", "/releases/products"),
  createProduct: (body: ProductInput) =>
    request<{ product: Product; client_token: string }>("POST", "/releases/products", body),
  updateProduct: (id: string, body: Partial<ProductInput> & { enabled?: boolean }) =>
    request<Product>("PUT", `/releases/products/${id}`, body),
  deleteProduct: (id: string) => request<void>("DELETE", `/releases/products/${id}`),
  rotateClientToken: (id: string) =>
    request<{ product: Product; client_token: string }>("POST", `/releases/products/${id}/token`),
  freezeProduct: (id: string, reason: string) => request<Product>("PUT", `/releases/products/${id}/freeze`, { reason }),
  unfreezeProduct: (id: string) => request<Product>("DELETE", `/releases/products/${id}/freeze`),
  launchStats: (id: string, days = 30) => request<LaunchStats>("GET", `/releases/products/${id}/launch?days=${days}`),
  /** Download the product's subscribers as CSV (admin). */
  downloadSubscribers: async (id: string, name: string) => {
    const res = await fetch(`/api/v1/releases/products/${id}/subscribers.csv`, { headers: headers() });
    if (!res.ok) throw new ApiError(res.status, res.statusText);
    const url = URL.createObjectURL(await res.blob());
    const a = document.createElement("a");
    a.href = url;
    a.download = `${name}-subscribers.csv`;
    a.click();
    URL.revokeObjectURL(url);
  },
  releases: (product?: string) => request<Release[]>("GET", `/releases${product ? `?product=${product}` : ""}`),
  release: (id: string) => request<ReleaseDetail>("GET", `/releases/${id}`),
  approveRelease: (id: string, revision: number) =>
    request<{ release: Release; started: boolean; note?: string }>("POST", `/releases/${id}/approve`, { revision }),
  startRelease: (id: string) => request<Release>("POST", `/releases/${id}/start`),
  releaseChanges: (id: string, text: string) => request<Release>("POST", `/releases/${id}/changes`, { text }),
  rejectRelease: (id: string, text: string) => request<Release>("POST", `/releases/${id}/reject`, { text }),
  pauseRelease: (id: string, text: string) => request<Release>("POST", `/releases/${id}/pause`, { text }),
  resumeRelease: (id: string) => request<Release>("POST", `/releases/${id}/resume`),
  promoteRelease: (id: string) => request<Release>("POST", `/releases/${id}/promote`),
  rollBackRelease: (id: string, text: string) => request<Release>("POST", `/releases/${id}/rollback`, { text }),
  testChannel: (id: string, to: string[]) => request<{ ok: boolean; error?: string }>("POST", `/org/channels/${id}/test`, { to }),
  pauseAll: () => request<void>("POST", "/org/pause-all"),
  resumeAll: () => request<void>("POST", "/org/resume-all"),
  messages: (q: { department?: string; agent?: string; limit?: number } = {}) => {
    const params = new URLSearchParams();
    if (q.department) params.set("department", q.department);
    if (q.agent) params.set("agent", q.agent);
    if (q.limit) params.set("limit", String(q.limit));
    const query = params.toString();
    return request<OrgMessage[]>("GET", `/org/messages${query ? `?${query}` : ""}`);
  },
  postMessage: (to: MessageTo, text: string) => request<OrgMessage>("POST", "/org/messages", { to, text }),
  departmentFiles: (id: string) => request<DepartmentFile[]>("GET", `/org/departments/${id}/files`),
  workItems: (department: string, closed = false) =>
    request<WorkItem[]>("GET", `/org/work-items?department=${department}${closed ? "&closed=true" : ""}`),
  assignWorkItem: (id: string, agent: string) =>
    request<WorkItem>("POST", `/org/work-items/${id}/assign`, { agent }),
  closeWorkItem: (id: string, note?: string) => request<WorkItem>("POST", `/org/work-items/${id}/close`, { note }),
  departmentFile: (id: string, path: string) =>
    request<{ path: string; content: string }>(
      "GET",
      `/org/departments/${id}/files/${path.split("/").map(encodeURIComponent).join("/")}`,
    ),
  async downloadAudit(id: string) {
    const res = await fetch(`/api/v1/sessions/${id}/audit`, { headers: headers() });
    if (!res.ok) throw new ApiError(res.status, res.statusText);
    const url = URL.createObjectURL(await res.blob());
    const a = Object.assign(document.createElement("a"), { href: url, download: `agentcore-audit-${id}.jsonl` });
    a.click();
    URL.revokeObjectURL(url);
  },
};

/** Read a server-sent event stream, calling `onMessage(type, data)` per message. */
async function readSse(res: Response, onMessage: (type: string, data: string) => void) {
  if (!res.body) return;
  const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
  let buffer = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) return;
    buffer += value;
    let split: number;
    while ((split = buffer.indexOf("\n\n")) >= 0) {
      const frame = buffer.slice(0, split);
      buffer = buffer.slice(split + 2);
      let type = "message";
      const data: string[] = [];
      for (const line of frame.split("\n")) {
        if (line.startsWith("event:")) type = line.slice(6).trim();
        else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
      }
      if (data.length) onMessage(type, data.join("\n"));
    }
  }
}

/**
 * Organisation change notifications from every node. Each one only says
 * what changed; `onChange` refetches. Reconnects automatically.
 */
export function streamOrg(onChange: (kind: string) => void): () => void {
  const controller = new AbortController();
  const run = async () => {
    while (!controller.signal.aborted) {
      try {
        const res = await fetch("/api/v1/org/stream", {
          headers: headers({ Accept: "text/event-stream" }),
          signal: controller.signal,
        });
        if (!res.ok) throw new ApiError(res.status, res.statusText);
        onChange("resync");
        await readSse(res, (type, data) => {
          if (type === "lagged") onChange("resync");
          else if (type === "org") {
            try {
              onChange((JSON.parse(data) as { kind?: string }).kind ?? "resync");
            } catch {
              onChange("resync");
            }
          }
        });
      } catch {
        if (controller.signal.aborted) return;
      }
      await new Promise((r) => setTimeout(r, 2000));
    }
  };
  void run();
  return () => controller.abort();
}

/**
 * Watch a running session's live view (terminal, model stream, files,
 * processes). Every (re)connection starts with a `terminal_reset` frame
 * carrying the recent screen. `onEnded` fires when the session is over.
 */
export function streamLive(
  sessionId: string,
  onFrame: (f: LiveFrame) => void,
  onConnected: (connected: boolean) => void,
  onEnded: () => void,
): () => void {
  const controller = new AbortController();
  const run = async () => {
    while (!controller.signal.aborted) {
      let ended = false;
      try {
        const res = await fetch(`/api/v1/sessions/${sessionId}/live`, {
          headers: headers({ Accept: "text/event-stream" }),
          signal: controller.signal,
        });
        if (!res.ok) throw new ApiError(res.status, res.statusText);
        onConnected(true);
        await readSse(res, (type, data) => {
          if (type === "frame") onFrame(JSON.parse(data) as LiveFrame);
          else if (type === "ended") ended = true;
        });
      } catch (err) {
        if (controller.signal.aborted) return;
        if (err instanceof ApiError && err.status === 404) return;
      }
      onConnected(false);
      if (ended) {
        onEnded();
        return;
      }
      await new Promise((r) => setTimeout(r, 1000));
    }
  };
  void run();
  return () => controller.abort();
}

/**
 * Subscribe to a session's server-sent events. Uses fetch (not EventSource)
 * so the bearer token travels in a header rather than the URL. Reconnects
 * automatically, resuming after the last seen sequence number.
 */
export function streamEvents(
  sessionId: string,
  onEvent: (e: AgentEvent) => void,
  onConnected: (connected: boolean) => void,
): () => void {
  const controller = new AbortController();
  let lastSeq: number | null = null;

  const run = async () => {
    while (!controller.signal.aborted) {
      try {
        const query = lastSeq === null ? "" : `?after=${lastSeq}`;
        const res = await fetch(`/api/v1/sessions/${sessionId}/stream${query}`, {
          headers: headers({ Accept: "text/event-stream" }),
          signal: controller.signal,
        });
        if (!res.ok || !res.body) throw new ApiError(res.status, res.statusText);
        onConnected(true);
        const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
        let buffer = "";
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          buffer += value;
          let split: number;
          while ((split = buffer.indexOf("\n\n")) >= 0) {
            const frame = buffer.slice(0, split);
            buffer = buffer.slice(split + 2);
            let type = "message";
            const data: string[] = [];
            for (const line of frame.split("\n")) {
              if (line.startsWith("event:")) type = line.slice(6).trim();
              else if (line.startsWith("data:")) data.push(line.slice(5).trimStart());
            }
            if (type === "event" && data.length) {
              const event = JSON.parse(data.join("\n")) as AgentEvent;
              if (lastSeq === null || event.seq > lastSeq) {
                lastSeq = event.seq;
                onEvent(event);
              }
            }
          }
        }
      } catch (err) {
        if (controller.signal.aborted) return;
        if (err instanceof ApiError && err.status === 404) return;
      }
      onConnected(false);
      await new Promise((r) => setTimeout(r, 1000));
    }
  };
  void run();
  return () => controller.abort();
}
