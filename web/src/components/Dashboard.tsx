import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { api } from "../api";
import type { DayMetrics, Me, OrgMetrics, OrgOverview, Signal } from "../types";

/** Compact numbers: 1,284 / 12.9K / 4.2M. */
export function compact(n: number) {
  if (Math.abs(n) >= 1e6) return `${(n / 1e6).toFixed(n >= 1e7 ? 0 : 1)}M`;
  if (Math.abs(n) >= 1e4) return `${(n / 1e3).toFixed(n >= 1e5 ? 0 : 1)}K`;
  return Math.round(n).toLocaleString();
}

export function duration(secs: number | null) {
  if (secs === null || Number.isNaN(secs)) return "–";
  if (secs < 60) return `${Math.round(secs)} s`;
  if (secs < 3600) return `${Math.round(secs / 60)} min`;
  if (secs < 86400) return `${(secs / 3600).toFixed(1)} h`;
  return `${(secs / 86400).toFixed(1)} d`;
}

function ago(at: string | null) {
  if (!at) return "never";
  return `${duration((Date.now() - new Date(at).getTime()) / 1000)} ago`;
}

function dayLabel(day: string) {
  return new Date(day).toLocaleDateString(undefined, { weekday: "short", day: "numeric", month: "short" });
}

/** A clean top for the y axis: 1, 2, 5 × 10^n. */
function niceMax(v: number) {
  if (v <= 0) return 1;
  const p = 10 ** Math.floor(Math.log10(v));
  for (const m of [1, 2, 5, 10]) if (m * p >= v) return m * p;
  return 10 * p;
}

function useWidth<T extends HTMLElement>(): [React.RefObject<T | null>, number] {
  const ref = useRef<T | null>(null);
  const [width, setWidth] = useState(300);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const observer = new ResizeObserver(([entry]) => setWidth(Math.max(120, entry.contentRect.width)));
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  return [ref, width];
}

/** Bar with a 4px rounded data end, square at the baseline. */
function columnPath(x: number, y: number, w: number, h: number) {
  const r = Math.min(4, w / 2, h);
  return `M${x},${y + h} V${y + r} Q${x},${y} ${x + r},${y} H${x + w - r} Q${x + w},${y} ${x + w},${y + r} V${y + h} Z`;
}

/** One measure per day: columns from one baseline, hover for the value. */
function DailyColumns({ title, days, value, unit }: { title: string; days: DayMetrics[]; value: (d: DayMetrics) => number; unit: string }) {
  const [ref, width] = useWidth<HTMLDivElement>();
  const [hover, setHover] = useState<number | null>(null);
  const height = 120;
  const top = 14;
  const bottom = 18;
  const values = days.map(value);
  const total = values.reduce((a, b) => a + b, 0);
  const max = niceMax(Math.max(...values, 0));
  const band = width / Math.max(days.length, 1);
  const barW = Math.max(2, Math.min(24, band - 2));
  const plotH = height - top - bottom;
  const y = (v: number) => top + plotH - (v / max) * plotH;
  const tick = (i: number) => days.length <= 7 || i === 0 || i === days.length - 1 || i % Math.ceil(days.length / 6) === 0;
  return (
    <figure className="viz-card" ref={ref}>
      <figcaption>
        <span>{title}</span>
        <strong>{compact(total)}</strong>
      </figcaption>
      <svg width={width} height={height} role="img" aria-label={`${title}: ${compact(total)} in ${days.length} days`}>
        <line className="viz-grid" x1={0} x2={width} y1={top} y2={top} />
        <text className="viz-axis" x={0} y={top - 4}>
          {compact(max)}
        </text>
        <line className="viz-baseline" x1={0} x2={width} y1={top + plotH} y2={top + plotH} />
        {days.map((d, i) => {
          const v = values[i];
          const x = i * band + (band - barW) / 2;
          return (
            <g key={d.day} onMouseEnter={() => setHover(i)} onMouseLeave={() => setHover(null)}>
              <rect x={i * band} y={0} width={band} height={height} fill="transparent" />
              {v > 0 && (
                <path
                  className={`viz-bar ${hover !== null && hover !== i ? "dim" : ""}`}
                  d={columnPath(x, y(v), barW, Math.max(1, top + plotH - y(v)))}
                />
              )}
              {tick(i) && (
                <text className="viz-axis" x={i * band + band / 2} y={height - 4} textAnchor="middle">
                  {new Date(d.day).getDate()}
                </text>
              )}
            </g>
          );
        })}
      </svg>
      {hover !== null && (
        <div className="viz-tooltip" style={{ left: Math.min(Math.max(hover * band + band / 2, 60), width - 60) }}>
          <span className="muted">{dayLabel(days[hover].day)}</span>
          <strong>
            {values[hover].toLocaleString()} {unit}
          </strong>
        </div>
      )}
    </figure>
  );
}

