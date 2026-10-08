import { useCallback, useEffect, useState } from "react";
import { ApiError, api } from "../api";
import type { Board, BoardItem, IssueSummary, Me, Project, SessionInfo, SystemCard, TeamRole } from "../types";

interface Props {
  me: Me;
  card: SystemCard;
  onStarted: (s: SessionInfo) => void;
  onError: (err: unknown) => void;
}

export function Projects({ me, card, onStarted, onError }: Props) {
  const [projects, setProjects] = useState<Project[] | null>(null);
  const [roles, setRoles] = useState<TeamRole[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [editing, setEditing] = useState<Project | "new" | null>(null);
  const isAdmin = me.role === "admin";

  const load = useCallback(() => {
    api
      .projects()
      .then((p) => {
        setProjects(p);
        setSelected((cur) => cur ?? p[0]?.id ?? null);
      })
      .catch(onError);
  }, [onError]);

  useEffect(() => {
    load();
    api.roles().then(setRoles).catch(onError);
  }, [load, onError]);

  const project = projects?.find((p) => p.id === selected) ?? null;

  return (
    <div className="layout">
      <aside className="sidebar">
        <h3 className="section-title">Projects</h3>
        {projects === null && <p className="muted small">Loading…</p>}
        {projects?.length === 0 && (
          <p className="muted small">
            No projects yet. {isAdmin ? "Create one to let agents work on a GitHub repository." : "Ask an admin to create one."}
          </p>
        )}
        <ul className="session-list">
          {projects?.map((p) => (
            <li key={p.id}>
              <button className={`session-item ${p.id === selected ? "active" : ""}`} onClick={() => { setSelected(p.id); setEditing(null); }}>
                <strong>{p.name}</strong>
                <div className="muted small mono">{p.repo_owner}/{p.repo_name}</div>
                <div className="muted small">role: {p.role} · agent: {p.agent}</div>
              </button>
            </li>
          ))}
        </ul>
        {isAdmin && (
          <button className="btn" onClick={() => setEditing("new")}>
            + New project
          </button>
        )}
      </aside>
      <main className="main">
        {editing ? (
          <ProjectForm
            project={editing === "new" ? null : editing}
            card={card}
            roles={roles}
            onSaved={(p) => {
              setEditing(null);
              setSelected(p.id);
              load();
            }}
            onDeleted={() => {
              setEditing(null);
              setSelected(null);
              load();
            }}
            onCancel={() => setEditing(null)}
            onError={onError}
          />
        ) : project ? (
          <ProjectView
            key={project.id}
            project={project}
            me={me}
            roles={roles}
            onEdit={isAdmin ? () => setEditing(project) : undefined}
            onStarted={onStarted}
            onError={onError}
          />
        ) : (
          <div className="empty">
            <h2>Agents on your team's real work</h2>
            <p>
              A project connects a GitHub repository (and optionally its project board) to agentcore. Agents pick up
              issues, work in a sandboxed checkout, follow the team's conventions and deliver pull requests.
            </p>
          </div>
        )}
      </main>
    </div>
  );
}

function ProjectView({
  project,
  me,
  roles,
  onEdit,
  onStarted,
  onError,
}: {
  project: Project;
  me: Me;
  roles: TeamRole[];
  onEdit?: () => void;
  onStarted: (s: SessionInfo) => void;
  onError: (err: unknown) => void;
}) {
  const [board, setBoard] = useState<Board | null>(null);
  const [issues, setIssues] = useState<IssueSummary[] | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [sprintOnly, setSprintOnly] = useState(false);
  const [starting, setStarting] = useState<{ issue?: number; title: string } | null>(null);
  const canStart = me.role !== "viewer";

  const load = useCallback(() => {
    setProblem(null);
    const fail = (err: unknown) => setProblem(err instanceof ApiError ? err.message : String(err));
    if (project.board) api.board(project.id).then((r) => setBoard(r.board)).catch(fail);
    else api.issues(project.id).then(setIssues).catch(fail);
  }, [project]);

  useEffect(load, [load]);

  const items = (board?.items ?? []).filter(
    (i) => !sprintOnly || !board?.current_iteration || i.iteration === board.current_iteration,
  );
  const columns = board ? [...board.columns, ...(items.some((i) => !i.status) ? ["No status"] : [])] : [];

  return (
    <div className="session">
      <section className="card session-header">
        <div className="session-header-main">
          <div className="session-title">
            <h2>{project.name}</h2>
          </div>
          <div className="context-chips">
            <a className="chip mono small" href={`https://github.com/${project.repo_owner}/${project.repo_name}`} target="_blank" rel="noreferrer">
              {project.repo_owner}/{project.repo_name}
            </a>
            <span className="chip">⎇ {project.default_branch}</span>
            <span className="chip">👤 default role: {roles.find((r) => r.name === project.role)?.title ?? project.role}</span>
            {board?.url && (
              <a className="chip" href={board.url} target="_blank" rel="noreferrer">
                Board: {board.title} ↗
              </a>
            )}
          </div>
          {project.notes && <p className="muted small">{project.notes}</p>}
        </div>
        <div className="session-actions">
          {canStart && (
            <button className="btn primary" onClick={() => setStarting({ title: "" })}>
              New task…
            </button>
          )}
          <button className="btn ghost" onClick={load}>
            Refresh
          </button>
          {onEdit && (
            <button className="btn ghost" onClick={onEdit}>
              Edit project
            </button>
          )}
        </div>
      </section>

      {problem && <div className="banner warn-banner">{problem}</div>}

      {board && (
        <>
          <div className="row">
            <h3 className="section-title">Board</h3>
            {board.current_iteration && (
              <label className="inline-check small">
                <input type="checkbox" checked={sprintOnly} onChange={(e) => setSprintOnly(e.target.checked)} /> Only{" "}
                {board.current_iteration} (current sprint)
              </label>
            )}
          </div>
          <div className="kanban">
            {columns.map((col) => {
              const cards = items.filter((i) => (i.status ?? "No status") === col);
              return (
                <div key={col} className="kanban-col">
                  <div className="kanban-head">
                    {col} <span className="muted small">{cards.length}</span>
                  </div>
                  {cards.map((item) => (
                    <BoardCard
                      key={item.id}
                      item={item}
                      repo={`${project.repo_owner}/${project.repo_name}`}
                      canStart={canStart}
                      onStart={() => setStarting({ issue: item.number ?? undefined, title: item.title })}
                    />
                  ))}
                </div>
              );
            })}
          </div>
        </>
      )}

      {!project.board && issues && (
        <div className="card pad">
          <h3 className="section-title">Open issues</h3>
          {issues.length === 0 && <p className="muted">No open issues.</p>}
          <ul className="issue-list">
            {issues.map((i) => (
              <li key={i.number}>
                <a href={i.url} target="_blank" rel="noreferrer">
                  #{i.number}
                </a>{" "}
                {i.title} {i.labels.map((l) => <span key={l} className="tag">{l}</span>)}
                {canStart && (
                  <button className="btn ghost small-btn" onClick={() => setStarting({ issue: i.number, title: i.title })}>
                    Start agent
                  </button>
                )}
              </li>
            ))}
          </ul>
        </div>
      )}

      {starting && (
        <StartDialog
          project={project}
          roles={roles}
          issue={starting.issue}
          issueTitle={starting.title}
          onClose={() => setStarting(null)}
          onStarted={(s) => {
            setStarting(null);
            onStarted(s);
          }}
          onError={onError}
        />
      )}
    </div>
  );
}

function BoardCard({ item, repo, canStart, onStart }: { item: BoardItem; repo: string; canStart: boolean; onStart: () => void }) {
  const isIssue = item.content_type === "Issue" && item.repository === repo && item.number !== null;
  return (
    <div className="kanban-card card">
      <div className="small muted">
        {item.content_type === "PullRequest" ? "PR" : item.content_type === "DraftIssue" ? "Draft" : "Issue"}
        {item.number !== null && ` #${item.number}`}
        {item.repository && item.repository !== repo && ` · ${item.repository}`}
      </div>
      {item.url ? (
        <a href={item.url} target="_blank" rel="noreferrer" className="kanban-title">
          {item.title}
        </a>
      ) : (
        <div className="kanban-title">{item.title}</div>
      )}
      <div className="kanban-meta">
        {item.labels.map((l) => (
          <span key={l} className="tag">{l}</span>
        ))}
        {item.iteration && <span className="tag">{item.iteration}</span>}
        {item.assignees.map((a) => (
          <span key={a} className="muted small">@{a}</span>
        ))}
      </div>
      {canStart && isIssue && (
        <button className="btn small-btn" onClick={onStart}>
          ▶ Start agent
        </button>
      )}
    </div>
  );
}

function StartDialog({
  project,
  roles,
  issue,
  issueTitle,
  onClose,
  onStarted,
  onError,
}: {
  project: Project;
  roles: TeamRole[];
  issue?: number;
  issueTitle: string;
  onClose: () => void;
  onStarted: (s: SessionInfo) => void;
  onError: (err: unknown) => void;
}) {
  const [role, setRole] = useState(project.role);
  const [task, setTask] = useState("");
  const [busy, setBusy] = useState(false);
  const roleInfo = roles.find((r) => r.name === role);
  return (
    <dialog
      className="dialog"
      ref={(el) => {
        if (el && !el.open) el.showModal();
      }}
      onClose={onClose}
    >
      <header className="dialog-header">
        <h2>{issue ? `Start an agent on #${issue}` : "New task"}</h2>
        <button className="btn ghost" onClick={onClose}>
          Close
        </button>
      </header>
      <form
        className="deliver-form"
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          try {
            onStarted(await api.startProjectSession(project.id, { issue_number: issue, task, role }));
          } catch (err) {
            onError(err);
          } finally {
            setBusy(false);
          }
        }}
      >
        {issue && <p><strong>{issueTitle}</strong></p>}
        <label>
          Role
          <select value={role} onChange={(e) => setRole(e.target.value)}>
            {roles.map((r) => (
              <option key={r.name} value={r.name}>
                {r.title || r.name}
              </option>
            ))}
          </select>
        </label>
        {roleInfo && <p className="muted small">{roleInfo.description}</p>}
        <label>
          {issue ? "Extra instructions (optional)" : "Task"}
          <textarea rows={4} value={task} onChange={(e) => setTask(e.target.value)} placeholder={issue ? "Anything the agent should know beyond the issue…" : "Describe the work…"} />
        </label>
        <p className="muted small">
          The agent gets a sandboxed checkout of {project.repo_owner}/{project.repo_name}, the role's playbook, the
          team's convention files from the repository{issue ? ", and the issue with its comments" : ""}.
        </p>
        <button className="btn primary" disabled={busy || (!issue && !task.trim())}>
          {busy ? "Starting…" : "Start agent"}
        </button>
      </form>
    </dialog>
  );
}

