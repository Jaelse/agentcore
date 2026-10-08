import { useEffect, useState } from "react";
import { api } from "../api";
import type { Channel, ChannelKind, Me, OrgOverview, OutboxItem, OutboxStatus } from "../types";

const STATUS: Record<OutboxStatus, { label: string; cls: string }> = {
  pending: { label: "waiting for you", cls: "status-awaiting_input" },
  changes_requested: { label: "sent back", cls: "status-paused" },
  sending: { label: "sending", cls: "status-running" },
  sent: { label: "sent", cls: "status-completed" },
  rejected: { label: "rejected", cls: "" },
  failed: { label: "failed", cls: "status-failed" },
};

const FILTERS: { id: string; label: string; match: (s: OutboxStatus) => boolean }[] = [
  { id: "pending", label: "To approve", match: (s) => s === "pending" || s === "changes_requested" || s === "failed" },
  { id: "sent", label: "Sent", match: (s) => s === "sent" || s === "sending" },
  { id: "rejected", label: "Rejected", match: (s) => s === "rejected" },
  { id: "all", label: "All", match: () => true },
];

const KIND_LABEL: Record<ChannelKind, string> = { email: "Email (SMTP)", slack: "Slack", webhook: "Webhook" };

/** What agents want to say to the outside world, waiting for people. */
export function Outbox({
  org,
  me,
  tick,
  act,
  onError,
}: {
  org: OrgOverview;
  me: Me;
  tick: number;
  act: (fn: () => Promise<unknown>) => Promise<void>;
  onError: (e: unknown) => void;
}) {
  const [items, setItems] = useState<OutboxItem[] | null>(null);
  const [filter, setFilter] = useState("pending");
  const [reload, setReload] = useState(0);
  useEffect(() => {
    api.outbox().then(setItems).catch(onError);
  }, [tick, reload, onError]);
  const shown = (items ?? []).filter((i) => FILTERS.find((f) => f.id === filter)!.match(i.status));
  return (
    <div className="session">
      <section className="card session-header">
        <div className="session-header-main">
          <h2>Outbox</h2>
          <p className="muted">
            Agents draft emails and posts; nothing leaves the organisation until a person approves it (unless a channel is
            set to send on its own). You can edit a draft, send it back with what should change, or reject it. A note that
            an AI wrote the message is added to everything that is sent.
          </p>
        </div>
        <div className="session-actions segmented" role="group" aria-label="Show">
          {FILTERS.map((f) => (
            <button key={f.id} className={`btn small-btn ${filter === f.id ? "primary" : "ghost"}`} onClick={() => setFilter(f.id)}>
              {f.label}
              {f.id === "pending" && org.outbox_pending > 0 ? ` (${org.outbox_pending})` : ""}
            </button>
          ))}
        </div>
      </section>
      {items === null && <div className="empty">Loading…</div>}
      {items !== null && shown.length === 0 && (
        <div className="empty">
          <p>
            {org.channels.length === 0
              ? "No channels yet: add one below so departments can reach customers, partners or your team."
              : filter === "pending"
                ? "Nothing waits for approval."
                : "Nothing here."}
          </p>
        </div>
      )}
      {shown.map((i) => (
        <OutboxCard
          key={`${i.id}-${i.revision}-${i.status}`}
          item={i}
          channel={org.channels.find((c) => c.id === i.channel_id)}
          me={me}
          onChanged={() => setReload((r) => r + 1)}
          onError={onError}
        />
      ))}
      <Channels org={org} me={me} act={act} onError={onError} />
    </div>
  );
}

