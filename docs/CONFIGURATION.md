# Configuration reference

agentcore reads one TOML file (`agentcore serve --config agentcore.toml`).
Relative paths in it are resolved against the file's directory. Unknown keys
are rejected, so typos fail at startup instead of being ignored.

Starting points: [`agentcore.example.toml`](../agentcore.example.toml)
(self-hosted, Docker sandbox), [`agentcore.dev.toml`](../agentcore.dev.toml)
(local development, no isolation),
[`deploy/agentcore.toml.example`](../deploy/agentcore.toml.example)
(docker-compose).

```mermaid
flowchart LR
    F["agentcore.toml"] --> C["Config"]
    E["Environment variables<br/>(override secrets & paths)"] --> C
    P["policies/*.toml<br/>guardrails"] --> C
    R["roles/*.toml<br/>ways of working"] --> C
    C --> S["agentcore serve"]
    DB[("PostgreSQL<br/>providers · GitHub · projects")] <--> S
    UI["Web UI (admins)"] --> DB
```

Static setup lives in files (reviewable, versioned). Things that change at
runtime or are secret (model providers, the GitHub connection, projects) are
managed in the web UI and stored in PostgreSQL.

## Environment variables

| Variable | Effect |
|---|---|
| `AGENTCORE_CONFIG` | Config file path (instead of `--config`) |
| `AGENTCORE_DATABASE_URL`, `DATABASE_URL` | PostgreSQL URL; overrides `[database].url` |
| `AGENTCORE_MASTER_KEY` | Base64 32-byte master key; overrides `[secrets].master_key_file` |
| `AGENTCORE_DATA` | Data directory; overrides `[storage].data_dir` (docker-compose sets it) |
| `AGENTCORE_LOG` | Log filter, e.g. `info`, `agentcore=debug,tower_http=info` |
| `AGENTCORE_LOG_FORMAT` | `pretty` (default) or `json` |
| `AGENTCORE_TEST_DATABASE_URL` | Superuser URL for the PostgreSQL tests (development only) |

## `[server]`

| Key | Default | Meaning |
|---|---|---|
| `bind` | `127.0.0.1:8080` | Listen address. A non-loopback address requires `[[server.operators]]`. |
| `gateway_url` | `http://<bind>` | agentcore's URL **as seen from inside the sandbox** (tool and model gateways). With docker-compose: `http://agentcore:8080`. |
| `public_url` | none | URL of the web UI, used for links in GitHub comments. |
| `ui_dir` | `web/dist` | Built web UI. |

### `[[server.operators]]`

| Key | Meaning |
|---|---|
| `name` | Shown in the audit log and on GitHub ("delivered by …") |
| `token_sha256` | SHA-256 of the bearer token: `agentcore hash-token '<token>'` |
| `role` | `viewer` (watch), `operator` (start, chat, approve, stop, deliver), `admin` (also providers, GitHub, projects) |

Without any operator, the server only accepts loopback connections and treats
callers as the admin `local`.

## `[database]`

| Key | Default | Meaning |
|---|---|---|
| `url` | none (required) | `postgres://user:password@host/db`. Prefer `$AGENTCORE_DATABASE_URL`. Migrations run at startup. |

## `[secrets]`

| Key | Default | Meaning |
|---|---|---|
| `master_key_file` | `<data_dir>/master.key` | Encrypts provider API keys and the GitHub token in the database. Generated (mode 0600) on first start. **Back it up**, separately from database backups. |

## `[model_gateway]`

| Key | Default | Meaning |
|---|---|---|
| `log_bodies` | `true` | Store full LLM requests and responses (hashes are always recorded) |
| `max_logged_body_bytes` | `1048576` | Bodies are cut beyond this size |
| `upstream_read_timeout_secs` | `300` | Give up on a provider that sends nothing for this long |

## `[storage]`

| Key | Default | Meaning |
|---|---|---|
| `data_dir` | `data` | Audit logs, workspaces, repository mirrors, master key |
| `audit_fsync` | `true` | `fsync` every audit record |
| `audit_retention_days` | `183` | Published in the system card; agentcore never deletes logs itself |

