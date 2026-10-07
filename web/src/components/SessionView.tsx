import { useEffect, useRef, useState } from "react";
import { api } from "../api";
import { useEventStream, useSessionModel } from "../hooks";
import type { ActionRecord, ModelCallEvent, TimelineItem } from "../hooks";
import type { AgentEvent, Me, ModelCallRecord, PendingApproval, SessionInfo } from "../types";
import { TERMINAL, actionSummary, principalLabel } from "../types";
import { StatusPill } from "./StatusPill";
import { Conversation } from "./Conversation";
import { ChangesView } from "./ChangesView";
import { DeliverDialog } from "./DeliverDialog";

interface Props {
  info: SessionInfo;
  me: Me;
  onChanged: () => void;
  onError: (err: unknown) => void;
}

type Tab = "conversation" | "activity" | "changes" | "output" | "audit";

type Detail = Awaited<ReturnType<typeof api.session>>;

export function SessionView({ info, me, onChanged, onError }: Props) {
  const { events, connected } = useEventStream(info.id);
  const { timeline, output, pending } = useSessionModel(events);
  const [tab, setTab] = useState<Tab>("conversation");
  const [stopping, setStopping] = useState(false);
  const [detail, setDetail] = useState<Detail | null>(null);
  const [delivering, setDelivering] = useState(false);
  const ended = TERMINAL.includes(info.status);
  const canAct = me.role !== "viewer";
  const ctx = info.context ?? {};
  const isRepo = !!ctx.repository;

  // Reload details (proposal, role) when something relevant happens.
  const lastRelevant = events.filter((e) =>
    ["pull_request_proposed", "delivered", "role_applied", "session_ended"].includes(e.event),
  ).length;
  useEffect(() => {
    api.session(info.id).then(setDetail).catch(() => {});
  }, [info.id, lastRelevant, info.status]);

  const finish = async () => {
    try {
      await api.finish(info.id);
      onChanged();
    } catch (err) {
      onError(err);
    }
  };

  const stop = async () => {
    setStopping(true);
    try {
      await api.stop(info.id, "stopped from web UI");
      onChanged();
    } catch (err) {
      onError(err);
    } finally {
      setStopping(false);
    }
  };

  return (
    <div className="session">
      <section className="session-header card">
        <div className="session-header-main">
          <div className="session-title">
            <h2>{info.agent}</h2>
            <StatusPill status={info.status} />
            <span className={`dot ${connected ? "on" : "off"}`} title={connected ? "Live" : "Reconnecting"} />
          </div>
          <p className="session-task-full">{info.task}</p>
          {(ctx.project_name || ctx.issue || ctx.role) && (
            <div className="context-chips">
              {ctx.project_name && <span className="chip">📁 {ctx.project_name}</span>}
              {ctx.repository && <span className="chip mono small">{ctx.repository}</span>}
              {ctx.issue && (
                <a className="chip" href={ctx.issue.url} target="_blank" rel="noreferrer">
                  #{ctx.issue.number} {ctx.issue.title}
                </a>
              )}
              {detail?.role && <span className="chip">👤 {detail.role.title}</span>}
              {ctx.work_branch && <span className="chip mono small">⎇ {ctx.work_branch}</span>}
              {ctx.pull_request_url && (
                <a className="chip ok" href={ctx.pull_request_url} target="_blank" rel="noreferrer">
                  Pull request ↗
                </a>
              )}
            </div>
          )}
          <dl className="meta">
            <div>
              <dt>Policy</dt>
              <dd>{info.policy}</dd>
            </div>
            <div>
              <dt>Started by</dt>
              <dd>{principalLabel(info.created_by)}</dd>
            </div>
            <div>
              <dt>Started</dt>
              <dd>{new Date(info.created_at).toLocaleString()}</dd>
            </div>
            <div>
              <dt>Actions</dt>
              <dd>{info.actions}</dd>
            </div>
            <div>
              <dt>Model calls</dt>
              <dd>{info.model_calls ?? 0}</dd>
            </div>
            <div>
              <dt>Session</dt>
              <dd className="mono small">{info.id}</dd>
            </div>
          </dl>
        </div>
        {canAct && !ended && (
          <div className="session-actions">
            {info.status === "awaiting_input" && isRepo && detail?.role?.delivery === "pull_request" && (
              <button className="btn primary" onClick={() => setDelivering(true)}>
                Deliver…
              </button>
            )}
            {info.status === "awaiting_input" && (
              <button className="btn" onClick={finish} title="End the session; the work stays as it is">
                Finish
              </button>
            )}
          <button
            className="btn stop-button"
            onClick={stop}
            disabled={stopping}
            title="Immediately kill the agent and everything it is running"
          >
            <span className="stop-icon" aria-hidden />
            {stopping ? "Stopping…" : "STOP AGENT"}
          </button>
          </div>
        )}
      </section>

      {pending.length > 0 && (
        <section className="approvals">
          <h3 className="section-title attention-text">
            Waiting for your decision ({pending.length})
          </h3>
          {pending.map((p) => (
            <ApprovalCard key={p.approval_id} sessionId={info.id} approval={p} canAct={canAct} onError={onError} />
          ))}
        </section>
      )}

      <nav className="tabs" role="tablist">
        {(
          [
            ["conversation", "Conversation"],
            ["activity", `Activity (${timeline.filter((t) => t.type !== "event").length})`],
            ...(isRepo ? [["changes", "Changes"]] : []),
            ["output", `Output (${output.length})`],
            ["audit", "Audit"],
          ] as [Tab, string][]
        ).map(([id, label]) => (
          <button
            key={id}
            role="tab"
            aria-selected={tab === id}
            className={`tab ${tab === id ? "active" : ""}`}
            onClick={() => setTab(id)}
          >
            {label}
          </button>
        ))}
      </nav>

      {tab === "conversation" && <Conversation info={info} me={me} events={events} onError={onError} />}
      {tab === "activity" && <Timeline items={timeline} sessionId={info.id} onError={onError} />}
      {tab === "changes" && <ChangesView sessionId={info.id} status={info.status} onError={onError} />}
      {tab === "output" && <Output lines={output} />}
      {tab === "audit" && <Audit sessionId={info.id} events={events} onError={onError} />}
      {delivering && (
        <DeliverDialog
          sessionId={info.id}
          proposal={detail?.proposal ?? null}
          onClose={() => {
            setDelivering(false);
            onChanged();
          }}
          onError={onError}
        />
      )}
    </div>
  );
}

