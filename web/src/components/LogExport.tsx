import { useState } from "react";
import { api } from "../api";
import type { LogExportOptions, LogFormat, LogLevel, LogSource } from "../types";

const RANGES: { label: string; hours: number }[] = [
  { label: "1 hour", hours: 1 },
  { label: "6 hours", hours: 6 },
  { label: "24 hours", hours: 24 },
  { label: "7 days", hours: 24 * 7 },
];

const SOURCES: { id: LogSource; label: string; hint: string }[] = [
  { id: "server", label: "Server", hint: "operational log of every node" },
  { id: "audit", label: "Sessions", hint: "actions, verdicts, approvals, model calls, agent output" },
  { id: "admin", label: "Configuration changes", hint: "who changed what" },
  { id: "activity", label: "Organisation activity", hint: "what department agents did" },
];

const FORMATS: { id: LogFormat; label: string }[] = [
  { id: "md", label: "Markdown, for Claude" },
  { id: "jsonl", label: "JSON Lines" },
  { id: "json", label: "JSON" },
  { id: "csv", label: "CSV" },
  { id: "txt", label: "Plain text" },
];

const SIZES: { label: string; chars: number }[] = [
  { label: "Small (≈25k tokens)", chars: 100_000 },
  { label: "Medium (≈75k tokens)", chars: 300_000 },
  { label: "Large (≈150k tokens)", chars: 600_000 },
];

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

const MIME: Record<LogFormat, string> = {
  md: "text/markdown",
  jsonl: "application/x-ndjson",
  json: "application/json",
  csv: "text/csv",
  txt: "text/plain",
};

/** Copy to the clipboard; falls back for pages served over plain HTTP. */
async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    const area = Object.assign(document.createElement("textarea"), { value: text });
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.append(area);
    area.select();
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch {
      ok = false;
    }
    area.remove();
    return ok;
  }
}

function saveFile(text: string, filename: string, format: LogFormat) {
  const url = URL.createObjectURL(new Blob([text], { type: MIME[format] }));
  const a = Object.assign(document.createElement("a"), { href: url, download: filename });
  a.click();
  URL.revokeObjectURL(url);
}

const tokens = (chars: number) => `≈${Math.max(1, Math.round(chars / 4000))}k tokens`;

type Outcome = { ok: boolean; message: string };

/** Fetch an export and copy it (Markdown) or save it. */
async function run(opts: LogExportOptions, copy: boolean): Promise<Outcome> {
  const { text, filename } = await api.exportLogs(opts);
  if (copy) {
    if (await copyText(text)) {
      return {
        ok: true,
        message: `Copied ${text.length.toLocaleString()} characters (${tokens(text.length)}). Paste it into a Claude session and ask, for example, “What went wrong, and why?”`,
      };
    }
    saveFile(text, filename, opts.format);
    return { ok: false, message: `The browser did not allow copying, so ${filename} was downloaded instead.` };
  }
  saveFile(text, filename, opts.format);
  return { ok: true, message: `Downloaded ${filename} (${text.length.toLocaleString()} characters).` };
}

