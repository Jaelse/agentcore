import { useEffect, useMemo, useState } from "react";
import { ApiError, api } from "../api";
import type { Blueprint, BuildPlan, Me, OrgOverview, Project, SystemCard, TemplateCatalog } from "../types";

export type BuildMode = "small" | "stages" | "all" | "pick";

interface Props {
  me: Me;
  card: SystemCard;
  org: OrgOverview;
  initialMode?: BuildMode;
  /** Preselected department templates (pick mode). */
  initialPick?: string[];
  onDone: (createdDepartmentId: string | null) => void;
  onCancel?: () => void;
  onError: (err: unknown) => void;
}

const MODES: { id: BuildMode; title: string; text: string }[] = [
  {
    id: "small",
    title: "Start small",
    text: "One department with one or two agents. Add more when you need them; suggestions show what usually comes next.",
  },
  {
    id: "stages",
    title: "Grow in stages",
    text: "Pick a growth path for your kind of business. Create the first stage now and the next ones when you are ready.",
  },
  {
    id: "all",
    title: "Everything at once",
    text: "The complete organisation from day one: every stage of a path, with full teams.",
  },
  {
    id: "pick",
    title: "Pick departments",
    text: "Browse every business function and choose exactly the departments you want.",
  },
];

const LEVEL_LABEL: Record<Blueprint["level"], string> = {
  starter: "start small",
  growing: "grows in stages",
  complete: "complete",
};