## `[policies]` and `[roles]`

| Key | Default | Meaning |
|---|---|---|
| `policies.dir` | `policies` | Guardrail policies ([reference](POLICIES.md)) |
| `policies.default` | `default` | Policy when neither the request, the role nor the agent names one |
| `roles.dir` | `roles` | Roles ([ways of working](WAYS_OF_WORKING.md)) |

Policy precedence for a session: explicit request → role's `policy` →
agent's `policy` → `policies.default`.

## `[sandbox]`

| Key | Default | Meaning |
|---|---|---|
| `backend` | `docker` | `docker`, or `process` (no isolation, development only) |

### `[sandbox.docker]`

| Key | Default | Meaning |
|---|---|---|
| `cli` | `docker` | Docker-compatible CLI (`docker`, `podman`, `nerdctl`) |
| `image` | `agentcore-sandbox:latest` | Default image (`docker build -t agentcore-sandbox:latest sandbox-image`) |
| `runtime` | none | OCI runtime, e.g. `runsc` (gVisor), strongly recommended for untrusted agents |
| `network` | `none` | Docker network; use an `--internal` network shared with agentcore so agents reach only the gateways |
| `user` | `1000:1000` | User the agent runs as |
| `memory` / `cpus` / `pids_limit` | `4g` / `2` / `512` | Resource limits |
| `tmpfs_size` | `1g` | Size of `/tmp` and the agent's HOME |
| `extra_args` | `[]` | Extra `docker run` arguments, verbatim |

### `[sandbox.process]`

| Key | Default | Meaning |
|---|---|---|
| `allow_insecure` | `false` | Must be `true`: this backend does not isolate anything |
| `path` | host `PATH` | `PATH` for agent processes |

## `[transparency]`

Published in the UI ("About this system") and at `/api/v1/system-card`
(EU AI Act Art. 13 and 50): `system_name`, `provider`, `contact`,
`intended_purpose`, `limitations` (list).

## `[[agents]]`

| Key | Meaning |
|---|---|
| `name` | Unique name shown in the UI |
| `adapter` | `command` (any CLI) or `opencode` |
| `description` | Shown in the UI |
| `image` | Sandbox image override |
| `command` | Program (opencode adapter: defaults to `opencode`) |
| `args` | Arguments; `{task}`, `{workspace}`, `{gateway_url}`, `{session_id}` are substituted. The first turn's `{task}` is the composed prompt (role, conventions, issue, task). |
| `follow_up_args` | `command` adapter: arguments for follow-up turns (`{task}` = the human's message). Empty = single run. opencode continues with `--continue` automatically. |
| `env` | Extra environment; values may use `{env:NAME}` to pass an agentcore environment variable. Not audited (secrets are fine), but model keys belong in the model gateway, not here. |
| `policy` | Default guardrail policy for this agent |

```toml
[[agents]]
name = "opencode"
adapter = "opencode"
description = "opencode, all side effects through agentcore."
# Pin a model: args = ["run", "--model", "anthropic/claude-sonnet-4-5", "{task}"]

[[agents]]
name = "my-cli-agent"
adapter = "command"
command = "my-agent"
args = ["--prompt", "{task}"]
follow_up_args = ["--continue", "--prompt", "{task}"]
```

## Guardrail limits (per policy)

Each policy has a `[limits]` table; see [POLICIES.md](POLICIES.md).

| Key | Default | Meaning |
|---|---|---|
| `max_session_secs` | `3600` | Wall-clock limit for the whole session |
| `idle_timeout_secs` | `3600` | Stop a session that waits for a human longer than this |
| `max_actions` | `1000` | Actions beyond this are denied |
| `max_model_calls` | `2000` | LLM calls beyond this are refused (429) |
| `approval_timeout_secs` | `900` | Unanswered approvals count as rejected |
| `max_output_bytes` | `65536` | Tool output returned to the agent is truncated |
