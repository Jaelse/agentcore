import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, streamOrg } from "../api";
import { useInterval } from "../hooks";
import type {
  Department,
  DepartmentFile,
  Me,
  MessageTo,
  OrgAgent,
  OrgMessage,
  OrgOverview,
  Suggestion,
  SystemCard,
} from "../types";
import { type BuildMode, OrgBuilder } from "./OrgBuilder";

interface Props {
  me: Me;
  card: SystemCard;
  onOpenSession: (sessionId: string) => void;
  onError: (err: unknown) => void;
}

const TOOL_GROUPS: { id: string; label: string; help: string }[] = [
  { id: "sandbox", label: "Sandbox", help: "commands and files in each agent's own sandbox" },
  { id: "files", label: "Department files", help: "a shared file store only this department can use" },
];

type Selection =
  | { kind: "org" }
  | { kind: "department"; id: string }
  | { kind: "new" }
  | { kind: "build"; mode?: BuildMode; pick?: string[] };

/** Departments (rooms) of agents, how they talk, and control at every level. */
export function Organisation({ me, card, onOpenSession, onError }: Props) {
  const [org, setOrg] = useState<OrgOverview | null>(null);
  const [selection, setSelection] = useState<Selection>({ kind: "org" });
  const [tick, setTick] = useState(0);
  const canOperate = me.role !== "viewer";
  const isAdmin = me.role === "admin";

  const load = useCallback(() => {
    api.org().then(setOrg).catch(onError);
    setTick((t) => t + 1);
  }, [onError]);

  // Live: every node announces changes; refetch at most every 300 ms.
  const pending = useRef<number | null>(null);
  useEffect(() => {
    const stop = streamOrg(() => {
      if (pending.current !== null) return;
      pending.current = window.setTimeout(() => {
        pending.current = null;
        load();
      }, 300);
    });
    return () => {
      stop();
      if (pending.current !== null) clearTimeout(pending.current);
    };
  }, [load]);
  // Safety net for anything a notification did not cover.
  useInterval(load, 10000);

  if (!org) return <div className="empty">Loading the organisation…</div>;

  const dept = selection.kind === "department" ? org.departments.find((d) => d.id === selection.id) : undefined;
  const running = org.departments.flatMap((d) => d.agents).filter((a) => a.desired !== "stopped").length;
  const allPaused = org.departments.length > 0 && org.departments.every((d) => d.state === "paused");

  const act = async (fn: () => Promise<unknown>) => {
    try {
      await fn();
    } catch (err) {
      onError(err);
    } finally {
      load();
    }
  };

  return (
    <div className="layout">
      <aside className="sidebar">
        <button className={`session-item ${selection.kind === "org" ? "active" : ""}`} onClick={() => setSelection({ kind: "org" })}>
          <div className="session-item-top">
            <strong>Whole organisation</strong>
            <span className="muted small">{running} running</span>
          </div>
          <div className="muted small">
            {org.departments.length}/{org.settings.max_departments} departments · every message
          </div>
        </button>
        {canOperate && org.departments.length > 0 && (
          <div className="row">
            {allPaused ? (
              <button className="btn pause-button resume" onClick={() => act(api.resumeAll)}>
                ▶ Resume all
              </button>
            ) : (
              <button className="btn pause-button" onClick={() => act(api.pauseAll)} title="Pause every department">
                ❚❚ Pause all
              </button>
            )}
          </div>
        )}

        <h3 className="section-title">Departments</h3>
        {org.departments.length === 0 && (
          <p className="muted small">
            No departments yet. {isAdmin ? "Create one to put agents to work together." : "Ask an admin to create one."}
          </p>
        )}
        <ul className="session-list">
          {org.departments.map((d) => {
            const live = d.agents.filter((a) => a.desired !== "stopped").length;
            return (
              <li key={d.id}>
                <button
                  className={`session-item ${dept?.id === d.id ? "active" : ""}`}
                  onClick={() => setSelection({ kind: "department", id: d.id })}
                >
                  <div className="session-item-top">
                    <strong>{d.name}</strong>
                    {d.state === "paused" ? (
                      <span className="pill status-paused">paused</span>
                    ) : live > 0 ? (
                      <span className="pill status-running">
                        <span className="pulse" />
                        {live} running
                      </span>
                    ) : (
                      <span className="pill">idle</span>
                    )}
                  </div>
                  <div className="muted small">
                    {d.agents.filter((a) => a.kind === "worker").length}/{org.settings.max_agents_per_department} agents
                    {" · "}+ communicator
                  </div>
                </button>
              </li>
            );
          })}
        </ul>
        {isAdmin && (
          <div className="row">
            <button className="btn" onClick={() => setSelection({ kind: "build", mode: "pick" })}>
              + Add departments
            </button>
            <button
              className="link small"
              disabled={org.departments.length >= org.settings.max_departments}
              title={
                org.departments.length >= org.settings.max_departments
                  ? "The organisation has reached its department limit"
                  : "A department without a template"
              }
              onClick={() => setSelection({ kind: "new" })}
            >
              blank department
            </button>
          </div>
        )}

        <Limits org={org} canEdit={isAdmin} onSaved={load} onError={onError} />
        <Nodes org={org} />
      </aside>

      <main className="main">
        {selection.kind === "build" || (selection.kind === "org" && org.departments.length === 0 && isAdmin) ? (
          <OrgBuilder
            key={selection.kind === "build" ? `${selection.mode}-${selection.pick?.join()}` : "first"}
            me={me}
            card={card}
            org={org}
            initialMode={selection.kind === "build" ? selection.mode : undefined}
            initialPick={selection.kind === "build" ? selection.pick : undefined}
            onDone={(id) => {
              load();
              setSelection(id ? { kind: "department", id } : { kind: "org" });
            }}
            onCancel={org.departments.length > 0 ? () => setSelection({ kind: "org" }) : undefined}
            onError={onError}
          />
        ) : selection.kind === "new" ? (
          <DepartmentForm
            department={null}
            card={card}
            onSaved={(d) => {
              load();
              setSelection({ kind: "department", id: d.id });
            }}
            onCancel={() => setSelection({ kind: "org" })}
            onError={onError}
          />
        ) : dept ? (
          <DepartmentRoom
            key={dept.id}
            dept={dept}
            org={org}
            me={me}
            card={card}
            tick={tick}
            act={act}
            onOpenSession={onOpenSession}
            onDeleted={() => {
              setSelection({ kind: "org" });
              load();
            }}
            onError={onError}
          />
        ) : (
          <div className="session">
            <section className="card session-header">
              <div className="session-header-main">
                <h2>{org.profile.company_name || "Organisation"}</h2>
                {org.profile.company_about && <p>{org.profile.company_about}</p>}
                <p className="muted">
                  Agents work together inside a department. Departments only talk to each other through their
                  communicators. You can see every message, talk to any agent or department, and pause or stop
                  anything.
                </p>
              </div>
              {isAdmin && org.departments.length > 0 && (
                <div className="session-actions">
                  <button className="btn" onClick={() => setSelection({ kind: "build" })}>
                    Build & grow…
                  </button>
                </div>
              )}
            </section>
            {org.departments.length === 0 && !isAdmin && (
              <div className="empty">
                <h2>No departments yet</h2>
                <p>An admin builds the organisation, from one department to a complete company.</p>
              </div>
            )}
            {org.departments.length > 0 && (
              <Grow
                me={me}
                card={card}
                tick={tick}
                act={act}
                onPick={(templates) => setSelection({ kind: "build", mode: "pick", pick: templates })}
                onError={onError}
              />
            )}
            <Feed org={org} tick={tick} canPost={canOperate} onError={onError} />
          </div>
        )}
      </main>
    </div>
  );
}

