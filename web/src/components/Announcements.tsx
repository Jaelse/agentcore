import { useEffect, useState } from "react";
import { api } from "../api";
import type { Me, OrgAnnouncement, OrgOverview } from "../types";

const isCurrent = (a: OrgAnnouncement, now = Date.now()) =>
  !a.withdrawn_at && (!a.valid_until || new Date(a.valid_until).getTime() > now);

const day = (iso: string) => new Date(iso).toLocaleDateString(undefined, { day: "numeric", month: "short", year: "numeric" });

/** The end of the chosen day, in the viewer's time zone. */
const endOfDay = (date: string) => new Date(`${date}T23:59:59`).toISOString();

/** Today, as a date input wants it, so a date in the past cannot be picked. */
const today = () => {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
};

/**
 * Writing an announcement: to everyone or one department. It is delivered
 * once and stays on the notice board, and in the agents' instructions,
 * until it expires or is withdrawn.
 */
export function AnnounceForm({
  org,
  act,
  onDone,
}: {
  org: OrgOverview;
  act: (fn: () => Promise<unknown>) => Promise<void>;
  onDone: () => void;
}) {
  const [to, setTo] = useState("");
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [until, setUntil] = useState("");
  const [askAck, setAskAck] = useState(false);
  const [busy, setBusy] = useState(false);
  const audience = to ? org.departments.find((d) => d.id === to) : null;
  const reach = (audience ? [audience] : org.departments).reduce((n, d) => n + d.agents.length, 0);
  return (
    <form
      className="card pad project-form announce-form"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        await act(() =>
          api.announce({
            title: title.trim(),
            body: body.trim(),
            department_id: to || null,
            ask_ack: askAck,
            valid_until: until ? endOfDay(until) : null,
          }),
        );
        setBusy(false);
        onDone();
      }}
    >
      <h3 className="section-title">📣 New announcement</h3>
      <div className="grid-2">
        <label>
          To
          <select value={to} onChange={(e) => setTo(e.target.value)}>
            <option value="">Everyone (every department)</option>
            {org.departments.map((d) => (
              <option key={d.id} value={d.id}>
                {d.name} only
              </option>
            ))}
          </select>
        </label>
        <label>
          Valid until (optional)
          <input type="date" value={until} min={today()} onChange={(e) => setUntil(e.target.value)} />
        </label>
      </div>
      <label>
        Title
        <input value={title} onChange={(e) => setTitle(e.target.value)} placeholder="From Monday, every release needs QA sign-off" maxLength={200} required />
      </label>
      <label>
        Message
        <textarea rows={4} value={body} onChange={(e) => setBody(e.target.value)} placeholder="What changes, why, and from when." />
      </label>
      <label className="inline-check">
        <input type="checkbox" checked={askAck} onChange={(e) => setAskAck(e.target.checked)} /> Ask every agent to
        acknowledge it
      </label>
      <p className="muted small">
        It reaches {reach} agent{reach === 1 ? "" : "s"} {audience ? `in ${audience.name}` : "in every department"} once,
        and stays in their instructions until it {until ? "expires or " : ""}is withdrawn. Agents act on it in their
        department; they do not reply to everyone.
      </p>
      <div className="row">
        <button className="btn primary" disabled={busy || !title.trim()}>
          {busy ? "Announcing…" : "Announce"}
        </button>
        <button type="button" className="btn ghost" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

/** The current announcements, who they are for, and who acknowledged them. */
export function NoticeBoard({
  org,
  me,
  tick,
  act,
  onError,
}: {
  org: OrgOverview;
  me: Me;
  tick: number;
  act: (fn: () => Promise<unknown>) => Promise<void>;
  onError: (e: unknown) => void;
}) {
  const [all, setAll] = useState<OrgAnnouncement[]>([]);
  const [showPast, setShowPast] = useState(false);
  useEffect(() => {
    api.announcements().then(setAll).catch(onError);
  }, [tick, onError]);
  const current = all.filter((a) => isCurrent(a));
  const past = all.filter((a) => !isCurrent(a));
  if (all.length === 0) return null;
  const shown = showPast ? all : current;
  return (
    <section className="card pad notice-board">
      <div className="notice-head">
        <h3 className="section-title">📌 Notice board</h3>
        {past.length > 0 && (
          <button className="link small" onClick={() => setShowPast(!showPast)}>
            {showPast ? "Only current" : `${past.length} past`}
          </button>
        )}
      </div>
      {shown.length === 0 && <p className="muted small">No current announcements.</p>}
      <ul className="notices">
        {shown.map((a) => (
          <Notice key={a.id} a={a} org={org} canWithdraw={me.role !== "viewer"} act={act} />
        ))}
      </ul>
    </section>
  );
}

function Notice({
  a,
  org,
  canWithdraw,
  act,
}: {
  a: OrgAnnouncement;
  org: OrgOverview;
  canWithdraw: boolean;
  act: (fn: () => Promise<unknown>) => Promise<void>;
}) {
  const [open, setOpen] = useState(false);
  const dept = a.department_id ? org.departments.find((d) => d.id === a.department_id) : null;
  const audience = (dept ? [dept] : org.departments).flatMap((d) => d.agents.map((ag) => ({ agent: ag, dept: d })));
  const acked = audience.filter((x) => a.acks.some((k) => k.agent_id === x.agent.id));
  const waiting = audience.filter((x) => !a.acks.some((k) => k.agent_id === x.agent.id));
  const current = isCurrent(a);
  return (
    <li className={`notice ${current ? "" : "past"}`}>
      <div className="notice-meta small muted">
        <span className={`chip ${dept ? "" : "ok"}`}>{dept ? `${dept.name} only` : "Everyone"}</span>
        <span>
          {a.created_by} · {day(a.created_at)}
          {a.valid_until && ` · until ${day(a.valid_until)}`}
        </span>
        {a.withdrawn_at && <span className="attention-text">withdrawn by {a.withdrawn_by}</span>}
        {!a.withdrawn_at && !current && <span>expired</span>}
      </div>
      <strong className="notice-title">{a.title}</strong>
      {a.body && <p className="notice-body">{a.body}</p>}
      <div className="row notice-actions">
        {a.ask_ack && (
          <button className="link small" onClick={() => setOpen(!open)} aria-expanded={open}>
            Acknowledged by {acked.length} of {audience.length} agent{audience.length === 1 ? "" : "s"}
          </button>
        )}
        {current && canWithdraw && (
          <button
            className="link small"
            onClick={() => {
              if (confirm(`Withdraw “${a.title}”? It leaves the notice board and the agents' instructions.`)) {
                void act(() => api.withdrawAnnouncement(a.id));
              }
            }}
          >
            Withdraw
          </button>
        )}
      </div>
      {open && a.ask_ack && (
        <ul className="ack-list small">
          {acked.map(({ agent, dept: d }) => {
            const ack = a.acks.find((k) => k.agent_id === agent.id)!;
            return (
              <li key={agent.id}>
                ✓ <strong>{agent.name}</strong> <span className="muted">({d.name})</span>
                {ack.note && <span> — {ack.note}</span>}
              </li>
            );
          })}
          {waiting.map(({ agent, dept: d }) => (
            <li key={agent.id} className="muted">
              ○ {agent.name} ({d.name})
            </li>
          ))}
        </ul>
      )}
    </li>
  );
}
