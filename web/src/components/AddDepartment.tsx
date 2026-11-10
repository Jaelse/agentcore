import { useEffect, useMemo, useState } from "react";
import { api } from "../api";
import { OPEN_ROUTING, type DepartmentTemplate, type OrgOverview, type SystemCard, type TemplateCatalog } from "../types";
import { type AgentChoice, AgentModelPicker, useModelChoices } from "./AgentModelPicker";

/** How colleagues address an agent: lower case, digits, `-` and `_`. */
export const AGENT_NAME = "[a-z0-9][a-z0-9_\\-]{0,31}";

/** The role goes at the start of the instructions: that is what tells the agent its job. */
export function withRole(role: string, instructions: string) {
  const r = role.trim();
  const text = instructions.trim();
  if (!r) return text;
  return `You are the ${r} of this department.${text ? `\n\n${text}` : ""}`;
}

interface OwnAgent {
  key: number;
  name: string;
  role: string;
  instructions: string;
}

/** The first sentence, for a one-line summary of an agent's job. */
function firstSentence(text: string) {
  const first = text.trim().split(/(?<=\.)\s/)[0] ?? "";
  return first.endsWith(".") ? first : `${first}.`;
}

let nextKey = 0;
const blankAgent = (): OwnAgent => ({ key: nextKey++, name: "", role: "", instructions: "" });

/**
 * A new department on the floor: one of the catalogue's, with the agents you
 * want of it, or one entirely your own. Either way you can add agents with
 * roles you make up.
 */
