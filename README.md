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
│ settings: model providers & API keys       │
└──────────────────┬─────────────────────────┘
                   │ REST + SSE (/api/v1)
┌──────────────────▼─────────────────────────┐  tools (MCP)  ┌───────── sandbox ─────────┐
│ agentcore server                           │◄──────────────┤ container: no caps, ro    │
│  session supervisor ─ policy engine        │               │ rootfs, non-root, internal│
│  HITL approvals ─ kill switch              │◄──────────────┤ network only, opt. gVisor │
│  model gateway ─ hash-chained audit log    │  LLM calls    │   agent (opencode, …)     │
└───────┬────────────────────────┬───────────┘               │   /workspace              │
        │ real API keys          │                           └───────────────────────────┘
        ▼                        ▼
  Anthropic / OpenAI        PostgreSQL (sessions, providers, model calls)
```

## Features

| Requirement | How agentcore covers it |
|---|---|
| Run open code securely | Per-session container: `--cap-drop=ALL`, `no-new-privileges`, read-only root fs, non-root user, pid/memory/cpu limits, `--network=none` or an internal-only network, optional gVisor/Kata runtime. |
| Coworker for developers | Agents get a workspace plus MCP tools (`run_command`, `read_file`, `write_file`, `list_files`) that are checked against policy on every call. |
| Hostable | Single binary + static UI, PostgreSQL, Docker image, docker-compose with an isolated sandbox network, token auth with admin/operator/viewer roles. |
| Model gateway | Agents call LLMs through agentcore with a per-session token. Real API keys are stored AES-256-GCM encrypted in PostgreSQL and never enter a sandbox; per-provider model allow-lists and per-session call limits apply. Every call is logged with token usage and full request/response. **Stop** aborts calls that are still streaming. Providers are managed in the web UI. |
| See what the agent does | Live timeline of every action, policy verdict, approval and outcome, plus raw agent output, streamed over SSE. |
| One-click stop | **STOP AGENT** per session and **Stop all agents** globally: kills the sandbox, denies pending approvals, and records who pressed it. Also enforced by time and action budgets. |
| Any agent | `AgentAdapter` trait; built-in `command` (any CLI) and `opencode` adapters. |
| User policies | TOML policies with allow / deny / require-approval rules over commands, paths, hosts and tools; deny-overrides semantics; limits. |
| HITL | Approve or reject with a comment from the UI; timeouts count as rejection. |
| Logging & tracing | Hash-chained JSONL audit log per session (fail-closed), including every model call with request/response hashes; full LLM traffic and configuration changes in PostgreSQL; structured `tracing` logs (JSON); integrity check and export from the UI/CLI. |
| EU AI Act | See [docs/EU_AI_ACT.md](docs/EU_AI_ACT.md) for the article-by-article mapping. |

## Quick start (local development)

Requires Rust ≥ 1.94, Node ≥ 20 and PostgreSQL (≥ 14).

```sh
docker compose up -d postgres                 # or point [database].url at your own
(cd web && npm ci && npm run build)
cargo run -p agentcore-cli -- serve --config agentcore.dev.toml
# open http://127.0.0.1:8080 and start the "demo" agent
```

Migrations run automatically at startup. On first start agentcore generates
`data/master.key`, which encrypts model API keys in the database. Back it up.

`agentcore.dev.toml` uses the `process` backend, which runs agents **directly on
your machine without isolation**. Use it only to develop agentcore itself.

For UI development with hot reload, run the server as above and `npm run dev`
in `web/` (Vite proxies `/api` to `127.0.0.1:8080`).

## Self-hosting

```sh
docker build -t agentcore-sandbox:latest sandbox-image   # image agents run in
cp deploy/agentcore.toml.example deploy/agentcore.toml
docker compose run --rm agentcore hash-token 'a-long-random-token'
# put the hash into [[server.operators]] (role = "admin") in deploy/agentcore.toml
export AGENTCORE_DATA=/srv/agentcore/data POSTGRES_PASSWORD=$(openssl rand -hex 16)
docker compose up -d
# open http://<host>:8080 → Settings → add your model provider (API key)
```

Everything after that happens in the web UI: provider keys, starting agents,
approvals, stopping, audit.

Put a TLS-terminating reverse proxy in front of port 8080. Read
[SECURITY.md](SECURITY.md) before exposing it; in particular, the Docker
socket gives agentcore root-equivalent access to its host.

## Running opencode

The `opencode` agent is defined in `agentcore.example.toml` and
`deploy/agentcore.toml.example`. agentcore launches `opencode run "<task>"`
inside the sandbox. It turns off opencode's own file, shell and web tools and
registers the agentcore MCP gateway, so every read, write and command goes
through your policy.

1. Make `opencode` available where the agent runs: the sandbox image installs it
   (`docker build -t agentcore-sandbox:latest sandbox-image`). With the dev
   `process` backend, install it on your machine (`npm i -g opencode-ai`).
2. In the web UI, open **Settings** and add a model provider (e.g. Anthropic
   with your API key). Only admins can do this.
3. Go back to **Sessions**, pick **opencode**, choose a policy, describe the
   task and click **Start agent**.

opencode's `anthropic` / `openai` providers are pointed at the model gateway,
so it works on the internal-only sandbox network: the sandbox needs no
internet access. Each LLM call shows up in the session's activity timeline
with tokens, timing and the full request and response.

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
| `crates/agentcore-store` | PostgreSQL: sessions, model providers (encrypted keys), model calls, admin log |
| `crates/agentcore-server` | REST/SSE API, MCP tool gateway, model gateway, auth, config, UI hosting |
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
export AGENTCORE_TEST_DATABASE_URL=postgres://postgres@localhost/postgres  # superuser; tests create throwaway databases
cargo test                       # unit + integration tests (process backend, mock LLM upstream)
cargo test -p agentcore-sandbox --test docker -- --ignored   # needs a Docker daemon
(cd web && npm run build)        # type-checks the UI
```

Without `AGENTCORE_TEST_DATABASE_URL`, the PostgreSQL tests are skipped.

## License

Apache-2.0