/** A small trend line for a stat tile (de-emphasised, last day accented). */
function Sparkline({ values }: { values: number[] }) {
  const w = 96;
  const h = 24;
  if (values.length < 2) return null;
  const max = Math.max(...values, 1);
  const pts = values.map((v, i) => [(i / (values.length - 1)) * (w - 6) + 3, h - 3 - (v / max) * (h - 6)]);
  const last = pts[pts.length - 1];
  return (
    <svg width={w} height={h} className="sparkline" aria-hidden>
      <polyline points={pts.map((p) => p.join(",")).join(" ")} />
      <circle cx={last[0]} cy={last[1]} r={3} />
    </svg>
  );
}

function Tile({ label, value, note, trend }: { label: string; value: string; note?: string; trend?: number[] }) {
  return (
    <div className="stat-tile">
      <span className="stat-label">{label}</span>
      <span className="stat-value">{value}</span>
      {note && <span className="muted small">{note}</span>}
      {trend && <Sparkline values={trend} />}
    </div>
  );
}

const SEVERITY: Record<Signal["severity"], { icon: string; label: string; cls: string }> = {
  high: { icon: "▲", label: "High", cls: "sev-critical" },
  medium: { icon: "●", label: "Medium", cls: "sev-warning" },
  low: { icon: "○", label: "Low", cls: "sev-good" },
};

