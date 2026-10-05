import { useCallback, useEffect, useState } from "react";
import { api } from "../api";
import type { AdminEvent, Me, ProviderInfo, ProviderKind } from "../types";

const DEFAULT_URL: Record<ProviderKind, string> = {
  anthropic: "https://api.anthropic.com",
  openai: "https://api.openai.com",
};

const splitModels = (text: string) =>
  text
    .split(/[\s,]+/)
    .map((m) => m.trim())
    .filter(Boolean);

export function Settings({ me, onError }: { me: Me; onError: (err: unknown) => void }) {
  const [providers, setProviders] = useState<ProviderInfo[] | null>(null);
  const [history, setHistory] = useState<AdminEvent[]>([]);
  const isAdmin = me.role === "admin";

  const refresh = useCallback(() => {
    api.providers().then(setProviders).catch(onError);
    if (isAdmin) api.adminEvents().then(setHistory).catch(onError);
  }, [isAdmin, onError]);

  useEffect(refresh, [refresh]);

  return (
    <div className="settings">
      <section className="card pad">
        <h2>Model providers</h2>
        <p className="muted">
          Agents reach language models only through the agentcore <strong>model gateway</strong>. API keys are stored
          encrypted and never enter a sandbox; agents authenticate with a per-session token instead. Every call is
          logged and stops when the session is stopped.
        </p>
        {!isAdmin && <p className="muted small">Only admins can change providers.</p>}
        {providers === null ? (
          <p className="muted">Loading…</p>
        ) : providers.length === 0 ? (
          <div className="banner warn-banner">
            No provider configured yet: agents cannot reach a model. {isAdmin && "Add one below."}
          </div>
        ) : (
          <div className="provider-list">
            {providers.map((p) => (
              <ProviderCard key={p.name} provider={p} canEdit={isAdmin} onChanged={refresh} onError={onError} />
            ))}
          </div>
        )}
      </section>

      {isAdmin && <AddProvider existing={providers ?? []} onAdded={refresh} onError={onError} />}

      {isAdmin && (
        <section className="card pad">
          <h2>Change history</h2>
          {history.length === 0 ? (
            <p className="muted">No configuration changes yet.</p>
          ) : (
            <ul className="history">
              {history.map((e) => (
                <li key={e.id}>
                  <time className="muted small">{new Date(e.at).toLocaleString()}</time>{" "}
                  <strong>{e.actor}</strong> <code>{e.action}</code> {e.target}
                  {Object.keys(e.details).length > 0 && (
                    <span className="muted small"> {JSON.stringify(e.details)}</span>
                  )}
                </li>
              ))}
            </ul>
          )}
        </section>
      )}
    </div>
  );
}

