import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "../api";
import type { AgentCheck, AgentEntry, CatalogAgent, Me, ProviderInfo, SystemCard } from "../types";

interface Props {
  me: Me;
  card: SystemCard;
  providers: ProviderInfo[];
  onChanged: () => void;
  onError: (err: unknown) => void;
}

const GUARDRAILS = {
  full: {
    label: "Full guardrails",
    tone: "tone-ok",
    help: "Its own side-effecting tools are off: every command and file change goes through your policy.",
  },
  sandbox: {
    label: "Sandbox only",
    tone: "tone-attention",
    help: "Works with its own tools inside the sandbox; the policy does not see each action. Model calls, the live terminal and the recording are still recorded.",
  },
} as const;

const DOMAIN_LABEL: Record<string, string> = {
  software: "Software",
  general: "General purpose",
  business: "Business",
  research: "Research",
};

/** Open-source agents: add them with a provider and a model, check they are installed. */
export function AgentCatalog({ me, card, providers, onChanged, onError }: Props) {
  const [catalog, setCatalog] = useState<CatalogAgent[] | null>(null);
  const [agents, setAgents] = useState<AgentEntry[]>([]);
  const [adding, setAdding] = useState<CatalogAgent | null>(null);
  const [checks, setChecks] = useState<Record<string, AgentCheck | "checking">>({});
  const [domain, setDomain] = useState<string>("all");
  const isAdmin = me.role === "admin";

  const load = useCallback(() => {
    api
      .agentCatalog()
      .then((c) => setCatalog(c.agents))
      .catch(onError);
    api.agentList().then(setAgents).catch(onError);
  }, [onError]);
  useEffect(load, [load]);

  const domains = useMemo(
    () => Array.from(new Set((catalog ?? []).flatMap((a) => a.domains))).sort(),
    [catalog],
  );

  const changed = () => {
    load();
    onChanged();
  };

  const check = async (name: string) => {
    setChecks((c) => ({ ...c, [name]: "checking" }));
    try {
      const result = await api.checkAgent(name);
      setChecks((c) => ({ ...c, [name]: result }));
    } catch (err) {
      setChecks((c) => {
        const { [name]: _, ...rest } = c;
        return rest;
      });
      onError(err);
    }
  };

  return (
    <section className="card pad agent-catalog">
      <h2>Agents</h2>
      <p className="muted">
        Agents are the programs that do the work inside a sandbox. Add popular open-source agents from the catalogue:
        choose which model provider and model they use, and agentcore wires them to its model gateway (your API keys
        never enter the sandbox) and, where the agent supports it, to its policy-checked tools.
      </p>

      <h3 className="section-title">Your agents</h3>
      <ul className="installed-agents">
        {agents.map((a) => {
          const result = checks[a.spec.name];
          return (
            <li key={a.spec.name} className="card installed-agent">
              <div className="provider-top">
                <div>
                  <strong className="provider-name">{a.spec.name}</strong>{" "}
                  {a.source === "config" ? (
                    <span className="tag">from configuration file</span>
                  ) : (
                    <span className="tag">catalogue: {a.catalog}</span>
                  )}
                  {!a.enabled && <span className="tag tone-bad">disabled</span>}
                  {a.shadowed_by_config && <span className="tag tone-attention">overridden by the configuration file</span>}
                </div>
                <div className="row">
                  <button className="btn small-btn" disabled={result === "checking"} onClick={() => check(a.spec.name)}>
                    {result === "checking" ? "Checking…" : "Check installation"}
                  </button>
                  {isAdmin && a.source === "catalog" && (
                    <>
                      <button
                        className="btn small-btn"
                        onClick={async () => {
                          try {
                            await api.updateAgentInstall(a.spec.name, { enabled: !a.enabled });
                            changed();
                          } catch (err) {
                            onError(err);
                          }
                        }}
                      >
                        {a.enabled ? "Disable" : "Enable"}
                      </button>
                      <button
                        className="btn small-btn ghost danger-text"
                        onClick={async () => {
                          if (!confirm(`Remove ${a.spec.name}? Running sessions are not affected.`)) return;
                          try {
                            await api.uninstallAgent(a.spec.name);
                            changed();
                          } catch (err) {
                            onError(err);
                          }
                        }}
                      >
                        Remove
                      </button>
                    </>
                  )}
                </div>
              </div>
              <div className="muted small">
                {a.spec.description}
                {a.spec.model && (
                  <>
                    {" · "}model <code>{a.spec.model}</code> via <code>{a.spec.provider}</code>
                  </>
                )}
                {a.spec.policy && (
                  <>
                    {" · "}policy <code>{a.spec.policy}</code>
                  </>
                )}
              </div>
              {isAdmin && a.source === "catalog" && (
                <ModelEditor entry={a} providers={providers} onSaved={changed} onError={onError} />
              )}
              {result && result !== "checking" && (
                <div className={`small ${result.available ? "ok-text" : "attention-text"}`}>
                  {result.available
                    ? `✓ ${result.program} found in the ${result.backend} sandbox (${result.path})${result.version ? ` · ${result.version}` : ""}`
                    : `✗ ${result.hint}`}
                </div>
              )}
            </li>
          );
        })}
      </ul>

      <div className="row" style={{ justifyContent: "space-between", marginTop: 16 }}>
        <h3 className="section-title">Open-source catalogue</h3>
        <div className="row small">
          {["all", ...domains].map((d) => (
            <button key={d} className={`chip ${domain === d ? "ok" : ""}`} onClick={() => setDomain(d)}>
              {d === "all" ? "All" : (DOMAIN_LABEL[d] ?? d)}
            </button>
          ))}
        </div>
      </div>
      <p className="muted small">
        Only agents under permissive open-source licenses (MIT, Apache-2.0, BSD) are listed. agentcore does not include
        or modify them: the sandbox image installs them from their official packages. See{" "}
        <code>docs/AGENT_CATALOG.md</code>.
      </p>
      {catalog === null ? (
        <p className="muted">Loading…</p>
      ) : (
        <div className="template-grid">
          {catalog
            .filter((a) => domain === "all" || a.domains.includes(domain))
            .map((a) => {
              const g = GUARDRAILS[a.guardrails];
              return (
                <div key={a.id} className="card template catalog-agent">
                  <div className="session-item-top">
                    <strong>{a.name}</strong>
                    <a className="tag" href={a.license_url} target="_blank" rel="noreferrer" title="License">
                      {a.license}
                    </a>
                  </div>
                  <span className="muted small">{a.vendor}</span>
                  <span className="small">{a.summary}</span>
                  <div className="kanban-meta">
                    <span className={`tag ${g.tone}`} title={g.help}>
                      {g.label}
                    </span>
                    {a.domains.map((d) => (
                      <span key={d} className="tag">
                        {DOMAIN_LABEL[d] ?? d}
                      </span>
                    ))}
                    <span className="tag" title="Model protocols it can use">
                      {a.protocols.join(" · ")}
                    </span>
                    {!a.conversation && (
                      <span className="tag" title="Runs once per task; cannot be woken by messages">
                        single run
                      </span>
                    )}
                  </div>
                  <details className="small">
                    <summary>Details</summary>
                    <p>{a.description}</p>
                    <p className="muted">
                      {a.package} · verified with {a.verified_version} ·{" "}
                      <a href={a.repository} target="_blank" rel="noreferrer">
                        source
                      </a>{" "}
                      ·{" "}
                      <a href={a.homepage} target="_blank" rel="noreferrer">
                        website
                      </a>
                    </p>
                  </details>
                  {a.installed.length > 0 && <span className="small ok-text">Added as {a.installed.join(", ")}</span>}
                  {isAdmin && (
                    <button className="btn small-btn primary" onClick={() => setAdding(a)}>
                      {a.installed.length > 0 ? "Add another" : "Add"}
                    </button>
                  )}
                </div>
              );
            })}
        </div>
      )}
      {adding && (
        <AddAgentDialog
          entry={adding}
          card={card}
          providers={providers}
          taken={agents.map((a) => a.spec.name)}
          onClose={() => setAdding(null)}
          onAdded={(name) => {
            setAdding(null);
            changed();
            void check(name);
          }}
          onError={onError}
        />
      )}
    </section>
  );
}