/** A guided way to build an organisation from templates, small or big. */
export function OrgBuilder({ me, card, org, initialMode, initialPick, onDone, onCancel, onError }: Props) {
  const [catalog, setCatalog] = useState<TemplateCatalog | null>(null);
  const [step, setStep] = useState<"start" | "choose" | "review">(initialMode ? "choose" : "start");
  const [mode, setMode] = useState<BuildMode>(initialMode ?? "small");
  const [company, setCompany] = useState(org.profile.company_name);
  const [about, setAbout] = useState(org.profile.company_about);
  const [blueprint, setBlueprint] = useState<string | null>(org.profile.blueprint);
  const [stages, setStages] = useState(1);
  const [picked, setPicked] = useState<string[]>(initialPick ?? []);
  const [size, setSize] = useState<"lean" | "full">("lean");
  const [agent, setAgent] = useState(card.agents[0]?.name ?? "");
  const [startNow, setStartNow] = useState(false);
  const [goal, setGoal] = useState("");
  const [keepRunning, setKeepRunning] = useState(false);
  const [projects, setProjects] = useState<Project[]>([]);
  const [projectId, setProjectId] = useState<string>("");
  const [raise, setRaise] = useState(false);
  const [plan, setPlan] = useState<BuildPlan | null>(null);
  const [busy, setBusy] = useState(false);
  const isAdmin = me.role === "admin";

  useEffect(() => {
    api.templates().then(setCatalog).catch(onError);
    api
      .projects()
      .then((p) => setProjects(p))
      .catch(() => setProjects([]));
  }, [onError]);

  const bp = catalog?.blueprints.find((b) => b.id === blueprint) ?? null;
  const blueprints = useMemo(() => {
    const all = catalog?.blueprints ?? [];
    if (mode === "small") return all.filter((b) => b.level !== "complete");
    if (mode === "stages") return all.filter((b) => b.level !== "complete");
    return all;
  }, [catalog, mode]);

  // Sensible defaults per mode.
  const chooseMode = (m: BuildMode) => {
    setMode(m);
    setStep("choose");
    if (m === "small") {
      setSize("lean");
      setStages(1);
    } else if (m === "stages") {
      setSize("lean");
      setStages(1);
    } else if (m === "all") {
      setSize("full");
      setBlueprint("complete-company");
      setStages(99);
    }
  };

  const departments = useMemo(() => {
    if (mode === "pick") return picked;
    if (!bp) return [];
    return bp.stages.slice(0, mode === "small" ? 1 : stages).flatMap((s) => s.departments);
  }, [mode, picked, bp, stages]);

  const profile = { company_name: company.trim(), company_about: about.trim(), blueprint: mode === "pick" ? org.profile.blueprint : blueprint };

  useEffect(() => {
    if (step !== "review" || departments.length === 0 || !isAdmin) {
      setPlan(null);
      return;
    }
    let cancelled = false;
    api
      .build({ profile, departments, size, agent, project_id: projectId || null, dry_run: true })
      .then((r) => {
        if (!cancelled) setPlan(r.plan);
      })
      .catch(onError);
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [step, departments.join(","), size, agent, company, about, blueprint, projectId]);

  if (!catalog) return <div className="empty">Loading templates…</div>;
  const template = (id: string) => catalog.departments.find((d) => d.id === id);

  const create = async () => {
    setBusy(true);
    try {
      const r = await api.build({
        profile,
        departments,
        size,
        agent,
        project_id: projectId || null,
        start: startNow,
        goal: goal.trim() || undefined,
        check_ins: keepRunning,
        raise_limits: raise,
      });
      for (const e of r.errors ?? []) onError(new Error(`${e.department}${e.agent ? `/${e.agent}` : ""}: ${e.error}`));
      onDone(r.created?.[0]?.id ?? null);
    } catch (err) {
      if (err instanceof ApiError && err.status === 409) {
        onError(new Error(`${err.message}. Tick “raise the limits” or choose fewer departments.`));
      } else {
        onError(err);
      }
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="session builder">
      <section className="card session-header">
        <div className="session-header-main">
          <h2>{org.departments.length === 0 ? "Build your organisation" : "Grow your organisation"}</h2>
          <ol className="steps">
            {(["start", "choose", "review"] as const).map((s, i) => (
              <li key={s} className={step === s ? "active" : ""}>
                <button className="link" onClick={() => setStep(s)} disabled={s === "review" && departments.length === 0}>
                  {i + 1}. {s === "start" ? "How to start" : s === "choose" ? "Choose" : "Review & create"}
                </button>
              </li>
            ))}
          </ol>
        </div>
        {onCancel && (
          <div className="session-actions">
            <button className="btn ghost" onClick={onCancel}>
              Close
            </button>
          </div>
        )}
      </section>

      {!isAdmin && (
        <div className="banner warn-banner">
          You can browse the templates; an admin creates departments. Operators can add agents to existing departments
          from the suggestions.
        </div>
      )}

      {step === "start" && (
        <>
          <section className="card pad project-form">
            <h3 className="section-title">About your company</h3>
            <p className="muted small">Every agent gets this as context, and missions mention your company by name.</p>
            <div className="grid-2">
              <label>
                Company name
                <input value={company} onChange={(e) => setCompany(e.target.value)} placeholder="Acme" />
              </label>
              <label>
                What it does
                <input value={about} onChange={(e) => setAbout(e.target.value)} placeholder="We build accounting software for small shops." />
              </label>
            </div>
          </section>
          <h3 className="section-title">How do you want to start?</h3>
          <div className="choice-grid">
            {MODES.map((m) => (
              <button key={m.id} className={`card choice ${mode === m.id ? "active" : ""}`} onClick={() => chooseMode(m.id)}>
                <strong>{m.title}</strong>
                <span className="muted small">{m.text}</span>
              </button>
            ))}
          </div>
        </>
      )}

      {step === "choose" && mode !== "pick" && (
        <>
          <h3 className="section-title">
            {mode === "small" ? "What do you do? We start you with its first department." : "Choose a growth path"}
          </h3>
          <div className="choice-grid">
            {blueprints.map((b) => (
              <button
                key={b.id}
                className={`card choice ${blueprint === b.id ? "active" : ""}`}
                onClick={() => setBlueprint(b.id)}
              >
                <div className="session-item-top">
                  <strong>{b.title}</strong>
                  <span className="tag">{LEVEL_LABEL[b.level]}</span>
                </div>
                <span className="small">{b.audience}</span>
                <ol className="stage-list small muted">
                  {b.stages.map((s, i) => (
                    <li key={s.title} className={mode === "small" && i > 0 ? "later" : ""}>
                      <strong>{s.title}:</strong> {s.departments.map((d) => template(d)?.name ?? d).join(", ")}
                    </li>
                  ))}
                </ol>
              </button>
            ))}
          </div>
          {bp && mode !== "small" && (
            <section className="card pad">
              <label>
                Create now
                <select value={Math.min(stages, bp.stages.length)} onChange={(e) => setStages(Number(e.target.value))}>
                  {bp.stages.map((s, i) => (
                    <option key={s.title} value={i + 1}>
                      {i + 1 === bp.stages.length ? "All stages" : `Stages 1–${i + 1}`} (
                      {bp.stages.slice(0, i + 1).reduce((n, st) => n + st.departments.length, 0)} departments)
                    </option>
                  ))}
                </select>
              </label>
              <p className="muted small">The rest of the path is suggested later, one stage at a time.</p>
            </section>
          )}
          <div className="row">
            <button className="btn primary" disabled={!bp} onClick={() => setStep("review")}>
              Next: review
            </button>
            <button className="link small" onClick={() => setMode("pick")}>
              or pick departments yourself
            </button>
          </div>
        </>
      )}

      {step === "choose" && mode === "pick" && (
        <>
          {catalog.categories.map((c) => (
            <section key={c.id}>
              <h3 className="section-title">
                {c.title} <span className="muted small">· {c.description}</span>
              </h3>
              <div className="template-grid">
                {catalog.departments
                  .filter((d) => d.category === c.id)
                  .map((d) => {
                    const exists = org.departments.some(
                      (x) => x.template === d.id || x.name.toLowerCase() === d.name.toLowerCase(),
                    );
                    const on = picked.includes(d.id);
                    return (
                      <label key={d.id} className={`card template ${on ? "active" : ""} ${exists ? "exists" : ""}`}>
                        <div className="session-item-top">
                          <span className="inline-check">
                            <input
                              type="checkbox"
                              disabled={exists}
                              checked={on || exists}
                              onChange={(e) =>
                                setPicked(e.target.checked ? [...picked, d.id] : picked.filter((p) => p !== d.id))
                              }
                            />
                            <strong>{d.name}</strong>
                          </span>
                          {exists && <span className="tag tone-ok">you have it</span>}
                        </div>
                        <span className="small">{d.summary}</span>
                        <span className="muted small">When: {d.when_to_add.replace("{company}", company || "your company")}</span>
                        <span className="muted small">
                          Agents: {d.agents.map((a) => a.title + (a.core ? "" : " (full)")).join(", ")}
                        </span>
                      </label>
                    );
                  })}
              </div>
            </section>
          ))}
          <div className="row composer card">
            <span>{picked.length} selected</span>
            <button className="btn primary" disabled={picked.length === 0} onClick={() => setStep("review")}>
              Next: review
            </button>
          </div>
        </>
      )}

      {step === "review" && (
        <>
          <section className="card pad project-form">
            <div className="grid-2">
              <label>
                Team size per department
                <select value={size} onChange={(e) => setSize(e.target.value as "lean" | "full")}>
                  <option value="lean">Lean: the core agents (add more later)</option>
                  <option value="full">Full: every agent of the template</option>
                </select>
              </label>
              <label>
                Agents run
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
              Works on a GitHub repository
              <select value={projectId} onChange={(e) => setProjectId(e.target.value)}>
                <option value="">No repository</option>
                {projects.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.name} ({p.repo_owner}/{p.repo_name})
                  </option>
                ))}
              </select>
            </label>
            {projects.length === 0 ? (
              <p className="muted small">
                To let departments work on an app, create a project for its repository under Projects first.
              </p>
            ) : (
              projectId && (
                <p className="muted small">
                  Engineering-type departments get a checkout of the repository on their own branch and deliver pull
                  requests that a person reviews; product, support and marketing-type departments get GitHub tools
                  for issues and the board.
                </p>
              )
            )}
            <label>
              A goal for the organisation (optional)
              <input
                value={goal}
                onChange={(e) => setGoal(e.target.value)}
                placeholder="Reach 10 paying customers by March"
                maxLength={200}
              />
            </label>
            <label className="inline-check">
              <input type="checkbox" checked={keepRunning} onChange={(e) => setKeepRunning(e.target.checked)} /> Keep it
              running: a daily check-in for each new department (and a weekly review in Strategy) wakes the agents to
              work towards the goals
            </label>
            <label className="inline-check">
              <input type="checkbox" checked={startNow} onChange={(e) => setStartNow(e.target.checked)} /> Start the new
              departments right away
            </label>
          </section>

          {!isAdmin ? null : !plan ? (
            <p className="muted">Preparing the plan…</p>
          ) : (
            <>
              <div className="row">
                <strong>
                  {plan.new_departments} new department{plan.new_departments === 1 ? "" : "s"}, {plan.new_agents} agents
                </strong>
                <span className="muted small">(each department also gets a communicator)</span>
              </div>
              {!plan.fits && (
                <div className="banner warn-banner">
                  This needs room for {plan.needs.max_departments} departments and {plan.needs.max_agents_per_department}{" "}
                  agents per department; the limits are {plan.limits.max_departments} and{" "}
                  {plan.limits.max_agents_per_department}.
                  <label className="inline-check">
                    <input type="checkbox" checked={raise} onChange={(e) => setRaise(e.target.checked)} /> Raise the limits
                    to fit
                  </label>
                </div>
              )}
              <div className="template-grid">
                {plan.departments.map((d) => (
                  <div key={d.template} className={`card template ${d.exists ? "exists" : ""}`}>
                    <div className="session-item-top">
                      <strong>{d.name}</strong>
                      {d.exists ? <span className="tag tone-ok">already there</span> : <span className="tag">new</span>}
                    </div>
                    {!d.exists && projectId && d.role && (
                      <span className="small ok-text">
                        on the repository as {d.role}
                        {d.tools.includes("sandbox") ? " · own checkout, pull requests" : " · GitHub tools"}
                      </span>
                    )}
                    <span className="small">{d.description}</span>
                    {!d.exists && (
                      <ul className="checks small">
                        {d.agents.map((a) => (
                          <li key={a.name}>
                            <code>{a.name}</code> · {a.title}
                          </li>
                        ))}
                        <li className="muted">
                          <code>communicator</code> · talks to other departments
                        </li>
                      </ul>
                    )}
                  </div>
                ))}
              </div>
              <div className="row composer card">
                <button
                  className="btn primary"
                  disabled={busy || plan.new_departments === 0 || (!plan.fits && !raise)}
                  onClick={create}
                >
                  {busy ? "Creating…" : `Create ${plan.new_departments} department${plan.new_departments === 1 ? "" : "s"}`}
                </button>
                <span className="muted small">You can rename, edit, add or remove anything afterwards.</span>
              </div>
            </>
          )}
        </>
      )}
    </div>
  );
}
