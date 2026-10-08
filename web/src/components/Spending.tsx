import { useEffect, useState } from "react";
import { api } from "../api";
import type { BudgetAction, BudgetPeriod, BudgetStatus, Me, OrgMetrics, OrgOverview, ProviderInfo, Spending as SpendingData } from "../types";
import { DailyColumns, money } from "./Dashboard";

const PERIODS: BudgetPeriod[] = ["day", "week", "month"];

function budgetState(b: BudgetStatus) {
  const used = b.spent_micros / b.limit_micros;
  if (used >= 1)
    return { cls: "sev-critical", fill: "meter-critical", icon: "▲", label: b.action === "pause" ? "used up · work paused" : "used up" };
  if (used * 100 >= b.warn_percent) return { cls: "sev-warning", fill: "meter-warning", icon: "●", label: "nearly used" };
  return { cls: "sev-good", fill: "", icon: "✓", label: "within budget" };
}

/** What the organisation spends on models, prices and budgets. */
export function Spending({
  org,
  me,
  tick,
  onError,
}: {
  org: OrgOverview;
  me: Me;
  tick: number;
  onError: (e: unknown) => void;
}) {
  const [data, setData] = useState<SpendingData | null>(null);
  const [metrics, setMetrics] = useState<OrgMetrics | null>(null);
  const [reload, setReload] = useState(0);
  useEffect(() => {
    api.spending(30).then(setData).catch(onError);
    api.metrics(30).then(setMetrics).catch(onError);
  }, [tick, reload, onError]);
  const isAdmin = me.role === "admin";
  const act = async (fn: () => Promise<unknown>) => {
    try {
      await fn();
    } catch (err) {
      onError(err);
    } finally {
      setReload((r) => r + 1);
    }
  };
  if (!data || !metrics) return <div className="empty">Loading…</div>;
  const cur = data.currency;
  const m = metrics.metrics;
  const total = m.daily.reduce((a, d) => a + d.cost_micros, 0);
  const byDept = [...m.departments].filter((d) => d.cost_micros > 0).sort((a, b) => b.cost_micros - a.cost_micros);
  const maxDept = Math.max(...byDept.map((d) => d.cost_micros), 1);
  const deptName = (id: string | null) => (id ? (org.departments.find((d) => d.id === id)?.name ?? "deleted department") : "Whole organisation");

  return (
    <div className="session dashboard">
      <section className="card session-header">
        <div className="session-header-main">
          <h2>Spending</h2>
          <p className="muted">
            What model calls cost, priced when each call is made. Budgets warn people or pause the work when money runs
            out; a paused budget also refuses model calls until the next period starts or the limit is raised.
          </p>
        </div>
        <div className="session-actions">
          <span className="stat-value">{money(total, cur)}</span>
          <span className="muted small">last 30 days</span>
        </div>
      </section>

      {data.prices.length === 0 && (
        <div className="callout small">
          <strong>No prices yet.</strong> Add the price per million tokens of the models you use below; until then spending
          shows as unknown and budgets cannot be enforced.
        </div>
      )}
      {data.unpriced_calls > 0 && (
        <div className="callout small">
          {data.unpriced_calls} call(s) in the last 30 days have no price.{" "}
          {isAdmin && (
            <button className="btn small-btn" onClick={() => act(api.pricePastCalls)}>
              Price them with the current prices
            </button>
          )}
        </div>
      )}

      <section className="card side-panel">
        <h3 className="section-title">Budgets</h3>
        {data.budgets.length === 0 && <p className="muted small">No budgets: nothing limits spending.</p>}
        <ul className="goal-list">
          {data.budgets.map((b) => (
            <BudgetRow key={b.id} b={b} cur={cur} name={deptName(b.department_id)} org={org} isAdmin={isAdmin} act={act} />
          ))}
        </ul>
        {isAdmin && <BudgetForm org={org} cur={cur} act={act} />}
      </section>

      <div className="room-grid">
        <DailyColumns title={`Spend per day (${cur})`} days={m.daily} value={(d) => d.cost_micros} unit="" format={(n) => money(n, cur)} />
        <section className="card side-panel">
          <h3 className="section-title">By department</h3>
          {byDept.length === 0 && <p className="muted small">Nothing spent yet.</p>}
          <ul className="hbar-list">
            {byDept.map((d) => (
              <li key={d.id}>
                <span className="hbar-name">{d.name}</span>
                <span className="hbar-track">
                  <span className="hbar" style={{ width: `${(d.cost_micros / maxDept) * 100}%` }} />
                </span>
                <span className="hbar-value">{money(d.cost_micros, cur)}</span>
              </li>
            ))}
          </ul>
        </section>
      </div>

      <Prices data={data} isAdmin={isAdmin} act={act} />
    </div>
  );
}

