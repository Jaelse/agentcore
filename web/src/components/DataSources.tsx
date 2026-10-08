import { useState } from "react";
import { api } from "../api";
import type { DataSource, DataSourceKind, Me, OrgOverview } from "../types";

const KINDS: { id: DataSourceKind; label: string; help: string }[] = [
  { id: "table", label: "Table (CSV)", help: "Upload a spreadsheet exported as CSV: customers, revenue per month, tickets…" },
  { id: "postgres", label: "PostgreSQL database", help: "Agents run read-only SELECT queries. Use a database user that can only read." },
  { id: "http", label: "HTTP API", help: "Agents GET paths below a base URL, e.g. analytics or billing APIs, with your API key." },
];

/** Read-only business data and which departments may use it. */
export function DataSources({
  org,
  me,
  act,
  onError,
}: {
  org: OrgOverview;
  me: Me;
  act: (fn: () => Promise<unknown>) => Promise<void>;
  onError: (e: unknown) => void;
}) {
  const [adding, setAdding] = useState(false);
  const isAdmin = me.role === "admin";
  return (
    <div className="session">
      <section className="card session-header">
        <div className="session-header-main">
          <h2>Business data</h2>
          <p className="muted">
            Data your departments may read to make decisions: sales, revenue, usage, support tickets. Access is read-only,
            every query is recorded, and only the departments you tick can use a source. Secrets are encrypted and never
            shown to agents.
          </p>
        </div>
        {isAdmin && !adding && (
          <div className="session-actions">
            <button className="btn primary" onClick={() => setAdding(true)}>
              + Add data
            </button>
          </div>
        )}
      </section>
      {adding && (
        <SourceForm
          org={org}
          onDone={() => {
            setAdding(false);
            void act(async () => {});
          }}
          onError={onError}
        />
      )}
      {org.data_sources.length === 0 && !adding && (
        <div className="empty">
          <p>No business data yet. {isAdmin ? "Add a table, a database or an API." : "An admin can add some."}</p>
        </div>
      )}
      {org.data_sources.map((s) => (
        <SourceCard key={s.id} s={s} org={org} isAdmin={isAdmin} act={act} onError={onError} />
      ))}
    </div>
  );
}

function DepartmentPicker({ org, value, onChange }: { org: OrgOverview; value: string[]; onChange: (v: string[]) => void }) {
  return (
    <div className="check-grid">
      {org.departments.map((d) => (
        <label key={d.id} className="inline-check">
          <input
            type="checkbox"
            checked={value.includes(d.id)}
            onChange={(e) => onChange(e.target.checked ? [...value, d.id] : value.filter((x) => x !== d.id))}
          />{" "}
          {d.name}
        </label>
      ))}
    </div>
  );
}

function SourceCard({
  s,
  org,
  isAdmin,
  act,
  onError,
}: {
  s: DataSource;
  org: OrgOverview;
  isAdmin: boolean;
  act: (fn: () => Promise<unknown>) => Promise<void>;
  onError: (e: unknown) => void;
}) {
  const [test, setTest] = useState<{ ok: boolean; text: string } | null>(null);
  const [secret, setSecret] = useState("");
  const [query, setQuery] = useState("");
  const kind = KINDS.find((k) => k.id === s.kind);
  const run = async () => {
    try {
      const args = !query.trim() ? undefined : s.kind === "postgres" ? { sql: query } : s.kind === "http" ? { path: query } : { contains: query };
      const r = await api.testDataSource(s.id, args);
      setTest({ ok: r.ok, text: r.ok ? JSON.stringify(r.result, null, 2).slice(0, 4000) : (r.error ?? "failed") });
    } catch (err) {
      onError(err);
    }
  };
  return (
    <section className="card side-panel">
      <div className="session-item-top">
        <strong className="mono">{s.name}</strong>
        <span className={`pill ${s.enabled ? "status-completed" : ""}`}>{s.enabled ? "on" : "off"}</span>
      </div>
      <div className="muted small">
        {kind?.label}
        {s.kind === "table" && s.rows !== null && ` · ${s.rows} rows`}
        {s.kind === "http" && ` · ${String(s.config.base_url ?? "")}`}
        {s.secret_hint && ` · secret ${s.secret_hint}`} · changed by {s.updated_by}
      </div>
      {s.description && <p className="small">{s.description}</p>}
      <div className="small">
        <strong>Departments that may read it:</strong>{" "}
        {isAdmin ? (
          <DepartmentPicker org={org} value={s.departments} onChange={(departments) => act(() => api.updateDataSource(s.id, { departments }))} />
        ) : (
          s.departments.map((id) => org.departments.find((d) => d.id === id)?.name).filter(Boolean).join(", ") || "none"
        )}
      </div>
      {isAdmin && (
        <>
          <div className="row">
            <input
              className="grow"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder={s.kind === "postgres" ? "SELECT … (optional)" : s.kind === "http" ? "/path?query (optional)" : "search text (optional)"}
            />
            <button className="btn small-btn" onClick={run}>
              Try it
            </button>
          </div>
          {test && <pre className={`mono-pre small ${test.ok ? "" : "danger-text"}`}>{test.text}</pre>}
          <div className="row">
            {s.kind !== "table" && (
              <>
                <input
                  type="password"
                  value={secret}
                  onChange={(e) => setSecret(e.target.value)}
                  placeholder={s.kind === "postgres" ? "new connection string" : "new API key header value"}
                  autoComplete="off"
                />
                <button
                  className="btn small-btn"
                  disabled={!secret}
                  onClick={() => act(() => api.updateDataSource(s.id, { secret })).then(() => setSecret(""))}
                >
                  Replace secret
                </button>
              </>
            )}
            {s.kind === "table" && (
              <label className="btn small-btn">
                Replace CSV…
                <input
                  type="file"
                  accept=".csv,text/csv"
                  hidden
                  onChange={async (e) => {
                    const file = e.target.files?.[0];
                    if (file) await act(async () => api.updateDataSource(s.id, { content: await file.text() }));
                  }}
                />
              </label>
            )}
            <button className="btn small-btn" onClick={() => act(() => api.updateDataSource(s.id, { enabled: !s.enabled }))}>
              Turn {s.enabled ? "off" : "on"}
            </button>
            <button
              className="link small danger-text"
              onClick={() => {
                if (confirm(`Delete the data source ${s.name}?`)) void act(() => api.deleteDataSource(s.id));
              }}
            >
              delete
            </button>
          </div>
        </>
      )}
    </section>
  );
}

