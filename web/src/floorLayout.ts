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

/** Drop `id` where `target` is: it takes the target's place and the rest shift. */
export function moveOnto(order: string[], id: string, target: string): string[] {
  if (id === target) return order;
  const next = order.filter((x) => x !== id);
  next.splice(order.indexOf(target), 0, id);
  return next;
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

  const setOrder = useCallback((next: string[]) => update({ order: next, hidden: [...hidden] }), [hidden, update]);
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

  return { order, hidden, setOrder, setHidden, reset };
}
