import { useEffect, useState } from "react";
import { api } from "../api";
import type { Audience, HealthGate, LaunchSettings, LaunchStats, Me, OrgOverview, Product, ProductSummary, Release, ReleaseDetail, ReleaseStatus, Stage } from "../types";

const STATUS: Record<ReleaseStatus, { label: string; cls: string }> = {
  proposed: { label: "waiting for you", cls: "status-awaiting_input" },
  changes_requested: { label: "sent back", cls: "status-paused" },
  rolling_out: { label: "rolling out", cls: "status-running" },
  paused: { label: "paused", cls: "status-paused" },
  released: { label: "released", cls: "status-completed" },
  rolled_back: { label: "rolled back", cls: "status-failed" },
  rejected: { label: "rejected", cls: "" },
};

const FILTERS: { id: string; label: string; match: (s: ReleaseStatus) => boolean }[] = [
  { id: "pending", label: "To approve", match: (s) => s === "proposed" || s === "changes_requested" },
  { id: "active", label: "Rolling out", match: (s) => s === "rolling_out" || s === "paused" },
  { id: "done", label: "History", match: (s) => s === "released" || s === "rolled_back" || s === "rejected" },
  { id: "all", label: "All", match: () => true },
];

const GATE_CLS: Record<string, string> = { pass: "chip ok", fail: "chip danger-text", waiting: "chip", error: "chip" };
const GATE_ICON: Record<string, string> = { pass: "✓", fail: "✕", waiting: "…", error: "!" };

const pct = (n: number | null | undefined) => (n == null ? "–" : `${(n * 100).toFixed(1)}%`);
const short = (sha: string) => `${sha.slice(0, 12)}…`;

/** Releases agents propose, people approve, and the server rolls out in stages. */
export function Releases({ org, me, tick, onError }: { org: OrgOverview; me: Me; tick: number; onError: (e: unknown) => void }) {
  const [releases, setReleases] = useState<Release[] | null>(null);
  const [products, setProducts] = useState<ProductSummary[]>([]);
  const [filter, setFilter] = useState("pending");
  const [product, setProduct] = useState("");
  const [reload, setReload] = useState(0);
  useEffect(() => {
    api.releases().then(setReleases).catch(onError);
    api.products().then(setProducts).catch(onError);
  }, [tick, reload, onError]);
  const changed = () => setReload((r) => r + 1);
  const shown = (releases ?? []).filter(
    (r) => FILTERS.find((f) => f.id === filter)!.match(r.status) && (!product || r.product_id === product),
  );
  const productOf = (id: string) => products.find((p) => p.product.id === id);
  return (
    <div className="session dashboard">
      <section className="card session-header">
        <div className="session-header-main">
          <h2>Releases</h2>
          <p className="muted">
            Agents propose releases of what they built: a version, the commit, notes, and files with their SHA-256. The server
            scores the risk from facts and adapts the rollout. People approve; before the last approval the server downloads
            every file and checks its digest. Then the release goes out stage by stage to a growing share of installations,
            watched by health gates that roll it back on their own when something breaks. Every manifest clients receive is
            signed.
          </p>
        </div>
        <div className="session-actions segmented" role="group" aria-label="Show">
          {FILTERS.map((f) => (
            <button key={f.id} className={`btn small-btn ${filter === f.id ? "primary" : "ghost"}`} onClick={() => setFilter(f.id)}>
              {f.label}
              {f.id === "pending" && org.releases_pending > 0 ? ` (${org.releases_pending})` : ""}
            </button>
          ))}
          {products.length > 1 && (
            <select value={product} onChange={(e) => setProduct(e.target.value)} aria-label="Product">
              <option value="">All products</option>
              {products.map((p) => (
                <option key={p.product.id} value={p.product.id}>
                  {p.product.name}
                </option>
              ))}
            </select>
          )}
        </div>
      </section>
      {releases === null && <div className="empty">Loading…</div>}
      {releases !== null && shown.length === 0 && (
        <div className="empty">
          <p>
            {products.length === 0
              ? "No products yet: add one below so a department's agents can propose releases."
              : filter === "pending"
                ? "No release waits for approval."
                : "Nothing here."}
          </p>
        </div>
      )}
      {shown.map((r) => (
        <ReleaseCard
          key={`${r.id}-${r.revision}-${r.status}-${r.stage}`}
          release={r}
          product={productOf(r.product_id)}
          me={me}
          tick={tick}
          onChanged={changed}
          onError={onError}
        />
      ))}
      <Products org={org} me={me} products={products} onChanged={changed} onError={onError} />
    </div>
  );
}

