-- Work items: the work a department does on its repository. The department
-- owns the work (its branch, last checkpoint and pull request); sessions only
-- borrow it. One session at a time holds an item's lease; when the session
-- ends, its node restarts or its node is lost, the lease is released and the
-- item can be resumed from its last checkpoint, by any worker on any node.
CREATE TABLE work_items (
    id               uuid PRIMARY KEY,
    department_id    uuid        NOT NULL REFERENCES departments (id) ON DELETE CASCADE,
    project_id       uuid        NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    title            text        NOT NULL,
    -- The branch on GitHub that holds the work between sessions.
    branch           text        NOT NULL,
    base_branch      text        NOT NULL,
    -- Last checkpoint pushed to `branch` (NULL: nothing pushed yet).
    head_commit      text,
    checkpoint_at    timestamptz,
    state            text        NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'closed')),
    -- The worker that carries the item on; NULL when its worker was removed.
    assignee         uuid REFERENCES org_agents (id) ON DELETE SET NULL,
    -- The lease: set while a session works on the item. `lease_generation`
    -- grows with every acquisition; checkpoints carry it, so a session that
    -- lost its lease cannot write any more.
    lease_id         uuid,
    lease_session    uuid,
    lease_node       text,
    lease_generation bigint      NOT NULL DEFAULT 0,
    leased_at        timestamptz,
    pull_request_number bigint,
    pull_request_url text,
    -- What last happened to it (e.g. "lost with node n2"), for people and agents.
    note             text,
    created_by       text        NOT NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    closed_at        timestamptz,
    closed_by        text,
    UNIQUE (project_id, branch),
    CHECK ((lease_id IS NULL) = (lease_node IS NULL))
);
CREATE INDEX work_items_department_idx ON work_items (department_id, state);
CREATE INDEX work_items_assignee_idx ON work_items (assignee) WHERE state = 'open';
CREATE INDEX work_items_lease_node_idx ON work_items (lease_node) WHERE lease_id IS NOT NULL;
CREATE UNIQUE INDEX work_items_lease_session_idx ON work_items (lease_session)
    WHERE lease_session IS NOT NULL;
