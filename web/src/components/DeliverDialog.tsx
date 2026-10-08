import { useEffect, useRef, useState } from "react";
import { ApiError, api } from "../api";
import type { CheckResult, PullRequestProposal } from "../types";

export function DeliverDialog({
  sessionId,
  proposal,
  onClose,
  onError,
}: {
  sessionId: string;
  proposal: PullRequestProposal | null;
  onClose: () => void;
  onError: (err: unknown) => void;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  const [title, setTitle] = useState(proposal?.title ?? "");
  const [body, setBody] = useState(proposal?.body ?? "");
  const [draft, setDraft] = useState(true);
  const [busy, setBusy] = useState<"checks" | "deliver" | null>(null);
  const [checks, setChecks] = useState<CheckResult[] | null>(null);
  const [result, setResult] = useState<{ url: string; branch: string } | null>(null);
  const [message, setMessage] = useState<string | null>(null);

  useEffect(() => {
    ref.current?.showModal();
  }, []);

  const runChecks = async () => {
    setBusy("checks");
    setMessage(null);
    try {
      const r = await api.runChecks(sessionId);
      setChecks(r.checks);
    } catch (err) {
      onError(err);
    } finally {
      setBusy(null);
    }
  };

  const deliver = async () => {
    setBusy("deliver");
    setMessage(null);
    try {
      const r = await api.deliver(sessionId, { title, body, draft });
      setChecks(r.checks);
      setResult({ url: r.pull_request.url, branch: r.branch });
    } catch (err) {
      if (err instanceof ApiError && err.body?.checks) {
        setChecks(err.body.checks as CheckResult[]);
        setMessage(err.message);
      } else if (err instanceof ApiError) {
        setMessage(err.message);
      } else onError(err);
    } finally {
      setBusy(null);
    }
  };

  return (
    <dialog ref={ref} className="dialog" onClose={onClose}>
      <header className="dialog-header">
        <h2>Deliver as a pull request</h2>
        <button className="btn ghost" onClick={() => ref.current?.close()}>
          Close
        </button>
      </header>
      {result ? (
        <div className="deliver-done">
          <p className="ok-text">
            ✓ Pushed <code>{result.branch}</code> and opened the pull request.
          </p>
          <a className="btn primary" href={result.url} target="_blank" rel="noreferrer">
            Open pull request
          </a>
          <p className="muted small">
            You can keep talking to the agent (e.g. to address review comments) and deliver again: the same pull
            request is updated.
          </p>
        </div>
      ) : (
        <form
          className="deliver-form"
          onSubmit={(e) => {
            e.preventDefault();
            void deliver();
          }}
        >
          <p className="muted small">
            agentcore runs the team's checks, pushes the agent's branch with its own credentials (the agent never
            has them), opens or updates the pull request, links the issue and moves the board card.
          </p>
          <label>
            Title
            <input value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Proposed by the agent" />
          </label>
          <label>
            Description
            <textarea rows={8} value={body} onChange={(e) => setBody(e.target.value)} />
          </label>
          <label className="inline-check">
            <input type="checkbox" checked={draft} onChange={(e) => setDraft(e.target.checked)} /> Open as draft
          </label>
          {message && <p className="error-text">{message}</p>}
          {checks && (
            <ul className="checks">
              {checks.map((r) => (
                <li key={r.name} className={r.skipped ? "skipped" : r.passed ? "ok" : r.optional ? "warn" : "bad"}>
                  {r.skipped ? "–" : r.passed ? "✓" : "✗"} {r.name}
                  {r.optional && <span className="muted small"> (optional)</span>}
                  {!r.passed && !r.skipped && <pre className="out">{r.detail}</pre>}
                </li>
              ))}
            </ul>
          )}
          <div className="row">
            <button type="button" className="btn" onClick={runChecks} disabled={busy !== null}>
              {busy === "checks" ? "Running checks…" : "Run checks"}
            </button>
            <button className="btn primary" disabled={busy !== null || !title.trim()}>
              {busy === "deliver" ? "Delivering…" : "Run checks & deliver"}
            </button>
          </div>
        </form>
      )}
    </dialog>
  );
}