export function AddDepartment({
  org,
  card,
  initialTemplate,
  onDone,
  onCancel,
  onError,
}: {
  org: OrgOverview;
  card: SystemCard;
  /** A catalogue department to start with, picked. */
  initialTemplate?: string;
  onDone: () => void;
  onCancel: () => void;
  onError: (e: unknown) => void;
}) {
  const [mode, setMode] = useState<"suggested" | "own">("suggested");
  const [catalog, setCatalog] = useState<TemplateCatalog | null>(null);
  const [picked, setPicked] = useState<DepartmentTemplate | null>(null);
  // The picked template's agents to leave out.
  const [skipped, setSkipped] = useState<Set<string>>(new Set());
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [mission, setMission] = useState("");
  const [own, setOwn] = useState<OwnAgent[]>([]);
  const [choice, setChoice] = useState<AgentChoice>({ agent: card.agents[0]?.name ?? "", provider: "", model: "" });
  const { providers, known } = useModelChoices(onError);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.templates().then(setCatalog).catch(onError);
  }, [onError]);
  useEffect(() => {
    const t = initialTemplate && catalog?.departments.find((d) => d.id === initialTemplate);
    if (t) pick(t);
    // Only when the catalogue arrives.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [catalog]);

  // The catalogue's departments the organisation does not have yet, by category.
  const groups = useMemo(() => {
    if (!catalog) return [];
    const have = new Set(org.departments.map((d) => d.template));
    return catalog.categories
      .map((c) => ({ ...c, departments: catalog.departments.filter((d) => d.category === c.id && !have.has(d.id)) }))
      .filter((c) => c.departments.length > 0);
  }, [catalog, org.departments]);

  // Templates name the company the way the server does when it creates the department.
  const fill = (text: string) => text.replaceAll("{company}", org.profile.company_name.trim() || "the company");
  const full = org.departments.length >= org.settings.max_departments;
  const kept = mode === "suggested" && picked ? picked.agents.filter((a) => !skipped.has(a.name)) : [];
  const ownAgents = own.filter((a) => a.name.trim());
  const total = kept.length + ownAgents.length;
  const tooMany = total > org.settings.max_agents_per_department;
  const names = [...kept.map((a) => a.name), ...ownAgents.map((a) => a.name.trim().toLowerCase())];
  const duplicate = names.find((n, i) => names.indexOf(n) !== i);
  const ready = !full && !tooMany && !duplicate && (mode === "suggested" ? !!picked : !!name.trim());

  const pick = (t: DepartmentTemplate) => {
    setPicked(t);
    // Start from the template's core team; the rest are a tick away.
    setSkipped(new Set(t.agents.filter((a) => !a.core).map((a) => a.name)));
  };

  const create = async () => {
    setBusy(true);
    try {
      let id: string;
      if (mode === "suggested" && picked) {
        const r = await api.build({ departments: [picked.id], size: "full", agent: choice.agent });
        for (const e of r.errors ?? []) onError(new Error(`${e.department}${e.agent ? `/${e.agent}` : ""}: ${e.error}`));
        const made = r.created?.[0];
        if (!made) throw new Error(`${picked.name} was not created`);
        id = made.id;
        // The build makes the whole team; take out the ones left unticked, and give the rest the chosen model.
        const now = (await api.org()).departments.find((d) => d.id === id);
        for (const a of now?.agents ?? []) {
          if (skipped.has(a.name)) await api.deleteAgent(a.id);
          else if (choice.provider || choice.model) await api.updateAgent(a.id, { provider: choice.provider, model: choice.model });
        }
      } else {
        const d = await api.saveDepartment(null, {
          name: name.trim(),
          description: description.trim(),
          mission: mission.trim(),
          policy: "",
          tools: ["files"],
          routing: OPEN_ROUTING,
          project_id: null,
          role: null,
          requires_repository: false,
        });
        id = d.id;
      }
      for (const a of ownAgents) {
        await api.addAgent(id, { name: a.name.trim().toLowerCase(), instructions: withRole(a.role, a.instructions), ...choice });
      }
      onDone();
    } catch (err) {
      onError(err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="session add-department">
      <section className="card session-header">
        <div className="session-header-main">
          <h2>Add a department</h2>
          <p className="muted">
            Take one from the catalogue with the agents you want, or make your own. Either way you can add agents with
            any role you like.
          </p>
        </div>
        <div className="session-actions">
          <button className="btn ghost" onClick={onCancel}>
            Close
          </button>
        </div>
      </section>

      {full && (
        <p className="card pad attention-text">
          The organisation is limited to {org.settings.max_departments} departments; raise the limit in the sidebar first.
        </p>
      )}

      <div className="choice-grid two">
        <button className={`card choice ${mode === "suggested" ? "active" : ""}`} onClick={() => setMode("suggested")}>
          <strong>From the catalogue</strong>
          <span className="muted small">A ready department with its mission and agents.</span>
        </button>
        <button className={`card choice ${mode === "own" ? "active" : ""}`} onClick={() => setMode("own")}>
          <strong>Your own</strong>
          <span className="muted small">Name it, give it a mission, and put your own agents in it.</span>
        </button>
      </div>

      {mode === "suggested" && picked && (
        <div className="add-dept-picked">
          <span className="muted small">From the catalogue:</span> <strong>{picked.name}</strong>{" "}
          <span className="muted small">{picked.summary}</span>{" "}
          <button className="link small" onClick={() => setPicked(null)}>
            choose another
          </button>
        </div>
      )}

      {mode === "suggested" &&
        !picked &&
        (!catalog ? (
          <p className="muted">Loading the catalogue…</p>
        ) : groups.length === 0 ? (
          <p className="muted">The organisation already has every department of the catalogue.</p>
        ) : (
          groups.map((g) => (
            <section key={g.id} className="add-dept-group">
              <h3 className="section-title">{g.title}</h3>
              <div className="template-grid">
                {g.departments.map((t) => (
                  <button key={t.id} className="card choice" onClick={() => pick(t)}>
                    <strong>{t.name}</strong>
                    <span className="muted small">{t.summary}</span>
                    <span className="small">{t.agents.map((a) => a.title).join(" · ")}</span>
                  </button>
                ))}
              </div>
            </section>
          ))
        ))}

      {mode === "suggested" && picked && (
        <section className="card pad add-dept-team">
          <h3 className="section-title">{picked.name}: its agents</h3>
          <p className="muted small">{fill(picked.mission)}</p>
          <ul className="add-dept-agents">
            {picked.agents.map((a) => (
              <li key={a.name}>
                <label className="inline-check">
                  <input
                    type="checkbox"
                    checked={!skipped.has(a.name)}
                    onChange={(e) => {
                      const next = new Set(skipped);
                      if (e.target.checked) next.delete(a.name);
                      else next.add(a.name);
                      setSkipped(next);
                    }}
                  />
                  <span>
                    <strong>{a.title}</strong> <code className="small">{a.name}</code>
                    <br />
                    <span className="muted small">{firstSentence(fill(a.instructions))}</span>
                  </span>
                </label>
              </li>
            ))}
          </ul>
        </section>
      )}

      {mode === "own" && (
        <section className="card pad project-form">
          <label>
            Name
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Brand studio" required />
          </label>
          <label>
            Description
            <input value={description} onChange={(e) => setDescription(e.target.value)} placeholder="What it is for, in one line" />
          </label>
          <label>
            Mission
            <textarea
              rows={3}
              value={mission}
              onChange={(e) => setMission(e.target.value)}
              placeholder="What the department works towards, and how"
            />
          </label>
        </section>
      )}

      {(mode === "own" || picked) && (
        <section className="card pad add-dept-own">
          <div className="add-dept-head">
            <h3 className="section-title">{mode === "own" ? "Agents" : "Your own agents"}</h3>
            <button className="btn small-btn" onClick={() => setOwn([...own, blankAgent()])}>
              + Add an agent
            </button>
          </div>
          {own.length === 0 && (
            <p className="muted small">
              {mode === "own" ? "No agents yet. Add one for every role the department needs." : "Add agents with roles the template does not have."}
            </p>
          )}
          {own.map((a, i) => {
            const set = (patch: Partial<OwnAgent>) => setOwn(own.map((x, j) => (j === i ? { ...x, ...patch } : x)));
            return (
              <div key={a.key} className="own-agent">
                <div className="grid-2">
                  <label>
                    Name (how colleagues address it)
                    <input value={a.name} onChange={(e) => set({ name: e.target.value })} placeholder="illustrator" pattern={AGENT_NAME} />
                  </label>
                  <label>
                    Role
                    <input value={a.role} onChange={(e) => set({ role: e.target.value })} placeholder="Brand illustrator" />
                  </label>
                </div>
                <label>
                  Instructions
                  <textarea
                    rows={2}
                    value={a.instructions}
                    onChange={(e) => set({ instructions: e.target.value })}
                    placeholder="What it does, and how it works with its colleagues"
                  />
                </label>
                <button className="link small" onClick={() => setOwn(own.filter((_, j) => j !== i))}>
                  remove
                </button>
              </div>
            );
          })}
        </section>
      )}

      {(mode === "own" || picked) && (
        <section className="card pad">
          <h3 className="section-title">Every new agent runs</h3>
          <AgentModelPicker card={card} value={choice} onChange={setChoice} providers={providers} known={known} />
          {tooMany && (
            <p className="small attention-text">
              A department can have at most {org.settings.max_agents_per_department} agents; this one would have {total}.
            </p>
          )}
          {duplicate && <p className="small attention-text">Two agents are called `{duplicate}`; names must differ.</p>}
          <div className="row">
            <button className="btn primary" disabled={busy || !ready} onClick={create}>
              {busy
                ? "Creating…"
                : `Create ${mode === "suggested" ? picked!.name : name.trim() || "department"} · ${total} agent${total === 1 ? "" : "s"}`}
            </button>
          </div>
        </section>
      )}
    </div>
  );
}
