import type { Me, SystemCard } from "../types";
import type { View } from "./TopBar";

interface Props {
  me: Me;
  card: SystemCard;
  onView: (v: View) => void;
  onShowCard: () => void;
  onSignOut: () => void;
}

// The phone "Settings" tab: everything that has no tab of its own.
export function MoreMenu({ me, card, onView, onShowCard, onSignOut }: Props) {
  return (
    <main className="main more-menu">
      <h1>Settings</h1>
      <p className="muted small">
        {me.name} · {me.role}
      </p>
      <ul className="menu-list card">
        <li>
          <button onClick={() => onView("projects")}>
            <span>Projects</span>
            <span aria-hidden>›</span>
          </button>
        </li>
        {me.role !== "viewer" && (
          <li>
            <button onClick={() => onView("settings")}>
              <span>Providers &amp; integrations</span>
              <span aria-hidden>›</span>
            </button>
          </li>
        )}
        <li>
          <button onClick={onShowCard}>
            <span>About this system</span>
            <span aria-hidden>›</span>
          </button>
        </li>
      </ul>
      <p className="muted small">
        Sandbox: <strong>{card.sandbox_backend}</strong>
        {card.sandbox_backend === "process" && <span className="warn-text"> (not isolated)</span>}
      </p>
      <button className="btn" onClick={onSignOut}>
        Sign out
      </button>
    </main>
  );
}
