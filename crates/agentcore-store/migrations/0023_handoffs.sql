-- Handoffs: work one department hands to another (a specification), which
-- waits for the receiving department until one of its workers takes it. The
-- router carries a notice of it like any message between departments.
CREATE TABLE handoffs (
    id              uuid PRIMARY KEY,
    from_department uuid        NOT NULL REFERENCES departments (id) ON DELETE CASCADE,
    -- NULL when a person handed it off, or its agent was removed.
    from_agent      uuid REFERENCES org_agents (id) ON DELETE SET NULL,
    from_name       text        NOT NULL,
    to_department   uuid        NOT NULL REFERENCES departments (id) ON DELETE CASCADE,
    title           text        NOT NULL,
    spec            text        NOT NULL,
    status          text        NOT NULL DEFAULT 'waiting'
                    CHECK (status IN ('held', 'waiting', 'taken', 'done', 'declined', 'cancelled')),
    -- The notice the router carried.
    message_id      uuid REFERENCES org_messages (id) ON DELETE SET NULL,
    taken_by        uuid REFERENCES org_agents (id) ON DELETE SET NULL,
    taken_by_name   text,
    taken_at        timestamptz,
    result          text        NOT NULL DEFAULT '',
    closed_at       timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX handoffs_to_idx ON handoffs (to_department, status);
CREATE INDEX handoffs_from_idx ON handoffs (from_department, status);
CREATE UNIQUE INDEX handoffs_message_idx ON handoffs (message_id) WHERE message_id IS NOT NULL;
