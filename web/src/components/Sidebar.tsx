import { useState } from "react";
import { api } from "../api";
import type { Me, SessionInfo, SystemCard } from "../types";
import { TERMINAL } from "../types";
import { StatusPill } from "./StatusPill";

interface Props {
  me: Me;
  card: SystemCard;
  sessions: SessionInfo[];
  showArchived: boolean;
  onShowArchived: (archived: boolean) => void;
  onArchivedSome: () => void;
  selected: string | null;
  onSelect: (id: string) => void;
  onCreated: (s: SessionInfo) => void;
  onError: (err: unknown) => void;
  providerCount: number | null;
  onOpenSettings: () => void;
}

export function Sidebar({
  me,
  card,
  sessions,
  showArchived,
  onShowArchived,
  onArchivedSome,
  selected,
  onSelect,
  onCreated,
  onError,
  providerCount,
  onOpenSettings,
}: Props) {
  const finished = sessions.filter((s) => TERMINAL.includes(s.status)).length;
  const archiveFinished = async () => {
    if (!confirm("Archive all finished sessions? They are hidden from this list, not deleted; find them under “Archived”.")) return;
    try {
      await api.archiveFinished();
      onArchivedSome();
    } catch (err) {
      onError(err);
    }
  };
  return (
    <aside className="sidebar">
      {me.role !== "viewer" && providerCount === 0 && (
        <div className="banner warn-banner small">
          No model provider is configured, so agents cannot reach an LLM.{" "}
          <button className="link" onClick={onOpenSettings}>
            Open settings
          </button>
        </div>
      )}
      {me.role !== "viewer" && <NewSession card={card} onCreated={onCreated} onError={onError} />}
      <h3 className="section-title">{showArchived ? "Archived sessions" : "Sessions"}</h3>
      <div className="row small">
        <button className="link" onClick={() => onShowArchived(!showArchived)}>
          {showArchived ? "← Back to active sessions" : "Show archived"}
        </button>
        {!showArchived && me.role !== "viewer" && finished > 0 && (
          <button className="link" onClick={() => void archiveFinished()}>
            Archive {finished} finished
          </button>
        )}
      </div>
      {sessions.length === 0 && (
        <p className="muted small">{showArchived ? "No archived sessions." : "No sessions yet."}</p>
      )}
      <ul className="session-list">
        {sessions.map((s) => (
          <li key={s.id}>
            <button className={`session-item ${s.id === selected ? "active" : ""}`} onClick={() => onSelect(s.id)}>
              <div className="session-item-top">
                <strong>{s.agent}</strong>
                <StatusPill status={s.status} />
              </div>
              {s.context?.department_name && (
                <div className="muted small">
                  🏢 {s.context.department_name}
                  {s.context.org_agent_kind === "communicator" && " · communicator"}
                </div>
              )}
              {s.context?.project_name && (
                <div className="muted small">
                  📁 {s.context.project_name}
                  {s.context.issue && ` · #${s.context.issue.number}`}
                  {s.context.role && ` · ${s.context.role}`}
                </div>
              )}
              <div className="session-task">{s.task}</div>
              <div className="muted small">
                {new Date(s.created_at).toLocaleString()} · {s.actions} actions · {s.model_calls ?? 0} model calls
                {s.pending_approvals > 0 && <span className="badge attention">{s.pending_approvals} to approve</span>}
              </div>
            </button>
          </li>
        ))}
      </ul>
    </aside>
  );
}

function NewSession({
  card,
  onCreated,
  onError,
}: {
  card: SystemCard;
  onCreated: (s: SessionInfo) => void;
  onError: (err: unknown) => void;
}) {
  const [agent, setAgent] = useState(card.agents[0]?.name ?? "");
  const [policy, setPolicy] = useState("");
  const [task, setTask] = useState("");
  const [busy, setBusy] = useState(false);
  const agentInfo = card.agents.find((a) => a.name === agent);

  if (card.agents.length === 0) {
    return <p className="muted small">No agents configured. Add [[agents]] to agentcore.toml.</p>;
  }

  return (
    <form
      className="card new-session"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!task.trim()) return;
        setBusy(true);
        try {
          onCreated(await api.createSession(agent, task.trim(), policy));
          setTask("");
        } catch (err) {
          onError(err);
        } finally {
          setBusy(false);
        }
      }}
    >
      <h3 className="section-title">New task</h3>
      <label>
        Agent
        <select value={agent} onChange={(e) => setAgent(e.target.value)}>
          {card.agents.map((a) => (
            <option key={a.name} value={a.name}>
              {a.name}
            </option>
          ))}
        </select>
      </label>
      {agentInfo?.description && <p className="muted small">{agentInfo.description}</p>}
      <label>
        Policy
        <select value={policy} onChange={(e) => setPolicy(e.target.value)}>
          <option value="">Agent default ({agentInfo?.policy ?? card.default_policy})</option>
          {card.policies.map((p) => (
            <option key={p.name} value={p.name}>
              {p.name}
            </option>
          ))}
        </select>
      </label>
      <label>
        Task
        <textarea
          rows={4}
          placeholder="Describe what the agent should do…"
          value={task}
          onChange={(e) => setTask(e.target.value)}
        />
      </label>
      <button className="btn primary" disabled={busy || !task.trim()}>
        {busy ? "Starting…" : "Start agent"}
      </button>
    </form>
  );
}
