import { useCallback, useEffect, useState } from "react";

export type View = "home" | "sessions" | "organisations" | "projects" | "settings" | "more";

/** Sections of the Organisation page that have their own URL. */
export const ORG_SECTIONS = [
  "floor",
  "all",
  "dashboard",
  "improvements",
  "outbox",
  "releases",
  "spending",
  "data",
] as const;
export type OrgSection = (typeof ORG_SECTIONS)[number];

/** What the Organisation page shows: a section, a department, one of its members or its files. */
export type OrgTarget = { section: OrgSection } | { department: string; member?: string; files?: boolean };

export const ORG_FLOOR: OrgTarget = { section: "floor" };

export interface Route {
  view: View;
  /** Selected session id, only for `/sessions/:id`. */
  sessionId: string | null;
  /** Only meaningful for the `organisations` view. */
  org: OrgTarget;
}

const VIEW_PATHS: Record<View, string> = {
  home: "/",
  sessions: "/sessions",
  organisations: "/organisations",
  projects: "/projects",
  settings: "/settings",
  more: "/more",
};

export function orgPath(target: OrgTarget): string {
  if ("department" in target) {
    const dept = `/organisations/departments/${encodeURIComponent(target.department)}`;
    if (target.member) return `${dept}/members/${encodeURIComponent(target.member)}`;
    return target.files ? `${dept}/files` : dept;
  }
  return target.section === "floor" ? "/organisations" : `/organisations/${target.section}`;
}

export function pathFor(route: Route): string {
  if (route.view === "sessions" && route.sessionId) return `/sessions/${encodeURIComponent(route.sessionId)}`;
  if (route.view === "organisations") return orgPath(route.org);
  return VIEW_PATHS[route.view];
}

function parseOrg(parts: string[]): OrgTarget {
  if (parts[0] === "departments" && parts[1]) {
    const department = decodeURIComponent(parts[1]);
    if (parts[2] === "members" && parts[3]) return { department, member: decodeURIComponent(parts[3]) };
    return parts[2] === "files" ? { department, files: true } : { department };
  }
  const section = ORG_SECTIONS.find((s) => s === parts[0]);
  return section ? { section } : ORG_FLOOR;
}

export function parseLocation(loc: { pathname: string; hash: string }): Route {
  const route = (view: View, sessionId: string | null = null, org: OrgTarget = ORG_FLOOR): Route => ({ view, sessionId, org });
  // Links from before path routing used `/#<session id>`.
  if (loc.hash.length > 1 && (loc.pathname === "/" || loc.pathname === "")) {
    return route("sessions", decodeURIComponent(loc.hash.slice(1)));
  }
  const parts = loc.pathname.split("/").filter(Boolean);
  switch (parts[0]) {
    case "sessions":
      return route("sessions", parts[1] ? decodeURIComponent(parts[1]) : null);
    case "organisations":
      return route("organisations", null, parseOrg(parts.slice(1)));
    case "projects":
    case "settings":
    case "more":
      return route(parts[0]);
    default:
      return route("home");
  }
}

export function useRoute() {
  const [route, setRoute] = useState<Route>(() => parseLocation(location));

  useEffect(() => {
    const onPop = () => setRoute(parseLocation(location));
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);

  // Normalise legacy hash links and unknown paths to the canonical path.
  useEffect(() => {
    const canonical = pathFor(route);
    if (location.pathname !== canonical || location.hash) history.replaceState(null, "", canonical);
  }, [route]);

  const go = useCallback((next: Route) => {
    const path = pathFor(next);
    // A different page starts at its top, not where the last one was scrolled to.
    const page = (p: string) => p.split("/")[1] ?? "";
    if (page(path) !== page(location.pathname)) window.scrollTo({ top: 0 });
    if (location.pathname !== path) history.pushState(null, "", path);
    setRoute(next);
  }, []);

  const navigate = useCallback(
    (view: View, sessionId?: string | null) => go({ view, sessionId: sessionId ?? null, org: ORG_FLOOR }),
    [go],
  );

  /** Open the Organisation page at a section or a department. */
  const openOrg = useCallback((org: OrgTarget) => go({ view: "organisations", sessionId: null, org }), [go]);

  return { route, navigate, openOrg };
}
