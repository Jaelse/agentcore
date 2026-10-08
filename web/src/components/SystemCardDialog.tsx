import { useEffect, useRef } from "react";
import type { SystemCard } from "../types";

export function SystemCardDialog({ card, onClose }: { card: SystemCard; onClose: () => void }) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    ref.current?.showModal();
  }, []);
  const t = card.transparency;
  return (
    <dialog ref={ref} className="dialog" onClose={onClose}>
      <header className="dialog-header">
        <h2>About this AI system</h2>
        <button className="btn ghost" onClick={() => ref.current?.close()}>
          Close
        </button>
      </header>
      <dl className="meta stacked">
        <div><dt>System</dt><dd>{t.system_name} v{card.version}</dd></div>
        <div><dt>Provider</dt><dd>{t.provider}</dd></div>
        <div><dt>Contact</dt><dd>{t.contact}</dd></div>
        <div><dt>Intended purpose</dt><dd>{t.intended_purpose}</dd></div>
        <div><dt>Sandbox</dt><dd>{card.sandbox_backend}</dd></div>
        <div><dt>Audit log retention</dt><dd>at least {card.audit_retention_days} days</dd></div>
      </dl>
      <h3>Known limitations</h3>
      <ul>{t.limitations.map((l) => <li key={l}>{l}</li>)}</ul>
      <h3>Agents</h3>
      <ul>
        {card.agents.map((a) => (
          <li key={a.name}>
            <strong>{a.name}</strong> ({a.adapter}){a.description && <> · {a.description}</>}
          </li>
        ))}
      </ul>
      <h3>Policies</h3>
      {card.policies.map((p) => (
        <details key={p.name} className="policy">
          <summary>
            <strong>{p.name}</strong> · default <code>{p.default}</code> · {p.rules.length} rules
          </summary>
          <p className="muted">{p.description}</p>
          <p className="mono small">sha256:{p.digest}</p>
          <ul>
            {p.rules.map((r) => (
              <li key={r.id}>
                <span className={`tag effect-${r.effect}`}>{r.effect.replace("_", " ")}</span> <code>{r.id}</code>
                {r.kinds.length > 0 && <span className="muted"> [{r.kinds.join(", ")}]</span>}
                {r.description && <> · {r.description}</>}
              </li>
            ))}
          </ul>
        </details>
      ))}
    </dialog>
  );
}
