import { useCallback, useEffect, useState } from "react";

export type View = "home" | "sessions" | "organisations" | "projects" | "settings" | "more";

export interface Route {
  view: View;
  /** Selected session id, only for `/sessions/:id`. */
  sessionId: string | null;
}

const VIEW_PATHS: Record<View, string> = {
  home: "/",
  sessions: "/sessions",
  organisations: "/organisations",
  projects: "/projects",
  settings: "/settings",
  more: "/more",
};

export function pathFor(view: View, sessionId?: string | null): string {
  if (view === "sessions" && sessionId) return `/sessions/${encodeURIComponent(sessionId)}`;
  return VIEW_PATHS[view];
}

export function parseLocation(loc: { pathname: string; hash: string }): Route {
  // Links from before path routing used `/#<session id>`.
  if (loc.hash.length > 1 && (loc.pathname === "/" || loc.pathname === "")) {
    return { view: "sessions", sessionId: decodeURIComponent(loc.hash.slice(1)) };
  }
  const parts = loc.pathname.split("/").filter(Boolean);
  switch (parts[0]) {
    case "sessions":
      return { view: "sessions", sessionId: parts[1] ? decodeURIComponent(parts[1]) : null };
    case "organisations":
      return { view: "organisations", sessionId: null };
    case "projects":
      return { view: "projects", sessionId: null };
    case "settings":
      return { view: "settings", sessionId: null };
    case "more":
      return { view: "more", sessionId: null };
    default:
      return { view: "home", sessionId: null };
  }
}

export function useRoute() {
  const [route, setRoute] = useState<Route>(() => parseLocation(location));

  useEffect(() => {
    const onPop = () => setRoute(parseLocation(location));
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);

  // Normalise legacy hash links to the canonical path.
  useEffect(() => {
    const canonical = pathFor(route.view, route.sessionId);
    if (location.pathname !== canonical || location.hash) history.replaceState(null, "", canonical);
  }, [route]);

  const navigate = useCallback((view: View, sessionId?: string | null) => {
    const path = pathFor(view, sessionId);
    if (location.pathname !== path) history.pushState(null, "", path);
    setRoute({ view, sessionId: sessionId ?? null });
  }, []);

  return { route, navigate };
}
