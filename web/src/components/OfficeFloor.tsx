import type { Department, OrgAgent, OrgOverview } from "../types";
import { PixelKitten } from "./PixelKitten";

interface Props {
  org: OrgOverview;
  onOpenDepartment: (id: string) => void;
  onOpenSession: (sessionId: string) => void;
}

type Mood = "working" | "waiting" | "asleep" | "paused" | "off" | "failed";

function mood(agent: OrgAgent, dept: Department): Mood {
  if (agent.desired === "stopped") return agent.status === "failed" ? "failed" : "off";
  if (agent.desired === "paused" || dept.state === "paused") return "paused";
  if (agent.status === "asleep") return "asleep";
  if (agent.status === "awaiting_input" || agent.status === "awaiting_approval") return "waiting";
  return "working";
}

const MOOD_LABEL: Record<Mood, string> = {
  working: "working",
  waiting: "needs you",
  asleep: "asleep",
  paused: "paused",
  off: "stopped",
  failed: "failed",
};

/** The organisation as a floor plan: each department a room, each agent a desk in it. */
export function OfficeFloor({ org, onOpenDepartment, onOpenSession }: Props) {
  return (
    <div className="session">
      <section className="card session-header">
        <div className="session-header-main">
          <h2>Floor plan</h2>
          <p className="muted">
            Every department is a room, every agent sits inside it. Click a room to open the department, or an
            agent to watch its session. Communicators sit by the door: they are how rooms talk to each other.
          </p>
        </div>
      </section>
      <Floor org={org} onOpenDepartment={onOpenDepartment} onOpenSession={onOpenSession} />
    </div>
  );
}

/** Just the rooms, for pages that bring their own heading. */
export function Floor({ org, onOpenDepartment, onOpenSession }: Props) {
  return (
    <div className="floor">
      {org.departments.map((d) => (
        <Room key={d.id} dept={d} onOpen={() => onOpenDepartment(d.id)} onOpenSession={onOpenSession} />
      ))}
    </div>
  );
}

function Room({ dept, onOpen, onOpenSession }: { dept: Department; onOpen: () => void; onOpenSession: (id: string) => void }) {
  const communicator = dept.agents.filter((a) => a.kind === "communicator");
  const workers = dept.agents.filter((a) => a.kind === "worker");
  const live = dept.agents.filter((a) => a.desired !== "stopped").length;
  return (
    <section className={`room ${dept.state === "paused" ? "paused" : ""}`}>
      <header className="room-header">
        <button className="link room-title" onClick={onOpen} title="Open department">
          {dept.name}
        </button>
        {dept.state === "paused" ? (
          <span className="pill status-paused">paused</span>
        ) : live > 0 ? (
          <span className="pill status-running">
            <span className="pulse" />
            {live} running
          </span>
        ) : (
          <span className="pill">idle</span>
        )}
      </header>
      {dept.mission && <p className="muted small room-mission">{dept.mission}</p>}
      <div className="room-floor">
        {workers.length === 0 && <span className="muted small">No agents at work yet.</span>}
        {workers.map((a) => (
          <Desk key={a.id} agent={a} dept={dept} onOpenSession={onOpenSession} />
        ))}
      </div>
      {communicator.length > 0 && (
        <footer className="room-door">
          {communicator.map((a) => (
            <Desk key={a.id} agent={a} dept={dept} onOpenSession={onOpenSession} />
          ))}
        </footer>
      )}
    </section>
  );
}

function Desk({ agent, dept, onOpenSession }: { agent: OrgAgent; dept: Department; onOpenSession: (id: string) => void }) {
  const m = mood(agent, dept);
  const title = `${agent.name} · ${MOOD_LABEL[m]}${agent.note ? ` · ${agent.note}` : ""}`;
  return (
    <button
      className={`desk ${agent.kind} mood-${m}`}
      title={title}
      disabled={!agent.session_id}
      onClick={() => agent.session_id && onOpenSession(agent.session_id)}
    >
      <span className="avatar">
        <PixelKitten seed={agent.id} eyesShut={m === "asleep" || m === "off"} headset={agent.kind === "communicator"} />
        {m === "asleep" && <span className="desk-badge zzz">z</span>}
        {m === "waiting" && <span className="desk-badge">!</span>}
        {m === "paused" && <span className="desk-badge">❚❚</span>}
        {m === "failed" && <span className="desk-badge bad">✕</span>}
      </span>
      <span className="desk-name">{agent.name}</span>
      <span className="desk-state small muted">{MOOD_LABEL[m]}</span>
    </button>
  );
}