function ProviderCard({
  provider: p,
  canEdit,
  onChanged,
  onError,
}: {
  provider: ProviderInfo;
  canEdit: boolean;
  onChanged: () => void;
  onError: (err: unknown) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [key, setKey] = useState("");
  const [models, setModels] = useState(p.allowed_models.join(", "));
  const [baseUrl, setBaseUrl] = useState(p.base_url);
  const [busy, setBusy] = useState(false);

  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await fn();
      onChanged();
    } catch (err) {
      onError(err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className={`card provider ${p.enabled ? "" : "disabled"}`}>
      <div className="provider-top">
        <div>
          <strong className="provider-name">{p.name}</strong> <span className="tag">{p.kind}</span>{" "}
          {!p.enabled && <span className="tag tone-bad">disabled</span>}
        </div>
        {canEdit && (
          <div className="row">
            <button className="btn ghost" disabled={busy} onClick={() => setEditing(!editing)}>
              {editing ? "Cancel" : "Edit"}
            </button>
            <button
              className="btn ghost"
              disabled={busy}
              onClick={() => run(() => api.updateProvider(p.name, { enabled: !p.enabled }))}
            >
              {p.enabled ? "Disable" : "Enable"}
            </button>
            <button
              className="btn ghost danger-text"
              disabled={busy}
              onClick={() => {
                if (confirm(`Delete provider “${p.name}”? Running agents using it will start failing.`)) {
                  void run(() => api.deleteProvider(p.name));
                }
              }}
            >
              Delete
            </button>
          </div>
        )}
      </div>
      <dl className="meta">
        <div>
          <dt>Endpoint</dt>
          <dd className="mono small">{p.base_url}</dd>
        </div>
        <div>
          <dt>API key</dt>
          <dd className="mono small">{p.api_key_hint}</dd>
        </div>
        <div>
          <dt>Allowed models</dt>
          <dd>{p.allowed_models.length ? p.allowed_models.join(", ") : "any"}</dd>
        </div>
        <div>
          <dt>Last changed</dt>
          <dd>
            {new Date(p.updated_at).toLocaleString()} by {p.updated_by}
          </dd>
        </div>
      </dl>
      {editing && (
        <form
          className="provider-edit"
          onSubmit={(e) => {
            e.preventDefault();
            void run(async () => {
              await api.updateProvider(p.name, {
                base_url: baseUrl,
                allowed_models: splitModels(models),
                ...(key.trim() ? { api_key: key.trim() } : {}),
              });
              setKey("");
              setEditing(false);
            });
          }}
        >
          <label>
            Endpoint
            <input value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} />
          </label>
          <label>
            Allowed models (globs, comma separated; empty = any)
            <input value={models} onChange={(e) => setModels(e.target.value)} placeholder="claude-*" />
          </label>
          <label>
            New API key (leave empty to keep the current one)
            <input type="password" autoComplete="off" value={key} onChange={(e) => setKey(e.target.value)} />
          </label>
          <button className="btn primary" disabled={busy}>
            Save
          </button>
        </form>
      )}
    </div>
  );
}

function AddProvider({
  existing,
  onAdded,
  onError,
}: {
  existing: ProviderInfo[];
  onAdded: () => void;
  onError: (err: unknown) => void;
}) {
  const [kind, setKind] = useState<ProviderKind>("anthropic");
  const [name, setName] = useState("anthropic");
  const [baseUrl, setBaseUrl] = useState("");
  const [key, setKey] = useState("");
  const [models, setModels] = useState("");
  const [busy, setBusy] = useState(false);
  const taken = existing.some((p) => p.name === name.trim().toLowerCase());

  return (
    <form
      className="card pad add-provider"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          await api.createProvider({
            name: name.trim(),
            kind,
            base_url: baseUrl.trim() || undefined,
            api_key: key.trim(),
            allowed_models: splitModels(models),
          });
          setKey("");
          setModels("");
          setName("");
          setBaseUrl("");
          onAdded();
        } catch (err) {
          onError(err);
        } finally {
          setBusy(false);
        }
      }}
    >
      <h2>Add a provider</h2>
      <div className="grid-2">
        <label>
          Type
          <select
            value={kind}
            onChange={(e) => {
              const k = e.target.value as ProviderKind;
              if (name === kind) setName(k);
              setKind(k);
            }}
          >
            <option value="anthropic">Anthropic</option>
            <option value="openai">OpenAI or OpenAI-compatible</option>
          </select>
        </label>
        <label>
          Name (used in the gateway URL)
          <input value={name} onChange={(e) => setName(e.target.value)} />
          {taken && <span className="error-text small">A provider with this name exists.</span>}
        </label>
        <label>
          Endpoint
          <input value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} placeholder={DEFAULT_URL[kind]} />
        </label>
        <label>
          Allowed models (optional)
          <input
            value={models}
            onChange={(e) => setModels(e.target.value)}
            placeholder={kind === "anthropic" ? "claude-*" : "gpt-*"}
          />
        </label>
      </div>
      <label>
        API key
        <input type="password" autoComplete="off" value={key} onChange={(e) => setKey(e.target.value)} />
      </label>
      <p className="muted small">
        The key is encrypted with the agentcore master key before it is stored, and is never shown again.
        {" "}Agents use the first enabled provider of each type.
      </p>
      <button className="btn primary" disabled={busy || !key.trim() || !name.trim() || taken}>
        {busy ? "Saving…" : "Add provider"}
      </button>
    </form>
  );
}
