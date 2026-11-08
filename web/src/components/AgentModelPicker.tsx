import { useEffect, useState } from "react";
import { api } from "../api";
import type { KnownModel, OrgAgent, ProviderInfo, ProviderKind, SystemCard } from "../types";

export interface AgentChoice {
  agent: string;
  /** Empty: the agent's own provider. */
  provider: string;
  /** Empty: the agent's own model. */
  model: string;
}

/** Enabled model providers and the models worth suggesting for each kind. */
export function useModelChoices(onError: (e: unknown) => void) {
  const [providers, setProviders] = useState<ProviderInfo[]>([]);
  const [known, setKnown] = useState<Partial<Record<ProviderKind, KnownModel[]>>>({});
  useEffect(() => {
    api
      .providers()
      .then((p) => setProviders(p.filter((x) => x.enabled)))
      .catch(onError);
    api
      .agentCatalog()
      .then((c) => setKnown(c.provider_models ?? {}))
      .catch(onError);
  }, [onError]);
  return { providers, known };
}

/** Which agent a new department member runs, and on which provider and model. */
export function AgentModelPicker({
  card,
  value,
  onChange,
  providers,
  known,
}: {
  card: SystemCard;
  value: AgentChoice;
  onChange: (next: AgentChoice) => void;
  providers: ProviderInfo[];
  known: Partial<Record<ProviderKind, KnownModel[]>>;
}) {
  const chosen = providers.find((p) => p.name === value.provider);
  const suggestions = (chosen && known[chosen.kind]) || [];
  const listId = `models-${chosen?.kind ?? "none"}`;
  return (
    <div className="grid-2">
      <label>
        Runs
        <select value={value.agent} onChange={(e) => onChange({ ...value, agent: e.target.value })}>
          {card.agents.map((a) => (
            <option key={a.name} value={a.name}>
              {a.name}
            </option>
          ))}
        </select>
      </label>
      <label>
        Model provider
        <select
          value={value.provider}
          onChange={(e) => {
            const next = providers.find((p) => p.name === e.target.value);
            // A model of the previous provider rarely exists on the next.
            onChange({ ...value, provider: e.target.value, model: (next && known[next.kind]?.[0]?.id) || "" });
          }}
        >
          <option value="">Agent default</option>
          {providers.map((p) => (
            <option key={p.name} value={p.name}>
              {p.name} ({p.kind})
            </option>
          ))}
        </select>
      </label>
      <label>
        Model
        <input
          value={value.model}
          list={listId}
          onChange={(e) => onChange({ ...value, model: e.target.value })}
          placeholder="Agent default"
        />
        <datalist id={listId}>
          {suggestions.map((m) => (
            <option key={m.id} value={m.id}>
              {m.description}
            </option>
          ))}
        </datalist>
      </label>
    </div>
  );
}

/** Whether a provider's allowed models (globs; none: any) include a model. */
function allows(provider: ProviderInfo, model: string) {
  if (provider.allowed_models.length === 0) return true;
  return provider.allowed_models.some((glob) => {
    const pattern = glob.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*").replace(/\?/g, ".");
    return new RegExp(`^${pattern}$`).test(model);
  });
}

/** Point a department agent at another existing provider (and model). */
export function ProviderSwitch({
  agent,
  providers,
  known,
  reason,
  startAfter,
  onSave,
  onCancel,
}: {
  agent: OrgAgent;
  providers: ProviderInfo[];
  known: Partial<Record<ProviderKind, KnownModel[]>>;
  /** Why it is asked, e.g. the agent's provider was deleted. */
  reason?: string;
  /** Saving also starts the agent. */
  startAfter: boolean;
  onSave: (provider: string, model: string) => Promise<void>;
  onCancel: () => void;
}) {
  const route = agent.model_route;
  // The agent's model where the provider allows it, else one it suggests.
  const modelFor = (p: ProviderInfo | undefined) => {
    if (!p) return route?.model ?? "";
    if (route?.model && allows(p, route.model)) return route.model;
    return known[p.kind]?.find((m) => allows(p, m.id))?.id ?? "";
  };
  const first = providers.find((p) => p.name === route?.provider) ?? providers[0];
  const [provider, setProvider] = useState(first?.name ?? "");
  const [model, setModel] = useState(() => modelFor(first));
  const [busy, setBusy] = useState(false);
  const chosen = providers.find((p) => p.name === provider);
  const suggestions = (chosen && known[chosen.kind]) || [];
  const listId = `switch-models-${agent.id}`;
  return (
    <form
      className="provider-edit provider-switch"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          await onSave(provider, model.trim());
        } finally {
          setBusy(false);
        }
      }}
    >
      {reason && <p className="small attention-text">{reason}</p>}
      {providers.length === 0 ? (
        <p className="small muted">No model provider is enabled. Add one under Settings → Model providers.</p>
      ) : (
        <>
          <label>
            Model provider
            <select
              value={provider}
              onChange={(e) => {
                setProvider(e.target.value);
                setModel(modelFor(providers.find((p) => p.name === e.target.value)));
              }}
            >
              {providers.map((p) => (
                <option key={p.name} value={p.name}>
                  {p.name} ({p.kind})
                </option>
              ))}
            </select>
          </label>
          <label>
            Model
            <input value={model} list={listId} onChange={(e) => setModel(e.target.value)} placeholder="model id" />
            <datalist id={listId}>
              {suggestions.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.description}
                </option>
              ))}
            </datalist>
          </label>
          {chosen && chosen.allowed_models.length > 0 && (
            <p className="small muted">
              {chosen.name} allows {chosen.allowed_models.join(", ")}
            </p>
          )}
        </>
      )}
      <div className="row">
        <button className="btn small-btn primary" disabled={busy || !provider}>
          {startAfter ? "Save and start" : "Save"}
        </button>
        <button type="button" className="link small" onClick={onCancel}>
          cancel
        </button>
      </div>
    </form>
  );
}
