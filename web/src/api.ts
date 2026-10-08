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
  ProviderInfo,
  ProviderKind,
  SessionInfo,
  SystemCard,
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
  sessions: () => request<SessionInfo[]>("GET", "/sessions"),
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
    },
  ) =>
    id ? request<Department>("PUT", `/org/departments/${id}`, body) : request<Department>("POST", "/org/departments", body),
  deleteDepartment: (id: string) => request<void>("DELETE", `/org/departments/${id}`),
  controlDepartment: (id: string, control: "start" | "pause" | "resume" | "stop") =>
    request<{ changed: number; failed: { agent: string; error: string }[] }>("POST", `/org/departments/${id}/${control}`),
  addAgent: (department: string, body: { name: string; agent: string; instructions: string }) =>
    request<OrgAgent>("POST", `/org/departments/${department}/agents`, body),
  updateAgent: (id: string, body: { agent?: string; instructions?: string }) =>
    request<OrgAgent>("PUT", `/org/agents/${id}`, body),
  deleteAgent: (id: string) => request<void>("DELETE", `/org/agents/${id}`),
  controlAgent: (id: string, control: "start" | "pause" | "resume" | "stop") =>
    request<OrgAgent>("POST", `/org/agents/${id}/${control}`),
  agentCatalog: () => request<{ agents: CatalogAgent[]; allowed_licenses: string[] }>("GET", "/agents/catalog"),
  agentList: () => request<AgentEntry[]>("GET", "/agents"),
  installAgent: (body: {
    catalog: string;
    name: string;
    provider: string;
    model: string;
    policy?: string;
    image?: string;
    description?: string;
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
    raise_limits?: boolean;
    dry_run?: boolean;
  }) =>
    request<{ plan: BuildPlan; created?: Department[]; errors?: { department: string; agent?: string; error: string }[] }>(
      "POST",
      "/org/build",
      body,
    ),
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
