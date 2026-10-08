// Mirrors the Rust types in crates/agentcore-core.

export type SessionStatus =
  | "pending"
  | "running"
  | "awaiting_approval"
  | "awaiting_input"
  | "paused"
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
  context: SessionContext;
}

export interface SessionContext {
  role?: string;
  project_id?: string;
  project_name?: string;
  repository?: string;
  base_branch?: string;
  issue?: { number: number; title: string; url: string };
  work_branch?: string;
  delivery_branch?: string;
  pull_request_url?: string;
  department_id?: string;
  department_name?: string;
  org_agent_id?: string;
  org_agent_kind?: AgentKind;
}

// ---- organisations (crates/agentcore-core/src/org.rs) ----

export type AgentKind = "worker" | "communicator";
export type Desired = "stopped" | "running" | "paused";

export interface OrgSettings {
  max_departments: number;
  max_agents_per_department: number;
}

export interface OrgAgent {
  id: string;
  department_id: string;
  name: string;
  kind: AgentKind;
  agent: string;
  instructions: string;
  desired: Desired;
  node: string | null;
  session_id: string | null;
  status: string | null;
  note: string | null;
  changed_by: string;
  created_at: string;
}

export interface Department {
  id: string;
  name: string;
  description: string;
  mission: string;
  policy: string;
  tools: string[];
  communicator_agent: string;
  state: "active" | "paused";
  template: string | null;
  created_at: string;
  updated_by: string;
  agents: OrgAgent[];
}

export interface NodeInfo {
  name: string;
  internal_url: string;
  capacity: number;
  version: string;
  started_at: string;
  last_seen: string;
  agents: number;
  alive: boolean;
}

export interface OrgOverview {
  node: string;
  profile: OrgProfile;
  settings: OrgSettings;
  departments: Department[];
  nodes: NodeInfo[];
  communicator_policy: string;
}

export interface OrgMessage {
  id: string;
  created_at: string;
  scope: "internal" | "inter_department" | "human";
  from_agent: string | null;
  from_department: string | null;
  from_name: string;
  to_kind: "agent" | "department" | "all_departments";
  to_agent: string | null;
  to_department: string | null;
  to_name: string;
  text: string;
  recipients: string[];
}

export interface DepartmentFile {
  path: string;
  size: number;
  updated_at: string;
  updated_by: string;
}

export interface OrgProfile {
  company_name: string;
  company_about: string;
  blueprint: string | null;
}

export interface TemplateAgent {
  name: string;
  title: string;
  core: boolean;
  instructions: string;
}

export interface DepartmentTemplate {
  id: string;
  name: string;
  category: string;
  summary: string;
  when_to_add: string;
  mission: string;
  tools: string[];
  policy: string | null;
  pairs_with: string[];
  agents: TemplateAgent[];
}

export interface Blueprint {
  id: string;
  title: string;
  level: "starter" | "growing" | "complete";
  focus: string;
  audience: string;
  description: string;
  stages: { title: string; description: string; departments: string[] }[];
}

export interface TemplateCatalog {
  categories: { id: string; title: string; description: string }[];
  departments: DepartmentTemplate[];
  blueprints: Blueprint[];
}

export interface PlannedDepartment {
  template: string;
  name: string;
  description: string;
  mission: string;
  tools: string[];
  policy: string | null;
  agents: { name: string; title: string; instructions: string }[];
  exists: boolean;
}

export interface BuildPlan {
  departments: PlannedDepartment[];
  new_departments: number;
  new_agents: number;
  limits: OrgSettings;
  needs: OrgSettings;
  fits: boolean;
}

export interface Suggestion {
  kind: "stage" | "department" | "agent";
  title: string;
  reason: string;
  templates: string[];
  department_id: string | null;
  agent: { name: string; title: string; instructions: string } | null;
  blocked: string | null;
}

export interface CatalogAgent {
  id: string;
  name: string;
  vendor: string;
  summary: string;
  description: string;
  domains: string[];
  homepage: string;
  repository: string;
  license: string;
  license_url: string;
  package: string;
  verified_version: string;
  protocols: ProviderKind[];
  suggested_models: string[];
  guardrails: "full" | "sandbox";
  conversation: boolean;
  installed: string[];
}

export interface AgentEntry {
  source: "config" | "catalog";
  catalog?: string;
  enabled: boolean;
  shadowed_by_config?: boolean;
  updated_at?: string;
  updated_by?: string;
  spec: {
    name: string;
    adapter: string;
    description: string;
    command: string | null;
    policy: string | null;
    provider: string | null;
    model: string | null;
    image: string | null;
    catalog: string | null;
    conversation: boolean;
  };
}