/** Settings → Logs: everything agentcore logged, in one file. Admins only. */
export function LogExport({ onError }: { onError: (err: unknown) => void }) {
  const [hours, setHours] = useState<number | null>(24);
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [sources, setSources] = useState<LogSource[]>(SOURCES.map((s) => s.id));
  const [level, setLevel] = useState<LogLevel>("debug");
  const [session, setSession] = useState("");
  const [format, setFormat] = useState<LogFormat>("md");
  const [size, setSize] = useState(300_000);
  const [busy, setBusy] = useState<"copy" | "download" | null>(null);
  const [outcome, setOutcome] = useState<Outcome | null>(null);

  const sessionId = session.trim();
  const sessionInvalid = sessionId !== "" && !UUID.test(sessionId);
  const customInvalid = hours === null && (!from || (to !== "" && new Date(from) > new Date(to)));
  const invalid = sources.length === 0 || sessionInvalid || customInvalid;

  const options = (fmt: LogFormat): LogExportOptions => {
    const until = hours === null && to ? new Date(to) : new Date();
    const since = hours === null ? new Date(from) : new Date(until.getTime() - hours * 3600_000);
    return {
      format: fmt,
      since: since.toISOString(),
      until: until.toISOString(),
      sources: sources.length === SOURCES.length ? undefined : sources,
      level,
      session: sessionId || undefined,
      max_chars: fmt === "md" ? size : undefined,
    };
  };

  const go = async (copy: boolean) => {
    setBusy(copy ? "copy" : "download");
    setOutcome(null);
    try {
      setOutcome(await run(options(copy ? "md" : format), copy));
    } catch (err) {
      onError(err);
    } finally {
      setBusy(null);
    }
  };

  return (
    <section className="card pad log-export">
      <h2>Logs</h2>
      <p className="muted">
        Everything agentcore logged, from every node, in one file: the servers’ own logs, each session’s audit log,
        configuration changes and organisation activity. <strong>Copy for Claude</strong> puts a Markdown version on
        your clipboard that explains itself, groups the problems and fits in a model’s context, ready to paste into
        another Claude session for analysis. Secrets are redacted, and every export is recorded in the change history.
      </p>

      <div className="log-export-field">
        <span className="label">Time range</span>
        <div className="row segmented" role="group" aria-label="Time range">
          {RANGES.map((r) => (
            <button
              key={r.hours}
              type="button"
              className={`btn small-btn ${hours === r.hours ? "primary" : "ghost"}`}
              onClick={() => setHours(r.hours)}
            >
              Last {r.label}
            </button>
          ))}
          <button
            type="button"
            className={`btn small-btn ${hours === null ? "primary" : "ghost"}`}
            onClick={() => setHours(null)}
          >
            Custom
          </button>
        </div>
        {hours === null && (
          <div className="grid-2">
            <label>
              From
              <input type="datetime-local" value={from} onChange={(e) => setFrom(e.target.value)} />
            </label>
            <label>
              To (empty: now)
              <input type="datetime-local" value={to} onChange={(e) => setTo(e.target.value)} />
            </label>
          </div>
        )}
      </div>

      <div className="log-export-field">
        <span className="label">Include</span>
        <div className="check-grid">
          {SOURCES.map((s) => (
            <label key={s.id} className="inline-check">
              <input
                type="checkbox"
                checked={sources.includes(s.id)}
                onChange={(e) =>
                  setSources((cur) => (e.target.checked ? [...cur, s.id] : cur.filter((x) => x !== s.id)))
                }
              />
              <span>
                {s.label} <span className="muted small">· {s.hint}</span>
              </span>
            </label>
          ))}
        </div>
      </div>

      <div className="grid-2">
        <label>
          Lowest level
          <select value={level} onChange={(e) => setLevel(e.target.value as LogLevel)}>
            <option value="debug">Everything (incl. agent output)</option>
            <option value="info">Info and up</option>
            <option value="warn">Warnings and errors</option>
            <option value="error">Errors only</option>
          </select>
        </label>
        <label>
          Only one session (optional)
          <input
            value={session}
            placeholder="session id"
            spellCheck={false}
            onChange={(e) => setSession(e.target.value)}
            aria-invalid={sessionInvalid}
          />
        </label>
        <label>
          Size for Claude
          <select value={size} onChange={(e) => setSize(Number(e.target.value))}>
            {SIZES.map((s) => (
              <option key={s.chars} value={s.chars}>
                {s.label}
              </option>
            ))}
          </select>
        </label>
      </div>
      {sessionInvalid && <p className="error-text small">That is not a session id.</p>}
      {sources.length === 0 && <p className="error-text small">Choose at least one source.</p>}

      <div className="row log-export-actions">
        <button className="btn primary" disabled={invalid || busy !== null} onClick={() => void go(true)}>
          {busy === "copy" ? "Collecting…" : "Copy for Claude"}
        </button>
        <span className="row log-export-download">
          <select value={format} onChange={(e) => setFormat(e.target.value as LogFormat)} aria-label="Format">
            {FORMATS.map((f) => (
              <option key={f.id} value={f.id}>
                {f.label}
              </option>
            ))}
          </select>
          <button className="btn" disabled={invalid || busy !== null} onClick={() => void go(false)}>
            {busy === "download" ? "Collecting…" : "Download"}
          </button>
        </span>
      </div>
      {outcome && <p className={`small ${outcome.ok ? "ok-text" : "attention-text"}`}>{outcome.message}</p>}
      <p className="muted small">
        Markdown is trimmed to the size you choose (errors and warnings are always kept). JSON Lines, JSON, CSV and
        plain text contain every record.
      </p>
    </section>
  );
}

/** Session → Audit: this session's logs from every source, for Claude. Admins only. */
export function SessionLogs({
  sessionId,
  createdAt,
  endedAt,
  onError,
}: {
  sessionId: string;
  createdAt: string;
  endedAt: string | null;
  onError: (err: unknown) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const margin = 5 * 60_000;
  const go = async (format: LogFormat, copy: boolean) => {
    setBusy(true);
    setOutcome(null);
    try {
      const since = new Date(new Date(createdAt).getTime() - margin);
      const until = endedAt ? new Date(new Date(endedAt).getTime() + margin) : new Date();
      setOutcome(
        await run(
          { format, session: sessionId, since: since.toISOString(), until: until.toISOString() },
          copy,
        ),
      );
    } catch (err) {
      onError(err);
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="session-logs">
      <div className="row">
        <button className="btn primary" disabled={busy} onClick={() => void go("md", true)}>
          {busy ? "Collecting…" : "Copy logs for Claude"}
        </button>
        <button className="btn ghost" disabled={busy} onClick={() => void go("jsonl", false)}>
          All logs (JSONL)
        </button>
        <span className="muted small">This session’s audit log plus what the servers logged about it.</span>
      </div>
      {outcome && <p className={`small ${outcome.ok ? "ok-text" : "attention-text"}`}>{outcome.message}</p>}
    </div>
  );
}
