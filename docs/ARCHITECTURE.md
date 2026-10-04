# Architecture

## Components

```
agentcore-cli ──► agentcore-server ──► agentcore-runtime ──► agentcore-sandbox
                       │                    │   │                (docker | process)
                       │                    │   └──► agentcore-policy
                       │                    └──────► agentcore-audit
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
* **server**: axum HTTP server with the operator API, an SSE event stream, the
  MCP gateway, bearer-token auth and the UI.

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
  which disables opencode's native edit/bash/web tools so the model has to
  use the policy-checked gateway tools) and register it in `AdapterRegistry`.

Agents with built-in tools that bypass the gateway are still confined by the
sandbox. The policy then governs only what goes through the gateway, so
prefer disabling native side-effecting tools as the opencode adapter does.

## Sandbox backends

Implement `SandboxProvider` + `Sandbox`. Candidates on the roadmap: Firecracker
microVMs, Kubernetes pods (one per session), and remote sandboxes.

## Roadmap

* **Model gateway**: proxy LLM API calls through agentcore so API keys never
  enter the sandbox and prompts/responses are logged (Art. 12).
* **Egress proxy**: allow network access per policy `network` rules, instead
  of all-or-nothing.
* **Git integration**: clone a repository into the workspace and deliver
  results as a branch / pull request.
* **OpenTelemetry export** of traces and events; SIEM forwarding of audit logs.
* **SSO (OIDC)** for operators; per-team policies.
* **Persistent session index** (survive restarts; archived sessions in the UI).
* **External anchoring** of audit chain heads (WORM storage / transparency log).