function BudgetRow({
  b,
  cur,
  name,
  org,
  isAdmin,
  act,
}: {
  b: BudgetStatus;
  cur: string;
  name: string;
  org: OrgOverview;
  isAdmin: boolean;
  act: (fn: () => Promise<unknown>) => Promise<void>;
}) {
  const [editing, setEditing] = useState(false);
  const [limit, setLimit] = useState(String(b.limit_micros / 1e6));
  const s = budgetState(b);
  const used = Math.min(1, b.spent_micros / b.limit_micros);
  return (
    <li className="goal">
      <div className="session-item-top">
        <strong>
          {name} · per {b.period}
        </strong>
        <span className={`sev ${s.cls}`}>
          <span aria-hidden>{s.icon}</span> {s.label}
        </span>
      </div>
      <div
        className="meter"
        role="meter"
        aria-valuemin={0}
        aria-valuemax={b.limit_micros}
        aria-valuenow={b.spent_micros}
        aria-label={`${name}: ${money(b.spent_micros, cur)} of ${money(b.limit_micros, cur)}`}
      >
        <span className={s.fill} style={{ width: `${used * 100}%` }} />
        <i style={{ left: `${b.warn_percent}%` }} title={`warning at ${b.warn_percent}%`} />
      </div>
      <div className="muted small">
        {money(b.spent_micros, cur)} of {money(b.limit_micros, cur)} · {b.action === "pause" ? "pauses the work" : "warns only"} at the
        limit · warns at {b.warn_percent}% · this {b.period} ends {new Date(b.period_end).toLocaleString()}
        {b.paused_departments.length > 0 &&
          ` · paused ${b.paused_departments.map((id) => org.departments.find((d) => d.id === id)?.name ?? "?").join(", ")} until then`}
      </div>
      {isAdmin &&
        (editing ? (
          <form
            className="row"
            onSubmit={(e) => {
              e.preventDefault();
              void act(() => api.updateBudget(b.id, { limit_micros: Math.round(Number(limit) * 1e6) })).then(() => setEditing(false));
            }}
          >
            <input type="number" min={0.01} step={0.01} value={limit} onChange={(e) => setLimit(e.target.value)} /> {cur}
            <button className="btn small-btn primary">Save</button>
            <button type="button" className="btn small-btn ghost" onClick={() => setEditing(false)}>
              Cancel
            </button>
          </form>
        ) : (
          <div className="row">
            <button className="link small" onClick={() => setEditing(true)}>
              change limit
            </button>
            <button
              className="link small"
              onClick={() => act(() => api.updateBudget(b.id, { action: b.action === "pause" ? "warn" : "pause" }))}
            >
              {b.action === "pause" ? "only warn" : "pause when used up"}
            </button>
            <button
              className="link small danger-text"
              onClick={() => {
                if (confirm("Delete this budget? Departments it paused are resumed.")) void act(() => api.deleteBudget(b.id));
              }}
            >
              delete
            </button>
          </div>
        ))}
    </li>
  );
}