function ApprovalCard({
  sessionId,
  approval,
  canAct,
  onError,
}: {
  sessionId: string;
  approval: PendingApproval;
  canAct: boolean;
  onError: (err: unknown) => void;
}) {
  const [comment, setComment] = useState("");
  const [busy, setBusy] = useState(false);
  const decide = async (approved: boolean) => {
    setBusy(true);
    try {
      await api.decide(sessionId, approval.approval_id, approved, comment);
    } catch (err) {
      onError(err);
      setBusy(false);
    }
  };
  return (
    <div className="card approval-card">
      <div className="approval-action">
        <span className="kind">{approval.action.type.replace("_", " ")}</span>
        <code>{actionSummary(approval.action)}</code>
      </div>
      <p className="muted">{approval.reason}</p>
      {canAct ? (
        <div className="approval-controls">
          <input
            placeholder="Comment (optional, recorded in the audit log)"
            value={comment}
            onChange={(e) => setComment(e.target.value)}
          />
          <button className="btn approve" disabled={busy} onClick={() => decide(true)}>
            Approve
          </button>
          <button className="btn reject" disabled={busy} onClick={() => decide(false)}>
            Reject
          </button>
        </div>
      ) : (
        <p className="muted small">An operator must decide.</p>
      )}
    </div>
  );
}

function Timeline({
  items,
  sessionId,
  onError,
}: {
  items: TimelineItem[];
  sessionId: string;
  onError: (err: unknown) => void;
}) {
  if (items.length === 0) return <p className="muted pad">Nothing has happened yet.</p>;
  return (
    <ol className="timeline">
      {items.map((item) =>
        item.type === "action" ? (
          <ActionRow key={item.record.actionId} record={item.record} />
        ) : item.type === "model" ? (
          <ModelCallRow key={item.call.call_id} call={item.call} sessionId={sessionId} onError={onError} />
        ) : (
          <LifecycleRow key={item.event.id} event={item.event} />
        ),
      )}
    </ol>
  );
}

