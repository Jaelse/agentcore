import { useState } from "react";
import type { Department, OrgAgent, OrgOverview } from "../types";
import { useFloorLayout } from "../floorLayout";
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
          <h2>Organisation floor</h2>
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

/**
 * Just the rooms, for pages that bring their own heading. People can put the
 * rooms they care about first, and hide the rest until needed.
 */
export function Floor({ org, onOpenDepartment, onOpenSession }: Props) {
  const byId = new Map(org.departments.map((d) => [d.id, d]));
  const layout = useFloorLayout(org.departments.map((d) => d.id));
  const [arranging, setArranging] = useState(false);
  const [showHidden, setShowHidden] = useState(false);
  const [dragging, setDragging] = useState<string | null>(null);
  const [over, setOver] = useState<string | null>(null);

  const ordered = layout.order.map((id) => byId.get(id)!);
  const revealAll = arranging || showHidden;
  const shown = ordered.filter((d) => revealAll || !layout.hidden.has(d.id));
  const hiddenCount = layout.hidden.size;
  const customised = hiddenCount > 0 || layout.order.some((id, i) => id !== org.departments[i]?.id);

  return (
    <>
      {org.departments.length > 1 && (
        <div className="floor-toolbar">
          <span className="muted small">
            {hiddenCount > 0 && !revealAll
              ? `Showing ${shown.length} of ${org.departments.length} departments`
              : `${org.departments.length} departments`}
          </span>
          {!arranging && hiddenCount > 0 && (
            <button className="link small" onClick={() => setShowHidden(!showHidden)}>
              {showHidden ? "Hide the hidden again" : `Show ${hiddenCount} hidden`}
            </button>
          )}
          <span className="spacer" />
          {arranging && customised && (
            <button className="link small" onClick={layout.reset}>
              Reset
            </button>
          )}
          <button className={`btn ghost small ${arranging ? "active" : ""}`} onClick={() => setArranging(!arranging)}>
            {arranging ? "Done" : "Arrange"}
          </button>
        </div>
      )}
      {arranging && <p className="muted small">Drag rooms, or use the buttons, to put the ones you care about first. Hidden rooms stay running.</p>}
      <div className="floor">
        {shown.map((d, i) => (
          <Room
            key={d.id}
            dept={d}
            onOpen={() => onOpenDepartment(d.id)}
            onOpenSession={onOpenSession}
            hidden={layout.hidden.has(d.id)}
            arrange={
              arranging
                ? {
                    first: i === 0,
                    last: i === shown.length - 1,
                    dropping: over === d.id && dragging !== d.id,
                    onFront: () => layout.move(d.id, 0),
                    onEarlier: () => layout.moveBefore(d.id, shown[i - 1].id),
                    onLater: () => layout.move(d.id, layout.order.indexOf(shown[i + 1].id)),
                    onToggleHidden: () => layout.setHidden(d.id, !layout.hidden.has(d.id)),
                    onDragStart: () => setDragging(d.id),
                    onDragOver: () => setOver(d.id),
                    onDrop: () => {
                      if (dragging) layout.moveBefore(dragging, d.id);
                      setDragging(null);
                      setOver(null);
                    },
                    onDragEnd: () => {
                      setDragging(null);
                      setOver(null);
                    },
                  }
                : undefined
            }
          />
        ))}
      </div>
    </>
  );
}

interface Arrange {
  first: boolean;
  last: boolean;
  dropping: boolean;
  onFront: () => void;
  onEarlier: () => void;
  onLater: () => void;
  onToggleHidden: () => void;
  onDragStart: () => void;
  onDragOver: () => void;
  onDrop: () => void;
  onDragEnd: () => void;
}

function Room({
  dept,
  onOpen,
  onOpenSession,
  hidden,
  arrange,
}: {
  dept: Department;
  onOpen: () => void;
  onOpenSession: (id: string) => void;
  hidden: boolean;
  arrange?: Arrange;
}) {
  const communicator = dept.agents.filter((a) => a.kind === "communicator");
  const workers = dept.agents.filter((a) => a.kind === "worker");
  const live = dept.agents.filter((a) => a.desired !== "stopped").length;
  return (
    <section
      className={`room ${dept.state === "paused" ? "paused" : ""} ${hidden ? "is-hidden" : ""} ${arrange ? "arranging" : ""} ${arrange?.dropping ? "drop-target" : ""}`}
      draggable={!!arrange}
      onDragStart={arrange?.onDragStart}
      onDragOver={
        arrange
          ? (e) => {
              e.preventDefault();
              arrange.onDragOver();
            }
          : undefined
      }
      onDrop={
        arrange
          ? (e) => {
              e.preventDefault();
              arrange.onDrop();
            }
          : undefined
      }
      onDragEnd={arrange?.onDragEnd}
    >
      {arrange && (
        <div className="room-arrange">
          <button className="btn ghost small" disabled={arrange.first} onClick={arrange.onFront} title="Move to the front">
            ⤒ First
          </button>
          <button className="btn ghost small" disabled={arrange.first} onClick={arrange.onEarlier} aria-label="Move earlier">
            ←
          </button>
          <button className="btn ghost small" disabled={arrange.last} onClick={arrange.onLater} aria-label="Move later">
            →
          </button>
          <button className="btn ghost small" onClick={arrange.onToggleHidden}>
            {hidden ? "Show" : "Hide"}
          </button>
        </div>
      )}
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