function ModelEditor({
  entry,
  providers,
  onSaved,
  onError,
}: {
  entry: AgentEntry;
  providers: ProviderInfo[];
  onSaved: () => void;
  onError: (e: unknown) => void;
}) {
  const [open, setOpen] = useState(false);
  const [provider, setProvider] = useState(entry.spec.provider ?? "");
  const [model, setModel] = useState(entry.spec.model ?? "");
  if (!open) {
    return (
      <button className="link small" style={{ justifySelf: "start" }} onClick={() => setOpen(true)}>
        change model
      </button>
    );
  }
  return (
    <form
      className="row"
      onSubmit={async (e) => {
        e.preventDefault();
        try {
          await api.updateAgentInstall(entry.spec.name, { provider, model });
          setOpen(false);
          onSaved();
        } catch (err) {
          onError(err);
        }
      }}
    >
      <select value={provider} onChange={(e) => setProvider(e.target.value)} style={{ width: "auto" }}>
        {providers.map((p) => (
          <option key={p.name} value={p.name}>
            {p.name}
          </option>
        ))}
      </select>
      <input value={model} onChange={(e) => setModel(e.target.value)} style={{ width: 220 }} />
      <button className="btn small-btn primary">Save</button>
      <button type="button" className="link small" onClick={() => setOpen(false)}>
        cancel
      </button>
    </form>
  );
}

