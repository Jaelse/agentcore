import { useEffect, useMemo, useRef, useState } from "react";
import { streamEvents } from "./api";
import type { Action, ActionOutcome, AgentEvent, PendingApproval, Principal, Verdict } from "./types";

export function useInterval(fn: () => void, ms: number) {
  const latest = useRef(fn);
  latest.current = fn;
  useEffect(() => {
    const id = setInterval(() => latest.current(), ms);
    return () => clearInterval(id);
  }, [ms]);
}

export function useEventStream(sessionId: string | null) {
  const [events, setEvents] = useState<AgentEvent[]>([]);
  const [connected, setConnected] = useState(false);

  useEffect(() => {
    setEvents([]);
    setConnected(false);
    if (!sessionId) return;
    let buffer: AgentEvent[] = [];
    let frame = 0;
    // Batch bursts of output into one render per animation frame.
    const flush = () => {
      frame = 0;
      const batch = buffer;
      buffer = [];
      setEvents((prev) => prev.concat(batch));
    };
    const stop = streamEvents(
      sessionId,
      (e) => {
        buffer.push(e);
        if (!frame) frame = requestAnimationFrame(flush);
      },
      setConnected,
    );
    return () => {
      stop();
      if (frame) cancelAnimationFrame(frame);
    };
  }, [sessionId]);

  return { events, connected };
}

export interface ActionRecord {
  actionId: string;
  seq: number;
  timestamp: string;
  action: Action;
  verdict?: Verdict;
  approval?: { approvalId: string; reason: string };
  resolution?: { approved: boolean; by: Principal; comment: string | null };
  outcome?: ActionOutcome;
}

export type TimelineItem =
  | { type: "action"; record: ActionRecord }
  | { type: "event"; event: AgentEvent };

/** Fold the raw event log into a timeline, pending approvals and output. */
export function useSessionModel(events: AgentEvent[]) {
  return useMemo(() => {
    const actions = new Map<string, ActionRecord>();
    const timeline: TimelineItem[] = [];
    const output: Extract<AgentEvent, { event: "output" }>[] = [];
    const pending = new Map<string, PendingApproval>();

    for (const e of events) {
      switch (e.event) {
        case "output":
          output.push(e);
          break;
        case "action_requested": {
          const record: ActionRecord = { actionId: e.action_id, seq: e.seq, timestamp: e.timestamp, action: e.action };
          actions.set(e.action_id, record);
          timeline.push({ type: "action", record });
          break;
        }
        case "policy_evaluated": {
          const r = actions.get(e.action_id);
          if (r) r.verdict = e.verdict;
          break;
        }
        case "approval_requested": {
          const r = actions.get(e.action_id);
          if (r) r.approval = { approvalId: e.approval_id, reason: e.reason };
          pending.set(e.approval_id, {
            approval_id: e.approval_id,
            action_id: e.action_id,
            action: e.action,
            reason: e.reason,
            requested_at: e.timestamp,
            expires_at: "",
          });
          break;
        }
        case "approval_resolved": {
          const r = actions.get(e.action_id);
          if (r) r.resolution = { approved: e.approved, by: e.by, comment: e.comment };
          pending.delete(e.approval_id);
          break;
        }
        case "action_completed": {
          const r = actions.get(e.action_id);
          if (r) r.outcome = e.outcome;
          break;
        }
        case "status_changed":
          break;
        default:
          timeline.push({ type: "event", event: e });
      }
    }
    // Records are mutated in place; copy so React sees fresh objects.
    const items = timeline.map((t) => (t.type === "action" ? { ...t, record: { ...t.record } } : t));
    return { timeline: items, output, pending: [...pending.values()] };
  }, [events]);
}