const OUTCOME_TONE: Record<string, string> = {
  completed: "ok",
  rejected: "bad",
  upstream_error: "bad",
  aborted: "bad",
};

function ModelCallRow({
  call,
  sessionId,
  onError,
}: {
  call: ModelCallEvent;
  sessionId: string;
  onError: (err: unknown) => void;
}) {
  const [open, setOpen] = useState(false);
  const [detail, setDetail] = useState<ModelCallRecord | null>(null);
  const failed = call.outcome === "completed" && call.http_status !== null && call.http_status >= 400;
  const tone = failed ? "bad" : OUTCOME_TONE[call.outcome];
  const tokens =
    call.input_tokens !== null || call.output_tokens !== null
      ? `${call.input_tokens ?? "?"} → ${call.output_tokens ?? "?"} tokens`
      : null;
  const toggle = () => {
    if (!open && !detail) api.modelCall(sessionId, call.call_id).then(setDetail).catch(onError);
    setOpen(!open);
  };
  return (
    <li className={`timeline-item action model tone-${tone}`}>
      <button className="timeline-row" onClick={toggle} aria-expanded={open}>
        <time>{new Date(call.timestamp).toLocaleTimeString()}</time>
        <span className="kind">model</span>
        <code className="summary">
          {call.provider} · {call.model ?? "unknown model"}
          {tokens && ` · ${tokens}`} · {(call.duration_ms / 1000).toFixed(1)}s
        </code>
        <span className={`tag tone-${tone}`}>
          {failed ? `HTTP ${call.http_status}` : call.outcome.replace("_", " ")}
        </span>
      </button>
      {open && (
        <div className="timeline-detail">
          {call.detail && <p>{call.detail}</p>}
          <p className="muted small mono">
            request sha256 {call.request_sha256.slice(0, 16)}…
            {call.response_sha256 && <> · response sha256 {call.response_sha256.slice(0, 16)}…</>}
          </p>
          {!detail ? (
            <p className="muted small">Loading…</p>
          ) : (
            <>
              {detail.bodies_truncated && <p className="muted small">Bodies were truncated for storage.</p>}
              <details>
                <summary>Request</summary>
                <pre className="out">{pretty(detail.request_body)}</pre>
              </details>
              <details open>
                <summary>Response</summary>
                <pre className="out">{pretty(detail.response_body)}</pre>
              </details>
            </>
          )}
        </div>
      )}
    </li>
  );
}

function pretty(body: string | null): string {
  if (body === null) return "(not stored)";
  try {
    return JSON.stringify(JSON.parse(body), null, 2);
  } catch {
    return body;
  }
}

function actionState(r: ActionRecord): { label: string; tone: string } {
  if (r.outcome?.status === "succeeded") return { label: "done", tone: "ok" };
  if (r.outcome?.status === "failed") return { label: "failed", tone: "bad" };
  if (r.outcome?.status === "denied") return { label: "denied", tone: "bad" };
  if (r.approval && !r.resolution) return { label: "awaiting approval", tone: "attention" };
  return { label: "running", tone: "neutral" };
}

function ActionRow({ record }: { record: ActionRecord }) {
  const [open, setOpen] = useState(false);
  const state = actionState(record);
  const verdict = record.verdict;
  return (
    <li className={`timeline-item action tone-${state.tone}`}>
      <button className="timeline-row" onClick={() => setOpen(!open)} aria-expanded={open}>
        <time>{new Date(record.timestamp).toLocaleTimeString()}</time>
        <span className="kind">{record.action.type.replace("_", " ")}</span>
        <code className="summary">{actionSummary(record.action)}</code>
        <span className={`tag tone-${state.tone}`}>{state.label}</span>
      </button>
      {open && (
        <div className="timeline-detail">
          {verdict && (
            <p>
              <strong>Policy:</strong> {verdict.verdict.replace("_", " ")}
              {verdict.rule && (
                <>
                  {" "}
                  by rule <code>{verdict.rule}</code>
                </>
              )}
              {"reason" in verdict && <> · {verdict.reason}</>}
            </p>
          )}
          {record.resolution && (
            <p>
              <strong>{record.resolution.approved ? "Approved" : "Rejected"}</strong> by{" "}
              {principalLabel(record.resolution.by)}
              {record.resolution.comment && <> · “{record.resolution.comment}”</>}
            </p>
          )}
          {record.outcome && <OutcomeView outcome={record.outcome} />}
        </div>
      )}
    </li>
  );
}

