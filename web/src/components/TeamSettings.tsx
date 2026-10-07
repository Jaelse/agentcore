import { useEffect, useState } from "react";
import { ApiError, api } from "../api";
import type { GitHubConnection, Me, TeamRole } from "../types";

export function GitHubSettings({
  me,
  onError,
  onChanged,
}: {
  me: Me;
  onError: (err: unknown) => void;
  onChanged?: () => void;
}) {
  const [conn, setConn] = useState<GitHubConnection | null | undefined>(undefined);
  const [token, setToken] = useState("");
  const [apiUrl, setApiUrl] = useState("https://api.github.com");
  const [webUrl, setWebUrl] = useState("https://github.com");
  const [commitName, setCommitName] = useState("agentcore[bot]");
  const [commitEmail, setCommitEmail] = useState("agentcore@users.noreply.github.com");
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const isAdmin = me.role === "admin";

  useEffect(() => {
    api
      .github()
      .then((c) => {
        setConn(c);
        if (c) {
          setApiUrl(c.config.api_url);
          setWebUrl(c.config.web_url);
          setCommitName(c.config.commit_name);
          setCommitEmail(c.config.commit_email);
        }
      })
      .catch(onError);
  }, [onError]);

  const test = async () => {
    setStatus(null);
    try {
      const r = await api.testGithub();
      setStatus(`✓ Connected as @${r.login}`);
    } catch (err) {
      setStatus(`✗ ${err instanceof ApiError ? err.message : String(err)}`);
    }
  };

  return (
    <section className="card pad">
      <h2>GitHub</h2>
      <p className="muted">
        Lets agents work on repositories: clone, read issues, milestones, discussions and project boards, and deliver
        pull requests. The token stays in agentcore (encrypted); agents never see it. Use a fine-grained token or a
        bot account with access to the repositories and boards you link (contents, issues, pull requests: read &amp;
        write; projects; discussions).
      </p>
      {conn === undefined ? (
        <p className="muted">Loading…</p>
      ) : conn ? (
        <p>
          Connected · token <code>{conn.token_hint}</code> · changed {new Date(conn.updated_at).toLocaleString()} by{" "}
          {conn.updated_by}
        </p>
      ) : (
        <div className="banner warn-banner">Not connected: projects cannot be used yet.</div>
      )}
      {isAdmin && (
        <form
          className="provider-edit"
          onSubmit={async (e) => {
            e.preventDefault();
            setBusy(true);
            try {
              setConn(
                await api.saveGithub({
                  token: token.trim() || undefined,
                  api_url: apiUrl,
                  web_url: webUrl,
                  commit_name: commitName,
                  commit_email: commitEmail,
                }),
              );
              setToken("");
              onChanged?.();
              await test();
            } catch (err) {
              onError(err);
            } finally {
              setBusy(false);
            }
          }}
        >
          <label>
            {conn ? "New token (leave empty to keep the current one)" : "Token"}
            <input type="password" autoComplete="off" value={token} onChange={(e) => setToken(e.target.value)} />
          </label>
          <div className="grid-2">
            <label>
              API URL
              <input value={apiUrl} onChange={(e) => setApiUrl(e.target.value)} />
            </label>
            <label>
              Web URL
              <input value={webUrl} onChange={(e) => setWebUrl(e.target.value)} />
            </label>
            <label>
              Commit author name
              <input value={commitName} onChange={(e) => setCommitName(e.target.value)} />
            </label>
            <label>
              Commit author email
              <input value={commitEmail} onChange={(e) => setCommitEmail(e.target.value)} />
            </label>
          </div>
          <div className="row">
            <button className="btn primary" disabled={busy || (!conn && !token.trim())}>
              {busy ? "Saving…" : conn ? "Save" : "Connect"}
            </button>
            {conn && (
              <button type="button" className="btn ghost" onClick={test}>
                Test connection
              </button>
            )}
            {status && <span className={status.startsWith("✓") ? "ok-text" : "error-text"}>{status}</span>}
          </div>
        </form>
      )}
    </section>
  );
}

export function RolesOverview({ onError }: { onError: (err: unknown) => void }) {
  const [roles, setRoles] = useState<TeamRole[] | null>(null);
  useEffect(() => {
    api.roles().then(setRoles).catch(onError);
  }, [onError]);
  return (
    <section className="card pad">
      <h2>Roles &amp; ways of working</h2>
      <p className="muted">
        A role describes <em>how</em> an agent works on your team: its playbook, which of the team's own convention
        files it must follow, which GitHub tools it gets, how cards move on the board, the guardrail policy, and the
        checks its work must pass before it leaves the sandbox. Roles are files in <code>roles/</code>, reviewed like
        code.
      </p>
      {roles === null && <p className="muted">Loading…</p>}
      <div className="provider-list">
        {roles?.map((r) => (
          <details key={r.name} className="card role-card">
            <summary>
              <strong>{r.title || r.name}</strong> <span className="muted small">— {r.description}</span>
            </summary>
            <dl className="meta stacked">
              <div>
                <dt>Guardrail policy</dt>
                <dd>{r.policy ?? "default"}</dd>
              </div>
              <div>
                <dt>Tools</dt>
                <dd className="mono small">{r.tools.join(", ") || "workspace tools only"}</dd>
              </div>
              <div>
                <dt>Follows the team's files (when present)</dt>
                <dd className="mono small">{r.repo_docs.join(", ") || "—"}</dd>
              </div>
              <div>
                <dt>Board</dt>
                <dd>
                  {r.workflow.on_start ? `start → ${r.workflow.on_start.replace("_", " ")}` : "—"}
                  {r.workflow.on_deliver ? ` · deliver → ${r.workflow.on_deliver.replace("_", " ")}` : ""}
                </dd>
              </div>
              <div>
                <dt>Delivers</dt>
                <dd>{r.delivery.kind === "pull_request" ? `pull request from ${r.delivery.branch}${r.delivery.draft ? " (draft)" : ""}` : "reports / GitHub updates"}</dd>
              </div>
              <div>
                <dt>Checks</dt>
                <dd>
                  <ul className="checks">
                    {r.checks.map((c) => (
                      <li key={c.name}>
                        {c.name}
                        {c.optional && <span className="muted small"> (optional)</span>}{" "}
                        <code className="small">{c.pattern ?? c.run ?? c.kind}</code>
                        {c.when_exists && <span className="muted small"> if {c.when_exists} exists</span>}
                      </li>
                    ))}
                    {r.checks.length === 0 && <li className="muted">none</li>}
                  </ul>
                </dd>
              </div>
            </dl>
            <details>
              <summary>Playbook</summary>
              <pre className="out">{r.instructions}</pre>
            </details>
            <p className="muted small mono">sha256:{r.digest}</p>
          </details>
        ))}
      </div>
    </section>
  );
}
