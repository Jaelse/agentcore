-- Multi-agent organisations (departments, agents, messages) and the node
-- registry for running agentcore on several VMs. See docs/MULTI_AGENT.md.

-- Nodes: one row per agentcore process sharing this database.
CREATE TABLE nodes (
    name         text PRIMARY KEY,
    internal_url text        NOT NULL,
    capacity     integer     NOT NULL CHECK (capacity >= 0),
    version      text        NOT NULL,
    started_at   timestamptz NOT NULL DEFAULT now(),
    last_seen    timestamptz NOT NULL DEFAULT now()
);

-- The node that runs (or ran) a session; it owns the sandbox, audit log
-- and recording. NULL for sessions from before clustering.
ALTER TABLE sessions ADD COLUMN node text;
CREATE INDEX sessions_node_live_idx ON sessions (node)
    WHERE status NOT IN ('stopped', 'completed', 'failed');

-- Limits set by an admin. Exactly one row.
CREATE TABLE org_settings (
    id                        boolean PRIMARY KEY DEFAULT true CHECK (id),
    max_departments           integer     NOT NULL CHECK (max_departments >= 0),
    max_agents_per_department integer     NOT NULL CHECK (max_agents_per_department >= 0),
    updated_at                timestamptz NOT NULL DEFAULT now(),
    updated_by                text        NOT NULL DEFAULT 'agentcore'
);
INSERT INTO org_settings (max_departments, max_agents_per_department) VALUES (10, 10);

CREATE TABLE departments (
    id                 uuid PRIMARY KEY,
    name               text        NOT NULL,
    description        text        NOT NULL DEFAULT '',
    mission            text        NOT NULL DEFAULT '',
    policy             text        NOT NULL,
    tools              text[]      NOT NULL DEFAULT '{}',
    communicator_agent text        NOT NULL,
    state              text        NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'paused')),
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    updated_by         text        NOT NULL
);
CREATE UNIQUE INDEX departments_name_idx ON departments (lower(name));

CREATE TABLE org_agents (
    id            uuid PRIMARY KEY,
    department_id uuid        NOT NULL REFERENCES departments (id) ON DELETE CASCADE,
    name          text        NOT NULL,
    kind          text        NOT NULL CHECK (kind IN ('worker', 'communicator')),
    agent         text        NOT NULL,
    instructions  text        NOT NULL DEFAULT '',
    desired       text        NOT NULL DEFAULT 'stopped'
                              CHECK (desired IN ('stopped', 'running', 'paused')),
    node          text,
    session_id    uuid,
    status        text,
    note          text,
    -- Who last changed `desired` (a person's name, or `agentcore`).
    changed_by    text        NOT NULL DEFAULT 'agentcore',
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (department_id, name)
);
-- One communicator per department.
CREATE UNIQUE INDEX org_agents_one_communicator ON org_agents (department_id)
    WHERE kind = 'communicator';
CREATE INDEX org_agents_node_idx ON org_agents (node) WHERE desired <> 'stopped';
CREATE UNIQUE INDEX org_agents_session_idx ON org_agents (session_id) WHERE session_id IS NOT NULL;

CREATE TABLE org_messages (
    id              uuid PRIMARY KEY,
    created_at      timestamptz NOT NULL DEFAULT now(),
    scope           text        NOT NULL CHECK (scope IN ('internal', 'inter_department', 'human')),
    from_agent      uuid REFERENCES org_agents (id) ON DELETE SET NULL,
    from_department uuid REFERENCES departments (id) ON DELETE SET NULL,
    from_name       text        NOT NULL,
    to_kind         text        NOT NULL CHECK (to_kind IN ('agent', 'department', 'all_departments')),
    to_agent        uuid REFERENCES org_agents (id) ON DELETE SET NULL,
    to_department   uuid REFERENCES departments (id) ON DELETE SET NULL,
    to_name         text        NOT NULL,
    text            text        NOT NULL
);
CREATE INDEX org_messages_created_idx ON org_messages (created_at);
CREATE INDEX org_messages_from_dept_idx ON org_messages (from_department, created_at);
CREATE INDEX org_messages_to_dept_idx ON org_messages (to_department, created_at);

-- One row per recipient; delivered_at is set when the agent received it.
CREATE TABLE org_inbox (
    message_id   uuid NOT NULL REFERENCES org_messages (id) ON DELETE CASCADE,
    agent_id     uuid NOT NULL REFERENCES org_agents (id) ON DELETE CASCADE,
    department_id uuid NOT NULL,
    delivered_at timestamptz,
    PRIMARY KEY (message_id, agent_id)
);
CREATE INDEX org_inbox_pending_idx ON org_inbox (agent_id) WHERE delivered_at IS NULL;
CREATE INDEX org_inbox_dept_idx ON org_inbox (department_id);

-- Files shared by the workers of one department.
CREATE TABLE department_files (
    department_id uuid        NOT NULL REFERENCES departments (id) ON DELETE CASCADE,
    path          text        NOT NULL,
    content       bytea       NOT NULL,
    updated_at    timestamptz NOT NULL DEFAULT now(),
    updated_by    text        NOT NULL,
    PRIMARY KEY (department_id, path)
);
