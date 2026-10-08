import { useState } from "react";
import { api } from "../api";
import type { CheckIn, Department, OrgGoal, OrgOverview } from "../types";

/** Ready-made check-in intervals. */
const EVERY: { minutes: number; label: string }[] = [
  { minutes: 60, label: "every hour" },
  { minutes: 4 * 60, label: "every 4 hours" },
  { minutes: 24 * 60, label: "every day" },
  { minutes: 7 * 24 * 60, label: "every week" },
];

/** Ready-made check-ins: a name and the message the agent receives. */
const PRESETS: { name: string; minutes: number; message: string }[] = [
  {
    name: "daily",
    minutes: 24 * 60,
    message:
      "Daily check-in. Read the goals, your notes and new messages. Decide the most valuable next step towards the goals for your department, do it (or ask a colleague to), update your notes, and report progress on any goal that moved.",
  },
  {
    name: "weekly review",
    minutes: 7 * 24 * 60,
    message:
      "Weekly review. Ask every department (through the communicator) what they did, what is blocked and what is next. Update the goals' progress, write the weekly summary, and list the decisions people need to make.",
  },
  {
    name: "triage",
    minutes: 4 * 60,
    message: "Triage. Look at new issues, messages and feedback; sort them by importance and hand them to the right colleague.",
  },
];

export function everyLabel(minutes: number) {
  const known = EVERY.find((e) => e.minutes === minutes);
  if (known) return known.label;
  if (minutes % (24 * 60) === 0) return `every ${minutes / (24 * 60)} days`;
  if (minutes % 60 === 0) return `every ${minutes / 60} hours`;
  return `every ${minutes} minutes`;
}

function when(at: string | null) {
  return at ? new Date(at).toLocaleString() : "never";
}

/** The organisation's goals: agents see them in their prompt and report
 * progress; people set, edit and close them. */
export function Goals({
  org,
  canEdit,
  act,
}: {
  org: OrgOverview;
  canEdit: boolean;
  act: (fn: () => Promise<unknown>) => Promise<void>;
}) {
  const [adding, setAdding] = useState(false);
  const [title, setTitle] = useState("");
  const [description, setDescription] = useState("");
  const [owner, setOwner] = useState("");
  const [showClosed, setShowClosed] = useState(false);
  const goals = org.goals ?? [];
  const active = goals.filter((g) => g.status === "active");
  const closed = goals.filter((g) => g.status !== "active");
  const deptName = (id: string | null) => org.departments.find((d) => d.id === id)?.name;

  const row = (g: OrgGoal) => (
    <li key={g.id} className="goal">
      <div className="session-item-top">
        <strong>{g.title}</strong>
        <span className={`pill ${g.status === "achieved" ? "status-completed" : g.status === "dropped" ? "" : "status-running"}`}>
          {g.status}
        </span>
      </div>
      <div className="muted small">
        {deptName(g.department_id) ? `owned by ${deptName(g.department_id)}` : "whole organisation"}
      </div>
      {g.description && <div className="small">{g.description}</div>}
      {g.progress ? (
        <div className="small bubble-body">
          <span className="muted">
            {g.progress_by} · {when(g.progress_at)}:
          </span>{" "}
          {g.progress}
        </div>
      ) : (
        <div className="muted small">No progress reported yet.</div>
      )}
      {canEdit && (
        <div className="row">
          {g.status === "active" ? (
            <>
              <button className="link small" onClick={() => act(() => api.updateGoal(g.id, { status: "achieved" }))}>
                mark achieved
              </button>
              <button className="link small" onClick={() => act(() => api.updateGoal(g.id, { status: "dropped" }))}>
                drop
              </button>
            </>
          ) : (
            <button className="link small" onClick={() => act(() => api.updateGoal(g.id, { status: "active" }))}>
              reopen
            </button>
          )}
          <button
            className="link small danger-text"
            onClick={() => {
              if (confirm(`Delete the goal "${g.title}"?`)) void act(() => api.deleteGoal(g.id));
            }}
          >
            delete
          </button>
        </div>
      )}
    </li>
  );

  return (
    <section className="card side-panel goals">
      <div className="row" style={{ justifyContent: "space-between" }}>
        <h3 className="section-title">Goals</h3>
        {canEdit && !adding && (
          <button className="btn small-btn" onClick={() => setAdding(true)}>
            + Goal
          </button>
        )}
      </div>
      <p className="muted small">Every agent sees the active goals and reports progress on them.</p>
      {adding && (
        <form
          className="provider-edit"
          onSubmit={async (e) => {
            e.preventDefault();
            await act(() => api.createGoal({ title: title.trim(), description, department_id: owner || null }));
            setAdding(false);
            setTitle("");
            setDescription("");
            setOwner("");
          }}
        >
          <label>
            Goal
            <input value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Reach 10 paying customers" maxLength={200} required />
          </label>
          <label>
            Details (how you measure it, by when)
            <textarea rows={2} value={description} onChange={(e) => setDescription(e.target.value)} />
          </label>
          <label>
            Owner
            <select value={owner} onChange={(e) => setOwner(e.target.value)}>
              <option value="">Whole organisation</option>
              {org.departments.map((d) => (
                <option key={d.id} value={d.id}>
                  {d.name}
                </option>
              ))}
            </select>
          </label>
          <div className="row">
            <button className="btn primary" disabled={!title.trim()}>
              Add goal
            </button>
            <button type="button" className="btn ghost" onClick={() => setAdding(false)}>
              Cancel
            </button>
          </div>
        </form>
      )}
      {active.length === 0 && !adding && <p className="muted small">No goals yet. Give the organisation something to aim for.</p>}
      <ul className="goal-list">{active.map(row)}</ul>
      {closed.length > 0 && (
        <button className="link small" onClick={() => setShowClosed((s) => !s)}>
          {showClosed ? "hide" : "show"} {closed.length} closed
        </button>
      )}
      {showClosed && <ul className="goal-list">{closed.map(row)}</ul>}
    </section>
  );
}

