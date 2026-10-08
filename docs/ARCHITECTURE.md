# Architecture

This document explains how agentcore is built and how its parts interact.
For *using* it, start with the [README](../README.md); for the concepts behind
roles, read [Ways of working](WAYS_OF_WORKING.md).

- [System overview](#system-overview)
- [Crates](#crates)
- [Session lifecycle](#session-lifecycle)
- [Event pipeline](#event-pipeline)
- [Life of an action](#life-of-an-action)
- [Stopping](#stopping)
- [Live view: watching the agent work](#live-view)
- [Pausing](#pausing)
- [Model gateway](#model-gateway)
- [Team work: repository, conversation, delivery](#team-work)
- [Persistence](#persistence)
- [Agents and adapters](#agents-and-adapters)
- [Sandbox backends](#sandbox-backends)
- [Roadmap](#roadmap)

## System overview

```mermaid
flowchart LR
    OP["Operators & viewers<br/>(browser)"]
    subgraph AC["agentcore"]
        UI["Web UI +<br/>operator API"] --> RT["Runtime<br/>sessions · policy ·<br/>approvals · audit"]
        GW["Gateways<br/>tools /mcp · models /llm"] --> RT
    end
    subgraph SB["Sandbox (one per session)"]
        AG["Agent<br/>+ /workspace checkout"]
    end
    OP --> UI
    AG -- "tool & LLM calls<br/>(session token)" --> GW
    GW -- "real API key" --> PROV["LLM providers"]
    RT -- "GitHub token" --> GH["GitHub<br/>repo · issues · board"]
    RT --> PG[("PostgreSQL")]
```

The agent can only reach agentcore. Every side effect is a tool call through
the tool gateway, which the runtime checks against policy and then executes
inside the sandbox (or on GitHub); every LLM call goes through the model
gateway. Real
credentials (model API keys, the GitHub token) never enter the sandbox.

## Crates

```mermaid
flowchart TD
    cli["agentcore-cli<br/><i>binary</i>"] --> server
    server["agentcore-server<br/>HTTP API · gateways · GitHub · auth"] --> runtime
    server --> store
    server --> roles
    runtime["agentcore-runtime<br/>sessions · turns · approvals · adapters"] --> sandbox
    runtime --> policy
    runtime --> audit
    runtime --> roles
    store["agentcore-store<br/>PostgreSQL · encryption"]
    sandbox["agentcore-sandbox<br/>docker · process"]
    policy["agentcore-policy<br/>guardrails"]
    audit["agentcore-audit<br/>hash chain"]
    roles["agentcore-roles<br/>playbooks · checks · prompt"]
    core["agentcore-core<br/>shared types"]
    server -.-> core
    runtime -.-> core
    store -.-> core
    sandbox -.-> core
    policy -.-> core
    audit -.-> core
```

| Crate | Responsibility |
|---|---|
| **core** | Shared types: `Action` (exec, file read/write, network, tool call), `Event`, `Principal` (human, agent, system), `SessionInfo`/`SessionContext`, `AgentAdapter`, model and work types (`Changes`, `CheckResult`, ...). |
| **policy** | Compiles TOML guardrail policies into glob matchers and evaluates actions with deny-overrides semantics. Normalises paths before matching. |
| **audit** | One append-only JSONL file per session; each record contains the SHA-256 of the previous one. Existing files are verified before they are extended. |
| **sandbox** | `SandboxProvider` creates a `Sandbox` that can spawn the agent (in a pseudo-terminal), run commands (streaming their output), read/write workspace files, list its processes, `pause()`/`resume()` and `kill()` everything. Backends: Docker (hardened), process (dev only). |
| **roles** | Role (playbook) files, capability → tool mapping, check definitions, and composition of the agent's first prompt. |
| **runtime** | `SessionManager` and `Session`: turns, policy gating, approvals, stop, pause/resume, the live view (`LiveHub`: terminal, recording, files, processes), change snapshots, checks, git bundle export, built-in adapters. |
| **store** | PostgreSQL: session index, model providers, GitHub connection (secrets AES-256-GCM encrypted), projects, model calls, change snapshots, admin log, organisations (departments, agents, messages, inboxes, department files, profile and limits), node registry. |
| **server** | axum: operator API, SSE event and live streams, MCP tool gateway, model gateway (including the token stream for the live view), GitHub client and tools, repository checkout and delivery, auth, UI hosting, organisations (`org.rs`), templates (`templates.rs`), cluster (`cluster.rs`). |
| **cli** | `agentcore serve`, `policy check/eval`, `audit verify/show`, `hash-token`. |

## Session lifecycle

```mermaid
stateDiagram-v2
    [*] --> live
    state live {
        [*] --> pending
        pending --> running: repo checked out, sandbox up
        running --> awaiting_approval: an action needs a human
        awaiting_approval --> running: decided or timed out
        running --> awaiting_input: turn ends, agent can continue
        awaiting_input --> running: a human sends a message
        running --> paused: Pause
        awaiting_approval --> paused: Pause
        awaiting_input --> paused: Pause
        paused --> running: Resume (back to the state before)
    }
    live --> completed: Finish, or a single-run agent exits 0
    live --> failed: a single-run agent fails, or an error
    live --> stopped: STOP · time budget · idle timeout · audit failure
    completed --> [*]
    failed --> [*]
    stopped --> [*]
```

* A **turn** is one run of the agent process. Agents that can continue a
  conversation (opencode with `--continue`, CLI agents with `follow_up_args`)
  go to `awaiting_input` after each turn, with the sandbox kept alive;
  others end after the first run.
* While `awaiting_input`, a human can send messages, look at the changes, run
  checks and deliver.
* Limits from the guardrail policy: `max_session_secs` (whole session),
  `idle_timeout_secs` (waiting for a human), `max_actions`,
  `max_model_calls`, `approval_timeout_secs`.
* On restart, sessions that were live are marked `failed`, their audit logs
  closed with a `session_ended` event, and leftover containers removed.

## Event pipeline

Every state change is an `Event`, recorded through one function:

```mermaid
flowchart LR
    S["Session::emit(event)"] --> A{"append to<br/>audit log"}
    A -- "fails" --> STOP["stop the session<br/>(fail closed)"]
    A -- "ok" --> H["in-memory state<br/>& history"]
    H --> T["tracing<br/>(agentcore::event)"]
    H --> B["broadcast"]
    B --> SSE["SSE → web UI"]
    B --> F["follower task →<br/>PostgreSQL session row"]
```

The UI, the audit log and the operational logs are three views of the same
event sequence. If an event cannot be recorded, the agent is stopped: an
unrecorded agent must not keep acting.

## Life of an action

```mermaid
sequenceDiagram
    autonumber
    participant AG as Agent (sandbox)
    participant GW as Tool gateway /mcp
    participant RT as Session
    participant PO as Policy
    participant H as Human (UI)
    participant EX as Sandbox / GitHub

    AG->>GW: tools/call (session token)
    GW->>RT: request_action(action)
    RT->>RT: normalise (absolute paths, no "..")
    Note over RT: event: action_requested
    RT->>PO: evaluate
    PO-->>RT: allow / deny / require_approval
    Note over RT: event: policy_evaluated
    alt require_approval
        Note over RT: event: approval_requested
        RT->>H: approval card
        H-->>RT: approve / reject (+ comment)
        Note over RT: event: approval_resolved
    end
    alt allowed or approved
        RT->>EX: exec / read / write / GitHub call
        EX-->>RT: result
    end
    Note over RT: event: action_completed
    RT-->>GW: outcome
    GW-->>AG: result, or "DENIED: reason. Do not retry."
```

Built-in tools (`run_command`, `read_file`, `write_file`, `list_files`) run in
the sandbox. Role tools (`github_*`, `propose_pull_request`) are executed by
agentcore, but go through exactly the same policy, approval and audit steps.

## Stopping

```mermaid
sequenceDiagram
    participant H as Human / system
    participant RT as Session
    participant SB as Sandbox
    participant LLM as Model gateway

    H->>RT: stop(by, reason)
    Note over RT: event: stop_requested
    RT->>RT: cancel token: no new actions or model calls
    RT->>RT: drop pending approvals (resolve as rejected)
    RT->>LLM: in-flight streams aborted
    RT->>SB: kill() → docker rm --force / SIGKILL process groups
    Note over RT: event: session_ended (stopped)
```

Triggered by **STOP AGENT**, **Stop all agents**, `POST /sessions/{id}/stop`,
`POST /stop-all`, the time budget, the idle timeout, an audit failure, or
server shutdown. Stopping also works while a session is paused.

<a id="live-view"></a>
## Live view: watching the agent work

People trust what they can see. The live view shows a running session the way
a screen share would: the agent's own terminal, the command it is running
right now with its output, what the model is thinking and writing as the
tokens arrive, the files it touches and the processes in its sandbox.

```mermaid
flowchart LR
    subgraph SB["Sandbox"]
        PTY["Agent in a<br/>pseudo-terminal"]
        CMD["Commands run<br/>for tool calls"]
        FS[("/workspace")]
        PS["Processes"]
    end
    LLM["Model gateway<br/>(response stream)"]
    PTY -- "terminal bytes" --> HUB["LiveHub<br/>(per session)"]
    CMD -- "stdout / stderr" --> HUB
    LLM -- "text · thinking ·<br/>tool calls" --> HUB
    FS -- "file watcher" --> HUB
    PS -- "polled while watched" --> HUB
    HUB -- "SSE /live" --> UI["Live tab"]
    PTY -- "plain text lines" --> AUD[("Audit log")]
    HUB -- "asciicast file" --> REC[("recordings/<br/>id.cast")]
    REC -- "SHA-256 on close" --> AUD
```

* **Terminal.** Agents run in a pseudo-terminal of 120×32 (`openpty` for the
  process backend, `docker exec --tty` for Docker), so they print exactly what
  a developer would see: colours, progress, tool banners. Set `tty = false`
  on an agent that misbehaves in a terminal.
* **Frames, not events.** Live frames (`terminal`, `tool_output`,
  `model_start/delta/end`, `files`, `processes`) are high-volume and
  ephemeral; they go to viewers only. What matters for the record is still
  audited: output as ANSI-free text lines, every action and model call, and
  the terminal recording.
* **Late viewers.** A viewer who opens the tab gets the last 256 KiB of
  terminal output (`terminal_reset`), recent file changes and the current
  processes first, so the screen is complete immediately. A viewer that falls
  behind is told to reconnect (`lagged`) and gets a fresh screen.
* **Recording.** The terminal is written to `data/recordings/<session>.cast`
  (asciicast v2, with a chapter marker per turn and banners for pause, resume
  and stop). When the session ends, its size and SHA-256 are recorded in the
  hash-chained audit log (`recording_closed`), so a replay can be proven to be
  the original. Finished sessions show a *Replay* tab.
* **Model stream.** The model gateway already relays the provider's response;
  it also parses Anthropic Messages, OpenAI Chat Completions and OpenAI
  Responses streams (and plain JSON answers) into text, thinking and tool-call
  deltas for the live view.
* **Cost.** The process list is only polled (every 2 s) while someone watches;
  file changes are debounced and ignore `.git`.

## Pausing

```mermaid
sequenceDiagram
    participant H as Operator
    participant RT as Session
    participant SB as Sandbox
    participant GW as Gateways

    H->>RT: pause(by)
    RT->>SB: docker pause / SIGSTOP every process group
    Note over RT: events: paused (by), status_changed (paused)
    Note over SB: the agent and its commands are frozen
    GW->>RT: calls already in flight wait
    H->>RT: resume(by)
    RT->>SB: docker unpause / SIGCONT
    Note over RT: events: resumed (by), status back to what it was
```

Pausing freezes everything without losing state: the agent continues exactly
where it was. Status changes that happen while paused (an approval answered,
say) are applied on resume. The session's time budget keeps running while it
is paused; approvals keep their timeouts.

## Model gateway

```mermaid
sequenceDiagram
    autonumber
    participant AG as Agent (sandbox)
    participant GW as Model gateway /llm
    participant DB as PostgreSQL
    participant UP as Provider

    AG->>GW: POST /llm/{session}/{provider}/v1/... (session token)
    GW->>GW: token belongs to the live session? (401)
    GW->>GW: under max_model_calls? (429) · session running? (403)
    GW->>DB: provider + decrypted key
    GW->>GW: provider enabled? (403) · model allowed? (403)
    GW->>UP: same request, real key, session token stripped
    UP-->>GW: response (streamed)
    GW-->>AG: relayed chunk by chunk (aborted if the session stops)
    GW->>GW: event model_call: model, status, tokens, duration, SHA-256s
    GW->>DB: full request and response bodies
```

Every agent gets `ANTHROPIC_BASE_URL`/`ANTHROPIC_API_KEY` and
`OPENAI_BASE_URL`/`OPENAI_API_KEY` pointing at the gateway (with the session
token as the key) for the first enabled provider of each kind; the opencode
adapter configures opencode's `anthropic`, `openai` and `opencode` (Zen)
providers the same way. Refused calls are recorded too.

## Team work

### Starting an agent on an issue

```mermaid
sequenceDiagram
    autonumber
    participant H as Operator (UI)
    participant S as Server
    participant GH as GitHub
    participant RT as Runtime
    participant SB as Sandbox

    H->>S: Start agent on #7 (role)
    S->>GH: read issue + comments
    S->>RT: create session (role, tools, work item)
    S-->>GH: move card → In Progress, comment "agent started"
    RT->>S: prepare workspace
    S->>GH: clone (token via env) → bare mirror
    S->>S: clone mirror → workspace, work branch, commit identity
    RT->>SB: start sandbox (workspace mounted at /workspace)
    RT->>SB: read the team's convention files
    RT->>RT: compose prompt · event role_applied
    RT->>SB: run agent (turn 1)
```

### The repository and its boundary

```mermaid
flowchart LR
    GH["GitHub repo"] -- "clone (token)" --> M[("data/repos/{session}.git<br/>bare mirror<br/><i>agentcore only</i>")]
    M -- "git clone file://" --> W[("data/workspaces/{session}<br/><i>agent's checkout</i>")]
    W -- "git bundle base..HEAD<br/>(created in the sandbox)" --> BUN["bundle (bytes)"]
    BUN -- "git fetch" --> M
    M -- "push session branch only (token)" --> GH
```

agentcore never runs git in the agent-controlled checkout on the host.
Everything crossing the boundary is a bundle, which is plain data. Hooks,
fsmonitor and credential helpers are disabled for every git command agentcore
runs, and the token is passed as an HTTP header through environment variables.

### First prompt

```mermaid
flowchart LR
    R["Role playbook<br/>(roles/*.toml)"] --> P["First prompt"]
    D["Team's files from the repo<br/>CONTRIBUTING.md · AGENTS.md ·<br/>PR template ..."] --> P
    N["Project notes<br/>(UI)"] --> P
    C["Role checks"] --> P
    I["Issue + comments<br/>(GitHub)"] --> P
    O["Operator's instructions"] --> P
    P --> E["event role_applied:<br/>role digest · files · prompt SHA-256"]
```

The convention files are read **inside the sandbox**, so a symlink in the
repository cannot make agentcore read a host file.

### Delivery

```mermaid
sequenceDiagram
    autonumber
    participant H as Operator (UI)
    participant S as Server
    participant RT as Session
    participant SB as Sandbox
    participant GH as GitHub

    H->>S: Deliver (title, description)
    S->>RT: run_checks()
    RT->>SB: commit messages, clean tree, commands
    Note over RT: event checks_completed
    alt a required check failed
        S-->>H: 409 + results → ask the agent to fix, deliver again
    else all passed
        S->>RT: export_bundle()
        RT->>SB: git bundle create base..HEAD
        SB-->>S: bundle bytes
        S->>S: fetch into mirror, verify head
        S->>GH: push agent branch
        S->>GH: open or update PR (issue link, AI disclosure)
        S->>GH: move card → In Review, comment on the issue
        S->>RT: record_delivery · event delivered
        S-->>H: PR link
    end
```

Delivery is possible while the agent waits for input. After review comments,
continue the session and deliver again: the same pull request is updated.

## Persistence

```mermaid
erDiagram
    projects ||--o{ sessions : "context.project_id"
    sessions ||--o{ model_calls : "made"
    sessions ||--o| session_changes : "latest snapshot"
    model_providers ||--o{ model_calls : "served"

    sessions {
        uuid id PK
        text agent
        text task
        text policy
        text policy_digest
        text status
        jsonb created_by
        jsonb context
        text audit_path
    }
    model_calls {
        uuid id PK
        uuid session_id FK
        text provider
        text model
        text outcome
        bigint input_tokens
        bigint output_tokens
        text request_body
        text response_body
        text request_sha256
    }
    model_providers {
        text name PK
        text kind
        text base_url
        bytea api_key_ciphertext
        text_array allowed_models
        bool enabled
    }
    projects {
        uuid id PK
        text name
        text repo_owner
        text repo_name
        text default_branch
        text role
        jsonb board
        text notes
    }
    session_changes {
        uuid session_id PK
        jsonb changes
    }
    integrations {
        text kind PK
        jsonb config
        bytea secret_ciphertext
    }
    admin_events {
        bigint id PK
        text actor
        text action
        text target
        jsonb details
    }
```

| Data | Where | Why |
|---|---|---|
| Session events | `data/audit/<session>.jsonl` | Tamper-evident hash chain; the authoritative record |
| Organisations, messages, department files, nodes | PostgreSQL (migrations `0004`, `0005`; see [Multi-agent organisations](MULTI_AGENT.md#data-model)) | Shared by every node |
| Sessions, projects, providers, GitHub connection, model calls, changes, admin log | PostgreSQL (migrations in `crates/agentcore-store/migrations`) | Queryable state that survives restarts |
| Repository mirrors and workspaces | `data/repos/`, `data/workspaces/` | Agent checkouts and agentcore's push source |
| Master key | `data/master.key` or `$AGENTCORE_MASTER_KEY` | Decrypts provider keys and the GitHub token; back it up |

## Agents and adapters

An agent is configured as an `AgentSpec` (`[[agents]]` in the config) and
launched by an `AgentAdapter`, which turns spec + task into a `LaunchPlan`
(program, args, env), and optionally a plan for follow-up turns.

```mermaid
flowchart LR
    SPEC["[[agents]] spec"] --> AD{"adapter"}
    AD -- "command" --> CMD["any CLI<br/>args with {task}<br/>follow_up_args"]
    AD -- "opencode" --> OC["opencode run<br/>native tools denied<br/>gateways configured<br/>--continue for follow-ups"]
    CMD --> PLAN["LaunchPlan + agentcore env"]
    OC --> PLAN
```

agentcore injects into every agent's environment (not into the audited
arguments):

| Variable | Purpose |
|---|---|
| `AGENTCORE_GATEWAY_URL`, `AGENTCORE_GATEWAY_TOKEN` | MCP tool gateway for this session, and its token |
| `AGENTCORE_MODEL_GATEWAY_URL` | Model gateway base for this session |
| `ANTHROPIC_*`, `OPENAI_*` | Standard SDK variables pointing at the model gateway |
| `AGENTCORE_SESSION_ID`, `AGENTCORE_WORKSPACE` | Context |
| `AGENTCORE_AI_GENERATED` | Marker for AI-generated output (Art. 50) |

Adding a new agent:

* **any CLI agent**: use the `command` adapter, point its MCP client at
  `$AGENTCORE_GATEWAY_URL` and its model SDK at the standard variables;
  set `follow_up_args` if it can continue a conversation;
* **deeper integration**: implement `AgentAdapter` (see `OpenCodeAdapter`,
  which disables all of opencode's native file, shell and web tools,
  including read/grep/glob, so the model has to use the policy-checked
  gateway tools) and register it in `AdapterRegistry`.

Agents with built-in tools that bypass the gateway are still confined by the
sandbox, but the policy only governs what goes through the gateway, so
disable native side-effecting tools where the agent allows it.

## Sandbox backends

| Backend | Isolation | Use |
|---|---|---|
| `docker` | Container per session: no capabilities, `no-new-privileges`, read-only root, non-root user, resource limits, `network=none` or an internal network; optional gVisor/Kata runtime | Production |
| `process` | None: host processes in the workspace directory | Developing agentcore only |

| Backend | Agent terminal | Pause | Processes |
|---|---|---|---|
| `docker` | `docker exec --tty`, size set with `stty` | `docker pause` (cgroup freezer) | `docker top` |
| `process` | `openpty` | `SIGSTOP`/`SIGCONT` to every process group | `ps`, filtered by process group |

New backends implement `SandboxProvider` + `Sandbox` (spawn, exec,
read/write file, pause, resume, processes, kill, destroy, cleanup of
orphans). Candidates: Firecracker
microVMs, Kubernetes pods, remote sandboxes.

## Organisations and clusters

Departments of agents, communicators, message routing and running on
several VMs are described in [Multi-agent organisations](MULTI_AGENT.md).
In short: `org.rs` (server) holds the API, the `team_*` tools and message
routing; `templates.rs` loads department templates and growth paths, plans
what to create and suggests what to add next; `cluster.rs` holds the node heartbeat, the `LISTEN/NOTIFY`
listener, the reconciler that converges this node's sessions to the desired
state in PostgreSQL, and request forwarding to the node that owns a session.

## Roadmap

* **Steering**: interrupt a running turn with a message (not only between
  turns), and take over the agent's terminal.
* **Annotations**: viewers bookmark and comment moments in a live session or
  replay.
* **Event-driven pickup**: agents take cards labelled `agent` from the *Ready*
  column automatically (within a WIP limit), and react to PR review comments
  by continuing their session.
* **GitHub App** authentication (per-installation tokens) instead of a token.
* **Other trackers**: GitLab, Jira, Linear behind the same project/board model.
* **Editors in the UI** for roles, policies, agents and operators.
* **Egress proxy**: network access per policy `network` rules.
* **OpenTelemetry export**; SIEM forwarding of audit logs.
* **SSO (OIDC)** for operators.
* **Spend limits**: token/cost budgets per session and provider.
* **External anchoring** of audit chain heads (WORM storage / transparency log).
