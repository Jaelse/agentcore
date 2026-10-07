import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { api, streamLive } from "../api";
import type { ActionRecord, TimelineItem } from "../hooks";
import type { FileChange, LiveFrame, ModelDeltaKind, ProcessInfo, SessionInfo } from "../types";
import { TERMINAL, actionSummary } from "../types";

const COLS = 120;
const ROWS = 32;
const MAX_TOOL_OUTPUT = 64 * 1024;
const MAX_CALLS = 6;

interface ModelStream {
  callId: string;
  provider: string;
  model: string | null;
  parts: { kind: ModelDeltaKind; text: string }[];
  done: boolean;
  startedAt: number;
}

/** A read-only xterm.js screen of the agent's terminal size. */
function Screen({ onTerminal }: { onTerminal: (t: Terminal | null) => void }) {
  const el = useRef<HTMLDivElement>(null);
  const latest = useRef(onTerminal);
  latest.current = onTerminal;
  useEffect(() => {
    if (!el.current) return;
    const term = new Terminal({
      cols: COLS,
      rows: ROWS,
      disableStdin: true,
      cursorBlink: false,
      cursorStyle: "bar",
      convertEol: false,
      scrollback: 5000,
      fontSize: 12,
      fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace',
      theme: { background: "#0b0d11", foreground: "#d6dae0", cursor: "#0b0d11" },
    });
    term.open(el.current);
    latest.current(term);
    // The agent's terminal has a fixed size: scale the font so all columns
    // fit the panel (small screens scroll inside the panel instead).
    const box = el.current.parentElement;
    const fit = () => {
      if (!box) return;
      const width = box.clientWidth - 16;
      const size = Math.max(8, Math.min(14, Math.floor((width / (COLS * 0.61)) * 2) / 2));
      if (term.options.fontSize !== size) term.options.fontSize = size;
    };
    fit();
    const observer = new ResizeObserver(fit);
    if (box) observer.observe(box);
    return () => {
      observer.disconnect();
      latest.current(null);
      term.dispose();
    };
  }, []);
  return <div className="xterm-host" ref={el} />;
}

function trimTail(s: string, max: number) {
  return s.length > max ? s.slice(s.length - max) : s;
}