/** A department's check-ins: messages sent on a schedule that wake its
 * agents and keep them working without a person asking. */
export function CheckIns({
  dept,
  org,
  canEdit,
  act,
}: {
  dept: Department;
  org: OrgOverview;
  canEdit: boolean;
  act: (fn: () => Promise<unknown>) => Promise<void>;
}) {
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState(PRESETS[0].name);
  const [message, setMessage] = useState(PRESETS[0].message);
  const [every, setEvery] = useState(PRESETS[0].minutes);
  const [agent, setAgent] = useState("");
  const checkIns: CheckIn[] = (org.check_ins ?? []).filter((c) => c.department_id === dept.id);
  const workers = dept.agents.filter((a) => a.kind === "worker");
  const agentName = (id: string | null) => dept.agents.find((a) => a.id === id)?.name;

  return (
    <section className="card side-panel">
      <div className="row" style={{ justifyContent: "space-between" }}>
        <h3 className="section-title">Check-ins</h3>
        {canEdit && !adding && (
          <button className="btn small-btn" onClick={() => setAdding(true)}>
            + Check-in
          </button>
        )}
      </div>
      <p className="muted small">
        Scheduled messages that wake sleeping agents, so the department keeps working towards the goals.
      </p>
      {adding && (
        <form
          className="provider-edit"
          onSubmit={async (e) => {
            e.preventDefault();
            await act(() =>
              api.createCheckIn(dept.id, { name: name.trim(), message: message.trim(), every_minutes: every, agent_id: agent || null }),
            );
            setAdding(false);
          }}
        >
          <div className="row">
            {PRESETS.map((p) => (
              <button
                key={p.name}
                type="button"
                className="btn small-btn ghost"
                onClick={() => {
                  setName(p.name);
                  setMessage(p.message);
                  setEvery(p.minutes);
                }}
              >
                {p.name}
              </button>
            ))}
          </div>
          <div className="grid-2">
            <label>
              Name
              <input value={name} onChange={(e) => setName(e.target.value)} required />
            </label>
            <label>
              How often
              <select value={every} onChange={(e) => setEvery(Number(e.target.value))}>
                {EVERY.map((o) => (
                  <option key={o.minutes} value={o.minutes}>
                    {o.label}
                  </option>
                ))}
              </select>
            </label>
          </div>
          <label>
            To
            <select value={agent} onChange={(e) => setAgent(e.target.value)}>
              <option value="">Everyone in {dept.name}</option>
              {workers.map((a) => (
                <option key={a.id} value={a.id}>
                  {a.name}
                </option>
              ))}
            </select>
          </label>
          <label>
            Message
            <textarea rows={3} value={message} onChange={(e) => setMessage(e.target.value)} required />
          </label>
          <div className="row">
            <button className="btn primary" disabled={!name.trim() || !message.trim()}>
              Add check-in
            </button>
            <button type="button" className="btn ghost" onClick={() => setAdding(false)}>
              Cancel
            </button>
          </div>
        </form>
      )}
      {checkIns.length === 0 && !adding && <p className="muted small">No check-ins: agents work only when someone writes to them.</p>}
      <ul className="goal-list">
        {checkIns.map((c) => (
          <li key={c.id} className="goal">
            <div className="session-item-top">
              <strong>{c.name}</strong>
              <span className={`pill ${c.enabled ? "status-running" : ""}`}>{c.enabled ? everyLabel(c.every_minutes) : "off"}</span>
            </div>
            <div className="muted small">
              to {agentName(c.agent_id) ?? `everyone in ${dept.name}`} · last {when(c.last_run_at)}
              {c.enabled && ` · next ${when(c.next_run_at)}`}
            </div>
            <div className="small">{c.message}</div>
            {canEdit && (
              <div className="row">
                {c.enabled && (
                  <button className="link small" onClick={() => act(() => api.runCheckIn(c.id))}>
                    run now
                  </button>
                )}
                <button className="link small" onClick={() => act(() => api.updateCheckIn(c.id, { enabled: !c.enabled }))}>
                  {c.enabled ? "turn off" : "turn on"}
                </button>
                <button
                  className="link small danger-text"
                  onClick={() => {
                    if (confirm(`Delete the check-in "${c.name}"?`)) void act(() => api.deleteCheckIn(c.id));
                  }}
                >
                  delete
                </button>
              </div>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}
