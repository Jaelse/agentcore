import { useEffect, useMemo, useState } from "react";
import { ApiError, api } from "../api";
import type { DataSource, Department, DepartmentFile, Me, OrgAgent, OrgOverview, WorkspaceListing } from "../types";

interface Props {
  org: OrgOverview;
  me: Me;
  tick: number;
  onError: (err: unknown) => void;
}

type Opened =
  | { kind: "shared"; dept: Department; path: string }
  | { kind: "workspace"; agent: OrgAgent; dept: Department; path: string }
  | { kind: "source"; source: DataSource };

function size(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

const pad = (depth: number) => ({ paddingLeft: 8 + depth * 14 });

/** Everything the organisation holds, read-only: each department's shared
 *  files, each agent's workspace and the business data sources. */
export function FileExplorer({ org, me, tick, onError }: Props) {
  const [opened, setOpened] = useState<Opened | null>(null);
  const isOpen = (o: Opened) => JSON.stringify(key(o)) === JSON.stringify(opened && key(opened));
  const tree = { isOpen, open: setOpened, tick, onError };

  return (
    <div className="explorer">
      <section className="card explorer-tree" aria-label="Files">
        <h3 className="section-title">Departments</h3>
        {org.departments.length === 0 && <p className="muted small">No departments yet.</p>}
        {org.departments.map((d) => (
          <DepartmentNode key={d.id} dept={d} {...tree} />
        ))}
        <h3 className="section-title">Business data</h3>
        {org.data_sources.length === 0 && <p className="muted small">No data sources yet.</p>}
        {org.data_sources.map((s) => (
          <Row
            key={s.id}
            depth={0}
            icon="🗄"
            label={s.name}
            meta={s.kind === "table" && s.rows !== null ? `${s.rows} rows` : s.kind}
            active={isOpen({ kind: "source", source: s })}
            onClick={() => setOpened({ kind: "source", source: s })}
          />
        ))}
      </section>
      <section className="card explorer-view">
        {!opened ? (
          <div className="empty">
            <h2>Organisation files</h2>
            <p>Pick a file on the left: department files, agents' workspaces or business data.</p>
          </div>
        ) : opened.kind === "source" ? (
          <SourceView key={opened.source.id} source={opened.source} org={org} me={me} onError={onError} />
        ) : (
          <FileView key={JSON.stringify(key(opened))} opened={opened} tick={tick} onError={onError} />
        )}
      </section>
    </div>
  );
}

function key(o: Opened) {
  return o.kind === "source" ? [o.kind, o.source.id] : o.kind === "shared" ? [o.kind, o.dept.id, o.path] : [o.kind, o.agent.id, o.path];
}

interface TreeProps {
  isOpen: (o: Opened) => boolean;
  open: (o: Opened) => void;
  tick: number;
  onError: (err: unknown) => void;
}

function Row({
  depth,
  icon,
  label,
  meta,
  active,
  expanded,
  onClick,
}: {
  depth: number;
  icon: string;
  label: string;
  meta?: string;
  active?: boolean;
  expanded?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      className={`tree-row ${active ? "active" : ""}`}
      style={pad(depth)}
      onClick={onClick}
      aria-expanded={expanded}
      title={label}
    >
      <span className="tree-caret" aria-hidden>
        {expanded === undefined ? "" : expanded ? "▾" : "▸"}
      </span>
      <span aria-hidden>{icon}</span>
      <span className="tree-label">{label}</span>
      {meta && <span className="muted small tree-meta">{meta}</span>}
    </button>
  );
}

function DepartmentNode({ dept, ...tree }: { dept: Department } & TreeProps) {
  const [expanded, setExpanded] = useState(false);
  return (
    <>
      <Row
        depth={0}
        icon="🏢"
        label={dept.name}
        meta={`${dept.agents.length} agent${dept.agents.length === 1 ? "" : "s"}`}
        expanded={expanded}
        onClick={() => setExpanded(!expanded)}
      />
      {expanded && (
        <>
          <SharedFiles dept={dept} {...tree} />
          {dept.agents.map((a) => (
            <WorkspaceDir key={a.id} agent={a} dept={dept} path="" label={`${a.name}'s workspace`} depth={1} {...tree} />
          ))}
        </>
      )}
    </>
  );
}

// ---- department (shared) files: flat paths shown as folders ----

interface Folder {
  folders: Map<string, Folder>;
  files: DepartmentFile[];
}

function folderTree(files: DepartmentFile[]): Folder {
  const root: Folder = { folders: new Map(), files: [] };
  for (const f of files) {
    const parts = f.path.split("/").filter(Boolean);
    let at = root;
    for (const part of parts.slice(0, -1)) {
      if (!at.folders.has(part)) at.folders.set(part, { folders: new Map(), files: [] });
      at = at.folders.get(part)!;
    }
    at.files.push(f);
  }
  return root;
}

function SharedFiles({ dept, ...tree }: { dept: Department } & TreeProps) {
  const [expanded, setExpanded] = useState(false);
  const [files, setFiles] = useState<DepartmentFile[] | null>(null);
  const { tick, onError } = tree;
  useEffect(() => {
    if (expanded) api.departmentFiles(dept.id).then(setFiles).catch(onError);
  }, [expanded, dept.id, tick, onError]);
  const root = useMemo(() => folderTree(files ?? []), [files]);
  return (
    <>
      <Row
        depth={1}
        icon="📁"
        label="Shared files"
        meta={files ? String(files.length) : undefined}
        expanded={expanded}
        onClick={() => setExpanded(!expanded)}
      />
      {expanded && files?.length === 0 && (
        <p className="muted small" style={pad(3)}>
          No shared files.
        </p>
      )}
      {expanded && <SharedFolder folder={root} dept={dept} depth={2} {...tree} />}
    </>
  );
}

function SharedFolder({ folder, dept, depth, ...tree }: { folder: Folder; dept: Department; depth: number } & TreeProps) {
  return (
    <>
      {[...folder.folders.entries()]
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([name, sub]) => (
          <SharedSubfolder key={name} name={name} folder={sub} dept={dept} depth={depth} {...tree} />
        ))}
      {folder.files.map((f) => {
        const o: Opened = { kind: "shared", dept, path: f.path };
        return (
          <Row
            key={f.path}
            depth={depth}
            icon="📄"
            label={f.path.split("/").pop() ?? f.path}
            meta={size(f.size)}
            active={tree.isOpen(o)}
            onClick={() => tree.open(o)}
          />
        );
      })}
    </>
  );
}