function elapsed(since: string | number, now: number) {
  const ms = now - (typeof since === "number" ? since : Date.parse(since));
  const s = Math.max(0, Math.round(ms / 1000));
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${s % 60}s`;
}

function useNow(ms: number) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(id);
  }, [ms]);
  return now;
}

interface Props {
  info: SessionInfo;
  timeline: TimelineItem[];
  onEnded: () => void;
}

/** Watch a running session as if the agent were sharing its screen. */
export function LiveView({ info, timeline, onEnded }: Props) {
  const term = useRef<Terminal | null>(null);
  const pendingTerminal = useRef<string[]>([]);
  const [connected, setConnected] = useState(false);
  const [calls, setCalls] = useState<ModelStream[]>([]);
  const [files, setFiles] = useState<FileChange[]>([]);
  const [processes, setProcesses] = useState<ProcessInfo[]>([]);
  const [toolOutput, setToolOutput] = useState<Record<string, string>>({});
  const endedRef = useRef(onEnded);
  endedRef.current = onEnded;

  const write = (data: string, reset = false) => {
    const t = term.current;
    if (!t) {
      if (reset) pendingTerminal.current = [];
      pendingTerminal.current.push(data);
      return;
    }
    if (reset) t.reset();
    t.write(data);
  };

  useEffect(() => {
    // Non-terminal frames are applied once per animation frame.
    let queue: LiveFrame[] = [];
    let raf = 0;
    const apply = () => {
      raf = 0;
      const batch = queue;
      queue = [];
      setCalls((prev) => {
        let next = prev;
        for (const f of batch) {
          if (f.frame === "model_start") {
            next = [
              { callId: f.call_id, provider: f.provider, model: f.model, parts: [], done: false, startedAt: Date.now() },
              ...next,
            ].slice(0, MAX_CALLS);
          } else if (f.frame === "model_delta" || f.frame === "model_end") {
            let idx = next.findIndex((c) => c.callId === f.call_id);
            if (idx < 0) {
              // Joined mid-call.
              next = [
                { callId: f.call_id, provider: "", model: null, parts: [], done: false, startedAt: Date.now() },
                ...next,
              ].slice(0, MAX_CALLS);
              idx = 0;
            } else if (next === prev) {
              next = [...prev];
            }
            const call = { ...next[idx], parts: [...next[idx].parts] };
            if (f.frame === "model_end") call.done = true;
            else {
              const last = call.parts[call.parts.length - 1];
              if (last && last.kind === f.kind && f.kind !== "tool_name") {
                call.parts[call.parts.length - 1] = { kind: last.kind, text: last.text + f.text };
              } else call.parts.push({ kind: f.kind, text: f.text });
            }
            next[idx] = call;
          }
        }
        return next;
      });
      const fileFrames = batch.filter((f) => f.frame === "files");
      if (fileFrames.length) {
        setFiles((prev) => {
          let next = prev;
          for (const f of fileFrames) {
            const paths = new Set(f.changes.map((c) => c.path));
            next = [...[...f.changes].reverse(), ...next.filter((c) => !paths.has(c.path))].slice(0, 100);
          }
          return next;
        });
      }
      for (const f of batch) if (f.frame === "processes") setProcesses(f.processes);
      const outputs = batch.filter((f) => f.frame === "tool_output");
      if (outputs.length) {
        setToolOutput((prev) => {
          const next = { ...prev };
          for (const f of outputs) {
            next[f.action_id] = trimTail((next[f.action_id] ?? "") + f.data, MAX_TOOL_OUTPUT);
          }
          return next;
        });
      }
    };
    const stop = streamLive(
      info.id,
      (f) => {
        if (f.frame === "terminal") write(f.data);
        else if (f.frame === "terminal_reset") write(f.data, true);
        else {
          queue.push(f);
          if (!raf) raf = requestAnimationFrame(apply);
        }
      },
      setConnected,
      () => endedRef.current(),
    );
    return () => {
      stop();
      if (raf) cancelAnimationFrame(raf);
    };
  }, [info.id]);

  const onTerminal = (t: Terminal | null) => {
    term.current = t;
    if (t && pendingTerminal.current.length) {
      t.write(pendingTerminal.current.join(""));
      pendingTerminal.current = [];
    }
  };

  const paused = info.status === "paused";
  return (
    <div className="live">
      <div className="live-main">
        <div className={`screen card ${paused ? "is-paused" : ""}`}>
          <div className="screen-bar">
            <span className={`live-badge ${connected ? "on" : ""}`}>{connected ? "● LIVE" : "○ connecting…"}</span>
            <span className="muted small">agent terminal · {COLS}×{ROWS} · recorded</span>
          </div>
          <div className="screen-body">
            <Screen onTerminal={onTerminal} />
            {paused && (
              <div className="paused-overlay" role="status">
                <strong>Paused</strong>
                <span>The agent and everything it started are frozen. Resume to continue.</span>
              </div>
            )}
          </div>
        </div>
        <NowRunning timeline={timeline} toolOutput={toolOutput} status={info.status} />
      </div>
      <aside className="live-side">
        <ModelPanel calls={calls} />
        <FilesPanel files={files} />
        <ProcessesPanel processes={processes} />
      </aside>
    </div>
  );
}

function NowRunning({
  timeline,
  toolOutput,
  status,
}: {
  timeline: TimelineItem[];
  toolOutput: Record<string, string>;
  status: SessionInfo["status"];
}) {
  const now = useNow(1000);
  const records = timeline.filter((t): t is { type: "action"; record: ActionRecord } => t.type === "action");
  const running = records.filter((t) => !t.record.outcome).map((t) => t.record);
  const last = records.length ? records[records.length - 1].record : null;
  const shown = running.length ? running : last ? [last] : [];
  return (
    <section className="card now-running">
      <h3 className="section-title">{running.length ? "Running now" : "Last action"}</h3>
      {shown.length === 0 && (
        <p className="muted small">
          {status === "awaiting_input" ? "The agent is waiting for your message." : "No tool calls yet."}
        </p>
      )}
      {shown.map((r) => (
        <RunningAction key={r.actionId} record={r} output={toolOutput[r.actionId]} now={now} />
      ))}
    </section>
  );
}

function RunningAction({ record, output, now }: { record: ActionRecord; output?: string; now: number }) {
  const ref = useRef<HTMLPreElement>(null);
  useEffect(() => {
    if (ref.current) ref.current.scrollTop = ref.current.scrollHeight;
  }, [output]);
  const waiting = record.approval && !record.resolution;
  const state = record.outcome
    ? record.outcome.status
    : waiting
      ? "waiting for approval"
      : `running · ${elapsed(record.timestamp, now)}`;
  const tone = record.outcome
    ? record.outcome.status === "succeeded"
      ? "ok"
      : "bad"
    : waiting
      ? "attention"
      : "neutral";
  return (
    <div className="running-action">
      <div className="running-head">
        <span className="kind">{record.action.type.replace("_", " ")}</span>
        <code className="summary">{actionSummary(record.action)}</code>
        <span className={`tag tone-${tone}`}>{state}</span>
      </div>
      {output ? (
        <pre className="out live-out" ref={ref}>
          {output}
        </pre>
      ) : (
        !record.outcome && record.action.type === "exec" && !waiting && <p className="muted small">No output yet.</p>
      )}
    </div>
  );
}

function ModelPanel({ calls }: { calls: ModelStream[] }) {
  const now = useNow(1000);
  return (
    <section className="card side-panel model-panel">
      <h3 className="section-title">Model</h3>
      {calls.length === 0 && <p className="muted small">What the model generates appears here as it streams.</p>}
      {calls.map((c, i) => (
        <details key={c.callId} className={`model-stream ${c.done ? "" : "active"}`} open={i === 0}>
          <summary>
            <span className={`dot ${c.done ? "off" : "on"}`} />
            <span className="mono small">
              {c.provider || "model"}
              {c.model ? ` · ${c.model}` : ""}
            </span>
            <span className="muted small">{c.done ? "done" : `streaming · ${elapsed(c.startedAt, now)}`}</span>
          </summary>
          <StreamParts parts={c.parts} />
        </details>
      ))}
    </section>
  );
}

function StreamParts({ parts }: { parts: ModelStream["parts"] }) {
  const ref = useRef<HTMLDivElement>(null);
  const size = parts.reduce((n, p) => n + p.text.length, 0);
  useEffect(() => {
    if (ref.current) ref.current.scrollTop = ref.current.scrollHeight;
  }, [size]);
  if (parts.length === 0) return <p className="muted small">Waiting for the first token…</p>;
  return (
    <div className="stream-parts" ref={ref}>
      {parts.map((p, i) =>
        p.kind === "thinking" ? (
          <p key={i} className="part thinking">
            {p.text}
          </p>
        ) : p.kind === "text" ? (
          <p key={i} className="part text">
            {p.text}
          </p>
        ) : p.kind === "tool_name" ? (
          <p key={i} className="part tool-name">
            → calls <code>{p.text}</code>
          </p>
        ) : (
          <pre key={i} className="part tool-input">
            {p.text}
          </pre>
        ),
      )}
    </div>
  );
}

const FILE_ICON: Record<FileChange["kind"], string> = { created: "+", modified: "~", removed: "−" };

function FilesPanel({ files }: { files: FileChange[] }) {
  const now = useNow(5000);
  return (
    <section className="card side-panel">
      <h3 className="section-title">Files ({files.length})</h3>
      {files.length === 0 && <p className="muted small">Files the agent creates or changes show up here.</p>}
      <ul className="file-list">
        {files.map((f) => (
          <li key={f.path} className={`file-${f.kind}`}>
            <span className="file-kind" title={f.kind}>
              {FILE_ICON[f.kind]}
            </span>
            <code title={f.path}>{f.path}</code>
            <span className="muted small">{elapsed(f.at, now)}</span>
          </li>
        ))}
      </ul>
    </section>
  );
}

function ProcessesPanel({ processes }: { processes: ProcessInfo[] }) {
  return (
    <section className="card side-panel">
      <h3 className="section-title">Processes ({processes.length})</h3>
      {processes.length === 0 && <p className="muted small">Nothing running in the sandbox.</p>}
      <ul className="proc-list">
        {processes.map((p) => (
          <li key={p.pid}>
            <span className="mono small muted">{p.pid}</span>
            <code title={p.command}>{p.command}</code>
            <span className="muted small">{p.elapsed}</span>
          </li>
        ))}
      </ul>
    </section>
  );
}

// ---- replay ------------------------------------------------------------------

interface Cast {
  events: { t: number; data: string }[];
  markers: { t: number; label: string }[];
  duration: number;
}

/** Parse asciicast v2, capping idle gaps so long waits don't stall playback. */
function parseCast(text: string, idleLimit = 2): Cast {
  const lines = text.split("\n").filter(Boolean);
  const events: Cast["events"] = [];
  const markers: Cast["markers"] = [];
  let last = 0;
  let shift = 0;
  for (const line of lines.slice(1)) {
    let item: unknown;
    try {
      item = JSON.parse(line);
    } catch {
      continue;
    }
    if (!Array.isArray(item) || item.length < 3) continue;
    const [raw, code, data] = item as [number, string, string];
    if (raw - last > idleLimit) shift += raw - last - idleLimit;
    last = raw;
    const t = raw - shift;
    if (code === "o") events.push({ t, data });
    else if (code === "m") markers.push({ t, label: data });
  }
  return { events, markers, duration: Math.max(last - shift, 0) };
}

const SPEEDS = [1, 2, 4, 8];

/** Play back the terminal recording of a finished session. */
export function Replay({ info, onError }: { info: SessionInfo; onError: (e: unknown) => void }) {
  const [cast, setCast] = useState<Cast | null | undefined>(undefined);
  const [position, setPosition] = useState(0);
  const [playing, setPlaying] = useState(false);
  const [speed, setSpeed] = useState(4);
  const term = useRef<Terminal | null>(null);
  const index = useRef(0);
  const pos = useRef(0);

  useEffect(() => {
    let cancelled = false;
    api
      .recording(info.id)
      .then((text) => !cancelled && setCast(text === null ? null : parseCast(text)))
      .catch((err) => {
        if (!cancelled) {
          setCast(null);
          onError(err);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [info.id, onError]);

  const seek = (t: number) => {
    if (!cast || !term.current) return;
    term.current.reset();
    let i = 0;
    let chunk = "";
    while (i < cast.events.length && cast.events[i].t <= t) chunk += cast.events[i++].data;
    term.current.write(chunk);
    index.current = i;
    pos.current = t;
    setPosition(t);
  };

  // Show the final screen first; playing starts from the beginning. (The
  // screen mounts with the recording, and child effects run first.)
  useEffect(() => {
    if (cast) seek(cast.duration);
  }, [cast]);

  useEffect(() => {
    if (!playing || !cast) return;
    let raf = 0;
    let prev = performance.now();
    const tick = (now: number) => {
      const t = Math.min(pos.current + ((now - prev) / 1000) * speed, cast.duration);
      prev = now;
      let chunk = "";
      while (index.current < cast.events.length && cast.events[index.current].t <= t) {
        chunk += cast.events[index.current++].data;
      }
      if (chunk) term.current?.write(chunk);
      pos.current = t;
      setPosition(t);
      if (t >= cast.duration) setPlaying(false);
      else raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [playing, speed, cast]);

  if (cast === undefined) return <p className="muted pad">Loading recording…</p>;
  if (cast === null)
    return <p className="muted pad">This session has no terminal recording (it was started before recordings existed).</p>;

  const play = () => {
    if (pos.current >= cast.duration) seek(0);
    setPlaying(!playing);
  };
  const ended = TERMINAL.includes(info.status);
  return (
    <div className="replay">
      <div className="screen card">
        <div className="screen-bar">
          <span className="live-badge replay">{ended ? "⏵ REPLAY" : "⏵ RECORDING SO FAR"}</span>
          <span className="muted small">idle gaps over 2 s are shortened</span>
        </div>
        <div className="screen-body">
          <Screen onTerminal={(t) => (term.current = t)} />
        </div>
        <div className="player">
          <button className="btn" onClick={play} aria-label={playing ? "Pause replay" : "Play replay"}>
            {playing ? "❚❚" : "▶"}
          </button>
          <input
            type="range"
            min={0}
            max={cast.duration || 1}
            step={0.1}
            value={position}
            onChange={(e) => {
              setPlaying(false);
              seek(Number(e.target.value));
            }}
            aria-label="Position"
          />
          <span className="mono small">
            {fmt(position)} / {fmt(cast.duration)}
          </span>
          <select value={speed} onChange={(e) => setSpeed(Number(e.target.value))} aria-label="Speed">
            {SPEEDS.map((s) => (
              <option key={s} value={s}>
                {s}×
              </option>
            ))}
          </select>
          <button className="btn ghost" onClick={() => api.downloadRecording(info.id).catch(onError)}>
            Download .cast
          </button>
        </div>
        {cast.markers.length > 0 && (
          <div className="chapters">
            {cast.markers.map((m, i) => (
              <button
                key={i}
                className="chip"
                onClick={() => {
                  setPlaying(false);
                  seek(m.t);
                }}
              >
                {fmt(m.t)} · {m.label}
              </button>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

function fmt(t: number) {
  const s = Math.floor(t);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}
