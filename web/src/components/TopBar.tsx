import { useCallback, useEffect, useState } from "react";
import { api } from "../api";
import { useInterval } from "../hooks";
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
  onError: (err: unknown) => void;
}

/** Whether the organisation has departments and all of them are paused; null until known. */
function useOrgPause(enabled: boolean, onError: (err: unknown) => void) {
  const [state, setState] = useState<{ departments: number; allPaused: boolean } | null>(null);
  const load = useCallback(() => {
    if (!enabled) return;
    api
      .org()
      .then((org) =>
        setState({
          departments: org.departments.length,
          allPaused: org.departments.length > 0 && org.departments.every((d) => d.state === "paused"),
        }),
      )
      .catch(() => setState(null));
  }, [enabled]);
  useEffect(load, [load]);
  useInterval(load, 5000);

  const toggle = useCallback(async () => {
    if (!state) return;
    try {
      await (state.allPaused ? api.resumeAll() : api.pauseAll());
    } catch (err) {
      onError(err);
    } finally {
      load();
    }
  }, [state, load, onError]);
  return { state, toggle };
}

export function TopBar({ me, card, liveSessions, onStopAll, onShowCard, onSignOut, view, onView, onError }: Props) {
  const [stopping, setStopping] = useState(false);
  const canOperate = me.role !== "viewer";
  const pause = useOrgPause(canOperate, onError);
  return (
    <header className="topbar">
      <div className="brand">
        <a
          className="brand-home"
          href="/"
          onClick={(e) => {
            // Let modified clicks open a new tab; a plain click stays in the app.
            if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
            e.preventDefault();
            onView("home");
          }}
        >
          <span className="logo" aria-hidden>
            ▲
          </span>
          {card.transparency.system_name}
        </a>
        <button className="badge ai" onClick={onShowCard} title="This is an AI system. Click for details.">
          AI system
        </button>
        <nav className="nav">
          <button className={`nav-item ${view === "home" ? "active" : ""}`} onClick={() => onView("home")}>
            Home
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
        {canOperate && pause.state && pause.state.departments > 0 && (
          <button
            className={`btn pause-button ${pause.state.allPaused ? "resume" : ""}`}
            title={pause.state.allPaused ? "Resume every department" : "Pause every department"}
            aria-label={pause.state.allPaused ? "Resume all" : "Pause all"}
            onClick={pause.toggle}
          >
            {pause.state.allPaused ? "▶" : "❚❚"}
            <span className="pause-label"> {pause.state.allPaused ? "Resume all" : "Pause all"}</span>
          </button>
        )}
        {canOperate && (
          <button
            className="btn danger"
            disabled={stopping || liveSessions === 0}
            title="Immediately kill every running agent"
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
