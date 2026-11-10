import { useEffect, useState } from "react";
import { api } from "../api";
import type { Department, Me, Suggestion, SystemCard } from "../types";
import { type AgentChoice, AgentModelPicker, useModelChoices } from "./AgentModelPicker";
import { AGENT_NAME, withRole } from "./AddDepartment";

/** What every way of showing suggestions needs to act on one. */
export interface GrowActions {
  me: Me;
  card: SystemCard;
  act: (fn: () => Promise<unknown>) => Promise<void>;
  /** Open the builder with these templates picked, to look before adding. */
  onReview: (templates: string[]) => void;
  onError: (e: unknown) => void;
}

/** Whether this person may act on the suggestion: adding agents needs an operator, departments an admin. */
export function canAct(s: Suggestion, me: Me) {
  return s.kind === "agent" ? me.role !== "viewer" : me.role === "admin";
}

const key = (s: Suggestion) => `${s.kind}-${s.title}`;
/** The open item in a room when it is the person's own agent. */
const OWN = "own-agent";

/** The suggestions for the organisation, fetched again whenever `tick` moves. */
export function useSuggestions(tick: number, onError: (e: unknown) => void) {
  const [all, setAll] = useState<Suggestion[]>([]);
  useEffect(() => {
    api.suggestions().then(setAll).catch(onError);
  }, [tick, onError]);
  return all;
}

/** One suggestion: what and why, and the buttons that act on it. */
export function SuggestionItem({ s, onCancel, ...g }: GrowActions & { s: Suggestion; onCancel?: () => void }) {
  const [picking, setPicking] = useState(false);
  const agent = g.card.agents[0]?.name ?? "";
  const add = () =>
    s.kind === "agent"
      ? setPicking(true)
      : void g.act(() => api.build({ departments: s.templates, size: "lean", agent }));
  return (
    <li className={`suggestion ${s.kind}`}>
      <div className="suggestion-text">
        <strong>{s.title}</strong>
        <span className="muted small">{s.reason}</span>
        {s.blocked && <span className="small attention-text">{s.blocked}</span>}
      </div>
      {picking && s.agent && s.department_id ? (
        <AddSuggestedAgent s={s} {...g} onDone={() => setPicking(false)} />
      ) : (
        canAct(s, g.me) && (
          <div className="row">
            <button className="btn small-btn primary" disabled={!!s.blocked} onClick={add}>
              {s.kind === "stage" ? "Add stage" : "Add"}
            </button>
            {s.kind !== "agent" && (
              <button className="link small" onClick={() => g.onReview(s.templates)}>
                review first
              </button>
            )}
            {onCancel && (
              <button className="link small" onClick={onCancel}>
                cancel
              </button>
            )}
          </div>
        )
      )}
    </li>
  );
}

/** Adding a suggested agent asks which agent, provider and model it gets. */
function AddSuggestedAgent({ s, card, act, onError, onDone }: GrowActions & { s: Suggestion; onDone: () => void }) {
  const { providers, known } = useModelChoices(onError);
  const [choice, setChoice] = useState<AgentChoice>({ agent: card.agents[0]?.name ?? "", provider: "", model: "" });
  const a = s.agent!;
  return (
    <form
      className="provider-edit"
      onSubmit={(e) => {
        e.preventDefault();
        onDone();
        void act(() => api.addAgent(s.department_id!, { name: a.name, instructions: a.instructions, ...choice }));
      }}
    >
      <AgentModelPicker card={card} value={choice} onChange={setChoice} providers={providers} known={known} />
      <div className="row">
        <button className="btn small-btn primary">Add {a.name}</button>
        <button type="button" className="link small" onClick={onDone}>
          cancel
        </button>
      </div>
    </form>
  );
}

