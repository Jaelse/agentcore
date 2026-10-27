import type { View } from "./TopBar";

interface Props {
  view: View;
  onView: (v: View) => void;
  // Sessions waiting on a person, shown as a dot on the Sessions tab.
  waiting: number;
}

const icons: Record<string, string> = {
  home: "M3 11l9-8 9 8v9a1 1 0 0 1-1 1h-5v-6H9v6H4a1 1 0 0 1-1-1z",
  sessions: "M4 5h16v11H8l-4 4zM8 9h8M8 12h5",
  org: "M12 4a3 3 0 1 1 0 6 3 3 0 0 1 0-6zM4 20v-1a5 5 0 0 1 5-5h6a5 5 0 0 1 5 5v1",
  more: "M12 9a3 3 0 1 0 0 6 3 3 0 0 0 0-6zM19 12l2-1-1-3-2 .5-1.5-1.5.5-2-3-1-1 2h-2l-1-2-3 1 .5 2L6 8.5 4 8l-1 3 2 1v0l-2 1 1 3 2-.5L7.5 17l-.5 2 3 1 1-2h2l1 2 3-1-.5-2 1.5-1.5 2 .5 1-3z",
};

// Phone-only tab bar (hidden on wide screens, where the top bar's nav is used).
// Projects, provider settings, sign out etc. live behind the last tab.
const tabs: { id: View; label: string; icon: keyof typeof icons; also?: View[] }[] = [
  { id: "home", label: "Home", icon: "home" },
  { id: "sessions", label: "Sessions", icon: "sessions" },
  { id: "organisations", label: "Org", icon: "org" },
  { id: "more", label: "Settings", icon: "more", also: ["settings", "projects"] },
];

export function BottomNav({ view, onView, waiting }: Props) {
  return (
    <nav className="bottom-nav" aria-label="Main">
      {tabs.map((t) => {
        const active = view === t.id || !!t.also?.includes(view);
        return (
          <button
            key={t.id}
            className={`bottom-tab ${active ? "active" : ""}`}
            aria-current={active ? "page" : undefined}
            onClick={() => onView(t.id)}
          >
            <svg viewBox="0 0 24 24" width="24" height="24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinejoin="round" strokeLinecap="round" aria-hidden>
              <path d={icons[t.icon]} />
            </svg>
            <span>{t.label}</span>
            {t.id === "sessions" && waiting > 0 && <i className="tab-dot" aria-label={`${waiting} waiting`} />}
          </button>
        );
      })}
    </nav>
  );
}