function SourceForm({ org, onDone, onError }: { org: OrgOverview; onDone: () => void; onError: (e: unknown) => void }) {
  const [kind, setKind] = useState<DataSourceKind>("table");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [secret, setSecret] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [header, setHeader] = useState("Authorization");
  const [paths, setPaths] = useState("");
  const [testPath, setTestPath] = useState("");
  const [content, setContent] = useState("");
  const [fileName, setFileName] = useState("");
  const [maxRows, setMaxRows] = useState("200");
  const [departments, setDepartments] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  return (
    <form
      className="card pad provider-edit"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          const config: Record<string, unknown> = { max_rows: Number(maxRows) || 200 };
          if (kind === "http") {
            config.base_url = baseUrl.trim();
            config.header = header.trim() || "Authorization";
            const prefixes = paths
              .split(",")
              .map((p) => p.trim())
              .filter(Boolean);
            if (prefixes.length) config.paths = prefixes;
            if (testPath.trim()) config.test_path = testPath.trim();
          }
          await api.createDataSource({
            name: name.trim(),
            kind,
            description,
            config,
            secret: secret || undefined,
            content: kind === "table" ? content : undefined,
            departments,
          });
          onDone();
        } catch (err) {
          onError(err);
        } finally {
          setBusy(false);
        }
      }}
    >
      <div className="row">
        {KINDS.map((k) => (
          <button key={k.id} type="button" className={`btn small-btn ${kind === k.id ? "primary" : "ghost"}`} onClick={() => setKind(k.id)}>
            {k.label}
          </button>
        ))}
      </div>
      <p className="muted small">{KINDS.find((k) => k.id === kind)?.help}</p>
      <div className="grid-2">
        <label>
          Name (how agents refer to it)
          <input value={name} onChange={(e) => setName(e.target.value.toLowerCase())} placeholder="revenue" pattern="[a-z0-9][a-z0-9_\-]{0,62}" required />
        </label>
        <label>
          Most rows per answer
          <input type="number" min={1} max={1000} value={maxRows} onChange={(e) => setMaxRows(e.target.value)} />
        </label>
      </div>
      <label>
        What it contains (agents read this)
        <textarea rows={2} value={description} onChange={(e) => setDescription(e.target.value)} placeholder="Monthly recurring revenue per customer since 2024" />
      </label>
      {kind === "table" && (
        <label>
          CSV file (first row: column names)
          <input
            type="file"
            accept=".csv,text/csv"
            onChange={async (e) => {
              const file = e.target.files?.[0];
              if (file) {
                setContent(await file.text());
                setFileName(file.name);
                if (!name) setName(file.name.replace(/\.csv$/i, "").toLowerCase().replace(/[^a-z0-9_-]+/g, "-"));
              }
            }}
            required
          />
          {fileName && <span className="muted small">{fileName}: {content.split("\n").filter(Boolean).length - 1} rows</span>}
        </label>
      )}
      {kind === "postgres" && (
        <label>
          Connection string (a read-only user)
          <input type="password" value={secret} onChange={(e) => setSecret(e.target.value)} placeholder="postgres://reader:password@db.example.com/sales" autoComplete="off" required />
        </label>
      )}
      {kind === "http" && (
        <>
          <label>
            Base URL
            <input value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} placeholder="https://api.example.com" required />
          </label>
          <div className="grid-2">
            <label>
              Header for the key
              <input value={header} onChange={(e) => setHeader(e.target.value)} />
            </label>
            <label>
              Header value (secret)
              <input type="password" value={secret} onChange={(e) => setSecret(e.target.value)} placeholder="Bearer sk_live_…" autoComplete="off" />
            </label>
          </div>
          <div className="grid-2">
            <label>
              Allowed paths (optional, comma-separated prefixes)
              <input value={paths} onChange={(e) => setPaths(e.target.value)} placeholder="/v1/charges, /v1/customers" />
            </label>
            <label>
              Path for “Try it”
              <input value={testPath} onChange={(e) => setTestPath(e.target.value)} placeholder="/v1/charges?limit=1" />
            </label>
          </div>
        </>
      )}
      <div>
        <span className="small">Departments that may read it</span>
        <DepartmentPicker org={org} value={departments} onChange={setDepartments} />
      </div>
      <div className="row">
        <button className="btn primary" disabled={busy || !name}>
          Add
        </button>
        <button type="button" className="btn ghost" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}
