# agentcore

Secure runtime layer for AI agents: sandboxed tools, policy-based permissions,
human approval, and audit logs. Written in Rust, works with any MCP agent.

agentcore lets a team run AI coding agents (opencode today, any CLI or MCP agent
tomorrow) as **supervised coworkers**: each agent works in its own isolated
sandbox, every side effect goes through a policy that people wrote, risky steps
wait for a human, everything is visible live in a web UI, one click stops it,
and an append-only, tamper-evident log records what happened.

```
┌────────────── Web UI (React) ──────────────┐
│ live timeline · approvals · STOP · audit   │
└──────────────────┬─────────────────────────┘
                   │ REST + SSE (/api/v1)
┌──────────────────▼─────────────────────────┐       ┌─────────── sandbox ───────────┐
│ agentcore server                           │       │ container (no caps, ro rootfs,│
│  session supervisor ─ policy engine        │◄──────┤ non-root, no network but the  │
│  HITL approvals ─ kill switch              │  MCP  │ gateway; optional gVisor)     │
│  hash-chained audit log ─ tracing          │       │   agent (opencode, …)         │
└────────────────────────────────────────────┘       │   /workspace                  │
                                                     └───────────────────────────────┘
```

## Features

| Requirement | How agentcore covers it |
|---|---|
| Run open code securely | Per-session container: `--cap-drop=ALL`, `no-new-privileges`, read-only root fs, non-root user, pid/memory/cpu limits, `--network=none` or an internal-only network, optional gVisor/Kata runtime. |
| Coworker for developers | Agents get a workspace plus MCP tools (`run_command`, `read_file`, `write_file`, `list_files`) that are checked against policy on every call. |
| Hostable | Single binary + static UI, Docker image, docker-compose with an isolated sandbox network, token auth with operator/viewer roles. |
| See what the agent does | Live timeline of every action, policy verdict, approval and outcome, plus raw agent output, streamed over SSE. |
| One-click stop | **STOP AGENT** per session and **Stop all agents** globally: kills the sandbox, denies pending approvals, and records who pressed it. Also enforced by time and action budgets. |
| Any agent | `AgentAdapter` trait; built-in `command` (any CLI) and `opencode` adapters. |
| User policies | TOML policies with allow / deny / require-approval rules over commands, paths, hosts and tools; deny-overrides semantics; limits. |
| HITL | Approve or reject with a comment from the UI; timeouts count as rejection. |
| Logging & tracing | Hash-chained JSONL audit log per session (fail-closed), structured `tracing` logs (JSON), integrity check and export from the UI/CLI. |
| EU AI Act | See [docs/EU_AI_ACT.md](docs/EU_AI_ACT.md) for the article-by-article mapping. |

## Quick start (local development)

Requires Rust ≥ 1.88 and Node ≥ 20.

```sh
(cd web && npm ci && npm run build)
cargo run -p agentcore-cli -- serve --config agentcore.dev.toml
# open http://127.0.0.1:8080 and start the "demo" agent
```

`agentcore.dev.toml` uses the `process` backend, which runs agents **directly on
your machine without isolation**. Use it only to develop agentcore itself.

For UI development with hot reload, run the server as above and `npm run dev`
in `web/` (Vite proxies `/api` to `127.0.0.1:8080`).

## Self-hosting

```sh
docker build -t agentcore-sandbox:latest sandbox-image   # image agents run in
cp deploy/agentcore.toml.example deploy/agentcore.toml
docker compose run --rm agentcore hash-token 'a-long-random-token'
# put the hash into [[server.operators]] in deploy/agentcore.toml
export AGENTCORE_DATA=/srv/agentcore/data ANTHROPIC_API_KEY=...
docker compose up -d
```

Put a TLS-terminating reverse proxy in front of port 8080. Read
[SECURITY.md](SECURITY.md) before exposing it; in particular, the Docker
socket gives agentcore root-equivalent access to its host.

## CLI

```sh
agentcore serve --config agentcore.toml
agentcore policy check policies/
agentcore policy eval default '{"type":"exec","command":"git","args":["push"]}'
agentcore audit verify data/audit/*.jsonl
agentcore audit show data/audit/<session>.jsonl
agentcore hash-token '<token>'
```

## Repository layout

| Path | What |
|---|---|
| `crates/agentcore-core` | Domain types: actions, events, sessions, the `AgentAdapter` trait |
| `crates/agentcore-policy` | Policy language and engine |
| `crates/agentcore-audit` | Hash-chained audit log |
| `crates/agentcore-sandbox` | `Sandbox` trait; Docker and (dev-only) process backends |
| `crates/agentcore-runtime` | Session supervisor, approvals, kill switch, adapters |
| `crates/agentcore-server` | REST/SSE API, MCP gateway, auth, config, UI hosting |
| `crates/agentcore-cli` | The `agentcore` binary |
| `web/` | React + TypeScript web UI (Vite) |
| `policies/` | Bundled policies: `default`, `read-only`, `supervised` |
| `docs/` | Architecture, policies, EU AI Act mapping |

## Documentation

- [Architecture](docs/ARCHITECTURE.md): components, the life of an action, how to add an agent or sandbox backend.
- [Policies](docs/POLICIES.md): policy language reference.
- [EU AI Act](docs/EU_AI_ACT.md): how agentcore supports the obligations for providers and deployers.
- [Security](SECURITY.md): threat model and hardening.

## Testing

```sh
cargo test                       # unit + integration tests (uses the process backend)
cargo test -p agentcore-sandbox --test docker -- --ignored   # needs a Docker daemon
(cd web && npm run build)        # type-checks the UI
```

## License

Apache-2.0
