import { useEffect, useState } from "react";
import { ApiError, api } from "../api";
import type { CloudflareConnection, CloudflareTest, GitHubConnection, Me, TeamRole, TokenConnection } from "../types";

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
        write; projects; discussions; read access to actions, checks, commit statuses and deployments, so agentcore can
        watch each pull request's checks and preview).
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

/** Vercel: read-only access to the build logs of failed preview deployments. */
export function VercelSettings({ me, onError }: { me: Me; onError: (err: unknown) => void }) {
  const [conn, setConn] = useState<TokenConnection | null | undefined>(undefined);
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const isAdmin = me.role === "admin";

  useEffect(() => {
    api.vercel().then(setConn).catch(onError);
  }, [onError]);

  return (
    <section className="card pad">
      <h2>Vercel</h2>
      <p className="muted">
        Optional. After an agent's pull request is opened, agentcore watches its checks and preview deployment on
        GitHub and sends failures back to the agent. With a Vercel token it can also hand the agent the build log of a
        failed preview, so it sees the actual error. The token stays in agentcore (encrypted); agents never see it.
      </p>
      {conn === undefined ? (
        <p className="muted">Loading…</p>
      ) : conn ? (
        <p>
          Connected · token <code>{conn.token_hint}</code> · changed {new Date(conn.updated_at).toLocaleString()} by{" "}
          {conn.updated_by}
        </p>
      ) : (
        <p className="muted">Not connected: failed Vercel previews reach the agent without their build log.</p>
      )}
      {isAdmin && (
        <form
          className="provider-edit"
          onSubmit={async (e) => {
            e.preventDefault();
            setBusy(true);
            try {
              setConn(await api.saveVercel(token.trim()));
              setToken("");
            } catch (err) {
              onError(err);
            } finally {
              setBusy(false);
            }
          }}
        >
          <label>
            {conn ? "New token" : "Token"}
            <input
              type="password"
              autoComplete="off"
              value={token}
              placeholder="Vercel → Account Settings → Tokens"
              onChange={(e) => setToken(e.target.value)}
            />
          </label>
          <div className="row">
            <button className="btn primary" disabled={busy || !token.trim()}>
              {busy ? "Saving…" : conn ? "Replace token" : "Connect"}
            </button>
            {conn && (
              <button
                type="button"
                className="btn ghost"
                disabled={busy}
                onClick={async () => {
                  try {
                    await api.deleteVercel();
                    setConn(null);
                  } catch (err) {
                    onError(err);
                  }
                }}
              >
                Disconnect
              </button>
            )}
          </div>
        </form>
      )}
    </section>
  );
}

const CLOUDFLARE_PRODUCTS: [string, string][] = [
  ["workers", "Workers"],
  ["pages", "Pages"],
  ["d1", "D1"],
  ["kv", "KV"],
  ["r2", "R2"],
  ["queues", "Queues"],
  ["zones", "DNS zones"],
];

/**
 * Cloudflare: the account agents in the Cloudflare deployment role provision and deploy to. The token stays in
 * agentcore; agents ask agentcore to act for them.
 */
export function CloudflareSettings({ me, onError }: { me: Me; onError: (err: unknown) => void }) {
  const [conn, setConn] = useState<CloudflareConnection | null | undefined>(undefined);
  const [accountId, setAccountId] = useState("");
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [test, setTest] = useState<CloudflareTest | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const isAdmin = me.role === "admin";

  useEffect(() => {
    api
      .cloudflare()
      .then((c) => {
        setConn(c);
        if (c) setAccountId(c.account_id);
      })
      .catch(onError);
  }, [onError]);

  const runTest = async () => {
    setStatus(null);
    setTest(null);
    try {
      const r = await api.testCloudflare();
      setTest(r);
      setStatus(`✓ Connected to ${r.account}`);
    } catch (err) {
      setStatus(`✗ ${err instanceof ApiError ? err.message : String(err)}`);
    }
  };

  const validId = /^[0-9a-fA-F]{32}$/.test(accountId.trim());

  return (
    <section className="card pad">
      <h2>Cloudflare</h2>
      <p className="muted">
        Lets agents in the <strong>Cloudflare deployment engineer</strong> role put applications on Cloudflare: they
        read the account, create what an application needs (D1, KV, R2, Queues, Pages projects), set secrets, DNS
        records and custom domains, and set up the repository so every pull request gets a preview and every merge
        deploys to production. The token stays in agentcore (encrypted); agents never see it. Creating or changing
        anything waits for a person's approval, and the token is copied into a repository's GitHub Actions secrets only
        when a person approves it.
      </p>
      {conn === undefined ? (
        <p className="muted">Loading…</p>
      ) : conn ? (
        <p>
          Connected to <strong>{conn.account_name || conn.account_id}</strong> · token <code>{conn.token_hint}</code>{" "}
          · changed {new Date(conn.updated_at).toLocaleString()} by {conn.updated_by}
        </p>
      ) : (
        <p className="muted">Not connected: Cloudflare deployment agents cannot reach Cloudflare yet.</p>
      )}
      {test && (
        <ul className="cf-access">
          {CLOUDFLARE_PRODUCTS.map(([key, label]) => (
            <li key={key} className={test.access[key] ? "ok-text" : "error-text"}>
              {test.access[key] ? "✓" : "✗"} {label}
            </li>
          ))}
        </ul>
      )}
      {isAdmin && (
        <form
          className="provider-edit"
          onSubmit={async (e) => {
            e.preventDefault();
            setBusy(true);
            setStatus(null);
            try {
              setConn(await api.saveCloudflare({ account_id: accountId.trim(), token: token.trim() || undefined }));
              setToken("");
              await runTest();
            } catch (err) {
              setStatus(`✗ ${err instanceof ApiError ? err.message : String(err)}`);
            } finally {
              setBusy(false);
            }
          }}
        >
          <div className="grid-2">
            <label>
              Account ID
              <input
                value={accountId}
                spellCheck={false}
                placeholder="32 characters, from the dashboard URL"
                onChange={(e) => setAccountId(e.target.value)}
              />
            </label>
            <label>
              {conn ? "New API token (leave empty to keep)" : "API token"}
              <input
                type="password"
                autoComplete="off"
                value={token}
                placeholder="My Profile → API Tokens → Create Token"
                onChange={(e) => setToken(e.target.value)}
              />
            </label>
          </div>
          <details className="muted">
            <summary>Which permissions the token needs</summary>
            <p>
              Account: <em>Workers Scripts</em>, <em>Cloudflare Pages</em>, <em>D1</em>,{" "}
              <em>Workers KV Storage</em>, <em>Workers R2 Storage</em>, <em>Queues</em> (Edit), <em>Account
              Settings</em> (Read). Zone: <em>DNS</em> (Edit), <em>Workers Routes</em> (Edit), <em>Zone</em> (Read).
              Agents use the same token, as a GitHub Actions secret, to deploy from CI, so give it nothing more.
            </p>
          </details>
          <div className="row">
            <button className="btn primary" disabled={busy || !validId || (!conn && !token.trim())}>
              {busy ? "Checking…" : conn ? "Save" : "Connect"}
            </button>
            {conn && (
              <>
                <button type="button" className="btn ghost" disabled={busy} onClick={runTest}>
                  Test connection
                </button>
                <button
                  type="button"
                  className="btn ghost"
                  disabled={busy}
                  onClick={async () => {
                    try {
                      await api.deleteCloudflare();
                      setConn(null);
                      setTest(null);
                      setStatus(null);
                      setAccountId("");
                    } catch (err) {
                      onError(err);
                    }
                  }}
                >
                  Disconnect
                </button>
              </>
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
