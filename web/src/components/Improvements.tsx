import { useEffect, useState } from "react";
import { api } from "../api";
import type { Me, OrgOverview, Proposal, ProposalAction, ProposalStatus } from "../types";

const KIND_LABELS: Record<string, string> = {
  update_instructions: "Change an agent's instructions",
  add_agent: "Add an agent",
  remove_agent: "Remove an agent",
  control_agent: "Start, pause or stop an agent",
  update_department: "Change a department",
  create_check_in: "Add a check-in",
  update_check_in: "Change a check-in",
  create_goal: "Add a goal",
  send_message: "Send a message",
  set_limits: "Change the limits",
};

const ADMIN_KINDS = ["add_agent", "remove_agent", "update_department", "set_limits"];

const STATUS: Record<ProposalStatus, { label: string; cls: string }> = {
  open: { label: "waiting for you", cls: "status-awaiting_input" },
  changes_requested: { label: "sent back", cls: "status-paused" },
  applied: { label: "applied", cls: "status-completed" },
  rejected: { label: "rejected", cls: "" },
  failed: { label: "failed", cls: "status-failed" },
};

const FILTERS: { id: string; label: string; match: (s: ProposalStatus) => boolean }[] = [
  { id: "pending", label: "To decide", match: (s) => s === "open" || s === "changes_requested" },
  { id: "applied", label: "Applied", match: (s) => s === "applied" || s === "failed" },
  { id: "rejected", label: "Rejected", match: (s) => s === "rejected" },
  { id: "all", label: "All", match: () => true },
];

/** One change in words, with names instead of ids. */
function describe(a: ProposalAction, org: OrgOverview): { title: string; body?: string } {
  const agent = (id: unknown) => {
    for (const d of org.departments) {
      const found = d.agents.find((x) => x.id === id);
      if (found) return `${found.name} (${d.name})`;
    }
    return "an agent that no longer exists";
  };
  const dept = (id: unknown) => org.departments.find((d) => d.id === id)?.name ?? "a department that no longer exists";
  const checkIn = (id: unknown) => {
    const c = org.check_ins.find((x) => x.id === id);
    return c ? `“${c.name}” in ${dept(c.department_id)}` : "a check-in that no longer exists";
  };
  const every = (m: unknown) => {
    const n = Number(m);
    return n % 1440 === 0 ? `every ${n / 1440} day(s)` : n % 60 === 0 ? `every ${n / 60} hour(s)` : `every ${n} minutes`;
  };
  switch (a.kind) {
    case "update_instructions":
      return { title: `New instructions for ${agent(a.agent)}`, body: String(a.instructions ?? "") };
    case "add_agent":
      return { title: `Add ${String(a.name)} to ${dept(a.department)}${a.agent ? `, running ${String(a.agent)}` : ""}`, body: String(a.instructions ?? "") };
    case "remove_agent":
      return { title: `Stop and remove ${agent(a.agent)}` };
    case "control_agent":
      return { title: `${String(a.control)} ${agent(a.agent)}` };
    case "update_department": {
      const parts = [
        a.policy ? `policy → ${String(a.policy)}` : null,
        Array.isArray(a.tools) ? `tools → ${(a.tools as string[]).join(", ") || "messaging only"}` : null,
        a.mission ? "new mission" : null,
      ].filter(Boolean);
      return { title: `Change ${dept(a.department)}: ${parts.join(", ")}`, body: a.mission ? String(a.mission) : undefined };
    }
    case "create_check_in":
      return {
        title: `Check-in “${String(a.name)}” for ${a.agent ? agent(a.agent) : `everyone in ${dept(a.department)}`}, ${every(a.every_minutes)}`,
        body: String(a.message ?? ""),
      };
    case "update_check_in": {
      const parts = [
        a.every_minutes ? every(a.every_minutes) : null,
        a.enabled === false ? "turn off" : a.enabled === true ? "turn on" : null,
        a.message ? "new message" : null,
      ].filter(Boolean);
      return { title: `Change check-in ${checkIn(a.check_in)}: ${parts.join(", ")}`, body: a.message ? String(a.message) : undefined };
    }
    case "create_goal":
      return { title: `New goal “${String(a.title)}”${a.department ? ` for ${dept(a.department)}` : ""}`, body: a.description ? String(a.description) : undefined };
    case "send_message":
      return { title: `Message to ${a.agent ? agent(a.agent) : dept(a.department)}`, body: String(a.text ?? "") };
    case "set_limits":
      return {
        title: `Limits: ${a.max_departments ?? "same"} departments, ${a.max_agents_per_department ?? "same"} agents each`,
      };
    default:
      return { title: a.kind };
  }
}

