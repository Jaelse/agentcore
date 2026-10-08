-- Business data, operating metrics and improvement proposals.

-- Read-only business data that departments may query: a PostgreSQL
-- database, an HTTP API, or an uploaded table (CSV). Secrets (connection
-- string, API key) are encrypted like provider keys, bound to the source id.
CREATE TABLE data_sources (
    id                uuid PRIMARY KEY,
    name              text        NOT NULL UNIQUE CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,62}$'),
    kind              text        NOT NULL CHECK (kind IN ('postgres', 'http', 'table')),
    description       text        NOT NULL DEFAULT '',
    config            jsonb       NOT NULL DEFAULT '{}',
    secret_ciphertext bytea,
    secret_nonce      bytea,
    secret_hint       text,
    -- CSV of a `table` source.
    content           text,
    departments       uuid[]      NOT NULL DEFAULT '{}',
    enabled           boolean     NOT NULL DEFAULT true,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    updated_by        text        NOT NULL
);

-- What department agents did that the session index does not show:
-- denied actions, approvals and how long they waited, why sessions ended,
-- goal progress, data queries. Feeds the organisation's metrics.
CREATE TABLE org_activity (
    id            bigserial PRIMARY KEY,
    at            timestamptz NOT NULL DEFAULT now(),
    department_id uuid,
    agent_id      uuid,
    session_id    uuid,
    kind          text        NOT NULL,
    value         bigint,
    detail        text
);
CREATE INDEX org_activity_at_idx ON org_activity (at);
CREATE INDEX org_activity_dept_idx ON org_activity (department_id, at);

-- Improvements proposed by agents (the retrospective) or people: what is
-- inefficient, the evidence, the solution and the concrete changes that
-- apply it. Nothing changes until a person applies it (or allowed the
-- kinds of change to be applied automatically).
CREATE TABLE org_proposals (
    id             uuid PRIMARY KEY,
    title          text        NOT NULL,
    problem        text        NOT NULL,
    evidence       text        NOT NULL DEFAULT '',
    solution       text        NOT NULL,
    actions        jsonb       NOT NULL DEFAULT '[]',
    status         text        NOT NULL DEFAULT 'open'
                               CHECK (status IN ('open', 'changes_requested', 'applied', 'rejected', 'failed')),
    revision       integer     NOT NULL DEFAULT 1,
    -- Earlier revisions and the feedback that led to each new one.
    history        jsonb       NOT NULL DEFAULT '[]',
    feedback       text,
    proposed_by    text        NOT NULL,
    proposer_agent uuid REFERENCES org_agents (id) ON DELETE SET NULL,
    decided_by     text,
    decided_at     timestamptz,
    result         jsonb,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX org_proposals_status_idx ON org_proposals (status, created_at DESC);

-- Kinds of change people allow to be applied without asking.
ALTER TABLE org_settings ADD COLUMN auto_apply text[] NOT NULL DEFAULT '{}';
ALTER TABLE org_settings ADD COLUMN auto_apply_by text;