function SharedSubfolder({ name, ...rest }: { name: string; folder: Folder; dept: Department; depth: number } & TreeProps) {
  const [expanded, setExpanded] = useState(false);
  return (
    <>
      <Row depth={rest.depth} icon="📁" label={name} expanded={expanded} onClick={() => setExpanded(!expanded)} />
      {expanded && <SharedFolder {...rest} depth={rest.depth + 1} />}
    </>
  );
}

// ---- agent workspaces: listed from the data volume as they are opened ----

function WorkspaceDir({
  agent,
  dept,
  path,
  label,
  depth,
  ...tree
}: { agent: OrgAgent; dept: Department; path: string; label: string; depth: number } & TreeProps) {
  const [expanded, setExpanded] = useState(false);
  const [listing, setListing] = useState<WorkspaceListing | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const { tick, onError } = tree;
  useEffect(() => {
    if (!expanded) return;
    api
      .workspace(agent.id, path)
      .then((l) => {
        setListing(l);
        setNote(null);
      })
      .catch((err) => {
        if (err instanceof ApiError && err.status === 404) setNote(err.message);
        else onError(err);
      });
  }, [expanded, agent.id, path, tick, onError]);
  return (
    <>
      <Row
        depth={depth}
        icon={path ? "📁" : "🤖"}
        label={label}
        expanded={expanded}
        onClick={() => setExpanded(!expanded)}
      />
      {expanded && note && (
        <p className="muted small" style={pad(depth + 2)}>
          {note}
        </p>
      )}
      {expanded && listing?.entries.length === 0 && (
        <p className="muted small" style={pad(depth + 2)}>
          Empty.
        </p>
      )}
      {expanded &&
        listing?.entries.map((e) => {
          const child = path ? `${path}/${e.name}` : e.name;
          if (e.kind === "dir")
            return <WorkspaceDir key={e.name} agent={agent} dept={dept} path={child} label={e.name} depth={depth + 1} {...tree} />;
          const o: Opened = { kind: "workspace", agent, dept, path: child };
          return (
            <Row
              key={e.name}
              depth={depth + 1}
              icon={e.kind === "symlink" ? "🔗" : "📄"}
              label={e.name}
              meta={e.kind === "file" ? size(e.size) : "link"}
              active={tree.isOpen(o)}
              onClick={() => tree.open(o)}
            />
          );
        })}
      {expanded && listing?.truncated && (
        <p className="muted small" style={pad(depth + 2)}>
          Only the first {listing.entries.length} entries are shown.
        </p>
      )}
    </>
  );
}

// ---- viewers ----