function RiskMeter({ release }: { release: Release }) {
  const { score, level } = release.risk;
  const fill = level === "high" ? "meter-critical" : level === "medium" ? "meter-warning" : "";
  return (
    <div>
      <div className="session-item-top small">
        <span>
          <strong>Risk {score}/100</strong> · {level}
        </span>
        <span className="muted">{release.risk.factors.length} factor(s)</span>
      </div>
      <div className="meter" role="meter" aria-valuemin={0} aria-valuemax={100} aria-valuenow={score} aria-label="Risk score">
        <span className={fill} style={{ width: `${Math.max(2, score)}%` }} />
      </div>
      <details className="small">
        <summary>Why</summary>
        <ul>
          {release.risk.factors.map((f) => (
            <li key={f.name}>
              <strong>+{f.points}</strong> {f.name}: {f.detail}
            </li>
          ))}
        </ul>
        {release.risk.assessment && (
          <p>
            <strong>Proposer's assessment:</strong> {release.risk.assessment}
          </p>
        )}
      </details>
    </div>
  );
}

function Rollout({ release, detail }: { release: Release; detail: ReleaseDetail | null }) {
  const active = release.status === "rolling_out" || release.status === "paused";
  const done = release.status === "released";
  return (
    <div className="grid-2">
      <div>
        <h4>Rollout</h4>
        <ol className="small" style={{ margin: 0, paddingLeft: 18 }}>
          {release.plan.map((s, i) => {
            const current = active && i === release.stage;
            const passed = done || (active && i < release.stage);
            return (
              <li key={s.name} style={{ fontWeight: current ? 700 : 400 }}>
                {passed ? "✓ " : current ? "▶ " : ""}
                {s.name}: {s.percent}% of units{s.soak_minutes > 0 && `, soak ${s.soak_minutes} min`}
                {current && release.stage_started_at && (
                  <span className="muted"> · since {new Date(release.stage_started_at).toLocaleTimeString()}</span>
                )}
              </li>
            );
          })}
        </ol>
      </div>
      {(active || done || release.status === "rolled_back") && (
        <div>
          <h4>Health</h4>
          {detail && (
            <div className="small">
              {detail.reports.units} unit(s) reported · crash-free {pct(detail.crash_free)} · installs ok {pct(detail.install_success)}
            </div>
          )}
          <div className="row" style={{ flexWrap: "wrap", gap: 4 }}>
            {release.gates.length === 0 && <span className="muted small">no gate results yet</span>}
            {release.gates.map((g) => (
              <span key={g.name} className={GATE_CLS[g.status]} title={g.detail}>
                {GATE_ICON[g.status]} {g.name}: {g.detail}
              </span>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

function ReleaseCard({
  release: r,
  product,
  me,
  tick,
  onChanged,
  onError,
}: {
  release: Release;
  product: ProductSummary | undefined;
  me: Me;
  tick: number;
  onChanged: () => void;
  onError: (e: unknown) => void;
}) {
  const [mode, setMode] = useState<"view" | "changes" | "reject" | "pause" | "rollback">("view");
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const [detail, setDetail] = useState<ReleaseDetail | null>(null);
  const live = r.status !== "proposed" && r.status !== "changes_requested" && r.status !== "rejected";
  useEffect(() => {
    if (live) api.release(r.id).then(setDetail).catch(onError);
  }, [r.id, live, tick, onError]);
  const canOperate = me.role !== "viewer";
  const approved = r.approvals.length >= r.approvals_required;
  const mine = r.approvals.some((a) => a.by === me.name);
  const isBaseline = product?.current_version === r.version;
  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    setNote(null);
    try {
      const out = await fn();
      const n = (out as { note?: string } | undefined)?.note;
      if (n) setNote(n);
      setMode("view");
      onChanged();
    } catch (err) {
      onError(err);
    } finally {
      setBusy(false);
    }
  };
  const submit = () => {
    switch (mode) {
      case "changes":
        return api.releaseChanges(r.id, text);
      case "reject":
        return api.rejectRelease(r.id, text);
      case "pause":
        return api.pauseRelease(r.id, text);
      default:
        return api.rollBackRelease(r.id, text);
    }
  };
  const prompts: Record<string, string> = {
    changes: "What should be different? The agent revises it.",
    reject: "Why not? (the agent is told)",
    pause: "Why pause? Units keep what they were offered.",
    rollback: "Why roll back? Units are offered the previous version again.",
  };
  return (
    <section className="card proposal">
      <div className="session-item-top">
        <span>
          <span className="chip">📦 {product?.product.name ?? "deleted product"}</span> <strong className="mono">{r.version}</strong>
          {r.commit && <span className="muted small mono"> @ {r.commit.slice(0, 10)}</span>}
        </span>
        <span className={`pill ${STATUS[r.status].cls}`}>
          {STATUS[r.status].label}
          {(r.status === "rolling_out" || r.status === "paused") && ` · ${r.plan[r.stage]?.percent ?? 0}%`}
        </span>
      </div>
      <div className="muted small">
        proposed by {r.proposed_by} · {new Date(r.created_at).toLocaleString()}
        {r.revision > 1 && ` · revision ${r.revision}`} · approvals {r.approvals.length} of {r.approvals_required}
        {r.approvals.length > 0 && ` (${r.approvals.map((a) => a.by).join(", ")})`}
        {r.released_at && ` · released ${new Date(r.released_at).toLocaleString()}`}
      </div>
      <RiskMeter release={r} />
      <div className="message-preview">{r.notes}</div>
      <div className="proposal-grid">
        <div>
          <h4>Artifacts</h4>
          <ul className="small" style={{ margin: 0, paddingLeft: 18 }}>
            {r.artifacts.map((a) => (
              <li key={a.name}>
                <span className="mono">{a.name}</span> ({a.platform}) · <span className="mono" title={a.sha256}>sha256 {short(a.sha256)}</span>{" "}
                {a.verified_at ? (
                  <span className="chip ok" title={`downloaded and checked ${new Date(a.verified_at).toLocaleString()}`}>
                    ✓ verified
                  </span>
                ) : (
                  <span className="muted">checked on approval</span>
                )}
              </li>
            ))}
          </ul>
        </div>
        <div>
          <h4>Evidence</h4>
          <div className="row" style={{ flexWrap: "wrap", gap: 4 }}>
            {r.checks.length === 0 && <span className="muted small">no checks reported</span>}
            {r.checks.map((c) => (
              <span key={c.name} className={c.passed ? "chip ok" : "chip danger-text"} title={c.detail}>
                {c.passed ? "✓" : "✕"} {c.name}
              </span>
            ))}
            <span className={r.sbom ? "chip ok" : "chip"}>{r.sbom ? "✓ SBOM" : "no SBOM"}</span>
          </div>
          {r.changes.lines_added != null && (
            <div className="small muted">
              {r.changes.files_changed ?? "?"} file(s), +{r.changes.lines_added} −{r.changes.lines_removed ?? 0}
              {r.changes.commits != null && `, ${r.changes.commits} commit(s)`}
            </div>
          )}
        </div>
      </div>
      <Rollout release={r} detail={detail} />
      {r.feedback && (
        <div className="callout small">
          <strong>Asked to change:</strong> {r.feedback}
        </div>
      )}
      {r.reason && (
        <div className={`callout small ${r.status === "rolled_back" ? "danger-text" : ""}`}>
          <strong>{r.status === "rolled_back" ? "Rolled back" : r.status === "paused" ? "Paused" : "Reason"}</strong>
          {r.decided_by && ` by ${r.decided_by}`}: {r.reason}
        </div>
      )}
      {note && <div className="callout small">{note}</div>}
      {r.attestation && (
        <details className="small">
          <summary>Signed attestation (in-toto, DSSE)</summary>
          <pre className="mono-pre">{JSON.stringify(r.attestation, null, 2)}</pre>
        </details>
      )}
      {canOperate && mode === "view" && (
        <div className="row">
          {r.status === "proposed" && !approved && !mine && (
            <button
              className="btn primary"
              disabled={busy}
              onClick={() => {
                if (confirm(`Approve ${r.version} (revision ${r.revision})? The server first downloads and checks every artifact.`))
                  void run(() => api.approveRelease(r.id, r.revision));
              }}
            >
              {busy ? "Checking artifacts…" : "✓ Approve"}
            </button>
          )}
          {r.status === "proposed" && approved && (
            <button className="btn primary" disabled={busy} onClick={() => void run(() => api.startRelease(r.id))}>
              ▶ Start rollout
            </button>
          )}
          {(r.status === "proposed" || r.status === "changes_requested") && (
            <>
              {r.agent_id && (
                <button className="btn" onClick={() => setMode("changes")}>
                  Send back…
                </button>
              )}
              <button className="btn ghost danger-text" onClick={() => setMode("reject")}>
                Reject…
              </button>
            </>
          )}
          {r.status === "rolling_out" && (
            <>
              <button
                className="btn"
                disabled={busy}
                title="Next stage now, without waiting for the soak time (gates must pass)"
                onClick={() => void run(() => api.promoteRelease(r.id))}
              >
                ⏩ Next stage
              </button>
              <button className="btn" onClick={() => setMode("pause")}>
                ⏸ Pause…
              </button>
            </>
          )}
          {r.status === "paused" && (
            <button className="btn primary" disabled={busy} onClick={() => void run(() => api.resumeRelease(r.id))}>
              ▶ Resume
            </button>
          )}
          {(r.status === "rolling_out" || r.status === "paused" || (r.status === "released" && isBaseline)) && (
            <button className="btn ghost danger-text" onClick={() => setMode("rollback")}>
              ↩ Roll back…
            </button>
          )}
        </div>
      )}
      {mode !== "view" && (
        <form
          className="provider-edit"
          onSubmit={(e) => {
            e.preventDefault();
            void run(submit);
          }}
        >
          <label>
            {prompts[mode]}
            <textarea rows={3} value={text} onChange={(e) => setText(e.target.value)} required={mode !== "reject"} autoFocus />
          </label>
          <div className="row">
            <button className={`btn ${mode === "reject" || mode === "rollback" ? "danger" : "primary"}`} disabled={busy}>
              {mode === "changes" ? "Send back" : mode === "reject" ? "Reject" : mode === "pause" ? "Pause" : "Roll back"}
            </button>
            <button type="button" className="btn ghost" onClick={() => setMode("view")}>
              Cancel
            </button>
          </div>
        </form>
      )}
    </section>
  );
}

// ---- products -----------------------------------------------------------------

function planText(plan: Stage[]) {
  return plan.map((s) => `${s.name} ${s.percent} ${s.soak_minutes}`).join("\n");
}

function parsePlan(text: string): Stage[] {
  return text
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean)
    .map((l) => {
      const [name, percent, soak] = l.split(/\s+/);
      return { name, percent: Number(percent), soak_minutes: Number(soak ?? 0) };
    });
}

const DEFAULT_PLAN: Stage[] = [
  { name: "canary", percent: 1, soak_minutes: 60 },
  { name: "early", percent: 10, soak_minutes: 120 },
  { name: "half", percent: 50, soak_minutes: 240 },
  { name: "everyone", percent: 100, soak_minutes: 0 },
];

function clientSnippet(product: string, token: string) {
  const origin = window.location.origin;
  return `# Which version should this installation run? (signed DSSE manifest)
curl -H "Authorization: Bearer ${token}" \\
  "${origin}/dist/${product}/update?unit=$INSTALL_ID&current=$VERSION&platform=linux-x86_64"
# Verify the envelope with a key from ${origin}/dist/keys, check expires_at,
# download the artifact and compare its sha256 before installing.

# Report what happened (feeds the crash-free and install-success gates)
curl -X POST -H "Authorization: Bearer ${token}" -H 'Content-Type: application/json' \\
  -d '{"unit":"'$INSTALL_ID'","version":"'$VERSION'","event":"installed"}' \\
  "${origin}/dist/${product}/report"      # events: installed, install_failed, crash, healthy`;
}

function Products({
  org,
  me,
  products,
  onChanged,
  onError,
}: {
  org: OrgOverview;
  me: Me;
  products: ProductSummary[];
  onChanged: () => void;
  onError: (e: unknown) => void;
}) {
  const [adding, setAdding] = useState(false);
  const [token, setToken] = useState<{ product: string; token: string } | null>(null);
  const isAdmin = me.role === "admin";
  return (
    <section className="card side-panel">
      <div className="row" style={{ justifyContent: "space-between" }}>
        <h3 className="section-title">Products</h3>
        {isAdmin && !adding && (
          <button className="btn small-btn" onClick={() => setAdding(true)}>
            + Product
          </button>
        )}
      </div>
      <p className="muted small">
        What the organisation ships, which departments may propose releases, how releases roll out and which health gates
        watch them. Update clients use the product's client token, never an operator token.
      </p>
      {token && (
        <div className="callout small">
          <strong>Client token for {token.product}</strong> (shown once; put it in your update client):
          <pre className="mono-pre">{clientSnippet(token.product, token.token)}</pre>
          <button className="btn small-btn" onClick={() => setToken(null)}>
            I saved it
          </button>
        </div>
      )}
      {adding && (
        <ProductForm
          org={org}
          onDone={(created) => {
            setAdding(false);
            if (created) setToken(created);
            onChanged();
          }}
          onError={onError}
        />
      )}
      <ul className="goal-list">
        {products.map((p) => (
          <ProductRow key={p.product.id} p={p} org={org} me={me} onToken={setToken} onChanged={onChanged} onError={onError} />
        ))}
      </ul>
    </section>
  );
}

function gateText(g: HealthGate) {
  const what = g.metric.kind === "data" ? `${g.metric.source}.${g.metric.field}` : g.metric.kind.replace("_", "-");
  const sym = { lt: "<", lte: "≤", gt: ">", gte: "≥" }[g.comparator];
  return `${g.name}: ${what} ${sym} ${g.threshold}${g.min_samples ? ` (≥ ${g.min_samples} units)` : ""}`;
}

function ProductRow({
  p,
  org,
  me,
  onToken,
  onChanged,
  onError,
}: {
  p: ProductSummary;
  org: OrgOverview;
  me: Me;
  onToken: (t: { product: string; token: string }) => void;
  onChanged: () => void;
  onError: (e: unknown) => void;
}) {
  const product: Product = p.product;
  const act = async (fn: () => Promise<unknown>) => {
    try {
      await fn();
      onChanged();
    } catch (err) {
      onError(err);
    }
  };
  return (
    <li className="goal">
      <div className="session-item-top">
        <strong className="mono">{product.name}</strong>
        <span className={`pill ${product.frozen ? "status-paused" : p.rollout ? "status-running" : "status-completed"}`}>
          {product.frozen ? "frozen" : p.rollout ? `${p.rollout.version} at ${p.rollout.percent}%` : (p.current_version ?? "not released yet")}
        </span>
      </div>
      {product.description && <div className="small">{product.description}</div>}
      <div className="muted small">
        {product.repository && `${product.repository} · `}
        {product.approvals_required} approval(s) · {product.plan.map((s) => `${s.percent}%`).join(" → ")}
        {product.client_token_hint && ` · client token ${product.client_token_hint}`}
        {product.webhook_url && ` · webhook ${product.webhook_url}`}
      </div>
      <div className="small">
        <strong>Health gates:</strong> {product.gates.length === 0 ? <span className="danger-text">none (rollouts are blind)</span> : product.gates.map(gateText).join("; ")}
      </div>
      <div className="small">
        <strong>Artifact hosts:</strong> {product.artifact_hosts.join(", ") || <span className="danger-text">none allowed yet</span>}
      </div>
      <div className="small">
        <strong>Departments:</strong>{" "}
        {product.departments.map((d) => org.departments.find((x) => x.id === d)?.name ?? "?").join(", ") || "none"}
      </div>
      <LaunchPanel product={product} org={org} me={me} onChanged={onChanged} onError={onError} />
      {product.frozen && (
        <div className="callout small">
          <strong>Change freeze</strong> by {product.frozen_by}: {product.frozen}
        </div>
      )}
      {me.role !== "viewer" && (
        <div className="row">
          {product.frozen ? (
            <button className="btn small-btn" onClick={() => void act(() => api.unfreezeProduct(product.id))}>
              Lift freeze
            </button>
          ) : (
            <button
              className="btn small-btn"
              onClick={() => {
                const reason = prompt("Why freeze releases? Nothing starts or moves on until the freeze is lifted.");
                if (reason) void act(() => api.freezeProduct(product.id, reason));
              }}
            >
              ❄ Freeze
            </button>
          )}
          {me.role === "admin" && (
            <>
              <button
                className="btn small-btn"
                onClick={() => {
                  if (confirm("Make a new client token? The current one stops working right away."))
                    void act(async () => {
                      const out = await api.rotateClientToken(product.id);
                      onToken({ product: product.name, token: out.client_token });
                    });
                }}
              >
                New client token
              </button>
              <button
                className="btn small-btn ghost danger-text"
                onClick={() => {
                  if (confirm(`Delete ${product.name} and its release history?`)) void act(() => api.deleteProduct(product.id));
                }}
              >
                Delete
              </button>
            </>
          )}
        </div>
      )}
    </li>
  );
}

const AUDIENCE: Record<Audience, string> = { developers: "developers", business: "businesses", both: "developers and businesses" };

/** How the product reaches people: its page, announcements and what worked. */
function LaunchPanel({
  product,
  org,
  me,
  onChanged,
  onError,
}: {
  product: Product;
  org: OrgOverview;
  me: Me;
  onChanged: () => void;
  onError: (e: unknown) => void;
}) {
  const [launch, setLaunch] = useState<LaunchStats | null>(null);
  const [editing, setEditing] = useState(false);
  useEffect(() => {
    api.launchStats(product.id).then(setLaunch).catch(onError);
  }, [product.id, product.updated_at, onError]);
  const s = launch?.stats;
  const announcers = product.announce_departments.map((d) => org.departments.find((x) => x.id === d)?.name ?? "?");
  return (
    <div className="callout small" style={{ display: "grid", gap: 6 }}>
      <div className="session-item-top">
        <strong>Reaching people</strong>
        {launch?.page ? (
          <a href={launch.page} target="_blank" rel="noopener noreferrer">
            🌐 public page
          </a>
        ) : (
          <span className="muted">no public page</span>
        )}
      </div>
      <div>
        For {AUDIENCE[product.audience]}
        {product.tagline && <> · “{product.tagline}”</>}
        {" · "}
        {announcers.length > 0 ? (
          <>announced by {announcers.join(", ")}</>
        ) : (
          <span className="danger-text">no department announces its releases</span>
        )}
      </div>
      {s && (
        <>
          <div className="stat-row">
            <div className="stat-tile">
              <span className="stat-label">Visits ({s.days} d)</span>
              <span className="stat-value">{s.visits}</span>
            </div>
            <div className="stat-tile">
              <span className="stat-label">Downloads</span>
              <span className="stat-value">{s.downloads}</span>
            </div>
            <div className="stat-tile">
              <span className="stat-label">Sign-ups</span>
              <span className="stat-value">{s.signups}</span>
            </div>
            <div className="stat-tile">
              <span className="stat-label">Subscribers</span>
              <span className="stat-value">{s.subscribers}</span>
              <span className="muted">
                {s.developers} dev · {s.business} business
              </span>
            </div>
          </div>
          {s.sources.length > 0 && (
            <table className="small" style={{ width: "100%", borderCollapse: "collapse" }}>
              <thead>
                <tr className="muted" style={{ textAlign: "left" }}>
                  <th>Source</th>
                  <th style={{ textAlign: "right" }}>Visits</th>
                  <th style={{ textAlign: "right" }}>Downloads</th>
                  <th style={{ textAlign: "right" }}>Sign-ups</th>
                </tr>
              </thead>
              <tbody>
                {s.sources.slice(0, 8).map((x) => (
                  <tr key={x.source}>
                    <td className="mono">{x.source}</td>
                    <td style={{ textAlign: "right" }}>{x.visits}</td>
                    <td style={{ textAlign: "right" }}>{x.downloads}</td>
                    <td style={{ textAlign: "right" }}>{x.signups}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </>
      )}
      {me.role === "admin" && !editing && (
        <div className="row">
          <button className="btn small-btn" onClick={() => setEditing(true)}>
            Launch settings…
          </button>
          {s && s.subscribers > 0 && (
            <button className="btn small-btn" onClick={() => void api.downloadSubscribers(product.id, product.name).catch(onError)}>
              ⬇ Subscribers (CSV)
            </button>
          )}
        </div>
      )}
      {editing && (
        <LaunchForm
          product={product}
          org={org}
          onDone={(saved) => {
            setEditing(false);
            if (saved) onChanged();
          }}
          onError={onError}
        />
      )}
    </div>
  );
}

function LaunchForm({
  product,
  org,
  onDone,
  onError,
}: {
  product: Product;
  org: OrgOverview;
  onDone: (saved: boolean) => void;
  onError: (e: unknown) => void;
}) {
  const [f, setF] = useState<LaunchSettings>({
    public: product.public,
    tagline: product.tagline,
    audience: product.audience,
    pitch: product.pitch,
    website: product.website ?? "",
    announce_departments: product.announce_departments,
    announce_patches: product.announce_patches,
  });
  const [busy, setBusy] = useState(false);
  const set = <K extends keyof LaunchSettings>(k: K, v: LaunchSettings[K]) => setF({ ...f, [k]: v });
  return (
    <form
      className="provider-edit"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          await api.updateProduct(product.id, f);
          onDone(true);
        } catch (err) {
          onError(err);
        } finally {
          setBusy(false);
        }
      }}
    >
      <label>
        <input type="checkbox" checked={f.public} onChange={(e) => set("public", e.target.checked)} /> Public page with downloads,
        changelog and a sign-up form (<span className="mono">/p/{product.name}</span>)
      </label>
      <div className="grid-2">
        <label>
          Tagline (one line)
          <input value={f.tagline} maxLength={140} onChange={(e) => set("tagline", e.target.value)} placeholder="Ship in minutes" />
        </label>
        <label>
          Who it is for
          <select value={f.audience} onChange={(e) => set("audience", e.target.value as Audience)}>
            <option value="developers">Developers</option>
            <option value="business">Businesses</option>
            <option value="both">Developers and businesses</option>
          </select>
        </label>
        <label>
          Website (optional)
          <input value={f.website} onChange={(e) => set("website", e.target.value)} placeholder="https://example.com" />
        </label>
      </div>
      <label>
        Positioning for the people and agents who announce it: the problem, for whom, why it is different (not shown publicly)
        <textarea rows={3} value={f.pitch} maxLength={5000} onChange={(e) => set("pitch", e.target.value)} />
      </label>
      <fieldset className="card">
        <legend className="small">Departments that announce its releases (they get a launch brief and the launch tools)</legend>
        {org.departments.map((d) => (
          <label key={d.id} className="small">
            <input
              type="checkbox"
              checked={f.announce_departments.includes(d.id)}
              onChange={(e) =>
                set(
                  "announce_departments",
                  e.target.checked ? [...f.announce_departments, d.id] : f.announce_departments.filter((x) => x !== d.id),
                )
              }
            />{" "}
            {d.name}
          </label>
        ))}
        <label className="small">
          <input type="checkbox" checked={f.announce_patches} onChange={(e) => set("announce_patches", e.target.checked)} /> Also
          brief them on patch releases
        </label>
      </fieldset>
      <p className="muted small">
        Running agents get the launch tools in their next session (when a message wakes them, or after a restart).
      </p>
      <div className="row">
        <button className="btn primary" disabled={busy}>
          Save
        </button>
        <button type="button" className="btn ghost" onClick={() => onDone(false)}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function ProductForm({
  org,
  onDone,
  onError,
}: {
  org: OrgOverview;
  onDone: (created?: { product: string; token: string }) => void;
  onError: (e: unknown) => void;
}) {
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [departments, setDepartments] = useState<string[]>([]);
  const [repository, setRepository] = useState("");
  const [hosts, setHosts] = useState("github.com, .githubusercontent.com");
  const [approvals, setApprovals] = useState(1);
  const [plan, setPlan] = useState(planText(DEFAULT_PLAN));
  const [crashFree, setCrashFree] = useState("0.99");
  const [minUnits, setMinUnits] = useState(20);
  const [webhook, setWebhook] = useState("");
  const [secret, setSecret] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <form
      className="provider-edit"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          const gates: HealthGate[] = crashFree
            ? [{ name: "crash-free", metric: { kind: "crash_free" }, comparator: "gte", threshold: Number(crashFree), min_samples: minUnits }]
            : [];
          const out = await api.createProduct({
            name,
            description,
            departments,
            repository: repository || undefined,
            plan: parsePlan(plan),
            gates,
            approvals_required: approvals,
            artifact_hosts: hosts
              .split(/[,\s]+/)
              .map((h) => h.trim())
              .filter(Boolean),
            webhook_url: webhook || undefined,
            webhook_secret: secret || undefined,
          });
          onDone({ product: out.product.name, token: out.client_token });
        } catch (err) {
          onError(err);
        } finally {
          setBusy(false);
        }
      }}
    >
      <div className="grid-2">
        <label>
          Name
          <input value={name} onChange={(e) => setName(e.target.value)} pattern="[a-z0-9][a-z0-9_-]*" required placeholder="desktop-app" />
        </label>
        <label>
          Repository (owner/name)
          <input value={repository} onChange={(e) => setRepository(e.target.value)} placeholder="acme/app" />
        </label>
      </div>
      <label>
        Description
        <input value={description} onChange={(e) => setDescription(e.target.value)} />
      </label>
      <fieldset className="card">
        <legend className="small">Departments whose agents may propose releases</legend>
        {org.departments.map((d) => (
          <label key={d.id} className="small">
            <input
              type="checkbox"
              checked={departments.includes(d.id)}
              onChange={(e) => setDepartments(e.target.checked ? [...departments, d.id] : departments.filter((x) => x !== d.id))}
            />{" "}
            {d.name}
          </label>
        ))}
      </fieldset>
      <label>
        Rollout plan (one stage per line: name, percent of units, soak minutes; the last reaches 100)
        <textarea rows={4} className="mono" value={plan} onChange={(e) => setPlan(e.target.value)} />
      </label>
      <div className="grid-2">
        <label>
          Crash-free gate (rate, empty for none)
          <input value={crashFree} onChange={(e) => setCrashFree(e.target.value)} placeholder="0.99" />
        </label>
        <label>
          …once this many units reported
          <input type="number" min={1} value={minUnits} onChange={(e) => setMinUnits(Number(e.target.value))} />
        </label>
        <label>
          Approvals per release
          <input type="number" min={1} max={5} value={approvals} onChange={(e) => setApprovals(Number(e.target.value))} />
        </label>
        <label>
          Artifact hosts (downloads are only allowed from these)
          <input value={hosts} onChange={(e) => setHosts(e.target.value)} />
        </label>
        <label>
          Webhook URL (optional: your deploy system)
          <input value={webhook} onChange={(e) => setWebhook(e.target.value)} placeholder="https://deploy.example.com/hooks/agentcore" />
        </label>
        <label>
          Webhook signing key
          <input type="password" value={secret} onChange={(e) => setSecret(e.target.value)} autoComplete="new-password" />
        </label>
      </div>
      <p className="muted small">
        Gates on your own numbers (error rates, latency, revenue) can be added through the API with a business data source.
      </p>
      <div className="row">
        <button className="btn primary" disabled={busy}>
          Add product
        </button>
        <button type="button" className="btn ghost" onClick={() => onDone()}>
          Cancel
        </button>
      </div>
    </form>
  );
}