/** How the organisation is doing: signals, figures, trends, departments, goals. */
export function Dashboard({
  org,
  me,
  tick,
  act,
  onOpenDepartment,
  onOpenImprovements,
  onError,
}: {
  org: OrgOverview;
  me: Me;
  tick: number;
  act: (fn: () => Promise<unknown>) => Promise<void>;
  onOpenDepartment: (id: string) => void;
  onOpenImprovements: () => void;
  onError: (e: unknown) => void;
}) {
  const [days, setDays] = useState(14);
  const [data, setData] = useState<OrgMetrics | null>(null);
  useEffect(() => {
    api.metrics(days).then(setData).catch(onError);
  }, [days, tick, onError]);

  const retro = org.departments.find((d) => d.tools.includes("insights"));
  const retroAgent = retro?.agents.find((a) => a.kind === "worker");
  const canOperate = me.role !== "viewer";

  if (!data) return <div className="empty">Loading the metrics…</div>;
  const m = data.metrics;
  const sum = (f: (d: DayMetrics) => number) => m.daily.reduce((a, d) => a + f(d), 0);
  const depts = m.departments;
  const working = depts.reduce((a, d) => a + d.active_agents, 0);
  const asleep = depts.reduce((a, d) => a + d.asleep_agents, 0);
  const hours = depts.reduce((a, d) => a + d.agent_hours, 0);
  const received = depts.reduce((a, d) => a + d.messages_received, 0);
  const waits = depts.filter((d) => d.avg_wait_secs !== null && d.messages_received > 0);
  const avgWait = received > 0 ? waits.reduce((a, d) => a + (d.avg_wait_secs ?? 0) * d.messages_received, 0) / received : null;
  const activeGoals = m.goals.filter((g) => g.status === "active");
  const moving = activeGoals.filter((g) => g.progress_reports > 0).length;
  const maxTokens = Math.max(...depts.map((d) => d.tokens), 1);

  return (
    <div className="session dashboard">
      <section className="card session-header">
        <div className="session-header-main">
          <h2>How the organisation is doing</h2>
          <p className="muted">Last {m.days} days. Everything here is measured from what the agents did; nothing is estimated.</p>
        </div>
        <div className="session-actions segmented" role="group" aria-label="Time range">
          {[7, 14, 30].map((d) => (
            <button key={d} className={`btn small-btn ${days === d ? "primary" : "ghost"}`} onClick={() => setDays(d)}>
              {d} days
            </button>
          ))}
        </div>
      </section>

      <section className="card side-panel">
        <div className="row" style={{ justifyContent: "space-between" }}>
          <h3 className="section-title">What looks inefficient</h3>
          {m.proposals.open + m.proposals.changes_requested > 0 && (
            <button className="link small" onClick={onOpenImprovements}>
              {m.proposals.open + m.proposals.changes_requested} improvement(s) waiting for you →
            </button>
          )}
        </div>
        {data.signals.length === 0 ? (
          <p className="muted small">✓ Nothing stands out in the last {m.days} days.</p>
        ) : (
          <ul className="signal-list">
            {data.signals.map((s, i) => (
              <li key={i} className="signal">
                <span className={`sev ${SEVERITY[s.severity].cls}`}>
                  <span aria-hidden>{SEVERITY[s.severity].icon}</span> {SEVERITY[s.severity].label}
                </span>
                <div>
                  <strong>{s.title}</strong>
                  <div className="muted small">{s.detail}</div>
                  <div className="small">💡 {s.suggestion}</div>
                </div>
                {canOperate && retroAgent && (
                  <button
                    className="btn small-btn"
                    title={`Ask ${retroAgent.name} in ${retro?.name} to propose a fix`}
                    onClick={() =>
                      act(() =>
                        api.postMessage({ agent: retroAgent.id }, `Please look into this and propose a fix: ${s.title}. ${s.detail}`),
                      )
                    }
                  >
                    Ask for a fix
                  </button>
                )}
              </li>
            ))}
          </ul>
        )}
        {!retro && (
          <RetroInvite isAdmin={me.role === "admin"} act={act} />
        )}
      </section>

      <div className="stat-row">
        <Tile label="Agents working" value={String(working)} note={`${asleep} asleep`} />
        <Tile label="Agent-hours" value={hours.toFixed(1)} note="sessions running" />
        <Tile label="Tokens" value={compact(sum((d) => d.tokens))} note={`${compact(sum((d) => d.model_calls))} model calls`} trend={m.daily.map((d) => d.tokens)} />
        <Tile label="Messages" value={compact(sum((d) => d.messages))} note={`${compact(sum((d) => d.inter_department))} between departments`} trend={m.daily.map((d) => d.messages)} />
        <Tile label="Wait for a reply" value={duration(avgWait)} note="average, message to recipient" />
        <Tile label="Goals moving" value={`${moving}/${activeGoals.length}`} note="with progress reports" />
        <Tile
          label="Improvements"
          value={String(m.proposals.applied)}
          note={`applied · ${m.proposals.open + m.proposals.changes_requested} waiting`}
        />
      </div>

      <div className="viz-grid-3">
        <DailyColumns title="Sessions" days={m.daily} value={(d) => d.sessions} unit="sessions" />
        <DailyColumns title="Messages" days={m.daily} value={(d) => d.messages} unit="messages" />
        <DailyColumns title="Tokens" days={m.daily} value={(d) => d.tokens} unit="tokens" />
        <DailyColumns title="Model calls" days={m.daily} value={(d) => d.model_calls} unit="calls" />
        <DailyColumns title="Failed sessions" days={m.daily} value={(d) => d.failed_sessions} unit="failed" />
        <DailyColumns title="Denied actions" days={m.daily} value={(d) => d.denied} unit="denied" />
      </div>

      <section className="card side-panel">
        <h3 className="section-title">Departments</h3>
        <div className="table-scroll">
          <table className="metrics-table">
            <thead>
              <tr>
                <th>Department</th>
                <th title="working / asleep / workers">Agents</th>
                <th>Sessions</th>
                <th>Agent-hours</th>
                <th>Tokens</th>
                <th title="sent / received">Messages</th>
                <th title="average time until the recipient got it">Wait</th>
                <th>Failed</th>
                <th>Denied</th>
                <th title="average time an approval waited">Approvals</th>
                <th title="progress reports on goals">Progress</th>
                <th title="business data queries">Data</th>
                <th>Last active</th>
              </tr>
            </thead>
            <tbody>
              {depts.map((d) => (
                <tr key={d.id}>
                  <td>
                    <button className="link" onClick={() => onOpenDepartment(d.id)}>
                      {d.name}
                    </button>
                    {d.state === "paused" && <span className="muted small"> (paused)</span>}
                  </td>
                  <td>
                    {d.active_agents} / {d.asleep_agents} / {d.workers}
                  </td>
                  <td>{d.sessions}</td>
                  <td>{d.agent_hours.toFixed(1)}</td>
                  <td>
                    <div className="inline-bar" title={`${d.tokens.toLocaleString()} tokens`}>
                      <span style={{ width: `${(d.tokens / maxTokens) * 100}%` }} />
                      <em>{compact(d.tokens)}</em>
                    </div>
                  </td>
                  <td>
                    {d.messages_sent} / {d.messages_received}
                  </td>
                  <td>{duration(d.avg_wait_secs)}</td>
                  <td>{d.failed_sessions || "–"}</td>
                  <td>{d.denied || "–"}</td>
                  <td>{d.approvals ? `${d.approvals} · ${duration(d.avg_approval_wait_secs)}` : "–"}</td>
                  <td>{d.progress_reports || "–"}</td>
                  <td>{d.data_queries || "–"}</td>
                  <td className="muted">{ago(d.last_activity)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </section>

      <div className="room-grid">
        <section className="card side-panel">
          <h3 className="section-title">Agents by tokens</h3>
          <div className="table-scroll">
            <table className="metrics-table">
              <thead>
                <tr>
                  <th>Agent</th>
                  <th>Status</th>
                  <th>Sessions</th>
                  <th>Hours</th>
                  <th>Tokens</th>
                  <th>Sent</th>
                  <th>Last active</th>
                </tr>
              </thead>
              <tbody>
                {[...m.agents]
                  .sort((a, b) => b.tokens - a.tokens)
                  .slice(0, 15)
                  .map((a) => (
                    <tr key={a.id}>
                      <td>
                        {a.kind === "communicator" ? "📡 " : ""}
                        {a.name} <span className="muted small">{depts.find((d) => d.id === a.department_id)?.name}</span>
                      </td>
                      <td>{a.desired === "stopped" ? "stopped" : (a.status ?? "starting").replace(/_/g, " ")}</td>
                      <td>
                        {a.sessions}
                        {a.failed_sessions > 0 && <span className="sev sev-critical"> ▲ {a.failed_sessions} failed</span>}
                      </td>
                      <td>{a.agent_hours.toFixed(1)}</td>
                      <td>{compact(a.tokens)}</td>
                      <td>{a.messages_sent}</td>
                      <td className="muted">{ago(a.last_active)}</td>
                    </tr>
                  ))}
              </tbody>
            </table>
          </div>
        </section>
        <section className="card side-panel">
          <h3 className="section-title">Goals</h3>
          {activeGoals.length === 0 && <p className="muted small">No active goals.</p>}
          <ul className="goal-list">
            {activeGoals.map((g) => {
              const since = (Date.now() - new Date(g.progress_at ?? g.created_at).getTime()) / 86400000;
              const state =
                since < 7
                  ? { cls: "sev-good", icon: "✓", label: "on track" }
                  : since < 14
                    ? { cls: "sev-warning", icon: "●", label: "slowing" }
                    : { cls: "sev-critical", icon: "▲", label: "stalled" };
              return (
                <li key={g.id} className="goal">
                  <div className="session-item-top">
                    <strong>{g.title}</strong>
                    <span className={`sev ${state.cls}`}>
                      <span aria-hidden>{state.icon}</span> {state.label}
                    </span>
                  </div>
                  <span className="muted small">
                    last progress {g.progress_at ? ago(g.progress_at) : "never"} · {g.progress_reports} report(s) in {m.days} days
                  </span>
                </li>
              );
            })}
          </ul>
        </section>
      </div>
    </div>
  );
}

function RetroInvite({ isAdmin, act }: { isAdmin: boolean; act: (fn: () => Promise<unknown>) => Promise<void> }) {
  return (
    <div className="callout small">
      <strong>No one is looking for improvements yet.</strong> A Retrospective agent reads these numbers every day and
      proposes fixes you can apply, change or send back.{" "}
      {isAdmin ? (
        <button
          className="btn small-btn primary"
          onClick={() => act(() => api.build({ departments: ["retrospective"], size: "lean", check_ins: true, start: true }))}
        >
          Add a Retrospective agent
        </button>
      ) : (
        <span className="muted">Ask an admin to add one.</span>
      )}
    </div>
  );
}