interface Shown {
  content: string | null;
  size: number;
  binary: boolean;
  truncated: boolean;
  by: string | null;
  at: string | null;
}

function FileView({ opened, tick, onError }: { opened: Exclude<Opened, { kind: "source" }>; tick: number; onError: (e: unknown) => void }) {
  const [file, setFile] = useState<Shown | null>(null);
  const [gone, setGone] = useState<string | null>(null);
  useEffect(() => {
    const load: Promise<Shown> =
      opened.kind === "shared"
        ? Promise.all([api.departmentFile(opened.dept.id, opened.path), api.departmentFiles(opened.dept.id)]).then(([f, all]) => {
            const info = all.find((x) => x.path === opened.path);
            return { content: f.content, size: info?.size ?? f.content.length, binary: false, truncated: false, by: info?.updated_by ?? null, at: info?.updated_at ?? null };
          })
        : api.workspaceFile(opened.agent.id, opened.path).then((f) => ({ ...f, by: null, at: f.modified }));
    load
      .then((f) => {
        setFile(f);
        setGone(null);
      })
      .catch((err) => {
        if (err instanceof ApiError && (err.status === 404 || err.status === 400)) setGone(err.message);
        else onError(err);
      });
  }, [opened, tick, onError]);

  const where = opened.kind === "shared" ? `${opened.dept.name} · shared files` : `${opened.dept.name} · ${opened.agent.name}'s workspace`;
  return (
    <div className="explorer-file">
      <div className="muted small">{where}</div>
      <h3 className="mono explorer-path">{opened.path}</h3>
      {gone ? (
        <p className="muted">{gone}</p>
      ) : !file ? (
        <p className="muted small">Loading…</p>
      ) : (
        <>
          <div className="muted small">
            {size(file.size)}
            {file.at && ` · changed ${new Date(file.at).toLocaleString()}`}
            {file.by && ` by ${file.by}`}
          </div>
          {file.binary ? (
            <p className="muted">This is a binary file; it cannot be shown here.</p>
          ) : (
            <pre className="mono-pre explorer-content">{file.content}</pre>
          )}
          {file.truncated && <p className="muted small">Only the first MB is shown.</p>}
        </>
      )}
    </div>
  );
}

function SourceView({ source, org, me, onError }: { source: DataSource; org: OrgOverview; me: Me; onError: (e: unknown) => void }) {
  const [preview, setPreview] = useState<{ ok: boolean; result?: unknown; error?: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const names = source.departments.map((id) => org.departments.find((d) => d.id === id)?.name ?? id);
  const result = preview?.result as { rows?: Record<string, unknown>[]; columns?: string[] } | undefined;
  const rows = result?.rows;
  // Tables name their columns in order; other rows only have keys.
  const columns = rows && rows.length > 0 ? (result?.columns ?? Object.keys(rows[0])) : [];

  return (
    <div className="explorer-file">
      <div className="muted small">Business data · {source.kind}</div>
      <h3 className="mono explorer-path">{source.name}</h3>
      {source.description && <p>{source.description}</p>}
      <div className="muted small">
        {source.enabled ? "On" : "Off"}
        {source.rows !== null && ` · ${source.rows} rows`} · read by {names.length > 0 ? names.join(", ") : "no department"} · changed{" "}
        {new Date(source.updated_at).toLocaleString()} by {source.updated_by}
      </div>
      {me.role === "admin" ? (
        <div className="row">
          <button
            className="btn"
            disabled={busy}
            onClick={async () => {
              setBusy(true);
              try {
                setPreview(await api.testDataSource(source.id, source.kind === "table" ? { limit: 50 } : undefined));
              } catch (err) {
                onError(err);
              } finally {
                setBusy(false);
              }
            }}
          >
            {busy ? "Reading…" : source.kind === "table" ? "Show the first rows" : "Try a read"}
          </button>
        </div>
      ) : (
        <p className="muted small">An admin can look inside this source.</p>
      )}
      {preview && !preview.ok && <p className="warn-text">{preview.error}</p>}
      {preview?.ok &&
        (columns.length > 0 ? (
          <div className="explorer-table">
            <table>
              <thead>
                <tr>
                  {columns.map((c) => (
                    <th key={c}>{c}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {rows!.map((r, i) => (
                  <tr key={i}>
                    {columns.map((c) => (
                      <td key={c}>{typeof r[c] === "object" ? JSON.stringify(r[c]) : String(r[c] ?? "")}</td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <pre className="mono-pre explorer-content">{JSON.stringify(preview.result, null, 2).slice(0, 20000)}</pre>
        ))}
    </div>
  );
}
