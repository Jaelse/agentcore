import { useEffect, useRef, useState } from "react";
import { api } from "../api";
import type { AgentEvent, CheckResult, Me, SessionInfo } from "../types";
import { actionSummary, principalLabel } from "../types";

type Item =
  | { kind: "task"; text: string; by: string; at: string }
  | { kind: "note"; text: string; at: string; tone?: string }
  | { kind: "agent"; turn: number; lines: string[]; done: boolean; exit: number | null; at: string }
  | { kind: "user"; text: string; by: string; at: string }
  | { kind: "checks"; passed: boolean; results: CheckResult[]; at: string }
  | { kind: "proposal"; title: string; body: string; at: string }
  | { kind: "delivered"; url: string | null; branch: string; by: string; at: string };

function build(events: AgentEvent[]): Item[] {
  const items: Item[] = [];
  let current: Extract<Item, { kind: "agent" }> | null = null;
  for (const e of events) {
    switch (e.event) {
      case "session_created":
        items.push({ kind: "task", text: e.task, by: principalLabel(e.created_by), at: e.timestamp });
        break;
      case "workspace_prepared":
        items.push({ kind: "note", text: `Checked out ${e.repository} @ ${e.branch} (${e.base_commit.slice(0, 7)})`, at: e.timestamp });
        break;
      case "role_applied":
        items.push({
          kind: "note",
          text: `Working as “${e.role}”${e.repo_docs.length ? ` · following the team's ${e.repo_docs.join(", ")}` : ""}`,
          at: e.timestamp,
        });
        break;
      case "turn_started":
        current = { kind: "agent", turn: e.turn, lines: [], done: false, exit: null, at: e.timestamp };
        items.push(current);
        break;
      case "output":
        if (current && e.stream === "stdout") current.lines.push(e.line);
        break;
      case "turn_ended":
        if (current) {
          current.done = true;
          current.exit = e.exit_code;
        }
        current = null;
        break;
      case "user_message":
        items.push({ kind: "user", text: e.text, by: principalLabel(e.by), at: e.timestamp });
        break;
      case "approval_requested":
        items.push({ kind: "note", text: `Asked for approval: ${actionSummary(e.action)}`, at: e.timestamp, tone: "attention" });
        break;
      case "approval_resolved":
        items.push({
          kind: "note",
          text: `${e.approved ? "Approved" : "Rejected"} by ${principalLabel(e.by)}${e.comment ? `: “${e.comment}”` : ""}`,
          at: e.timestamp,
          tone: e.approved ? "ok" : "bad",
        });
        break;
      case "checks_completed":
        items.push({ kind: "checks", passed: e.passed, results: e.results, at: e.timestamp });
        break;
      case "pull_request_proposed":
        items.push({ kind: "proposal", title: e.proposal.title, body: e.proposal.body, at: e.timestamp });
        break;
      case "delivered":
        items.push({ kind: "delivered", url: e.pull_request_url, branch: e.branch, by: principalLabel(e.by), at: e.timestamp });
        break;
      case "paused":
        items.push({ kind: "note", text: `Paused by ${principalLabel(e.by)}`, at: e.timestamp, tone: "attention" });
        break;
      case "resumed":
        items.push({ kind: "note", text: `Resumed by ${principalLabel(e.by)}`, at: e.timestamp, tone: "ok" });
        break;
      case "stop_requested":
        items.push({ kind: "note", text: `Stopped by ${principalLabel(e.by)}: ${e.reason}`, at: e.timestamp, tone: "bad" });
        break;
      case "session_ended":
        items.push({
          kind: "note",
          text: `Session ${e.status}${e.reason ? `: ${e.reason}` : ""}`,
          at: e.timestamp,
          tone: e.status === "completed" ? "ok" : "bad",
        });
        break;
    }
  }
  // Clone the mutable agent items so React sees new objects.
  return items.map((i) => (i.kind === "agent" ? { ...i, lines: [...i.lines] } : i));
}