/** The agents a department's template has and the department does not, on its own page. */
export function GrowDepartment({ department, tick, ...g }: GrowActions & { department: string; tick: number }) {
  const list = useSuggestions(tick, g.onError).filter((s) => s.department_id === department);
  if (list.length === 0) return null;
  return (
    <section className="card side-panel grow compact">
      <h3 className="section-title">Grow this department</h3>
      <ul className="suggestions">
        {list.slice(0, 3).map((s) => (
          <SuggestionItem key={key(s)} s={s} {...g} />
        ))}
      </ul>
    </section>
  );
}

/** Short name for a suggestion in a room: the department or the role it adds. */
function label(s: Suggestion) {
  if (s.kind === "agent") return s.agent?.title.toLowerCase() ?? s.title;
  return s.title.replace(/^Add /, "");
}

/**
 * The room's way to grow, at its door: departments that usually work with it
 * and agents it does not have yet. Tapping one says why and offers to add it.
 */
export function RoomGrow({ dept, maxAgents, list, ...g }: GrowActions & { dept: Department; maxAgents: number; list: Suggestion[] }) {
  const [open, setOpen] = useState<string | null>(null);
  const usable = list.filter((s) => canAct(s, g.me));
  const workers = dept.agents.filter((a) => a.kind === "worker").length;
  const ownAgent = g.me.role !== "viewer" && workers < maxAgents;
  if (usable.length === 0 && !ownAgent) return null;
  const current = usable.find((s) => key(s) === open);
  return (
    <div className="room-door room-grow">
      <span className="muted small">Grow</span>
      {usable.map((s) => (
        <button
          key={key(s)}
          className={`chip grow-chip ${s.kind} ${open === key(s) ? "active" : ""}`}
          aria-expanded={open === key(s)}
          title={s.kind === "agent" ? "An agent this department does not have yet" : "A department that usually works with this one"}
          onClick={() => setOpen(open === key(s) ? null : key(s))}
        >
          + {label(s)}
        </button>
      ))}
      {ownAgent && (
        <button
          className={`chip grow-chip own ${open === OWN ? "active" : ""}`}
          aria-expanded={open === OWN}
          title="Add an agent with your own name and instructions"
          onClick={() => setOpen(open === OWN ? null : OWN)}
        >
          + Your own agent
        </button>
      )}
      {current && (
        <ul className="suggestions room-grow-detail">
          <SuggestionItem s={current} {...g} onCancel={() => setOpen(null)} />
        </ul>
      )}
      {open === OWN && ownAgent && (
        <div className="room-grow-detail">
          <AddAgent
            dept={dept}
            card={g.card}
            onDone={() => {
              setOpen(null);
              void g.act(async () => {});
            }}
            onError={g.onError}
          />
        </div>
      )}
    </div>
  );
}

/** A member the person makes up themselves: name, agent, model and instructions. */
export function AddAgent({ dept, card, onDone, onError }: { dept: Department; card: SystemCard; onDone: () => void; onError: (e: unknown) => void }) {
  const [name, setName] = useState("");
  const [choice, setChoice] = useState<AgentChoice>({ agent: card.agents[0]?.name ?? "", provider: "", model: "" });
  const [role, setRole] = useState("");
  const [instructions, setInstructions] = useState("");
  const [busy, setBusy] = useState(false);
  const { providers, known } = useModelChoices(onError);
  return (
    <form
      className="card pad provider-edit"
      style={{ marginBottom: 10 }}
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          await api.addAgent(dept.id, { name: name.trim().toLowerCase(), instructions: withRole(role, instructions), ...choice });
          onDone();
        } catch (err) {
          onError(err);
        } finally {
          setBusy(false);
        }
      }}
    >
      <label>
        Name (how colleagues address it)
        <input value={name} onChange={(e) => setName(e.target.value)} placeholder="analyst" pattern={AGENT_NAME} required />
      </label>
      <label>
        Role
        <input value={role} onChange={(e) => setRole(e.target.value)} placeholder="Market analyst" />
      </label>
      <AgentModelPicker card={card} value={choice} onChange={setChoice} providers={providers} known={known} />
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