function OutboxCard({
  item,
  channel,
  me,
  onChanged,
  onError,
}: {
  item: OutboxItem;
  channel: Channel | undefined;
  me: Me;
  onChanged: () => void;
  onError: (e: unknown) => void;
}) {
  const [mode, setMode] = useState<"view" | "edit" | "changes" | "reject">("view");
  const [to, setTo] = useState(item.recipients.join(", "));
  const [subject, setSubject] = useState(item.subject);
  const [body, setBody] = useState(item.body);
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const open = item.status === "pending" || item.status === "changes_requested" || item.status === "failed";
  const canOperate = me.role !== "viewer";
  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await fn();
      setMode("view");
      onChanged();
    } catch (err) {
      onError(err);
    } finally {
      setBusy(false);
    }
  };
  return (
    <section className="card proposal">
      <div className="session-item-top">
        <span>
          <span className="chip">
            {channel ? `${channel.kind === "email" ? "✉" : channel.kind === "slack" ? "#" : "↗"} ${channel.name}` : "deleted channel"}
          </span>{" "}
          <strong>{item.subject || (channel?.kind === "email" ? "(no subject)" : "")}</strong>
        </span>
        <span className={`pill ${STATUS[item.status].cls}`}>{STATUS[item.status].label}</span>
      </div>
      <div className="muted small">
        drafted by {item.drafted_by} · {new Date(item.created_at).toLocaleString()}
        {item.revision > 1 && ` · revision ${item.revision}`}
        {item.decided_by && ` · ${item.status === "rejected" ? "rejected" : "approved"} by ${item.decided_by}`}
        {item.sent_at && ` · sent ${new Date(item.sent_at).toLocaleString()}`}
      </div>
      {mode === "edit" ? (
        <form
          className="provider-edit"
          onSubmit={(e) => {
            e.preventDefault();
            void run(() =>
              api.editOutbox(item.id, {
                recipients: to
                  .split(/[,;]/)
                  .map((r) => r.trim())
                  .filter(Boolean),
                subject,
                body,
                note: "edited before sending",
              }),
            );
          }}
        >
          {channel?.kind === "email" && (
            <label>
              To
              <input value={to} onChange={(e) => setTo(e.target.value)} />
            </label>
          )}
          <label>
            Subject
            <input value={subject} onChange={(e) => setSubject(e.target.value)} maxLength={300} />
          </label>
          <label>
            Message
            <textarea rows={Math.min(16, body.split("\n").length + 2)} value={body} onChange={(e) => setBody(e.target.value)} required />
          </label>
          <div className="row">
            <button className="btn primary" disabled={busy}>
              Save as revision {item.revision + 1}
            </button>
            <button type="button" className="btn ghost" onClick={() => setMode("view")}>
              Cancel
            </button>
          </div>
        </form>
      ) : (
        <>
          {item.recipients.length > 0 && <div className="small">To: {item.recipients.join(", ")}</div>}
          <div className="message-preview">
            {item.body}
            {channel?.disclosure && <div className="muted">— {channel.disclosure}</div>}
          </div>
        </>
      )}
      {item.feedback && (
        <div className="callout small">
          <strong>{item.status === "rejected" ? "Why it was rejected" : "Asked to change"}:</strong> {item.feedback}
        </div>
      )}
      {item.error && (
        <div className="callout small danger-text">
          <strong>Could not send:</strong> {item.error}
        </div>
      )}
      {open && canOperate && mode === "view" && (
        <div className="row">
          <button
            className="btn primary"
            disabled={busy || !channel}
            onClick={() => {
              const where = channel?.kind === "email" ? `to ${item.recipients.join(", ")}` : `to ${channel?.name}`;
              if (confirm(`Send this ${where} now?`)) void run(() => api.sendOutbox(item.id, item.revision));
            }}
          >
            {item.status === "failed" ? "↻ Retry" : "✓ Approve & send"}
          </button>
          <button className="btn" onClick={() => setMode("edit")}>
            Edit…
          </button>
          {item.agent_id && (
            <button className="btn" onClick={() => setMode("changes")}>
              Send back…
            </button>
          )}
          <button className="btn ghost danger-text" onClick={() => setMode("reject")}>
            Reject…
          </button>
        </div>
      )}
      {(mode === "changes" || mode === "reject") && (
        <form
          className="provider-edit"
          onSubmit={(e) => {
            e.preventDefault();
            void run(() => (mode === "changes" ? api.outboxChanges(item.id, text) : api.rejectOutbox(item.id, text)));
          }}
        >
          <label>
            {mode === "changes" ? "What should be different? The agent rewrites it." : "Why not? (the agent is told)"}
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

function Channels({
  org,
  me,
  act,
  onError,
}: {
  org: OrgOverview;
  me: Me;
  act: (fn: () => Promise<unknown>) => Promise<void>;
  onError: (e: unknown) => void;
}) {
  const [adding, setAdding] = useState(false);
  const isAdmin = me.role === "admin";
  return (
    <section className="card side-panel">
      <div className="row" style={{ justifyContent: "space-between" }}>
        <h3 className="section-title">Channels</h3>
        {isAdmin && !adding && (
          <button className="btn small-btn" onClick={() => setAdding(true)}>
            + Channel
          </button>
        )}
      </div>
      <p className="muted small">
        Where agents may reach people outside the organisation, and which departments may use each one. For social media,
        a CRM or anything else, use a webhook to Zapier, Make, n8n or your own service.
      </p>
      {adding && (
        <ChannelForm
          org={org}
          onDone={() => {
            setAdding(false);
            void act(async () => {});
          }}
          onError={onError}
        />
      )}
      <ul className="goal-list">
        {org.channels.map((c) => (
          <ChannelRow key={c.id} c={c} org={org} isAdmin={isAdmin} act={act} onError={onError} />
        ))}
      </ul>
    </section>
  );
}

function ChannelRow({
  c,
  org,
  isAdmin,
  act,
  onError,
}: {
  c: Channel;
  org: OrgOverview;
  isAdmin: boolean;
  act: (fn: () => Promise<unknown>) => Promise<void>;
  onError: (e: unknown) => void;
}) {
  const [testTo, setTestTo] = useState("");
  const [result, setResult] = useState<string | null>(null);
  return (
    <li className="goal">
      <div className="session-item-top">
        <strong className="mono">{c.name}</strong>
        <span className={`pill ${c.enabled ? (c.requires_approval ? "status-completed" : "status-awaiting_input") : ""}`}>
          {!c.enabled ? "off" : c.requires_approval ? "every message approved" : "sends without approval"}
        </span>
      </div>
      <div className="muted small">
        {KIND_LABEL[c.kind]}
        {c.kind === "email" && ` · from ${String(c.config.from ?? "")}`}
        {c.kind === "email" && Array.isArray(c.config.allowed_domains) && c.config.allowed_domains.length > 0 && ` · only to @${(c.config.allowed_domains as string[]).join(", @")}`}
        {c.kind === "webhook" && ` · ${String(c.config.url ?? "")}`} · up to {c.max_per_day} a day
        {c.secret_hint && ` · secret ${c.secret_hint}`}
      </div>
      {c.description && <div className="small">{c.description}</div>}
      <div className="small">
        <strong>Departments:</strong>{" "}
        {isAdmin ? (
          <span className="check-grid">
            {org.departments.map((d) => (
              <label key={d.id} className="inline-check">
                <input
                  type="checkbox"
                  checked={c.departments.includes(d.id)}
                  onChange={(e) =>
                    act(() =>
                      api.updateChannel(c.id, {
                        departments: e.target.checked ? [...c.departments, d.id] : c.departments.filter((x) => x !== d.id),
                      }),
                    )
                  }
                />{" "}
                {d.name}
              </label>
            ))}
          </span>
        ) : (
          c.departments.map((id) => org.departments.find((d) => d.id === id)?.name).filter(Boolean).join(", ") || "none"
        )}
      </div>
      {isAdmin && (
        <div className="row">
          {c.kind === "email" && <input value={testTo} onChange={(e) => setTestTo(e.target.value)} placeholder="test recipient" style={{ maxWidth: 260 }} />}
          <button
            className="btn small-btn"
            onClick={async () => {
              try {
                const r = await api.testChannel(c.id, testTo ? [testTo] : []);
                setResult(r.ok ? "✓ test message sent" : `▲ ${r.error}`);
              } catch (err) {
                onError(err);
              }
            }}
          >
            Send a test
          </button>
          <button className="btn small-btn" onClick={() => act(() => api.updateChannel(c.id, { requires_approval: !c.requires_approval }))}>
            {c.requires_approval ? "Let it send without approval" : "Require approval"}
          </button>
          <button className="btn small-btn" onClick={() => act(() => api.updateChannel(c.id, { enabled: !c.enabled }))}>
            Turn {c.enabled ? "off" : "on"}
          </button>
          <button
            className="link small danger-text"
            onClick={() => {
              if (confirm(`Delete the channel ${c.name} and its outbox?`)) void act(() => api.deleteChannel(c.id));
            }}
          >
            delete
          </button>
          {result && <span className="small">{result}</span>}
        </div>
      )}
    </li>
  );
}

function ChannelForm({ org, onDone, onError }: { org: OrgOverview; onDone: () => void; onError: (e: unknown) => void }) {
  const [kind, setKind] = useState<ChannelKind>("email");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [host, setHost] = useState("");
  const [port, setPort] = useState("587");
  const [tls, setTls] = useState("starttls");
  const [username, setUsername] = useState("");
  const [from, setFrom] = useState("");
  const [domains, setDomains] = useState("");
  const [url, setUrl] = useState("");
  const [header, setHeader] = useState("Authorization");
  const [secret, setSecret] = useState("");
  const [departments, setDepartments] = useState<string[]>([]);
  const [approval, setApproval] = useState(true);
  const [maxPerDay, setMaxPerDay] = useState("50");
  const [disclosure, setDisclosure] = useState("This message was written by an AI agent.");
  const [busy, setBusy] = useState(false);
  return (
    <form
      className="provider-edit"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          const config: Record<string, unknown> =
            kind === "email"
              ? {
                  host: host.trim(),
                  port: Number(port) || 587,
                  tls,
                  username: username.trim() || undefined,
                  from: from.trim(),
                  allowed_domains: domains
                    .split(",")
                    .map((d) => d.trim())
                    .filter(Boolean),
                }
              : kind === "webhook"
                ? { url: url.trim(), header: header.trim() || "Authorization" }
                : {};
          await api.createChannel({
            name: name.trim(),
            kind,
            description,
            config,
            secret: secret || undefined,
            departments,
            requires_approval: approval,
            max_per_day: Number(maxPerDay) || 0,
            disclosure,
          });
          onDone();
        } catch (err) {
          onError(err);
        } finally {
          setBusy(false);
        }
      }}
    >
      <div className="row">
        {(Object.keys(KIND_LABEL) as ChannelKind[]).map((k) => (
          <button key={k} type="button" className={`btn small-btn ${kind === k ? "primary" : "ghost"}`} onClick={() => setKind(k)}>
            {KIND_LABEL[k]}
          </button>
        ))}
      </div>
      <div className="grid-2">
        <label>
          Name
          <input value={name} onChange={(e) => setName(e.target.value.toLowerCase())} placeholder={kind === "email" ? "customers" : kind === "slack" ? "team" : "social"} pattern="[a-z0-9][a-z0-9_\-]{0,62}" required />
        </label>
        <label>
          What it is for (agents read this)
          <input value={description} onChange={(e) => setDescription(e.target.value)} placeholder="Replies to customers" />
        </label>
      </div>
      {kind === "email" && (
        <>
          <div className="grid-2">
            <label>
              SMTP host
              <input value={host} onChange={(e) => setHost(e.target.value)} placeholder="smtp.example.com" required />
            </label>
            <label>
              Port and security
              <span className="row">
                <input type="number" value={port} onChange={(e) => setPort(e.target.value)} style={{ width: 90 }} />
                <select
                  value={tls}
                  onChange={(e) => {
                    setTls(e.target.value);
                    setPort(e.target.value === "tls" ? "465" : e.target.value === "none" ? "25" : "587");
                  }}
                >
                  <option value="starttls">STARTTLS</option>
                  <option value="tls">TLS</option>
                  <option value="none">none (local relay only)</option>
                </select>
              </span>
            </label>
            <label>
              User
              <input value={username} onChange={(e) => setUsername(e.target.value)} autoComplete="off" />
            </label>
            <label>
              Password
              <input type="password" value={secret} onChange={(e) => setSecret(e.target.value)} autoComplete="off" />
            </label>
            <label>
              From
              <input value={from} onChange={(e) => setFrom(e.target.value)} placeholder="Acme Support <support@acme.com>" required />
            </label>
            <label>
              Only to these domains (optional)
              <input value={domains} onChange={(e) => setDomains(e.target.value)} placeholder="acme.com, partner.com" />
            </label>
          </div>
        </>
      )}
      {kind === "slack" && (
        <label>
          Incoming webhook URL (secret)
          <input type="password" value={secret} onChange={(e) => setSecret(e.target.value)} placeholder="https://hooks.slack.com/services/…" required autoComplete="off" />
        </label>
      )}
      {kind === "webhook" && (
        <div className="grid-2">
          <label>
            URL (receives a JSON POST)
            <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://hooks.zapier.com/…" required />
          </label>
          <label>
            Header and value (secret, optional)
            <span className="row">
              <input value={header} onChange={(e) => setHeader(e.target.value)} style={{ width: 140 }} />
              <input type="password" value={secret} onChange={(e) => setSecret(e.target.value)} autoComplete="off" />
            </span>
          </label>
        </div>
      )}
      <label>
        Added to every message
        <input value={disclosure} onChange={(e) => setDisclosure(e.target.value)} />
      </label>
      <div className="grid-2">
        <label className="inline-check">
          <input type="checkbox" checked={approval} onChange={(e) => setApproval(e.target.checked)} /> A person approves every message
        </label>
        <label>
          At most per day
          <input type="number" min={0} value={maxPerDay} onChange={(e) => setMaxPerDay(e.target.value)} />
        </label>
      </div>
      <div>
        <span className="small">Departments that may use it</span>
        <div className="check-grid">
          {org.departments.map((d) => (
            <label key={d.id} className="inline-check">
              <input
                type="checkbox"
                checked={departments.includes(d.id)}
                onChange={(e) => setDepartments(e.target.checked ? [...departments, d.id] : departments.filter((x) => x !== d.id))}
              />{" "}
              {d.name}
            </label>
          ))}
        </div>
      </div>
      <div className="row">
        <button className="btn primary" disabled={busy || !name}>
          Add channel
        </button>
        <button type="button" className="btn ghost" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}
