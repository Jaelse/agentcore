import { useCallback, useMemo, useState } from "react";

const KEY = "agentcore.floor-layout";

interface Stored {
  order: string[];
  hidden: string[];
}

function load(): Stored {
  try {
    const v = JSON.parse(localStorage.getItem(KEY) ?? "null");
    if (v && Array.isArray(v.order) && Array.isArray(v.hidden)) return v;
  } catch {
    // Private mode or bad data: fall back to the default order.
  }
  return { order: [], hidden: [] };
}

/**
 * The user's own arrangement of departments on the floor: order and which are
 * hidden. Kept per browser, because it is a view preference, not organisation data.
 */
export function useFloorLayout(ids: string[]) {
  const [stored, setStored] = useState<Stored>(load);

  const update = useCallback((next: Stored) => {
    setStored(next);
    try {
      localStorage.setItem(KEY, JSON.stringify(next));
    } catch {
      // The arrangement then lasts until the page is closed.
    }
  }, []);

  // Known departments in the saved order, new ones at the end, deleted ones dropped.
  const key = ids.join("\u0000");
  const order = useMemo(() => {
    const live = new Set(ids);
    const known = stored.order.filter((id) => live.has(id));
    const seen = new Set(known);
    return [...known, ...ids.filter((id) => !seen.has(id))];
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, stored.order]);
  const hidden = useMemo(() => new Set(stored.hidden.filter((id) => ids.includes(id))), [key, stored.hidden]);

  const move = useCallback(
    (id: string, to: number) => {
      const next = order.filter((x) => x !== id);
      next.splice(Math.max(0, Math.min(to, next.length)), 0, id);
      update({ order: next, hidden: [...hidden] });
    },
    [order, hidden, update],
  );
  const moveBefore = useCallback(
    (id: string, target: string) => {
      if (id === target) return;
      const next = order.filter((x) => x !== id);
      next.splice(next.indexOf(target), 0, id);
      update({ order: next, hidden: [...hidden] });
    },
    [order, hidden, update],
  );
  const setHidden = useCallback(
    (id: string, value: boolean) => {
      const next = new Set(hidden);
      if (value) next.add(id);
      else next.delete(id);
      update({ order, hidden: [...next] });
    },
    [order, hidden, update],
  );
  const reset = useCallback(() => update({ order: [], hidden: [] }), [update]);

  return { order, hidden, move, moveBefore, setHidden, reset };
}