function Limits({ org, canEdit, onSaved, onError }: { org: OrgOverview; canEdit: boolean; onSaved: () => void; onError: (e: unknown) => void }) {
  const [editing, setEditing] = useState(false);
  const [maxDepts, setMaxDepts] = useState(String(org.settings.max_departments));
  const [maxAgents, setMaxAgents] = useState(String(org.settings.max_agents_per_department));
  return (
    <div className="card side-panel">
      <div className="row" style={{ justifyContent: "space-between" }}>
        <h3 className="section-title">Limits</h3>
        {canEdit && !editing && (
          <button
            className="link small"
            onClick={() => {
              setMaxDepts(String(org.settings.max_departments));
              setMaxAgents(String(org.settings.max_agents_per_department));
              setEditing(true);
            }}
          >
            change
          </button>
        )}
      </div>
      {editing ? (
        <form
          className="provider-edit"
          onSubmit={async (e) => {
            e.preventDefault();
            try {
              await api.saveOrgSettings({
                max_departments: Number(maxDepts),
                max_agents_per_department: Number(maxAgents),
              });
              setEditing(false);
              onSaved();
            } catch (err) {
              onError(err);
            }
          }}
        >
          <label>
            Max departments
            <input type="number" min={0} value={maxDepts} onChange={(e) => setMaxDepts(e.target.value)} />
          </label>
          <label>
            Max agents per department
            <input type="number" min={0} value={maxAgents} onChange={(e) => setMaxAgents(e.target.value)} />
          </label>
          <p className="muted small">Lowering a limit removes nothing; it only blocks new additions.</p>
          <div className="row">
            <button className="btn primary">Save</button>
            <button type="button" className="btn ghost" onClick={() => setEditing(false)}>
              Cancel
            </button>
          </div>
        </form>
      ) : (
        <dl className="meta stacked small">
          <div>
            <dt>Departments</dt>
            <dd>
              {org.departments.length} of {org.settings.max_departments}
            </dd>
          </div>
          <div>
            <dt>Agents per department</dt>
            <dd>up to {org.settings.max_agents_per_department} (plus its communicator)</dd>
          </div>
        </dl>
      )}
    </div>
  );
}

