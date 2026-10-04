# Security

## Threat model

agentcore assumes the **agent is untrusted**. Its model can be wrong, and it can
be manipulated by content it reads (prompt injection via issues, code, web pages).

Trust boundaries:

1. **Sandbox (hard boundary).** Contains everything the agent process does,
   including actions that never touch the gateway.
2. **Gateway policy (fine-grained boundary).** Governs what the agent may do
   through agentcore tools, and adds human approval and audit.
3. **Operator API.** Only authenticated humans can start, approve or stop.

## Defaults

* Containers: `--cap-drop=ALL`, `--security-opt=no-new-privileges`,
  `--read-only` root with small `tmpfs` mounts, `--user 1000:1000`,
  `--pids-limit`, memory and CPU limits, `--init`, `--network=none`.
* Only `/workspace` is writable and shared with the host.
* File paths are normalised before policy evaluation; the process backend
  additionally resolves symlinks and refuses paths that leave the workspace.
  The Docker backend performs file operations inside the container.
* Each session gets a random gateway token; the gateway answers 401 for both
  unknown sessions and wrong tokens.
* Operator tokens are stored only as SHA-256 hashes and compared in constant time.
* Without operators configured, the server refuses to bind to a non-loopback address.
* Audit logging is fail-closed: if an event cannot be recorded, the session is stopped.
* Secrets passed to agents (`{env:NAME}`) go into the environment, which is
  never audited, and never into the audited command line.

## Hardening checklist

- [ ] Use gVisor (`runtime = "runsc"`) or Kata Containers for untrusted agents.
- [ ] Run agentcore on a dedicated VM. **Access to the Docker socket is root-equivalent.** Prefer rootless Docker or Podman.
- [ ] Terminate TLS in front of agentcore; never expose port 8080 directly.
- [ ] Use long random operator tokens; give most people the `viewer` role.
- [ ] Keep `network = "none"` or an `internal` network. Agents that need model
      access require an egress path; restrict it to the model provider's hosts.
- [ ] Provide model API keys with minimal scope and spending limits. Anything
      in the sandbox environment can be read by the agent.
- [ ] Store `data/` on an encrypted volume with backups; restrict who can read audit logs.
- [ ] Never use the `process` backend outside local development.

## Known limitations

* The `process` backend provides **no isolation**.
* Docker file writes and reads run inside the container, which is safe against
  host symlink tricks. Policies match the requested path, not a symlink's
  target inside the container.
* Agents with native tools that bypass the gateway are only constrained by the
  sandbox, not by policy.

## Reporting vulnerabilities

Please report vulnerabilities privately via GitHub security advisories on this repository.