function OutcomeView({ outcome }: { outcome: NonNullable<ActionRecord["outcome"]> }) {
  if (outcome.status === "failed") return <pre className="out bad">{outcome.error}</pre>;
  if (outcome.status === "denied") return <pre className="out bad">{outcome.reason}</pre>;
  const o = outcome.output ?? {};
  if (typeof o.stdout === "string" || typeof o.stderr === "string") {
    return (
      <>
        <p className="muted small">
          exit code {String(o.exit_code ?? "—")}
          {o.timed_out ? " · timed out" : ""}
          {o.truncated ? " · output truncated" : ""}
        </p>
        {o.stdout ? <pre className="out">{String(o.stdout)}</pre> : null}
        {o.stderr ? <pre className="out stderr">{String(o.stderr)}</pre> : null}
      </>
    );
  }
  if (typeof o.content === "string") return <pre className="out">{o.content}</pre>;
  return <pre className="out">{JSON.stringify(o, null, 2)}</pre>;
}

function LifecycleRow({ event }: { event: AgentEvent }) {
  let text: string;
  let tone = "neutral";
  switch (event.event) {
    case "session_created":
      text = `Session created by ${principalLabel(event.created_by)} under policy “${event.policy}” (sha256:${event.policy_digest.slice(0, 12)}…)`;
      break;
    case "sandbox_started":
      text = `Sandbox started (${event.backend})`;
      break;
    case "agent_started":
      text = `Agent started: ${event.program}`;
      break;
    case "stop_requested":
      text = `Stop requested by ${principalLabel(event.by)}: ${event.reason}`;
      tone = "bad";
      break;
    case "session_ended":
      text = `Session ${event.status}${event.exit_code !== null ? ` (exit ${event.exit_code})` : ""}${event.reason ? `: ${event.reason}` : ""}`;
      tone = event.status === "completed" ? "ok" : "bad";
      break;
    default:
      text = event.event;
  }
  return (
    <li className={`timeline-item lifecycle tone-${tone}`}>
      <div className="timeline-row static">
        <time>{new Date(event.timestamp).toLocaleTimeString()}</time>
        <span>{text}</span>
      </div>
    </li>
  );
}

function Output({ lines }: { lines: Extract<AgentEvent, { event: "output" }>[] }) {
  const ref = useRef<HTMLDivElement>(null);
  const [follow, setFollow] = useState(true);
  useEffect(() => {
    if (follow && ref.current) ref.current.scrollTop = ref.current.scrollHeight;
  }, [lines.length, follow]);
  return (
    <div className="terminal-wrap">
      <label className="follow small">
        <input type="checkbox" checked={follow} onChange={(e) => setFollow(e.target.checked)} /> Follow output
      </label>
      <div className="terminal" ref={ref}>
        {lines.length === 0 && <span className="muted">No output yet.</span>}
        {lines.map((l) => (
          <div key={l.id} className={l.stream}>
            {l.line}
          </div>
        ))}
      </div>
    </div>
  );
}

function Audit({ sessionId, events, onError }: { sessionId: string; events: AgentEvent[]; onError: (e: unknown) => void }) {
  const [result, setResult] = useState<Awaited<ReturnType<typeof api.verifyAudit>> | null>(null);
  return (
    <div className="card pad audit">
      <p>
        Every event of this session is written to an append-only, hash-chained audit log before it is shown here.
        Altering, removing or reordering any record breaks the chain.
      </p>
      <div className="row">
        <button
          className="btn"
          onClick={() => api.verifyAudit(sessionId).then(setResult).catch(onError)}
        >
          Verify integrity
        </button>
        <button className="btn ghost" onClick={() => api.downloadAudit(sessionId).catch(onError)}>
          Download log (JSONL)
        </button>
        <span className="muted small">{events.length} events streamed</span>
      </div>
      {result &&
        (result.valid ? (
          <p className="ok-text">
            ✓ Chain intact: {result.records} records, head <code>{result.head?.slice(0, 16)}…</code>
          </p>
        ) : (
          <p className="error-text">✗ Verification failed: {result.error}</p>
        ))}
    </div>
  );
}