function Nodes({ org }: { org: OrgOverview }) {
  return (
    <div className="card side-panel">
      <h3 className="section-title">Nodes</h3>
      <ul className="file-list">
        {org.nodes.map((n) => (
          <li key={n.name} title={`${n.internal_url} · last seen ${new Date(n.last_seen).toLocaleTimeString()}`}>
            <span className={`dot ${n.alive ? "on" : "off"}`} />
            <code>
              {n.name}
              {n.name === org.node && " (this one)"}
            </code>
            <span className="muted small">
              {n.alive ? `${n.agents}/${n.capacity} agents` : "unreachable"}
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function DepartmentRoom({
  dept,
  org,
  me,
  card,
  tick,
  act,
  onOpenSession,
  onDeleted,
  onError,
}: {
  dept: Department;
  org: OrgOverview;
  me: Me;
  card: SystemCard;
  tick: number;
  act: (fn: () => Promise<unknown>) => Promise<void>;
  onOpenSession: (id: string) => void;
  onDeleted: () => void;
  onError: (e: unknown) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [adding, setAdding] = useState(false);
  const canOperate = me.role !== "viewer";
  const isAdmin = me.role === "admin";
  const workers = dept.agents.filter((a) => a.kind === "worker");
  const anyLive = dept.agents.some((a) => a.desired !== "stopped");
  const anyStopped = dept.agents.some((a) => a.desired === "stopped");

  if (editing) {
    return (
      <DepartmentForm
        department={dept}
        card={card}
        onSaved={() => {
          setEditing(false);
          void act(async () => {});
        }}
        onCancel={() => setEditing(false)}
        onError={onError}
      />
    );
  }

  return (
    <div className="session">
      <section className="card session-header">
        <div className="session-header-main">
          <div className="session-title">
            <h2>{dept.name}</h2>
            {dept.state === "paused" && <span className="pill status-paused">paused</span>}
          </div>
          {dept.description && <p className="muted">{dept.description}</p>}
          <div className="context-chips">
            <span className="chip" title="Guardrail policy of this department's agents">
              🛡 {dept.policy}
            </span>
            {dept.tools.length === 0 && <span className="chip">messaging only</span>}
            {dept.tools.map((t) => (
              <span key={t} className="chip">
                {TOOL_GROUPS.find((g) => g.id === t)?.label ?? t}
              </span>
            ))}
            <span className="chip" title="Agent the communicator runs">
              📡 communicator: {dept.communicator_agent}
            </span>
          </div>
          {dept.mission && (
            <details>
              <summary className="small">Mission</summary>
              <p className="bubble-body">{dept.mission}</p>
            </details>
          )}
        </div>
        <div className="session-actions">
          {canOperate && anyStopped && dept.state === "active" && (
            <button className="btn primary" onClick={() => act(() => api.controlDepartment(dept.id, "start"))}>
              ▶ Start all
            </button>
          )}
          {canOperate &&
            (dept.state === "paused" ? (
              <button className="btn pause-button resume" onClick={() => act(() => api.controlDepartment(dept.id, "resume"))}>
                ▶ Resume
              </button>
            ) : (
              <button className="btn pause-button" onClick={() => act(() => api.controlDepartment(dept.id, "pause"))}>
                ❚❚ Pause
              </button>
            ))}
          {canOperate && anyLive && (
            <button
              className="stop-button"
              onClick={() => {
                if (confirm(`Stop every agent in ${dept.name}?`)) void act(() => api.controlDepartment(dept.id, "stop"));
              }}
            >
              ■ STOP DEPARTMENT
            </button>
          )}
          {isAdmin && (
            <button className="btn ghost" onClick={() => setEditing(true)}>
              Edit
            </button>
          )}
          {isAdmin && !anyLive && (
            <button
              className="btn ghost danger-text"
              onClick={async () => {
                if (!confirm(`Delete the ${dept.name} department, its agents, messages and files?`)) return;
                try {
                  await api.deleteDepartment(dept.id);
                  onDeleted();
                } catch (err) {
                  onError(err);
                }
              }}
            >
              Delete
            </button>
          )}
        </div>
      </section>

      <section>
        <div className="row" style={{ justifyContent: "space-between", marginBottom: 8 }}>
          <h3 className="section-title">
            Members · {workers.length}/{org.settings.max_agents_per_department} agents + communicator
          </h3>
          {canOperate && workers.length < org.settings.max_agents_per_department && !adding && (
            <button className="btn small-btn" onClick={() => setAdding(true)}>
              + Add agent
            </button>
          )}
        </div>
        {adding && (
          <AddAgent
            dept={dept}
            card={card}
            onDone={() => {
              setAdding(false);
              void act(async () => {});
            }}
            onError={onError}
          />
        )}
        <Grow
          me={me}
          card={card}
          tick={tick}
          act={act}
          department={dept.id}
          onPick={() => {}}
          onError={onError}
        />
        <div className="member-grid">
          {dept.agents.map((a) => (
            <AgentCard key={a.id} agent={a} dept={dept} canOperate={canOperate} act={act} onOpenSession={onOpenSession} />
          ))}
        </div>
      </section>

      <div className="room-grid">
        <Feed org={org} dept={dept} tick={tick} canPost={canOperate} onError={onError} />
        {dept.tools.includes("files") && <Files dept={dept} tick={tick} onError={onError} />}
      </div>
    </div>
  );
}

function statusClass(agent: OrgAgent) {
  if (agent.desired === "stopped") return agent.status === "failed" ? "status-failed" : "";
  return `status-${agent.status ?? "pending"}`;
}

function AgentCard({
  agent,
  dept,
  canOperate,
  act,
  onOpenSession,
}: {
  agent: OrgAgent;
  dept: Department;
  canOperate: boolean;
  act: (fn: () => Promise<unknown>) => Promise<void>;
  onOpenSession: (id: string) => void;
}) {
  const live = agent.desired !== "stopped";
  const label = !live
    ? agent.status && agent.status !== "stopped"
      ? `stopped · ${agent.status}`
      : "stopped"
    : agent.desired === "paused" || dept.state === "paused"
      ? "paused"
      : (agent.status ?? "starting").replace(/_/g, " ");
  return (
    <div className={`card member ${agent.kind}`}>
      <div className="session-item-top">
        <strong>
          {agent.kind === "communicator" ? "📡 " : ""}
          {agent.name}
        </strong>
        <span className={`pill ${statusClass(agent)}`}>
          {live && agent.status === "running" && <span className="pulse" />}
          {label}
        </span>
      </div>
      <div className="muted small">
        {agent.kind === "communicator" ? "talks to other departments" : agent.agent}
        {agent.node && live && ` · on ${agent.node}`}
      </div>
      {agent.instructions && <div className="session-task small">{agent.instructions}</div>}
      {agent.note && <div className="small attention-text">{agent.note}</div>}
      <div className="row">
        {agent.session_id && (
          <button className="btn small-btn" onClick={() => onOpenSession(agent.session_id!)}>
            {live ? "👁 Watch" : "History"}
          </button>
        )}
        {canOperate && !live && (
          <button className="btn small-btn primary" onClick={() => act(() => api.controlAgent(agent.id, "start"))}>
            ▶ Start
          </button>
        )}
        {canOperate && agent.desired === "running" && (
          <button className="btn small-btn" onClick={() => act(() => api.controlAgent(agent.id, "pause"))}>
            ❚❚ Pause
          </button>
        )}
        {canOperate && agent.desired === "paused" && (
          <button className="btn small-btn" onClick={() => act(() => api.controlAgent(agent.id, "resume"))}>
            ▶ Resume
          </button>
        )}
        {canOperate && live && (
          <button className="btn small-btn danger" onClick={() => act(() => api.controlAgent(agent.id, "stop"))}>
            ■ Stop
          </button>
        )}
        {canOperate && !live && agent.kind === "worker" && (
          <button
            className="link small"
            onClick={() => {
              if (confirm(`Remove ${agent.name} from ${dept.name}?`)) void act(() => api.deleteAgent(agent.id));
            }}
          >
            remove
          </button>
        )}
      </div>
    </div>
  );
}

function AddAgent({ dept, card, onDone, onError }: { dept: Department; card: SystemCard; onDone: () => void; onError: (e: unknown) => void }) {
  const [name, setName] = useState("");
  const [agent, setAgent] = useState(card.agents[0]?.name ?? "");
  const [instructions, setInstructions] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <form
      className="card pad provider-edit"
      style={{ marginBottom: 10 }}
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          await api.addAgent(dept.id, { name: name.trim().toLowerCase(), agent, instructions });
          onDone();
        } catch (err) {
          onError(err);
        } finally {
          setBusy(false);
        }
      }}
    >
      <div className="grid-2">
        <label>
          Name (how colleagues address it)
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="analyst" pattern="[a-z0-9][a-z0-9_\-]{0,31}" required />
        </label>
        <label>
          Runs
          <select value={agent} onChange={(e) => setAgent(e.target.value)}>
            {card.agents.map((a) => (
              <option key={a.name} value={a.name}>
                {a.name}
              </option>
            ))}
          </select>
        </label>
      </div>
      <label>
        Instructions (its job in {dept.name})
        <textarea rows={3} value={instructions} onChange={(e) => setInstructions(e.target.value)} placeholder="You research competitors and summarise…" />
      </label>
      <div className="row">
        <button className="btn primary" disabled={busy || !name.trim()}>
          Add agent
        </button>
        <button type="button" className="btn ghost" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function DepartmentForm({
  department,
  card,
  onSaved,
  onCancel,
  onError,
}: {
  department: Department | null;
  card: SystemCard;
  onSaved: (d: Department) => void;
  onCancel: () => void;
  onError: (e: unknown) => void;
}) {
  const [name, setName] = useState(department?.name ?? "");
  const [description, setDescription] = useState(department?.description ?? "");
  const [mission, setMission] = useState(department?.mission ?? "");
  const [policy, setPolicy] = useState(department?.policy ?? "");
  const [tools, setTools] = useState<string[]>(department?.tools ?? ["files"]);
  const [communicator, setCommunicator] = useState(department?.communicator_agent ?? card.agents[0]?.name ?? "");
  const [busy, setBusy] = useState(false);
  return (
    <form
      className="card pad project-form"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          onSaved(
            await api.saveDepartment(department?.id ?? null, {
              name,
              description,
              mission,
              policy,
              tools,
              communicator_agent: communicator,
            }),
          );
        } catch (err) {
          onError(err);
        } finally {
          setBusy(false);
        }
      }}
    >
      <h2>{department ? `Edit ${department.name}` : "New department"}</h2>
      <div className="grid-2">
        <label>
          Name
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Research" required />
        </label>
        <label>
          Guardrail policy for its agents
          <select value={policy} onChange={(e) => setPolicy(e.target.value)}>
            <option value="">Default for departments</option>
            {card.policies
              .filter((p) => p.name !== "communicator")
              .map((p) => (
                <option key={p.name} value={p.name}>
                  {p.name}
                </option>
              ))}
          </select>
        </label>
      </div>
      <label>
        Description
        <input value={description} onChange={(e) => setDescription(e.target.value)} />
      </label>
      <label>
        Mission (given to every agent in the department)
        <textarea rows={4} value={mission} onChange={(e) => setMission(e.target.value)} />
      </label>
      <fieldset className="card pad">
        <legend className="small muted">Tools and data its agents may use (messaging is always on)</legend>
        {TOOL_GROUPS.map((g) => (
          <label key={g.id} className="inline-check">
            <input
              type="checkbox"
              checked={tools.includes(g.id)}
              onChange={(e) => setTools(e.target.checked ? [...tools, g.id] : tools.filter((t) => t !== g.id))}
            />
            {g.label} <span className="muted small">— {g.help}</span>
          </label>
        ))}
      </fieldset>
      <label>
        Communicator runs
        <select value={communicator} onChange={(e) => setCommunicator(e.target.value)}>
          {card.agents.map((a) => (
            <option key={a.name} value={a.name}>
              {a.name}
            </option>
          ))}
        </select>
      </label>
      <p className="muted small">
        Every department has one communicator. It only has messaging tools and is the only way this department talks
        to others.
      </p>
      <div className="row">
        <button className="btn primary" disabled={busy || !name.trim()}>
          {department ? "Save" : "Create department"}
        </button>
        <button type="button" className="btn ghost" onClick={onCancel}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function scopeTone(m: OrgMessage) {
  return m.scope === "inter_department" ? "inter" : m.scope === "human" ? "human" : "internal";
}

/** Messages of the organisation or one department, with a composer. */
function Feed({
  org,
  dept,
  tick,
  canPost,
  onError,
}: {
  org: OrgOverview;
  dept?: Department;
  tick: number;
  canPost: boolean;
  onError: (e: unknown) => void;
}) {
  const [messages, setMessages] = useState<OrgMessage[] | null>(null);
  const [to, setTo] = useState("room");
  const [text, setText] = useState("");
  const end = useRef<HTMLDivElement>(null);
  const deptId = dept?.id;

  useEffect(() => {
    api
      .messages({ department: deptId, limit: 300 })
      .then(setMessages)
      .catch(onError);
  }, [deptId, tick, onError]);

  const count = messages?.length ?? 0;
  useEffect(() => {
    end.current?.scrollIntoView({ block: "nearest" });
  }, [count]);

  const targets = useMemo(() => {
    const list: { value: string; label: string; to: MessageTo }[] = [];
    if (dept) {
      list.push({ value: "room", label: `everyone in ${dept.name}`, to: { department: dept.id } });
      for (const a of dept.agents) list.push({ value: a.id, label: a.name, to: { agent: a.id } });
    } else {
      list.push({ value: "room", label: "all departments (every agent)", to: "all_departments" });
      for (const d of org.departments) list.push({ value: d.id, label: `everyone in ${d.name}`, to: { department: d.id } });
    }
    return list;
  }, [dept, org.departments]);

  const deptName = (id: string | null) => org.departments.find((d) => d.id === id)?.name;

  return (
    <section className="card feed">
      <div className="feed-head">
        <h3 className="section-title">{dept ? `${dept.name} room` : "All messages"}</h3>
        <span className="muted small">
          <span className="tag tone-internal">inside a department</span>{" "}
          <span className="tag tone-inter">between departments</span>{" "}
          <span className="tag tone-human">from a person</span>
        </span>
      </div>
      <div className="feed-list">
        {messages === null && <p className="muted small">Loading…</p>}
        {messages?.length === 0 && <p className="muted small">No messages yet.</p>}
        {messages?.map((m) => (
          <div key={m.id} className={`message ${scopeTone(m)}`}>
            <div className="bubble-head">
              <strong>{m.from_name}</strong>→<span>{m.to_name}</span>
              {!dept && m.scope === "inter_department" && (
                <span className="tag tone-inter">
                  {deptName(m.from_department)} → {deptName(m.to_department) ?? "all"}
                </span>
              )}
              <span className="muted">{new Date(m.created_at).toLocaleTimeString()}</span>
            </div>
            <div className="bubble-body">{m.text}</div>
          </div>
        ))}
        <div ref={end} />
      </div>
      {canPost && (
        <form
          className="composer-inline"
          onSubmit={async (e) => {
            e.preventDefault();
            const target = targets.find((t) => t.value === to) ?? targets[0];
            if (!target || !text.trim()) return;
            try {
              await api.postMessage(target.to, text.trim());
              setText("");
            } catch (err) {
              onError(err);
            }
          }}
        >
          <select value={to} onChange={(e) => setTo(e.target.value)} aria-label="Send to">
            {targets.map((t) => (
              <option key={t.value} value={t.value}>
                To {t.label}
              </option>
            ))}
          </select>
          <input value={text} onChange={(e) => setText(e.target.value)} placeholder="Write a message… (it wakes the agents)" />
          <button className="btn primary" disabled={!text.trim()}>
            Send
          </button>
        </form>
      )}
    </section>
  );
}

function Files({ dept, tick, onError }: { dept: Department; tick: number; onError: (e: unknown) => void }) {
  const [files, setFiles] = useState<DepartmentFile[]>([]);
  const [open, setOpen] = useState<{ path: string; content: string } | null>(null);
  useEffect(() => {
    api.departmentFiles(dept.id).then(setFiles).catch(onError);
  }, [dept.id, tick, onError]);
  return (
    <section className="card side-panel">
      <h3 className="section-title">Department files</h3>
      {files.length === 0 && <p className="muted small">No files yet.</p>}
      <ul className="file-list">
        {files.map((f) => (
          <li key={f.path}>
            <button
              className="link small"
              onClick={() =>
                open?.path === f.path ? setOpen(null) : api.departmentFile(dept.id, f.path).then(setOpen).catch(onError)
              }
            >
              {f.path}
            </button>
            <span className="muted small">
              {f.size} B · {f.updated_by}
            </span>
          </li>
        ))}
      </ul>
      {open && <pre className="mono-pre bubble-body">{open.content}</pre>}
    </section>
  );
}

/** What to add next: the growth path's next stage, departments that work
 * with the existing ones, and agents departments do not have yet. */
function Grow({
  me,
  card,
  tick,
  act,
  department,
  onPick,
  onError,
}: {
  me: Me;
  card: SystemCard;
  tick: number;
  act: (fn: () => Promise<unknown>) => Promise<void>;
  /** Only agent suggestions for this department. */
  department?: string;
  onPick: (templates: string[]) => void;
  onError: (e: unknown) => void;
}) {
  const [all, setAll] = useState<Suggestion[]>([]);
  useEffect(() => {
    api.suggestions().then(setAll).catch(onError);
  }, [tick, onError]);
  const list = department ? all.filter((s) => s.department_id === department) : all;
  if (list.length === 0) return null;
  const isAdmin = me.role === "admin";
  const canOperate = me.role !== "viewer";
  const agent = card.agents[0]?.name ?? "";

  const run = (s: Suggestion) => {
    if (s.kind === "agent" && s.agent && s.department_id) {
      const a = s.agent;
      return act(() => api.addAgent(s.department_id!, { name: a.name, agent, instructions: a.instructions }));
    }
    return act(() => api.build({ departments: s.templates, size: "lean", agent }));
  };

  return (
    <section className={`card side-panel grow ${department ? "compact" : ""}`}>
      <h3 className="section-title">{department ? "Grow this department" : "Grow your organisation"}</h3>
      <ul className="suggestions">
        {list.slice(0, department ? 3 : 8).map((s) => {
          const allowed = s.kind === "agent" ? canOperate : isAdmin;
          return (
            <li key={`${s.kind}-${s.title}`} className={`suggestion ${s.kind}`}>
              <div className="suggestion-text">
                <strong>{s.title}</strong>
                <span className="muted small">{s.reason}</span>
                {s.blocked && <span className="small attention-text">{s.blocked}</span>}
              </div>
              {allowed && (
                <div className="row">
                  <button className="btn small-btn primary" disabled={!!s.blocked} onClick={() => run(s)}>
                    {s.kind === "stage" ? "Add stage" : "Add"}
                  </button>
                  {s.kind !== "agent" && (
                    <button className="link small" onClick={() => onPick(s.templates)}>
                      review first
                    </button>
                  )}
                </div>
              )}
            </li>
          );
        })}
      </ul>
    </section>
  );
}
