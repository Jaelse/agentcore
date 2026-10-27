import { useCallback, useEffect, useState } from "react";
import { api } from "../api";
import { useInterval } from "../hooks";
import type { BudgetStatus, OrgMetrics } from "../types";
import { compact, money } from "./Dashboard";

const DAYS = 7;

/** What the budget's period used, as a share of its limit. */
function used(b: BudgetStatus) {
  return (b.unit === "tokens" ? b.spent_tokens : b.spent_micros) / b.limit_micros;
}

function describe(b: BudgetStatus, cur: string, deptName: string | null) {
  const spent = b.unit === "tokens" ? `${compact(b.spent_tokens)} tokens` : money(b.spent_micros, cur);
  const limit = b.unit === "tokens" ? `${compact(b.limit_micros)} tokens` : money(b.limit_micros, cur);
  return `${deptName ?? "Organisation"} · ${b.period}: ${spent} of ${limit}`;
}

/** Tokens used, agent-hours and how fast the budgets are burning, at a glance. */
export function HomeMetrics({ onOpen, onError }: { onOpen: () => void; onError: (err: unknown) => void }) {
  const [data, setData] = useState<OrgMetrics | null>(null);

  const load = useCallback(() => {
    api.metrics(DAYS).then(setData).catch(onError);
  }, [onError]);
  useEffect(load, [load]);
  useInterval(load, 30000);

  if (!data) return null;
  const m = data.metrics;
  const tokens = m.daily.reduce((a, d) => a + d.tokens, 0);
  const hours = m.departments.reduce((a, d) => a + d.agent_hours, 0);
  // The budget closest to running out is the one worth watching.
  const burn = [...data.budgets].sort((a, b) => used(b) - used(a))[0];
  const share = burn ? used(burn) : 0;
  const level = share >= 1 ? "meter-critical" : burn && share * 100 >= burn.warn_percent ? "meter-warning" : "";
  const deptName = burn?.department_id ? (m.departments.find((d) => d.id === burn.department_id)?.name ?? null) : null;

  return (
    <section className="home-metrics" aria-label="Usage">
      <div className="stat-tile">
        <span className="stat-label">Tokens used</span>
        <span className="stat-value" title={`${tokens.toLocaleString()} tokens`}>
          {compact(tokens)}
        </span>
        <span className="muted small">
          last {DAYS} days · {compact(m.daily.reduce((a, d) => a + d.model_calls, 0))} model calls
        </span>
      </div>
      <div className="stat-tile">
        <span className="stat-label">Agent-hours</span>
        <span className="stat-value">{hours.toFixed(1)}</span>
        <span className="muted small">last {DAYS} days · sessions running</span>
      </div>
      <button className="stat-tile home-burn" onClick={onOpen} aria-label="Open the organisation">
        <span className="stat-label">Budget burn</span>
        {burn ? (
          <>
            <span className="stat-value">{Math.round(share * 100)}%</span>
            <div
              className="meter"
              role="progressbar"
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={Math.min(100, Math.round(share * 100))}
            >
              <span className={level} style={{ width: `${Math.min(100, share * 100)}%` }} />
            </div>
            <span className="muted small">{describe(burn, data.currency, deptName)}</span>
          </>
        ) : (
          <>
            <span className="stat-value">–</span>
            <span className="muted small">no budgets set</span>
          </>
        )}
      </button>
    </section>
  );
}
