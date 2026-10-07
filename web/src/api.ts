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
  async downloadAudit(id: string) {
    const res = await fetch(`/api/v1/sessions/${id}/audit`, { headers: headers() });
    if (!res.ok) throw new ApiError(res.status, res.statusText);
    const url = URL.createObjectURL(await res.blob());
    const a = Object.assign(document.createElement("a"), { href: url, download: `agentcore-audit-${id}.jsonl` });
    a.click();
    URL.revokeObjectURL(url);
  },
};

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