function ProjectForm({
  project,
  card,
  roles,
  onSaved,
  onDeleted,
  onCancel,
  onError,
}: {
  project: Project | null;
  card: SystemCard;
  roles: TeamRole[];
  onSaved: (p: Project) => void;
  onDeleted: () => void;
  onCancel: () => void;
  onError: (err: unknown) => void;
}) {
  const [name, setName] = useState(project?.name ?? "");
  const [repository, setRepository] = useState(project ? `${project.repo_owner}/${project.repo_name}` : "");
  const [branch, setBranch] = useState(project?.default_branch ?? "main");
  const [agent, setAgent] = useState(project?.agent ?? card.agents[0]?.name ?? "");
  const [role, setRole] = useState(project?.role ?? roles[0]?.name ?? "developer");
  const [useBoard, setUseBoard] = useState(!!project?.board);
  const [boardOwner, setBoardOwner] = useState(project?.board?.owner ?? "");
  const [boardNumber, setBoardNumber] = useState(String(project?.board?.number ?? ""));
  const [statusField, setStatusField] = useState(project?.board?.status_field ?? "Status");
  const [iterationField, setIterationField] = useState(project?.board?.iteration_field ?? "");
  const [columns, setColumns] = useState(
    project?.board?.columns ?? { ready: "Todo", in_progress: "In Progress", in_review: "In Review", done: "Done" },
  );
  const [notes, setNotes] = useState(project?.notes ?? "");
  const [busy, setBusy] = useState(false);

  return (
    <form
      className="card pad project-form"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          const saved = await api.saveProject(project?.id ?? null, {
            name,
            repository,
            default_branch: branch,
            agent,
            role,
            board: useBoard
              ? {
                  owner: boardOwner.trim(),
                  number: Number(boardNumber),
                  status_field: statusField || "Status",
                  iteration_field: iterationField.trim() || null,
                  columns,
                }
              : null,
            notes,
          });
          onSaved(saved);
        } catch (err) {
          onError(err);
        } finally {
          setBusy(false);
        }
      }}
    >
      <h2>{project ? `Edit ${project.name}` : "New project"}</h2>
      <div className="grid-2">
        <label>
          Name
          <input value={name} onChange={(e) => setName(e.target.value)} required />
        </label>
        <label>
          GitHub repository
          <input value={repository} onChange={(e) => setRepository(e.target.value)} placeholder="owner/name" required />
        </label>
        <label>
          Base branch
          <input value={branch} onChange={(e) => setBranch(e.target.value)} />
        </label>
        <label>
          Agent
          <select value={agent} onChange={(e) => setAgent(e.target.value)}>
            {card.agents.map((a) => (
              <option key={a.name} value={a.name}>{a.name}</option>
            ))}
          </select>
        </label>
        <label>
          Default role
          <select value={role} onChange={(e) => setRole(e.target.value)}>
            {roles.map((r) => (
              <option key={r.name} value={r.name}>{r.title || r.name}</option>
            ))}
          </select>
        </label>
      </div>
      <label className="inline-check">
        <input type="checkbox" checked={useBoard} onChange={(e) => setUseBoard(e.target.checked)} /> Link a GitHub
        Projects board (kanban / sprints)
      </label>
      {useBoard && (
        <div className="grid-2">
          <label>
            Board owner (user or organisation)
            <input value={boardOwner} onChange={(e) => setBoardOwner(e.target.value)} required />
          </label>
          <label>
            Board number
            <input value={boardNumber} onChange={(e) => setBoardNumber(e.target.value)} inputMode="numeric" required />
          </label>
          <label>
            Status field
            <input value={statusField} onChange={(e) => setStatusField(e.target.value)} />
          </label>
          <label>
            Sprint (iteration) field, optional
            <input value={iterationField} onChange={(e) => setIterationField(e.target.value)} placeholder="Sprint" />
          </label>
          {(["ready", "in_progress", "in_review", "done"] as const).map((k) => (
            <label key={k}>
              Column for “{k.replace("_", " ")}”
              <input value={columns[k]} onChange={(e) => setColumns({ ...columns, [k]: e.target.value })} />
            </label>
          ))}
        </div>
      )}
      <label>
        Project notes for agents
        <textarea rows={4} value={notes} onChange={(e) => setNotes(e.target.value)} placeholder="Anything not written down in the repository: release freezes, who to ask, areas to avoid…" />
      </label>
      <div className="row">
        <button className="btn primary" disabled={busy}>
          {busy ? "Saving…" : "Save project"}
        </button>
        <button type="button" className="btn ghost" onClick={onCancel}>
          Cancel
        </button>
        {project && (
          <button
            type="button"
            className="btn ghost danger-text"
            onClick={async () => {
              if (!confirm(`Delete project “${project.name}”? Sessions and audit logs are kept.`)) return;
              try {
                await api.deleteProject(project.id);
                onDeleted();
              } catch (err) {
                onError(err);
              }
            }}
          >
            Delete project
          </button>
        )}
      </div>
    </form>
  );
}
