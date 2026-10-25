import { useEffect, useState } from "react";
import { api } from "../api";
import type { KnownModel, ProviderInfo, ProviderKind, SystemCard } from "../types";

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
