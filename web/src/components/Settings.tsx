import { useCallback, useEffect, useState } from "react";
import { api } from "../api";
import type { AdminEvent, Me, PoolToken, ProviderInfo, ProviderKind, SystemCard } from "../types";
import { AgentCatalog } from "./AgentCatalog";
import { GitHubSettings, RolesOverview } from "./TeamSettings";

const DEFAULT_URL: Record<ProviderKind, string> = {
  anthropic: "https://api.anthropic.com",
  openai: "https://api.openai.com",
  opencode_zen: "https://opencode.ai/zen",
  claude_subscription: "https://api.anthropic.com",
};

const DEFAULT_NAME: Record<ProviderKind, string> = {
  anthropic: "anthropic",
  openai: "openai",
  opencode_zen: "opencode",
  claude_subscription: "claude",
};

const KIND_LABEL: Record<ProviderKind, string> = {
  anthropic: "Anthropic",
  openai: "OpenAI",
  opencode_zen: "OpenCode Zen",
  claude_subscription: "Claude subscription",
};

const splitModels = (text: string) =>
  text
    .split(/[\s,]+/)
    .map((m) => m.trim())
    .filter(Boolean);

export function Settings({
  me,
  card,
  onAgentsChanged,
  onError,
}: {
  me: Me;
  card: SystemCard;
  onAgentsChanged: () => void;
  onError: (err: unknown) => void;
}) {
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

      <AgentCatalog
        me={me}
        card={card}
        providers={providers ?? []}
        onChanged={() => {
          refresh();
          onAgentsChanged();
        }}
        onError={onError}
      />

      <GitHubSettings me={me} onError={onError} onChanged={refresh} />

      <RolesOverview onError={onError} />

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

/** Subscription tokens from `claude setup-token`; each running session uses one. */
function TokenPool({
  provider,
  canEdit,
  onError,
}: {
  provider: string;
  canEdit: boolean;
  onError: (err: unknown) => void;
}) {
  const [tokens, setTokens] = useState<PoolToken[] | null>(null);
  const [label, setLabel] = useState("");
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);

  const load = () => {
    api.poolTokens(provider).then(setTokens).catch(onError);
  };
  useEffect(load, [provider]);

  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await fn();
      load();
    } catch (err) {
      onError(err);
    } finally {
      setBusy(false);
    }
  };

  const free = tokens?.filter((t) => t.enabled && !t.in_use_by).length ?? 0;
  return (
    <div className="token-pool">
      <h3>Token pool</h3>
      <div className="muted small">
        <p>
          Run several agents at once without sharing one login. Each running session uses its own token.
        </p>
        <ol>
          <li>
            On a computer with Claude Code installed, run <code>claude setup-token</code> and log in with a Claude
            account (Pro, Max, Team or Enterprise). Copy the token it prints (starts with <code>sk-ant-oat</code>).
          </li>
          <li>Give it a label below and add it. Repeat for as many tokens as you want to run agents in parallel.</li>
          <li>
            A session takes a free token when it makes its first model call and gives it back when it ends. When all
            tokens are in use, new model calls fail with a message asking you to add another token here.
          </li>
        </ol>
        <p>
          Tokens are encrypted before they are stored and never reach a sandbox. While this list is empty, the
          provider’s own key (entered when it was created) is used by every session; once you add a token here, only
          pool tokens are used. Tokens from the same Claude account share that account’s usage limits.
        </p>
      </div>
      {tokens && tokens.length > 0 && (
        <p className="small">
          {free} of {tokens.length} free
        </p>
      )}
      <ul className="plain">
        {(tokens ?? []).map((t) => (
          <li key={t.id} className="row">
            <strong>{t.label}</strong> <span className="mono small">{t.hint}</span>{" "}
            {!t.enabled && <span className="tag tone-bad">disabled</span>}
            {t.in_use_by && <span className="tag">in use</span>}
            {canEdit && (
              <>
                <button
                  className="btn ghost"
                  disabled={busy}
                  onClick={() => run(() => api.setPoolTokenEnabled(provider, t.id, !t.enabled))}
                >
                  {t.enabled ? "Disable" : "Enable"}
                </button>
                <button
                  className="btn ghost danger-text"
                  disabled={busy}
                  onClick={() => {
                    if (confirm(`Remove token “${t.label}”?`)) void run(() => api.removePoolToken(provider, t.id));
                  }}
                >
                  Remove
                </button>
              </>
            )}
          </li>
        ))}
      </ul>
      {canEdit && (
        <form
          className="row"
          onSubmit={(e) => {
            e.preventDefault();
            void run(async () => {
              await api.addPoolToken(provider, label.trim(), token.trim());
              setLabel("");
              setToken("");
            });
          }}
        >
          <input placeholder="Label" value={label} onChange={(e) => setLabel(e.target.value)} />
          <input
            type="password"
            autoComplete="off"
            placeholder="Token from claude setup-token"
            value={token}
            onChange={(e) => setToken(e.target.value)}
          />
          <button className="btn primary" disabled={busy || !label.trim() || !token.trim()}>
            Add token
          </button>
        </form>
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
          <strong className="provider-name">{p.name}</strong> <span className="tag">{KIND_LABEL[p.kind] ?? p.kind}</span>{" "}
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
      {p.kind === "claude_subscription" && <TokenPool provider={p.name} canEdit={canEdit} onError={onError} />}
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
              if (name === DEFAULT_NAME[kind]) setName(DEFAULT_NAME[k]);
              setKind(k);
            }}
          >
            <option value="anthropic">Anthropic</option>
            <option value="openai">OpenAI or OpenAI-compatible</option>
            <option value="opencode_zen">OpenCode Zen (e.g. Big Pickle)</option>
            <option value="claude_subscription">Claude subscription (Claude Code only)</option>
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
            placeholder={
              kind === "anthropic" || kind === "claude_subscription" ? "claude-*" : kind === "openai" ? "gpt-*" : "big-pickle"
            }
          />
        </label>
      </div>
      {kind === "claude_subscription" && (
        <div className="banner small">
          Lets Claude Code use a Claude Pro, Max, Team or Enterprise subscription. On a computer where Claude Code is
          installed, run <code>claude setup-token</code>, log in with the subscription's account, and paste the token
          it prints (it starts with <code>sk-ant-oat</code>). The gateway sends it to Anthropic for Claude Code agents
          only. To run several agents at once, add more tokens afterwards in this provider’s Token pool: each running
          session then uses its own. To log in from inside a session instead, add Claude Code with “Log in from the live terminal”.
        </div>
      )}
      <label>
        {kind === "claude_subscription" ? "Subscription token (from claude setup-token)" : "API key"}
        {kind === "opencode_zen" && " (from opencode.ai/zen; the keyless free tier does not work through a proxy)"}
        <input type="password" autoComplete="off" value={key} onChange={(e) => setKey(e.target.value)} />
      </label>
      <p className="muted small">
        The key is encrypted with the agentcore master key before it is stored, and is never shown again.
        {" "}Agents use the first enabled provider of each type.
      </p>
      <button
        className="btn primary"
        disabled={busy || !key.trim() || !name.trim() || taken}
      >
        {busy ? "Saving…" : "Add provider"}
      </button>
    </form>
  );
}
