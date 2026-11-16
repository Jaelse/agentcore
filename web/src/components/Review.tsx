import { useEffect, useState } from "react";
import { api } from "../api";
import type { AgentEvent, ReviewDevice, ReviewInfo, ReviewStep } from "../types";

/** Where the review of this session's pull request stands, from its events. */
export type ReviewState =
  | { kind: "none" }
  | { kind: "open"; review: ReviewInfo }
  | { kind: "waiting"; review: ReviewInfo }
  | { kind: "approved" };

export function reviewState(events: AgentEvent[]): ReviewState {
  let state: ReviewState = { kind: "none" };
  for (const e of events) {
    if (e.event === "review_started") state = { kind: "open", review: e.review };
    if (e.event === "review_finished") {
      if (e.verdict === "approved") state = { kind: "approved" };
      else if (state.kind === "open") state = { kind: "waiting", review: state.review };
    }
  }
  return state;
}

function openSession(id: string) {
  history.pushState(null, "", `/sessions/${encodeURIComponent(id)}`);
  window.dispatchEvent(new PopStateEvent("popstate"));
}

/** “Review with agent”: the agent shows the reviewer the change on the preview. */
export function ReviewStart({
  sessionId,
  onStarted,
  onError,
}: {
  sessionId: string;
  onStarted: () => void;
  onError: (err: unknown) => void;
}) {
  const [choosing, setChoosing] = useState(false);
  const [starting, setStarting] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const start = async (device: ReviewDevice) => {
    setStarting(true);
    try {
      const res = await api.startReview(sessionId, device);
      setChoosing(false);
      if (res.session_id && res.session_id !== sessionId) openSession(res.session_id);
      else if (res.note) setNote(res.note);
      onStarted();
    } catch (err) {
      onError(err);
    } finally {
      setStarting(false);
    }
  };
  if (note) return <span className="muted small review-note">{note}</span>;
  if (!choosing)
    return (
      <button
        className="btn primary"
        onClick={() => setChoosing(true)}
        title="The agent opens the preview, shows you how the change works step by step, and tries what you ask"
      >
        Review with agent
      </button>
    );
  return (
    <span className="review-choose">
      <span className="muted small">Show it on</span>
      <button className="btn primary" disabled={starting} onClick={() => start("phone")}>
        📱 Phone
      </button>
      <button className="btn" disabled={starting} onClick={() => start("desktop")}>
        🖥 Desktop
      </button>
      <button className="btn ghost" disabled={starting} onClick={() => setChoosing(false)}>
        Cancel
      </button>
    </span>
  );
}

/** Approve or request changes while a review is open; what happens next after. */
export function ReviewBar({
  sessionId,
  state,
  canAct,
  onError,
}: {
  sessionId: string;
  state: ReviewState;
  canAct: boolean;
  onError: (err: unknown) => void;
}) {
  const [asking, setAsking] = useState(false);
  const [comment, setComment] = useState("");
  const [busy, setBusy] = useState(false);
  if (state.kind === "none" || state.kind === "approved") return null;
  if (state.kind === "waiting")
    return (
      <div className="review-bar card waiting">
        <strong>Changes requested.</strong>{" "}
        <span className="muted">
          The agent is making them. When the new preview is ready it shows you what changed, here.
        </span>
      </div>
    );
  const finish = async (verdict: "approved" | "changes_requested") => {
    setBusy(true);
    try {
      const res = await api.finishReview(sessionId, verdict, comment.trim());
      setAsking(false);
      setComment("");
      if (res.session_id && res.session_id !== sessionId) openSession(res.session_id);
    } catch (err) {
      onError(err);
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="review-bar card">
      <div className="review-bar-head">
        <span>
          <strong>Reviewing with the agent</strong>{" "}
          <span className="muted small">
            on the {state.review.device} preview ·{" "}
            <a href={state.review.preview_url} target="_blank" rel="noreferrer">
              open it yourself ↗
            </a>
          </span>
        </span>
        {canAct && !asking && (
          <span className="review-actions">
            <button className="btn approve" disabled={busy} onClick={() => finish("approved")}>
              ✓ Approve
            </button>
            <button className="btn" disabled={busy} onClick={() => setAsking(true)}>
              Request changes
            </button>
          </span>
        )}
      </div>
      {canAct && asking && (
        <form
          className="review-ask"
          onSubmit={(e) => {
            e.preventDefault();
            void finish("changes_requested");
          }}
        >
          <textarea
            rows={3}
            autoFocus
            placeholder="What should change? The agent also goes by what you asked during the review."
            value={comment}
            onChange={(e) => setComment(e.target.value)}
          />
          <div className="row">
            <button type="button" className="btn ghost" onClick={() => setAsking(false)}>
              Cancel
            </button>
            <button className="btn primary" disabled={busy}>
              Ask for these changes
            </button>
          </div>
        </form>
      )}
      {!asking && (
        <p className="muted small review-hint">
          Ask the agent below to try anything on the preview: “what happens if I leave the name empty?”
        </p>
      )}
    </div>
  );
}

const OUTCOME = {
  as_expected: { icon: "✓", label: "Works as expected", tone: "ok" },
  problem: { icon: "!", label: "Problem", tone: "bad" },
  info: { icon: "i", label: "", tone: "" },
} as const;

/** One step the agent showed: screenshot, title, what it did and expected. */
export function ReviewStepCard({
  sessionId,
  step,
  time,
}: {
  sessionId: string;
  step: ReviewStep;
  time: React.ReactNode;
}) {
  const [src, setSrc] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    if (!step.screenshot) return;
    let url: string | null = null;
    let live = true;
    api
      .screenshot(sessionId, step.screenshot)
      .then((u) => {
        url = u;
        if (live) setSrc(u);
        else URL.revokeObjectURL(u);
      })
      .catch(() => live && setFailed(true));
    return () => {
      live = false;
      if (url) URL.revokeObjectURL(url);
    };
  }, [sessionId, step.screenshot]);
  const outcome = OUTCOME[step.outcome];
  return (
    <div className={`card conv-card review-step ${outcome.tone}`}>
      <div className="review-step-head">
        <strong>
          {outcome.icon !== "i" && <span className={`review-icon ${outcome.tone}`}>{outcome.icon}</span>} {step.title}
        </strong>{" "}
        {time}
      </div>
      {step.note && <p className="review-step-note">{step.note}</p>}
      {step.screenshot && (
        <a
          className="review-shot"
          href={src ?? undefined}
          target="_blank"
          rel="noreferrer"
          title="Open the screenshot full size"
        >
          {src ? (
            <img src={src} alt={`The preview after: ${step.title}`} />
          ) : (
            <span className="muted small">{failed ? "The screenshot could not be loaded." : "Loading screenshot…"}</span>
          )}
        </a>
      )}
      <div className="muted small review-url">
        {outcome.label && <span className={`${outcome.tone}-text`}>{outcome.label} · </span>}
        <span className="mono">{step.url}</span>
      </div>
    </div>
  );
}
