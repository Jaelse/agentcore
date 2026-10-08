import { useCallback, useEffect, useState } from "react";
import { ApiError, api, getToken, setToken } from "./api";
import { useInterval } from "./hooks";
import type { Me, SessionInfo, SystemCard } from "./types";
import { Login } from "./components/Login";
import { TopBar } from "./components/TopBar";
import { Sidebar } from "./components/Sidebar";
import { SessionView } from "./components/SessionView";
import { SystemCardDialog } from "./components/SystemCardDialog";
import { Settings } from "./components/Settings";
import { Projects } from "./components/Projects";
import { Organisation } from "./components/Organisation";

type View = "sessions" | "org" | "projects" | "settings";

export default function App() {
  const [me, setMe] = useState<Me | null>(null);
  const [needsLogin, setNeedsLogin] = useState(false);
  const [card, setCard] = useState<SystemCard | null>(null);
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [selected, setSelected] = useState<string | null>(() => location.hash.slice(1) || null);
  const [showCard, setShowCard] = useState(false);
  const [view, setView] = useState<View>("sessions");
  const [providerCount, setProviderCount] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  const handleError = useCallback((err: unknown) => {
    if (err instanceof ApiError && err.status === 401) {
      setMe(null);
      setNeedsLogin(true);
    } else {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, []);

  const login = useCallback(async () => {
    try {
      const [who, sc] = await Promise.all([api.whoami(), api.systemCard()]);
      setMe(who);
      setCard(sc);
      setNeedsLogin(false);
    } catch (err) {
      handleError(err);
    }
  }, [handleError]);

  useEffect(() => {
    void login();
  }, [login]);

  const refresh = useCallback(() => {
    if (!me) return;
    api.sessions().then(setSessions).catch(handleError);
  }, [me, handleError]);

  useInterval(refresh, 2000);

  useEffect(() => {
    if (!me || me.role === "viewer") return;
    api
      .providers()
      .then((p) => setProviderCount(p.filter((x) => x.enabled).length))
      .catch(() => setProviderCount(null));
  }, [me, view]);
  useEffect(refresh, [refresh]);

  useEffect(() => {
    history.replaceState(null, "", selected ? `#${selected}` : location.pathname);
  }, [selected]);

  if (needsLogin) {
    return (
      <Login
        hadToken={!!getToken()}
        onSubmit={(token) => {
          setToken(token);
          void login();
        }}
      />
    );
  }
  if (!me || !card) return <div className="splash">Connecting to agentcore…</div>;

  const live = sessions.filter((s) => !["stopped", "completed", "failed"].includes(s.status)).length;
  const current = sessions.find((s) => s.id === selected) ?? null;

  return (
    <div className="app">
      <TopBar
        me={me}
        card={card}
        liveSessions={live}
        onStopAll={async () => {
          try {
            await api.stopAll("emergency stop from web UI");
            refresh();
          } catch (err) {
            handleError(err);
          }
        }}
        onShowCard={() => setShowCard(true)}
        view={view}
        onView={setView}
        onSignOut={() => {
          setToken(null);
          setMe(null);
          setNeedsLogin(true);
        }}
      />
      {error && (
        <div className="banner error" role="alert">
          {error}
          <button className="link" onClick={() => setError(null)}>
            dismiss
          </button>
        </div>
      )}
      {view === "settings" ? (
        <main className="main settings-main">
          <Settings me={me} onError={handleError} />
        </main>
      ) : view === "org" ? (
        <Organisation
          me={me}
          card={card}
          onOpenSession={(id) => {
            setSelected(id);
            setView("sessions");
            refresh();
          }}
          onError={handleError}
        />
      ) : view === "projects" ? (
        <Projects
          me={me}
          card={card}
          onStarted={(s) => {
            setSelected(s.id);
            setView("sessions");
            refresh();
          }}
          onError={handleError}
        />
      ) : (
      <div className={`layout ${current ? "has-session" : ""}`}>
        <Sidebar
          providerCount={providerCount}
          onOpenSettings={() => setView("settings")}
          me={me}
          card={card}
          sessions={sessions}
          selected={selected}
          onSelect={setSelected}
          onCreated={(s) => {
            setSelected(s.id);
            refresh();
          }}
          onError={handleError}
        />
        <main className="main">
          {current ? (
            <SessionView key={current.id} info={current} me={me} onChanged={refresh} onError={handleError} />
          ) : (
            <div className="empty">
              <h2>No session selected</h2>
              <p>Start an agent on a task, or pick a session to watch what it is doing.</p>
            </div>
          )}
        </main>
      </div>
      )}
      <footer className="footer">
        You are interacting with an AI system. Agent output may be incorrect and must be reviewed by a person.{" "}
        <button className="link" onClick={() => setShowCard(true)}>
          About this system
        </button>
      </footer>
      {showCard && <SystemCardDialog card={card} onClose={() => setShowCard(false)} />}
    </div>
  );
}
