-- Announcements: what the people who run the company tell everyone (or one
-- department). They are delivered once, stay on the notice board and in the
-- agents' instructions until they expire or are withdrawn, and can ask every
-- agent they reach to acknowledge them.
CREATE TABLE org_announcements (
    id            uuid PRIMARY KEY,
    title         text        NOT NULL,
    body          text        NOT NULL DEFAULT '',
    -- NULL: everyone in the organisation.
    department_id uuid REFERENCES departments (id) ON DELETE CASCADE,
    ask_ack       boolean     NOT NULL DEFAULT false,
    valid_until   timestamptz,
    withdrawn_at  timestamptz,
    withdrawn_by  text,
    created_by    text        NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE org_announcement_acks (
    announcement_id uuid        NOT NULL REFERENCES org_announcements (id) ON DELETE CASCADE,
    agent_id        uuid        NOT NULL REFERENCES org_agents (id) ON DELETE CASCADE,
    note            text        NOT NULL DEFAULT '',
    acked_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (announcement_id, agent_id)
);
