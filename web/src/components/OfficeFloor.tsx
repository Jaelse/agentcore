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

  // Pointer events instead of HTML5 drag and drop, so it works with a finger too.
  const grab = (e: React.PointerEvent<HTMLElement>, id: string) => {
    e.preventDefault();
    const handle = e.currentTarget;
    try {
      // Keeps the move events coming while the finger leaves the handle.
      handle.setPointerCapture(e.pointerId);
    } catch {
      // Not an active pointer; the drag still follows events on the handle.
    }
    setDragging(id);
    const roomAt = (x: number, y: number) =>
      document.elementFromPoint(x, y)?.closest<HTMLElement>("[data-room-id]")?.dataset.roomId ?? null;
    // Dragging near the top or bottom edge scrolls, so far rooms can be reached on a phone.
    let pointerY = e.clientY;
    let pointerX = e.clientX;
    const scroller = (() => {
      for (let el: HTMLElement | null = handle.parentElement; el; el = el.parentElement) {
        if (/(auto|scroll)/.test(getComputedStyle(el).overflowY) && el.scrollHeight > el.clientHeight) return el;
      }
      return null;
    })();
    const tick = () => {
      const top = scroller ? scroller.getBoundingClientRect().top : 0;
      const bottom = scroller ? scroller.getBoundingClientRect().bottom : window.innerHeight;
      const edge = 64;
      const dy = pointerY < top + edge ? -14 : pointerY > bottom - edge ? 14 : 0;
      if (dy) {
        if (scroller) scroller.scrollTop += dy;
        else window.scrollBy(0, dy);
        setOver(roomAt(pointerX, pointerY));
      }
    };
    const scrolling = window.setInterval(tick, 16);
    const move = (ev: PointerEvent) => {
      pointerX = ev.clientX;
      pointerY = ev.clientY;
      setOver(roomAt(pointerX, pointerY));
    };
    const end = (ev: PointerEvent) => {
      window.clearInterval(scrolling);
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", end);
      handle.removeEventListener("pointercancel", end);
      const target = ev.type === "pointerup" ? roomAt(ev.clientX, ev.clientY) : null;
      if (target) layout.moveOnto(id, target);
      setDragging(null);
      setOver(null);
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", end);
    handle.addEventListener("pointercancel", end);
  };

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
      {arranging && <p className="muted small">Drag a room by its handle to put the ones you care about first. Hidden rooms stay running.</p>}
      <div className="floor">
        {shown.map((d) => (
          <Room
            key={d.id}
            dept={d}
            onOpen={() => onOpenDepartment(d.id)}
            onOpenSession={onOpenSession}
            hidden={layout.hidden.has(d.id)}
            arrange={
              arranging
                ? {
                    dragging: dragging === d.id,
                    dropping: over === d.id && dragging !== d.id,
                    onToggleHidden: () => layout.setHidden(d.id, !layout.hidden.has(d.id)),
                    onGrab: (e) => grab(e, d.id),
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
  dragging: boolean;
  dropping: boolean;
  onToggleHidden: () => void;
  onGrab: (e: React.PointerEvent<HTMLElement>) => void;
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
      data-room-id={dept.id}
      className={`room ${dept.state === "paused" ? "paused" : ""} ${hidden ? "is-hidden" : ""} ${arrange ? "arranging" : ""} ${arrange?.dropping ? "drop-target" : ""} ${arrange?.dragging ? "dragging" : ""}`}
    >
      {arrange && (
        <div className="room-arrange">
          <button className="drag-handle" onPointerDown={arrange.onGrab} aria-label={`Drag ${dept.name}`} title="Drag to move">
            ⠿ Drag
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
