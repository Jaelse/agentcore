import { useState } from "react";
import type { Me, SystemCard } from "../types";

import type { View } from "../route";

export type { View };

interface Props {
  me: Me;
  card: SystemCard;
  liveSessions: number;
  onStopAll: () => Promise<void>;
  onShowCard: () => void;
  onSignOut: () => void;
  view: View;
  onView: (v: View) => void;
}

export function TopBar({ me, card, liveSessions, onStopAll, onShowCard, onSignOut, view, onView }: Props) {
  const [stopping, setStopping] = useState(false);
  return (
    <header className="topbar">
      <div className="brand">
        <span className="logo" aria-hidden>
          ▲
        </span>
        {card.transparency.system_name}
        <button className="badge ai" onClick={onShowCard} title="This is an AI system. Click for details.">
          AI system
        </button>
        <nav className="nav">
          <button className={`nav-item ${view === "home" ? "active" : ""}`} onClick={() => onView("home")}>
            Home
          </button>
          <button className={`nav-item ${view === "sessions" ? "active" : ""}`} onClick={() => onView("sessions")}>
            Sessions
          </button>
          <button className={`nav-item ${view === "organisations" ? "active" : ""}`} onClick={() => onView("organisations")}>
            Organisation
          </button>
          <button className={`nav-item ${view === "projects" ? "active" : ""}`} onClick={() => onView("projects")}>
            Projects
          </button>
          {me.role !== "viewer" && (
            <button className={`nav-item ${view === "settings" ? "active" : ""}`} onClick={() => onView("settings")}>
              Settings
            </button>
          )}
        </nav>
      </div>
      <div className={`topbar-right`}>
        <span className="muted small topbar-info">
          sandbox: <strong>{card.sandbox_backend}</strong>
          {card.sandbox_backend === "process" && <span className="warn-text"> (not isolated)</span>}
        </span>
        <span className="muted small topbar-info">
          {me.name} · {me.role}
        </span>
        {me.role !== "viewer" && (
          <button
            className="btn danger"
            disabled={stopping || liveSessions === 0}
            title="Immediately kill every running agent, on every node"
            onClick={async () => {
              setStopping(true);
              await onStopAll();
              setStopping(false);
            }}
          >
            ■ Stop all agents{liveSessions > 0 ? ` (${liveSessions})` : ""}
          </button>
        )}
        <button className="btn ghost topbar-info" onClick={onSignOut}>
          Sign out
        </button>
      </div>
    </header>
  );
}