function BudgetForm({ org, cur, act }: { org: OrgOverview; cur: string; act: (fn: () => Promise<unknown>) => Promise<void> }) {
  const [open, setOpen] = useState(false);
  const [scope, setScope] = useState("");
  const [period, setPeriod] = useState<BudgetPeriod>("month");
  const [limit, setLimit] = useState("");
  const [action, setAction] = useState<BudgetAction>("pause");
  const [warn, setWarn] = useState("80");
  if (!open)
    return (
      <div className="row">
        <button className="btn small-btn" onClick={() => setOpen(true)}>
          + Budget
        </button>
      </div>
    );
  return (
    <form
      className="provider-edit"
      onSubmit={(e) => {
        e.preventDefault();
        void act(() =>
          api.createBudget({
            department_id: scope || null,
            period,
            limit_micros: Math.round(Number(limit) * 1e6),
            action,
            warn_percent: Number(warn) || 80,
          }),
        ).then(() => setOpen(false));
      }}
    >
      <div className="grid-2">
        <label>
          For
          <select value={scope} onChange={(e) => setScope(e.target.value)}>
            <option value="">The whole organisation</option>
            {org.departments.map((d) => (
              <option key={d.id} value={d.id}>
                {d.name}
              </option>
            ))}
          </select>
        </label>
        <label>
          Per
          <select value={period} onChange={(e) => setPeriod(e.target.value as BudgetPeriod)}>
            {PERIODS.map((p) => (
              <option key={p} value={p}>
                {p} (UTC)
              </option>
            ))}
          </select>
        </label>
        <label>
          Limit ({cur})
          <input type="number" min={0.01} step={0.01} value={limit} onChange={(e) => setLimit(e.target.value)} required placeholder="50" />
        </label>
        <label>
          Warn at (% of the limit)
          <input type="number" min={1} max={100} value={warn} onChange={(e) => setWarn(e.target.value)} />
        </label>
      </div>
      <label className="inline-check">
        <input type="radio" checked={action === "pause"} onChange={() => setAction("pause")} /> Pause the work and refuse model calls
        when it is used up (resumes next period)
      </label>
      <label className="inline-check">
        <input type="radio" checked={action === "warn"} onChange={() => setAction("warn")} /> Only warn
      </label>
      <p className="muted small">Setting a budget for the same scope and period again replaces it.</p>
      <div className="row">
        <button className="btn primary" disabled={!(Number(limit) > 0)}>
          Save budget
        </button>
        <button type="button" className="btn ghost" onClick={() => setOpen(false)}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function Prices({ data, isAdmin, act }: { data: SpendingData; isAdmin: boolean; act: (fn: () => Promise<unknown>) => Promise<void> }) {
  const [providers, setProviders] = useState<ProviderInfo[]>([]);
  const [provider, setProvider] = useState("");
  const [model, setModel] = useState("");
  const [input, setInput] = useState("");
  const [output, setOutput] = useState("");
  const [currency, setCurrency] = useState(data.currency);
  useEffect(() => {
    if (isAdmin)
      api
        .providers()
        .then((p) => {
          setProviders(p);
          setProvider((cur) => cur || p[0]?.name || "");
        })
        .catch(() => setProviders([]));
  }, [isAdmin]);
  return (
    <section className="card side-panel">
      <h3 className="section-title">Prices per million tokens ({data.currency})</h3>
      <p className="muted small">
        Use your provider's price list. A model can be an exact name, a prefix ending in <code>*</code> (
        <code>claude-sonnet-*</code>) or <code>*</code> for every model of the provider; the most specific price wins.
        Calls are priced when they are made, so a new price does not change past costs.
      </p>
      <div className="table-scroll">
        <table className="metrics-table">
          <thead>
            <tr>
              <th>Provider</th>
              <th>Model</th>
              <th>Input</th>
              <th>Output</th>
              <th>Changed</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {data.prices.map((p) => (
              <tr key={`${p.provider}/${p.model}`}>
                <td>{p.provider}</td>
                <td className="mono">{p.model}</td>
                <td>{p.input_per_mtok}</td>
                <td>{p.output_per_mtok}</td>
                <td className="muted">
                  {p.updated_by}, {new Date(p.updated_at).toLocaleDateString()}
                </td>
                <td>
                  {isAdmin && (
                    <button className="link small danger-text" onClick={() => act(() => api.deletePrice(p.provider, p.model))}>
                      delete
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {isAdmin && (
        <>
          <form
            className="price-form"
            onSubmit={(e) => {
              e.preventDefault();
              void act(() =>
                api.setPrice({ provider, model: model.trim(), input_per_mtok: Number(input), output_per_mtok: Number(output) }),
              ).then(() => {
                setModel("");
                setInput("");
                setOutput("");
              });
            }}
          >
            <select value={provider} onChange={(e) => setProvider(e.target.value)} required>
              {providers.map((p) => (
                <option key={p.name} value={p.name}>
                  {p.name}
                </option>
              ))}
            </select>
            <input value={model} onChange={(e) => setModel(e.target.value)} placeholder="model or prefix-*" required />
            <input type="number" min={0} step="any" value={input} onChange={(e) => setInput(e.target.value)} placeholder="input" required />
            <input type="number" min={0} step="any" value={output} onChange={(e) => setOutput(e.target.value)} placeholder="output" required />
            <button className="btn small-btn primary" disabled={!provider}>
              Set price
            </button>
          </form>
          <form
            className="price-form currency-form"
            onSubmit={(e) => {
              e.preventDefault();
              void act(() => api.setCurrency(currency));
            }}
          >
            <label className="small">
              Currency{" "}
              <input value={currency} onChange={(e) => setCurrency(e.target.value.toUpperCase())} maxLength={3} size={4} />
            </label>
            <button className="btn small-btn" disabled={currency === data.currency}>
              Save
            </button>
          </form>
        </>
      )}
    </section>
  );
}