function AddAgentDialog({
  entry,
  card,
  providers,
  taken,
  onClose,
  onAdded,
  onError,
}: {
  entry: CatalogAgent;
  card: SystemCard;
  providers: ProviderInfo[];
  taken: string[];
  onClose: () => void;
  onAdded: (name: string) => void;
  onError: (e: unknown) => void;
}) {
  const usable = providers.filter((p) => p.enabled && entry.protocols.includes(p.kind));
  const firstName = taken.includes(entry.id) ? `${entry.id}-2` : entry.id;
  const [name, setName] = useState(firstName);
  const [provider, setProvider] = useState(usable[0]?.name ?? "");
  const [model, setModel] = useState(entry.suggested_models[0] ?? "");
  const [policy, setPolicy] = useState("");
  const [image, setImage] = useState("");
  const [busy, setBusy] = useState(false);
  const chosen = usable.find((p) => p.name === provider);

  return (
    <dialog
      className="dialog"
      ref={(el) => {
        if (el && !el.open) el.showModal();
      }}
      onClose={onClose}
    >
      <header className="dialog-header">
        <h2>Add {entry.name}</h2>
        <button className="btn ghost" onClick={onClose}>
          Close
        </button>
      </header>
      {usable.length === 0 ? (
        <div className="banner warn-banner">
          {entry.name} needs a model provider of type {entry.protocols.join(" or ")}. Add one under Model providers
          above first.
        </div>
      ) : (
        <form
          className="deliver-form"
          onSubmit={async (e) => {
            e.preventDefault();
            setBusy(true);
            try {
              await api.installAgent({
                catalog: entry.id,
                name: name.trim(),
                provider,
                model: model.trim(),
                policy: policy || undefined,
                image: image.trim() || undefined,
              });
              onAdded(name.trim());
            } catch (err) {
              onError(err);
            } finally {
              setBusy(false);
            }
          }}
        >
          <div className="grid-2">
            <label>
              Name (shown when starting agents)
              <input value={name} onChange={(e) => setName(e.target.value)} pattern="[a-z0-9][a-z0-9_\-]{0,62}" required />
            </label>
            <label>
              Model provider
              <select value={provider} onChange={(e) => setProvider(e.target.value)}>
                {usable.map((p) => (
                  <option key={p.name} value={p.name}>
                    {p.name} ({p.kind})
                  </option>
                ))}
              </select>
            </label>
            <label>
              Model
              <input value={model} onChange={(e) => setModel(e.target.value)} list={`models-${entry.id}`} required />
              <datalist id={`models-${entry.id}`}>
                {entry.suggested_models.map((m) => (
                  <option key={m} value={m} />
                ))}
              </datalist>
            </label>
            <label>
              Guardrail policy
              <select value={policy} onChange={(e) => setPolicy(e.target.value)}>
                <option value="">Server default ({card.default_policy})</option>
                {card.policies.map((p) => (
                  <option key={p.name} value={p.name}>
                    {p.name}
                  </option>
                ))}
              </select>
            </label>
          </div>
          {chosen && chosen.allowed_models.length > 0 && (
            <p className="muted small">This provider allows: {chosen.allowed_models.join(", ")}</p>
          )}
          <details className="small">
            <summary>Advanced: sandbox image</summary>
            <label>
              Image with {entry.name} installed (empty: the default sandbox image)
              <input value={image} onChange={(e) => setImage(e.target.value)} placeholder="agentcore-sandbox:full" />
            </label>
          </details>
          <p className="muted small">
            {GUARDRAILS[entry.guardrails].help} The sandbox image must include {entry.name}: build it with{" "}
            <code>--build-arg AGENTS="{entry.id}"</code> (or <code>AGENTS=all</code>). Use “Check installation” after
            adding.
          </p>
          <button className="btn primary" disabled={busy || !name.trim() || !model.trim()}>
            {busy ? "Adding…" : `Add ${entry.name}`}
          </button>
        </form>
      )}
    </dialog>
  );
}
