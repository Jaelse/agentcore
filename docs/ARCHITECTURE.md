# Architecture

## Components

```
agentcore-cli ──► agentcore-server ──► agentcore-runtime ──► agentcore-sandbox
                       │                    │   │                (docker | process)
                       │                    │   └──► agentcore-policy
                       │                    └──────► agentcore-audit
                       ├── agentcore-store ──► PostgreSQL
                       └── web/ (React UI, served as static files)
                 all crates share agentcore-core (types)
```

* **core**: `Action` (exec, file read/write, network, generic tool call),
  `Event` (everything that can happen in a session), `Principal` (human, agent,
  system), and the `AgentAdapter` trait.
* **policy**: compiles TOML policies into glob matchers and evaluates actions
  with deny-overrides semantics. Paths are normalised before matching.
* **audit**: one append-only JSONL file per session; each record contains the
  SHA-256 of the previous one. Existing files are verified before they are extended.
* **sandbox**: `SandboxProvider` creates a `Sandbox`, which can spawn the
  agent, run tool commands, read and write workspace files, and `kill()` everything.
* **runtime**: `SessionManager` and `Session`. A session drives the agent
  process, gates every action through policy and approvals, and emits events.
* **store**: PostgreSQL via sqlx (migrations run at startup). Holds the
  session index, model providers (API keys AES-256-GCM encrypted with the
  master key, provider name bound as associated data), every model call, and
  the configuration change log.
* **server**: axum HTTP server with the operator API, an SSE event stream, the
  MCP tool gateway, the model gateway, bearer-token auth and the UI.

## Persistence

| Data | Where | Why |
|---|---|---|
| Session events | `data/audit/<session>.jsonl` | Tamper-evident hash chain; the authoritative record |
| Session index | `sessions` table | List and search sessions; survives restarts |
| Model providers | `model_providers` table | Keys encrypted at rest; managed from the UI |
| LLM requests/responses | `model_calls` table | Full bodies (hashes are also in the audit chain) |
| Configuration changes | `admin_events` table | Who changed which provider, when |
| Master key | `data/master.key` or `$AGENTCORE_MASTER_KEY` | Decrypts provider keys; back it up |

On startup agentcore marks sessions that were running when it last stopped as
`failed`, appends a closing `session_ended` event to their audit logs, and
removes any sandbox containers left behind.

## Model gateway

```
agent ──POST /llm/{session}/{provider}/v1/messages──► agentcore
        x-api-key: <session token>
          1. token must belong to the live session          → 401
          2. session running, under max_model_calls           → 403 / 429
          3. provider configured and enabled                 → 404 / 403
          4. model matches the provider's allowed_models     → 403
          5. swap in the decrypted real key, strip the token
        ──► upstream (Anthropic / OpenAI-compatible)
        ◄── response streamed back chunk by chunk; aborted if the session stops
          6. audit event `model_call` (model, status, tokens, duration,
             SHA-256 of request and response) + full bodies in `model_calls`
```

agentcore gives every agent `ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY` and
`OPENAI_BASE_URL` / `OPENAI_API_KEY` that point at the gateway, using the session
token as the key, for the first enabled provider of each kind. The opencode
adapter also writes these into opencode's provider config. Refused calls are
recorded too. Because LLM traffic goes through agentcore, sandboxes can run on
an internal network with no route to the internet.

## Event pipeline

Every state change is an `Event` and goes through `Session::emit`, which:

1. appends it to the audit log. If that fails, the session is stopped
   (**fail closed**: an unrecorded agent must not keep acting);
2. updates in-memory state and history;
3. logs it via `tracing` (`target = "agentcore::event"`);
4. broadcasts it to SSE subscribers (the UI).

The UI, the audit log and the operational logs are therefore three views of the
same event sequence.

## Life of an action

```
agent ──tools/call──► /mcp/{session}  (per-session bearer token)
        ──► Action (normalised: absolute paths, no "..", lower-case hosts)
        ──► event: action_requested
        ──► policy.evaluate ──► event: policy_evaluated
              deny             ──► outcome: denied
              require_approval ──► event: approval_requested
                                   wait for a human / timeout / stop
                                   ──► event: approval_resolved
              allow / approved ──► sandbox.exec | read_file | write_file
        ──► event: action_completed (outcome; writes record a SHA-256 of the content)
        ──► result to the agent (denials say "do not retry")
```

## Stopping

`Session::stop(by, reason)` (UI button, `POST /sessions/{id}/stop`,
`POST /stop-all`, time budget, server shutdown):

1. records `stop_requested` with the principal and reason;
2. cancels the session token, so no further actions are accepted;
3. drops all pending approvals (their waiters resolve as rejected);
4. `sandbox.kill()`: `docker rm --force` (SIGKILL to every process in the
   container), or SIGKILL to every process group for the process backend.

The run loop then records `session_ended` with status `stopped`.

## Agents

An agent is configured as an `AgentSpec` (`[[agents]]` in the config) and
launched by an `AgentAdapter`, which turns spec + task into a `LaunchPlan`
(program, args, env). agentcore injects:

| Variable | Purpose |
|---|---|
| `AGENTCORE_GATEWAY_URL` | MCP endpoint for this session |
| `AGENTCORE_GATEWAY_TOKEN` | Bearer token for that endpoint only |
| `AGENTCORE_SESSION_ID`, `AGENTCORE_WORKSPACE` | Context |
| `AGENTCORE_AI_GENERATED` | Marker for AI-generated output (Art. 50) |

Adding a new agent:

* **any CLI agent**: use the `command` adapter and point the agent's MCP
  client at `$AGENTCORE_GATEWAY_URL`;
* **deeper integration**: implement `AgentAdapter` (see `OpenCodeAdapter`,
  which disables all of opencode's native file, shell and web tools (including
  read/grep/glob, which would bypass path rules) so the model has to
  use the policy-checked gateway tools) and register it in `AdapterRegistry`.

Agents with built-in tools that bypass the gateway are still confined by the
sandbox. The policy then governs only what goes through the gateway, so
prefer disabling native side-effecting tools as the opencode adapter does.

## Sandbox backends

Implement `SandboxProvider` + `Sandbox`. Candidates on the roadmap: Firecracker
microVMs, Kubernetes pods (one per session), and remote sandboxes.

## Roadmap

* **Egress proxy**: allow network access per policy `network` rules, instead
  of all-or-nothing.
* **Git integration**: clone a repository into the workspace and deliver
  results as a branch / pull request.
* **OpenTelemetry export** of traces and events; SIEM forwarding of audit logs.
* **SSO (OIDC)** for operators; per-team policies.
* **Spend limits**: per-session and per-provider token/cost budgets.
* **More from the UI**: operator management, agent definitions and a policy editor.
* **External anchoring** of audit chain heads (WORM storage / transparency log).
