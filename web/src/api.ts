import type { AgentEvent, Me, PendingApproval, SessionInfo, SystemCard } from "./types";

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
  constructor(public status: number, message: string) {
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
    try {
      message = (await res.json()).error ?? message;
    } catch {
      // Non-JSON error body.
    }
    throw new ApiError(res.status, message);
  }
  return res.status === 204 ? (undefined as T) : res.json();
}

export const api = {
  whoami: () => request<Me>("GET", "/whoami"),
  systemCard: () => request<SystemCard>("GET", "/system-card"),
  sessions: () => request<SessionInfo[]>("GET", "/sessions"),
  session: (id: string) =>
    request<{ session: SessionInfo; policy: { name: string; digest: string }; approvals: PendingApproval[] }>(
      "GET",
      `/sessions/${id}`,
    ),
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
