import { useEffect, useMemo, useRef, useState } from "react";
import { api, streamEvents } from "../api";
import type { AgentEvent, Department, Me, OrgAgent, OrgMessage } from "../types";

type Line =
  | { kind: "you"; id: string; at: string; by: string; text: string }
  | { kind: "agent"; id: string; at: string; text: string; done: boolean };

/** What happens to a message sent now, in the member's current state. */
function stateHint(agent: OrgAgent, dept: Department): string {
  if (agent.desired === "stopped") {
    return `${agent.name} is stopped. Your message starts it just to answer you; it does not take up its work, and it stops again once the conversation is over.`;
  }
  if (agent.desired === "paused" || dept.state === "paused") {
    return `${agent.name} is paused. It reads your message when it is resumed.`;
  }
  if (agent.status === "asleep") return `${agent.name} is asleep. Your message wakes it.`;
  if (agent.status === "awaiting_input") return `${agent.name} is waiting, and reads your message right away.`;
  if (agent.status === "running") return `${agent.name} is working. It reads your message when it finishes what it is doing.`;
  return `${agent.name} is starting.`;
}

/** The agent's answers: what it printed in each turn of its session. */
function agentTurns(events: AgentEvent[]): Extract<Line, { kind: "agent" }>[] {
  const turns: Extract<Line, { kind: "agent" }>[] = [];
  let current: Extract<Line, { kind: "agent" }> | null = null;
  const lines: string[] = [];
  for (const e of events) {
    if (e.event === "turn_started") {
      lines.length = 0;
      current = { kind: "agent", id: `${e.session_id}-${e.seq}`, at: e.timestamp, text: "", done: false };
      turns.push(current);
    } else if (e.event === "output" && e.stream === "stdout" && current) {
      lines.push(e.line);
      current.text = lines.join("\n");
    } else if (e.event === "turn_ended" && current) {
      current.done = true;
      current = null;
    }
  }
  return turns.map((t) => ({ ...t }));
}

