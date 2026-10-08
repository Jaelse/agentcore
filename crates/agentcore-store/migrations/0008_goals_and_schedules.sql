-- Goals people set for the organisation, and check-ins that wake departments
-- on a schedule. See docs/MULTI_AGENT.md.
CREATE TABLE org_goals (
    id            uuid PRIMARY KEY,
    title         text        NOT NULL,
    description   text        NOT NULL DEFAULT '',
    department_id uuid REFERENCES departments (id) ON DELETE SET NULL,
    status        text        NOT NULL DEFAULT 'active'
                              CHECK (status IN ('active', 'achieved', 'dropped')),
    progress      text        NOT NULL DEFAULT '',
    progress_by   text,
    progress_at   timestamptz,
    created_by    text        NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE org_schedules (
    id            uuid PRIMARY KEY,
    department_id uuid        NOT NULL REFERENCES departments (id) ON DELETE CASCADE,
    -- One agent, or (NULL) everyone in the department.
    agent_id      uuid REFERENCES org_agents (id) ON DELETE CASCADE,
    name          text        NOT NULL,
    message       text        NOT NULL,
    every_minutes integer     NOT NULL CHECK (every_minutes >= 1),
    next_run_at   timestamptz NOT NULL,
    last_run_at   timestamptz,
    enabled       boolean     NOT NULL DEFAULT true,
    created_by    text        NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX org_schedules_due_idx ON org_schedules (next_run_at) WHERE enabled;