/** Improvements proposed by the retrospective (or people): decide, change,
 * send back, apply. */
export function Improvements({
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
  const [proposals, setProposals] = useState<Proposal[] | null>(null);
  const [filter, setFilter] = useState("pending");
  const [reload, setReload] = useState(0);
  useEffect(() => {
    api.proposals().then(setProposals).catch(onError);
  }, [tick, reload, onError]);
  const refresh = () => setReload((r) => r + 1);
  const retro = org.departments.find((d) => d.tools.includes("insights"));

  const shown = (proposals ?? []).filter((p) => FILTERS.find((f) => f.id === filter)!.match(p.status));
  return (
    <div className="session">
      <section className="card session-header">
        <div className="session-header-main">
          <h2>Improvements</h2>
          <p className="muted">
            {retro
              ? `The retrospective in ${retro.name} watches the metrics and proposes fixes. Nothing changes until you apply one; you can change it first or send it back with what should be different.`
              : "Proposals for making the organisation work better. Add a Retrospective department (from the dashboard or the builder) to get them automatically."}
          </p>
        </div>
        <div className="session-actions segmented" role="group" aria-label="Show">
          {FILTERS.map((f) => (
            <button key={f.id} className={`btn small-btn ${filter === f.id ? "primary" : "ghost"}`} onClick={() => setFilter(f.id)}>
              {f.label}
              {f.id === "pending" && org.proposals_pending > 0 ? ` (${org.proposals_pending})` : ""}
            </button>
          ))}
        </div>
      </section>
      {proposals === null && <div className="empty">Loading…</div>}
      {proposals !== null && shown.length === 0 && (
        <div className="empty">
          <p>{filter === "pending" ? "Nothing to decide right now." : "Nothing here."}</p>
        </div>
      )}
      {shown.map((p) => (
        <ProposalCard key={`${p.id}-${p.revision}`} p={p} org={org} me={me} onChanged={refresh} onError={onError} />
      ))}
      <AutoApply me={me} onError={onError} />
    </div>
  );
}

function ProposalCard({
  p,
  org,
  me,
  onChanged,
  onError,
}: {
  p: Proposal;
  org: OrgOverview;
  me: Me;
  onChanged: () => void;
  onError: (e: unknown) => void;
}) {
  const [mode, setMode] = useState<"view" | "edit" | "changes" | "reject">("view");
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const pending = p.status === "open" || p.status === "changes_requested";
  const canOperate = me.role !== "viewer";
  const needsAdmin = p.actions.some((a) => ADMIN_KINDS.includes(a.kind));
  const canApply = canOperate && (!needsAdmin || me.role === "admin");

  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await fn();
      setMode("view");
      setText("");
      onChanged();
    } catch (err) {
      onError(err);
    } finally {
      setBusy(false);
    }
  };

  if (mode === "edit") {
    return <ProposalEditor p={p} org={org} onDone={() => { setMode("view"); onChanged(); }} onError={onError} />;
  }

  return (
    <section className="card proposal">
      <div className="session-item-top">
        <h3>{p.title}</h3>
        <span className={`pill ${STATUS[p.status].cls}`}>{STATUS[p.status].label}</span>
      </div>
      <div className="muted small">
        by {p.proposed_by} · {new Date(p.created_at).toLocaleString()}
        {p.revision > 1 && ` · revision ${p.revision}`}
        {p.decided_by && ` · ${p.status} by ${p.decided_by}`}
      </div>
      <div className="proposal-grid">
        <div>
          <h4>Problem</h4>
          <p>{p.problem}</p>
        </div>
        {p.evidence && (
          <div>
            <h4>Evidence</h4>
            <p>{p.evidence}</p>
          </div>
        )}
        <div>
          <h4>Solution</h4>
          <p>{p.solution}</p>
        </div>
      </div>
      <h4>Changes it makes {p.actions.length === 0 && <span className="muted small">(none: advice only)</span>}</h4>
      <ol className="change-list">
        {p.actions.map((a, i) => {
          const d = describe(a, org);
          const r = p.result?.[i];
          return (
            <li key={i}>
              <div className="session-item-top">
                <span>
                  {d.title}
                  {ADMIN_KINDS.includes(a.kind) && <span className="chip small">admin</span>}
                </span>
                {r && (
                  <span className={`sev ${r.ok ? "sev-good" : "sev-critical"}`}>
                    <span aria-hidden>{r.ok ? "✓" : "▲"}</span> {r.ok ? "done" : "not done"}
                  </span>
                )}
              </div>
              {d.body && <div className="bubble-body small">{d.body}</div>}
              {r && <div className="muted small">{r.detail}</div>}
            </li>
          );
        })}
      </ol>
      {p.feedback && (
        <div className="callout small">
          <strong>{p.status === "rejected" ? "Why it was rejected" : "Asked to change"}:</strong> {p.feedback}
        </div>
      )}
      {p.history.length > 0 && (
        <details>
          <summary className="small">History ({p.history.length})</summary>
          <ul className="history small">
            {p.history.map((h, i) => (
              <li key={i}>
                {"feedback" in h && !("revision" in h) ? (
                  <>
                    <strong>{String(h.by)}</strong> asked for changes: {String(h.feedback)}
                  </>
                ) : (
                  <>
                    Revision {String(h.revision)} replaced by <strong>{String(h.replaced_by)}</strong>
                    {h.note ? `: ${String(h.note)}` : ""} — was “{String(h.title)}”: {String(h.solution)}
                  </>
                )}
                {h.at ? <span className="muted"> · {new Date(String(h.at)).toLocaleString()}</span> : null}
              </li>
            ))}
          </ul>
        </details>
      )}
      {pending && canOperate && mode === "view" && (
        <div className="row">
          <button
            className="btn primary"
            disabled={busy || !canApply}
            title={canApply ? "Apply these changes now" : "Some changes need an admin"}
            onClick={() => {
              if (confirm(`Apply “${p.title}”? ${p.actions.length} change(s) will be made now.`)) void run(() => api.applyProposal(p.id, p.revision));
            }}
          >
            ✓ Apply
          </button>
          <button className="btn" onClick={() => setMode("edit")}>
            Change it…
          </button>
          {p.proposer_agent && (
            <button className="btn" onClick={() => setMode("changes")}>
              Send back…
            </button>
          )}
          <button className="btn ghost danger-text" onClick={() => setMode("reject")}>
            Reject…
          </button>
          {!canApply && <span className="muted small">Some changes need an admin to apply.</span>}
        </div>
      )}
      {(mode === "changes" || mode === "reject") && (
        <form
          className="provider-edit"
          onSubmit={(e) => {
            e.preventDefault();
            void run(() => (mode === "changes" ? api.requestChanges(p.id, text) : api.rejectProposal(p.id, text)));
          }}
        >
          <label>
            {mode === "changes" ? "What should be different? The proposer revises it." : "Why not? (the proposer will not suggest it again)"}
            <textarea rows={3} value={text} onChange={(e) => setText(e.target.value)} required={mode === "changes"} autoFocus />
          </label>
          <div className="row">
            <button className={`btn ${mode === "reject" ? "danger" : "primary"}`} disabled={busy}>
              {mode === "changes" ? "Send back" : "Reject"}
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

/** A person changes the proposal: texts, and each change (as JSON). */
function ProposalEditor({ p, org, onDone, onError }: { p: Proposal; org: OrgOverview; onDone: () => void; onError: (e: unknown) => void }) {
  const [title, setTitle] = useState(p.title);
  const [problem, setProblem] = useState(p.problem);
  const [evidence, setEvidence] = useState(p.evidence);
  const [solution, setSolution] = useState(p.solution);
  const [actions, setActions] = useState(p.actions.map((a) => JSON.stringify(a, null, 2)));
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const parsed = actions.map((a) => {
    try {
      return JSON.parse(a) as ProposalAction;
    } catch {
      return null;
    }
  });
  const valid = parsed.every((a) => a && typeof a.kind === "string");
  return (
    <form
      className="card proposal provider-edit"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          await api.updateProposal(p.id, {
            title,
            problem,
            evidence,
            solution,
            actions: parsed as ProposalAction[],
            note: note || "changed before applying",
          });
          onDone();
        } catch (err) {
          onError(err);
        } finally {
          setBusy(false);
        }
      }}
    >
      <h3>Change the proposal</h3>
      <label>
        Title
        <input value={title} onChange={(e) => setTitle(e.target.value)} required maxLength={200} />
      </label>
      <label>
        Problem
        <textarea rows={2} value={problem} onChange={(e) => setProblem(e.target.value)} required />
      </label>
      <label>
        Evidence
        <textarea rows={2} value={evidence} onChange={(e) => setEvidence(e.target.value)} />
      </label>
      <label>
        Solution
        <textarea rows={3} value={solution} onChange={(e) => setSolution(e.target.value)} required />
      </label>
      <h4>Changes</h4>
      {actions.map((a, i) => (
        <div key={i} className="change-edit">
          <div className="session-item-top">
            <span className="small">{parsed[i] ? describe(parsed[i]!, org).title : <span className="danger-text">not valid JSON</span>}</span>
            <button type="button" className="link small danger-text" onClick={() => setActions(actions.filter((_, j) => j !== i))}>
              remove
            </button>
          </div>
          <textarea
            className="mono"
            rows={Math.min(10, a.split("\n").length + 1)}
            value={a}
            onChange={(e) => setActions(actions.map((x, j) => (j === i ? e.target.value : x)))}
          />
        </div>
      ))}
      <div className="row">
        <select
          value=""
          onChange={(e) => {
            if (e.target.value) setActions([...actions, JSON.stringify({ kind: e.target.value }, null, 2)]);
          }}
        >
          <option value="">+ add a change…</option>
          {Object.entries(KIND_LABELS).map(([k, label]) => (
            <option key={k} value={k}>
              {label}
            </option>
          ))}
        </select>
        <span className="muted small">Ids are on the agents' and departments' pages and in the API.</span>
      </div>
      <label>
        What you changed (kept in the history)
        <input value={note} onChange={(e) => setNote(e.target.value)} placeholder="Once a day is enough" />
      </label>
      <div className="row">
        <button className="btn primary" disabled={busy || !valid}>
          Save as revision {p.revision + 1}
        </button>
        <button type="button" className="btn ghost" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

/** Kinds of change applied without asking (admins set this). */
function AutoApply({ me, onError }: { me: Me; onError: (e: unknown) => void }) {
  const [state, setState] = useState<{ kinds: string[]; by: string | null; available: string[] } | null>(null);
  useEffect(() => {
    api.autoApply().then(setState).catch(onError);
  }, [onError]);
  if (!state) return null;
  const isAdmin = me.role === "admin";
  const toggle = async (kind: string, on: boolean) => {
    const kinds = on ? [...state.kinds, kind] : state.kinds.filter((k) => k !== kind);
    try {
      await api.setAutoApply(kinds);
      setState({ ...state, kinds, by: me.name });
    } catch (err) {
      onError(err);
    }
  };
  return (
    <section className="card side-panel">
      <h3 className="section-title">Apply without asking</h3>
      <p className="muted small">
        Proposals from agents whose changes are all of the kinds ticked here are applied at once, in the name of the admin
        who allowed it. Everything else waits for a person.
        {state.by && state.kinds.length > 0 && ` Allowed by ${state.by}.`}
      </p>
      <div className="check-grid">
        {state.available.map((k) => (
          <label key={k} className="inline-check">
            <input type="checkbox" disabled={!isAdmin} checked={state.kinds.includes(k)} onChange={(e) => void toggle(k, e.target.checked)} />{" "}
            {KIND_LABELS[k] ?? k}
            {ADMIN_KINDS.includes(k) && <span className="chip small">admin</span>}
          </label>
        ))}
      </div>
    </section>
  );
}
