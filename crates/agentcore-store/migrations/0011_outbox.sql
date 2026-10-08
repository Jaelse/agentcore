-- Outward channels (email, Slack, webhooks) and the outbox: what agents
-- want to say to the outside world, approved by a person before it is sent.

CREATE TABLE org_channels (
    id                uuid PRIMARY KEY,
    name              text        NOT NULL UNIQUE CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,62}$'),
    kind              text        NOT NULL CHECK (kind IN ('email', 'slack', 'webhook')),
    description       text        NOT NULL DEFAULT '',
    -- email: host, port, tls, username, from, allowed_domains, max_recipients;
    -- webhook: url, header. Slack's webhook URL is the secret.
    config            jsonb       NOT NULL DEFAULT '{}',
    secret_ciphertext bytea,
    secret_nonce      bytea,
    secret_hint       text,
    departments       uuid[]      NOT NULL DEFAULT '{}',
    requires_approval boolean     NOT NULL DEFAULT true,
    max_per_day       integer     NOT NULL DEFAULT 50 CHECK (max_per_day >= 0),
    -- Appended to every message (AI transparency).
    disclosure        text        NOT NULL DEFAULT '',
    enabled           boolean     NOT NULL DEFAULT true,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    updated_by        text        NOT NULL
);

CREATE TABLE org_outbox (
    id            uuid PRIMARY KEY,
    channel_id    uuid        NOT NULL REFERENCES org_channels (id) ON DELETE CASCADE,
    department_id uuid REFERENCES departments (id) ON DELETE SET NULL,
    agent_id      uuid REFERENCES org_agents (id) ON DELETE SET NULL,
    drafted_by    text        NOT NULL,
    recipients    text[]      NOT NULL DEFAULT '{}',
    subject       text        NOT NULL DEFAULT '',
    body          text        NOT NULL,
    status        text        NOT NULL DEFAULT 'pending'
                              CHECK (status IN ('pending', 'changes_requested', 'sending', 'sent', 'rejected', 'failed')),
    revision      integer     NOT NULL DEFAULT 1,
    history       jsonb       NOT NULL DEFAULT '[]',
    feedback      text,
    decided_by    text,
    decided_at    timestamptz,
    sent_at       timestamptz,
    error         text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX org_outbox_status_idx ON org_outbox (status, created_at DESC);
CREATE INDEX org_outbox_sent_idx ON org_outbox (channel_id, sent_at) WHERE status = 'sent';
