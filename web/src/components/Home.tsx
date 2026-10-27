import { useCallback, useEffect, useState } from "react";
import { api } from "../api";
import { useInterval } from "../hooks";
import type { FeedItem, FeedKind, Me, OrgOverview } from "../types";
import { Floor } from "./OfficeFloor";

interface Props {
  me: Me;
  onOpenSession: (id: string) => void;
  onOpenOrganisation: () => void;
  onError: (err: unknown) => void;
}

const PAGE = 40;

const KINDS: { id: FeedKind | "all"; label: string; icon: string }[] = [
  { id: "all", label: "All", icon: "" },
  { id: "session", label: "Sessions", icon: "⚙️" },
  { id: "message", label: "Messages", icon: "💬" },
  { id: "proposal", label: "Proposals", icon: "💡" },
  { id: "outbox", label: "Outbox", icon: "✉️" },
  { id: "release", label: "Releases", icon: "🚀" },
  { id: "goal", label: "Goals", icon: "🎯" },
];
const ICON = Object.fromEntries(KINDS.map((k) => [k.id, k.icon]));

function relative(at: string) {
  const secs = Math.max(0, (Date.now() - new Date(at).getTime()) / 1000);
  if (secs < 45) return "just now";
  if (secs < 3600) return `${Math.round(secs / 60)} min ago`;
  if (secs < 86400) return `${Math.round(secs / 3600)} h ago`;
  return `${Math.round(secs / 86400)} d ago`;
}

function dayLabel(at: string) {
  const day = new Date(at);
  const today = new Date();
  const yesterday = new Date();
  yesterday.setDate(today.getDate() - 1);
  if (day.toDateString() === today.toDateString()) return "Today";
  if (day.toDateString() === yesterday.toDateString()) return "Yesterday";
  return day.toLocaleDateString(undefined, { weekday: "long", day: "numeric", month: "long" });
}

/** The newest items stay fresh; older pages already loaded are kept. */
function merge(fresh: FeedItem[], hasMore: boolean, current: FeedItem[]) {
  if (!hasMore || fresh.length === 0) return fresh;
  const oldest = fresh[fresh.length - 1].at;
  return fresh.concat(current.filter((i) => i.at < oldest));
}

export function Home({ me, onOpenSession, onOpenOrganisation, onError }: Props) {
  const [kind, setKind] = useState<FeedKind | "all">("all");
  const [feed, setFeed] = useState<{ items: FeedItem[]; hasMore: boolean } | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [org, setOrg] = useState<OrgOverview | null>(null);

  const load = useCallback(() => {
    api
      .feed({ kind: kind === "all" ? undefined : kind, limit: PAGE })
      .then((page) => {
        // Polling refreshes the newest page; older pages already shown stay.
        setFeed((current) => {
          const paged = current !== null && current.items.length > page.items.length;
          return {
            items: merge(page.items, page.has_more, current?.items ?? []),
            hasMore: paged ? current.hasMore : page.has_more,
          };
        });
      })
      .catch(onError);
    api.org().then(setOrg).catch(() => setOrg(null));
  }, [kind, onError]);

  useEffect(() => setFeed(null), [kind]);
  useEffect(load, [load]);
  useInterval(load, 10000);

  const older = async () => {
    const items = feed?.items;
    if (!items?.length) return;
    setLoadingMore(true);
    try {
      const page = await api.feed({
        kind: kind === "all" ? undefined : kind,
        before: items[items.length - 1].at,
        limit: PAGE,
      });
      setFeed((current) => ({ items: (current?.items ?? []).concat(page.items), hasMore: page.has_more }));
    } catch (err) {
      onError(err);
    } finally {
      setLoadingMore(false);
    }
  };

  const waiting = org
    ? [
        { label: "proposals", n: org.proposals_pending },
        { label: "drafts to send", n: org.outbox_pending },
        { label: "releases", n: org.releases_pending },
      ].filter((w) => w.n > 0)
    : [];

  const showOrg = (org?.departments.length ?? 0) > 0;

  let lastDay = "";
  return (
    <main className={`main home-main ${showOrg ? "with-org" : ""}`}>
      <header className="home-head">
        <h1>{org?.profile.company_name ? `${org.profile.company_name}` : "Home"}</h1>
        <p className="muted">
          What has been happening lately, newest first. Welcome back, {me.name}.
        </p>
        {waiting.length > 0 && (
          <div className="home-waiting" role="status">
            <strong>Waiting for a person:</strong>
            {waiting.map((w) => (
              <button key={w.label} className="chip warn" onClick={onOpenOrganisation}>
                {w.n} {w.label}
              </button>
            ))}
          </div>
        )}
      </header>

      <div className={`home-grid ${showOrg ? "with-org" : ""}`}>
        {showOrg && org && (
          <section className="home-org" aria-label="Organisation">
            <div className="home-org-head">
              <h2 className="section-title">Organisation</h2>
              <button className="link small" onClick={onOpenOrganisation}>
                Open →
              </button>
            </div>
            <Floor org={org} onOpenDepartment={onOpenOrganisation} onOpenSession={onOpenSession} />
          </section>
        )}

        <section className="home-feed" aria-label="Feed">
          <div className="home-filters" role="tablist" aria-label="Filter the feed">
            {KINDS.map((k) => (
              <button
                key={k.id}
                role="tab"
                aria-selected={kind === k.id}
                className={`tab ${kind === k.id ? "active" : ""}`}
                onClick={() => setKind(k.id)}
              >
                {k.icon && <span aria-hidden>{k.icon} </span>}
                {k.label}
              </button>
            ))}
          </div>

          {feed === null ? (
            <div className="empty">Loading the feed…</div>
          ) : feed.items.length === 0 ? (
            <div className="empty">
              <h2>Nothing yet</h2>
              <p>
                {kind === "all"
                  ? "Start an agent or set a goal, and what happens will show up here."
                  : "Nothing of this kind has happened lately."}
              </p>
            </div>
          ) : (
            <ol className="home-feed-list">
              {feed.items.map((item) => {
                const label = dayLabel(item.at);
                const heading = label !== lastDay;
                lastDay = label;
                const open = item.session ? () => onOpenSession(item.session!) : onOpenOrganisation;
                return (
                  <li key={item.id} className="feed-entry">
                    {heading && <h2 className="feed-day section-title">{label}</h2>}
                    <button className={`card feed-item tone-${item.tone}`} onClick={open}>
                      <span className="feed-icon" aria-hidden>
                        {ICON[item.kind]}
                      </span>
                      <span className="feed-body">
                        <strong className="feed-title">{item.title}</strong>
                        {item.detail && <span className="feed-detail">{item.detail}</span>}
                        <span className="muted small">
                          {[item.actor, item.department, item.project].filter(Boolean).join(" · ")}
                          {(item.actor || item.department || item.project) && " · "}
                          <time dateTime={item.at} title={new Date(item.at).toLocaleString()}>
                            {relative(item.at)}
                          </time>
                        </span>
                      </span>
                      {item.tone === "warn" && <span className="tag tone-attention">waiting</span>}
                      {item.tone === "bad" && <span className="tag tone-bad">problem</span>}
                    </button>
                  </li>
                );
              })}
            </ol>
          )}

          {feed?.hasMore && (
            <div className="home-more">
              <button className="btn" disabled={loadingMore} onClick={older}>
                {loadingMore ? "Loading…" : "Show older"}
              </button>
            </div>
          )}
        </section>
      </div>
    </main>
  );
}
