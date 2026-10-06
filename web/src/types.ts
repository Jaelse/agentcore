// Mirrors the Rust types in crates/agentcore-core.

export type SessionStatus =
  | "pending"
  | "running"
  | "awaiting_approval"
  | "stopped"
  | "completed"
  | "failed";

export const TERMINAL: SessionStatus[] = ["stopped", "completed", "failed"];

export type Principal =
  | { kind: "human"; id: string }
  | { kind: "agent"; id: string }
  | { kind: "system" };

export type Action =
  | { type: "exec"; command: string; args: string[]; cwd?: string | null }
  | { type: "file_read"; path: string }
  | { type: "file_write"; path: string; bytes: number }
  | { type: "network"; host: string; port?: number | null }
  | { type: "tool_call"; tool: string; arguments: unknown };

export type Verdict =
  | { verdict: "allow"; rule: string | null }
  | { verdict: "deny"; rule: string | null; reason: string }
  | { verdict: "require_approval"; rule: string | null; reason: string };

export type ActionOutcome =
  | { status: "succeeded"; output?: Record<string, unknown> }
  | { status: "failed"; error: string }
  | { status: "denied"; reason: string };

export interface SessionInfo {
  id: string;
  agent: string;
  task: string;
  policy: string;
  status: SessionStatus;
  created_by: Principal;
  created_at: string;
  ended_at: string | null;
  pending_approvals: number;
  actions: number;
  model_calls: number;
}

export interface PendingApproval {
  approval_id: string;
  action_id: string;
  action: Action;
  reason: string;
  requested_at: string;
  expires_at: string;
}

interface EventBase {
  id: string;
  session_id: string;
  seq: number;
  timestamp: string;
}

export type EventKind =
  | { event: "session_created"; agent: string; task: string; policy: string; policy_digest: string; created_by: Principal }
  | { event: "sandbox_started"; backend: string; details: unknown }
  | { event: "agent_started"; program: string; args: string[] }
  | { event: "output"; stream: "stdout" | "stderr"; line: string }
  | { event: "action_requested"; action_id: string; action: Action; requested_by: Principal }
  | { event: "policy_evaluated"; action_id: string; verdict: Verdict }
  | { event: "approval_requested"; approval_id: string; action_id: string; action: Action; reason: string }
  | { event: "approval_resolved"; approval_id: string; action_id: string; approved: boolean; by: Principal; comment: string | null }
  | { event: "action_completed"; action_id: string; outcome: ActionOutcome }
  | {
      event: "model_call";
      call_id: string;
      provider: string;
      model: string | null;
      path: string;
      http_status: number | null;
      outcome: ModelCallOutcome;
      input_tokens: number | null;
      output_tokens: number | null;
      duration_ms: number;
      request_sha256: string;
      response_sha256: string | null;
      detail: string | null;
    }
  | { event: "status_changed"; status: SessionStatus }
  | { event: "stop_requested"; by: Principal; reason: string }
  | { event: "session_ended"; status: SessionStatus; exit_code: number | null; reason: string | null };

export type AgentEvent = EventBase & EventKind;

export type ModelCallOutcome = "completed" | "rejected" | "upstream_error" | "aborted";

export type Role = "viewer" | "operator" | "admin";

export interface Me {
  name: string;
  role: Role;
}

export type ProviderKind = "anthropic" | "openai" | "opencode_zen";

export interface ProviderInfo {
  name: string;
  kind: ProviderKind;
  base_url: string;
  api_key_hint: string;
  allowed_models: string[];
  enabled: boolean;
  updated_at: string;
  updated_by: string;
}

export interface ModelCallRecord {
  id: string;
  session_id: string;
  provider: string;
  model: string | null;
  method: string;
  path: string;
  http_status: number | null;
  outcome: ModelCallOutcome;
  detail: string | null;
  input_tokens: number | null;
  output_tokens: number | null;
  started_at: string;
  duration_ms: number;
  request_body: string | null;
  response_body: string | null;
  bodies_truncated: boolean;
  request_sha256: string;
  response_sha256: string | null;
}

export interface AdminEvent {
  id: number;
  at: string;
  actor: string;
  action: string;
  target: string;
  details: Record<string, unknown>;
}

export interface SystemCard {
  ai_system: boolean;
  version: string;
  sandbox_backend: string;
  model_gateway: { log_bodies: boolean; max_logged_body_bytes: number };
  default_policy: string;
  audit_retention_days: number;
  transparency: {
    system_name: string;
    provider: string;
    contact: string;
    intended_purpose: string;
    limitations: string[];
  };
  agents: { name: string; adapter: string; description: string; policy: string | null }[];
  policies: {
    name: string;
    description: string;
    digest: string;
    default: string;
    limits: Record<string, number>;
    rules: { id: string; description: string; effect: string; kinds: string[] }[];
  }[];
}

export function principalLabel(p: Principal): string {
  return p.kind === "system" ? "system" : `${p.kind}:${p.id}`;
}

export function actionSummary(a: Action): string {
  switch (a.type) {
    case "exec":
      return [a.command, ...a.args].join(" ");
    case "file_read":
      return `read ${a.path}`;
    case "file_write":
      return `write ${a.path} (${a.bytes} bytes)`;
    case "network":
      return `connect ${a.host}${a.port ? `:${a.port}` : ""}`;
    case "tool_call":
      return `tool ${a.tool}`;
  }
}