export function Conversation({
  info,
  me,
  events,
  onError,
}: {
  info: SessionInfo;
  me: Me;
  events: AgentEvent[];
  onError: (err: unknown) => void;
}) {
  const items = build(events);
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const end = useRef<HTMLDivElement>(null);
  const canSend = info.status === "awaiting_input" && me.role !== "viewer";

  useEffect(() => {
    end.current?.scrollIntoView({ block: "nearest" });
  }, [items.length, events.length]);

  const send = async () => {
    if (!draft.trim()) return;
    setSending(true);
    try {
      await api.sendMessage(info.id, draft.trim());
      setDraft("");
    } catch (err) {
      onError(err);
    } finally {
      setSending(false);
    }
  };

  return (
    <div className="conversation">
      {items.map((item, i) => (
        <ConversationItem key={i} item={item} />
      ))}
      <div ref={end} />
      {canSend && (
        <form
          className="composer card"
          onSubmit={(e) => {
            e.preventDefault();
            void send();
          }}
        >
          <textarea
            rows={3}
            placeholder="Answer a question, correct the course, or ask for more work…"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) void send();
            }}
          />
          <div className="row">
            <span className="muted small">The agent continues in the same workspace and conversation. ⌘/Ctrl+Enter to send.</span>
            <button className="btn primary" disabled={sending || !draft.trim()}>
              {sending ? "Sending…" : "Send to agent"}
            </button>
          </div>
        </form>
      )}
      {info.status === "running" && <p className="muted small pad">The agent is working… (watch it in the Live tab)</p>}
      {info.status === "paused" && <p className="muted small pad">The agent is paused.</p>}
    </div>
  );
}

function ConversationItem({ item }: { item: Item }) {
  const time = <time className="muted small">{new Date(item.at).toLocaleTimeString()}</time>;
  switch (item.kind) {
    case "task":
      return (
        <div className="bubble user">
          <div className="bubble-head">
            <strong>{item.by}</strong> assigned the task {time}
          </div>
          <div className="bubble-body">{item.text}</div>
        </div>
      );
    case "user":
      return (
        <div className="bubble user">
          <div className="bubble-head">
            <strong>{item.by}</strong> {time}
          </div>
          <div className="bubble-body">{item.text}</div>
        </div>
      );
    case "agent":
      return (
        <div className="bubble agent">
          <div className="bubble-head">
            <strong>Agent</strong> · turn {item.turn} {time}
            {!item.done && <span className="pulse inline" aria-label="working" />}
            {item.done && item.exit !== 0 && item.exit !== null && <span className="tag tone-bad">exit {item.exit}</span>}
          </div>
          <div className="bubble-body mono-pre">
            {item.lines.length ? item.lines.join("\n") : item.done ? "(no reply)" : "Working…"}
          </div>
        </div>
      );
    case "note":
      return (
        <div className={`conv-note tone-${item.tone ?? "neutral"}`}>
          {item.text} {time}
        </div>
      );
    case "checks":
      return (
        <div className={`card conv-card ${item.passed ? "ok" : "bad"}`}>
          <strong>{item.passed ? "✓ Team checks passed" : "✗ Team checks failed"}</strong> {time}
          <ul className="checks">
            {item.results.map((r) => (
              <li key={r.name} className={r.skipped ? "skipped" : r.passed ? "ok" : r.optional ? "warn" : "bad"}>
                {r.skipped ? "–" : r.passed ? "✓" : "✗"} {r.name}
                {r.optional && <span className="muted small"> (optional)</span>}
                {!r.passed && !r.skipped && <pre className="out">{r.detail}</pre>}
              </li>
            ))}
          </ul>
        </div>
      );
    case "proposal":
      return (
        <div className="card conv-card">
          <strong>Proposed pull request:</strong> {item.title} {time}
          <pre className="out">{item.body}</pre>
        </div>
      );
    case "delivered":
      return (
        <div className="card conv-card ok">
          <strong>Delivered</strong> by {item.by} to <code>{item.branch}</code> {time}
          {item.url && (
            <div>
              <a href={item.url} target="_blank" rel="noreferrer">
                {item.url}
              </a>
            </div>
          )}
        </div>
      );
  }
}