export interface AgentCheck {
  agent: string;
  program: string;
  available: boolean;
  path: string | null;
  version: string | null;
  image: string | null;
  backend: string;
  hint: string | null;
}

export type MessageTo = { agent: string } | { department: string } | "all_departments";

export interface CheckResult {
  name: string;
  passed: boolean;
  optional: boolean;
  skipped: boolean;
  detail: string;
}

export interface PullRequestProposal {
  title: string;
  body: string;
}

export interface Changes {
  base: string;
  head: string | null;
  commits: { sha: string; author: string; subject: string }[];
  files: { path: string; status: string; additions: number | null; deletions: number | null }[];
  patch: string;
  patch_truncated: boolean;
  uncommitted: boolean;
  captured_at: string;
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
  | { event: "workspace_prepared"; repository: string; branch: string; base_commit: string }
  | { event: "role_applied"; role: string; role_digest: string; repo_docs: string[]; prompt_sha256: string }
  | { event: "turn_started"; turn: number }
  | { event: "turn_ended"; turn: number; exit_code: number | null }
  | { event: "user_message"; by: Principal; text: string }
  | { event: "checks_completed"; requested_by: Principal; passed: boolean; results: CheckResult[] }
  | { event: "pull_request_proposed"; proposal: PullRequestProposal }
  | { event: "delivered"; by: Principal; branch: string; commit: string; pull_request_url: string | null }
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
  | { event: "paused"; by: Principal }
  | { event: "resumed"; by: Principal }
  | { event: "recording_closed"; file: string; bytes: number; sha256: string }
  | { event: "session_ended"; status: SessionStatus; exit_code: number | null; reason: string | null };

export type AgentEvent = EventBase & EventKind;

export type ModelCallOutcome = "completed" | "rejected" | "upstream_error" | "aborted";

export type Role = "viewer" | "operator" | "admin";

export interface TeamRole {
  name: string;
  title: string;
  description: string;
  policy: string | null;
  capabilities: string[];
  tools: string[];
  repo_docs: string[];
  instructions: string;
  workflow: { on_start: string | null; on_deliver: string | null; announce_on_issue: boolean };
  delivery: { kind: "pull_request" | "none"; branch: string; draft: boolean };
  checks: { name: string; optional: boolean; kind: string; pattern?: string; run?: string; when_exists?: string }[];
  digest: string;
}

export interface BoardConfig {
  owner: string;
  number: number;
  status_field: string;
  iteration_field: string | null;
  columns: { ready: string; in_progress: string; in_review: string; done: string };
}

export interface Project {
  id: string;
  name: string;
  repo_owner: string;
  repo_name: string;
  default_branch: string;
  agent: string;
  role: string;
  board: BoardConfig | null;
  notes: string;
  updated_at: string;
  updated_by: string;
}

export interface BoardItem {
  id: string;
  status: string | null;
  iteration: string | null;
  content_type: string;
  number: number | null;
  title: string;
  url: string | null;
  state: string | null;
  repository: string | null;
  labels: string[];
  assignees: string[];
  milestone: string | null;
}

export interface Board {
  id: string;
  title: string;
  url: string | null;
  columns: string[];
  current_iteration: string | null;
  items: BoardItem[];
}

export interface IssueSummary {
  number: number;
  title: string;
  state: string;
  url: string;
  labels: string[];
  assignees: string[];
  milestone: string | null;
}

export interface GitHubConnection {
  config: { api_url: string; web_url: string; commit_name: string; commit_email: string };
  token_hint: string;
  updated_at: string;
  updated_by: string;
}

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

// ---- live view (crates/agentcore-core/src/live.rs) ----

export type ModelDeltaKind = "text" | "thinking" | "tool_name" | "tool_input";

export interface FileChange {
  path: string;
  kind: "created" | "modified" | "removed";
  at: string;
}

export interface ProcessInfo {
  pid: number;
  elapsed: string;
  command: string;
}

export type LiveFrame =
  | { frame: "terminal"; data: string }
  | { frame: "terminal_reset"; data: string; cols: number; rows: number }
  | { frame: "tool_output"; action_id: string; stream: "stdout" | "stderr"; data: string }
  | { frame: "model_start"; call_id: string; provider: string; model: string | null }
  | { frame: "model_delta"; call_id: string; kind: ModelDeltaKind; text: string }
  | { frame: "model_end"; call_id: string }
  | { frame: "files"; changes: FileChange[] }
  | { frame: "processes"; processes: ProcessInfo[] };