/** A conversation between people and one member of a department. */
export function TalkPanel({
  agent,
  dept,
  me,
  tick,
  act,
  onClose,
  onOpenSession,
  onError,
}: {
  agent: OrgAgent;
  dept: Department;
  me: Me;
  tick: number;
  act: (fn: () => Promise<unknown>) => Promise<void>;
  /** Shown as a close button when given. */
  onClose?: () => void;
  onOpenSession: (id: string) => void;
  onError: (err: unknown) => void;
}) {
  const [messages, setMessages] = useState<OrgMessage[] | null>(null);
  const [events, setEvents] = useState<AgentEvent[]>([]);
  // Its earlier sessions in this conversation, by id (a talk can span several).
  const [earlier, setEarlier] = useState<Record<string, AgentEvent[]>>({});
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const list = useRef<HTMLDivElement>(null);
  const canTalk = me.role !== "viewer";

  // What people said to it.
  useEffect(() => {
    api
      .messages({ agent: agent.id, limit: 200 })
      .then((all) => setMessages(all.filter((m) => m.to_agent === agent.id && !m.from_agent)))
      .catch(onError);
  }, [agent.id, tick, onError]);

  // What it answered, live from its current (or last) session.
  useEffect(() => {
    setEvents([]);
    if (!agent.session_id) return;
    return streamEvents(agent.session_id, (e) => setEvents((prev) => [...prev, e]), () => {});
  }, [agent.session_id]);

  // When the conversation began: its answers before that are not part of it.
  const since = useMemo(() => (messages?.length ? messages.map((m) => m.created_at).sort()[0] : null), [messages]);

  useEffect(() => {
    if (!since) return;
    let cancelled = false;
    void (async () => {
      try {
        const sessions = (await Promise.all([api.sessions(), api.sessions(true)])).flat();
        const ids = sessions
          .filter((s) => s.context?.org_agent_id === agent.id && s.id !== agent.session_id)
          // Sessions still going when the conversation began count too.
          .filter((s) => s.ended_at === null || s.ended_at >= since)
          .map((s) => s.id);
        // One unreadable session should not hide the rest of the conversation.
        const loaded = await Promise.all(
          ids.map(async (id) => [id, await api.events(id).catch(() => [] as AgentEvent[])] as const),
        );
        if (!cancelled) setEarlier(Object.fromEntries(loaded));
      } catch (err) {
        if (!cancelled) onError(err);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [agent.id, agent.session_id, since, onError]);

  const thread = useMemo(() => {
    const yours: Line[] = (messages ?? []).map((m) => ({
      kind: "you",
      id: m.id,
      at: m.created_at,
      by: m.from_name,
      text: m.text,
    }));
    // Its answers since the conversation began, not everything it ever did.
    const turns = [...Object.values(earlier), events].flatMap(agentTurns);
    const answers = since ? turns.filter((t) => t.at >= since && (t.text || !t.done)) : [];
    return [...yours, ...answers].sort((a, b) => a.at.localeCompare(b.at));
  }, [messages, events, earlier, since]);

  const last = thread[thread.length - 1];
  const live = agent.desired !== "stopped";
  const thinking = live && agent.status === "running" && last?.kind === "you";

  useEffect(() => {
    if (list.current) list.current.scrollTop = list.current.scrollHeight;
  }, [thread.length, last?.kind === "agent" ? last.text.length : 0]);

  const send = async () => {
    const text = draft.trim();
    if (!text) return;
    setSending(true);
    try {
      await api.talkToAgent(agent.id, text);
      setDraft("");
    } catch (err) {
      onError(err);
    } finally {
      setSending(false);
      void act(async () => {});
    }
  };

  return (
    <section className="card talk" aria-label={`Talk with ${agent.name}`}>
      <div className="talk-head">
        <h3 className="section-title">💬 Talking with {agent.name}</h3>
        <div className="row">
          {agent.session_id && (
            <button className="link small" onClick={() => onOpenSession(agent.session_id!)}>
              full session
            </button>
          )}
          {canTalk && live && agent.talk_only && (
            <button className="btn small-btn" onClick={() => act(() => api.controlAgent(agent.id, "stop"))}>
              End conversation
            </button>
          )}
          {onClose && (
            <button className="btn ghost small-btn" onClick={onClose} aria-label="Close">
              ✕
            </button>
          )}
        </div>
      </div>
      <p className="muted small">{stateHint(agent, dept)}</p>
      <div className="talk-list" ref={list}>
        {messages === null && <p className="muted small">Loading…</p>}
        {messages?.length === 0 && (
          <p className="muted small">Ask {agent.name} about its work: what it is doing, what is blocking it, what it plans next.</p>
        )}
        {thread.map((l) =>
          l.kind === "you" ? (
            <div key={l.id} className="talk-line you">
              <div className="bubble-head">
                <strong>{l.by}</strong>
                <span className="muted">{new Date(l.at).toLocaleTimeString()}</span>
              </div>
              <div className="bubble-body">{l.text}</div>
            </div>
          ) : (
            <div key={l.id} className="talk-line agent">
              <div className="bubble-head">
                <strong>{agent.name}</strong>
                <span className="muted">{new Date(l.at).toLocaleTimeString()}</span>
              </div>
              <div className="bubble-body">{l.text || <span className="muted">thinking…</span>}</div>
            </div>
          ),
        )}
        {thinking && <p className="muted small">{agent.name} is busy; it answers when it gets to your message.</p>}
      </div>
      {canTalk && (
        <form
          className="talk-compose"
          onSubmit={(e) => {
            e.preventDefault();
            void send();
          }}
        >
          <textarea
            value={draft}
            rows={2}
            placeholder={`Message ${agent.name}…`}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                void send();
              }
            }}
          />
          <button className="btn primary" disabled={sending || !draft.trim()}>
            Send
          </button>
        </form>
      )}
    </section>
  );
}
