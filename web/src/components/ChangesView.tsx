import { useCallback, useEffect, useState } from "react";
import { api } from "../api";
import type { Changes, SessionStatus } from "../types";

export function ChangesView({
  sessionId,
  status,
  onError,
}: {
  sessionId: string;
  status: SessionStatus;
  onError: (err: unknown) => void;
}) {
  const [changes, setChanges] = useState<Changes | null | undefined>(undefined);
  const [loading, setLoading] = useState(false);

  const load = useCallback(() => {
    setLoading(true);
    api
      .changes(sessionId)
      .then(setChanges)
      .catch(onError)
      .finally(() => setLoading(false));
  }, [sessionId, onError]);

  // Refresh whenever the agent finishes a turn (or the session ends).
  useEffect(load, [load, status]);

  if (changes === undefined) return <p className="muted pad">Loading changes…</p>;
  if (changes === null) return <p className="muted pad">This session does not work in a repository.</p>;

  const added = changes.files.reduce((n, f) => n + (f.additions ?? 0), 0);
  const removed = changes.files.reduce((n, f) => n + (f.deletions ?? 0), 0);
  return (
    <div className="changes">
      <div className="row">
        <strong>
          {changes.files.length} file{changes.files.length === 1 ? "" : "s"} changed
        </strong>
        <span className="ok-text">+{added}</span>
        <span className="error-text">−{removed}</span>
        {changes.uncommitted && <span className="tag tone-attention">uncommitted changes</span>}
        <span className="muted small">as of {new Date(changes.captured_at).toLocaleTimeString()}</span>
        <button className="btn ghost" onClick={load} disabled={loading}>
          {loading ? "Refreshing…" : "Refresh"}
        </button>
      </div>
      {changes.commits.length > 0 && (
        <div className="card pad">
          <h4>Commits</h4>
          <ul className="commits">
            {changes.commits.map((c) => (
              <li key={c.sha}>
                <code>{c.sha.slice(0, 7)}</code> {c.subject} <span className="muted small">— {c.author}</span>
              </li>
            ))}
          </ul>
        </div>
      )}
      <div className="card pad">
        <h4>Files</h4>
        <ul className="files">
          {changes.files.map((f) => (
            <li key={f.path}>
              <span className={`tag file-${f.status}`}>{f.status}</span> <code>{f.path}</code>{" "}
              {f.additions !== null && <span className="ok-text small">+{f.additions}</span>}{" "}
              {f.deletions !== null && <span className="error-text small">−{f.deletions}</span>}
            </li>
          ))}
        </ul>
      </div>
      <Diff patch={changes.patch} />
      {changes.patch_truncated && <p className="muted small">The diff was truncated.</p>}
    </div>
  );
}

function Diff({ patch }: { patch: string }) {
  if (!patch.trim()) return <p className="muted pad">No differences.</p>;
  return (
    <pre className="diff">
      {patch.split("\n").map((line, i) => {
        const cls = line.startsWith("diff --git")
          ? "file"
          : line.startsWith("@@")
            ? "hunk"
            : line.startsWith("+") && !line.startsWith("+++")
              ? "add"
              : line.startsWith("-") && !line.startsWith("---")
                ? "del"
                : "";
        return (
          <div key={i} className={cls}>
            {line || " "}
          </div>
        );
      })}
    </pre>
  );
}
