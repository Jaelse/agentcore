import type { SessionStatus } from "../types";

const LABELS: Record<SessionStatus, string> = {
  pending: "Starting",
  running: "Running",
  awaiting_approval: "Needs approval",
  awaiting_input: "Waiting for you",
  stopped: "Stopped",
  completed: "Completed",
  failed: "Failed",
};

export function StatusPill({ status }: { status: SessionStatus }) {
  return (
    <span className={`pill status-${status}`}>
      {(status === "running" || status === "pending") && <span className="pulse" aria-hidden />}
      {LABELS[status]}
    </span>
  );
}
