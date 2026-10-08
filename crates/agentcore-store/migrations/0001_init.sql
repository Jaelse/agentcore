-- Index of all sessions. The authoritative record of what happened in a
-- session is its hash-chained audit file; this table makes sessions
-- listable and survives restarts.
CREATE TABLE sessions (
    id             uuid PRIMARY KEY,
    agent          text        NOT NULL,
    task           text        NOT NULL,
    policy         text        NOT NULL,
    policy_digest  text        NOT NULL,
    status         text        NOT NULL,
    created_by     jsonb       NOT NULL,
    created_at     timestamptz NOT NULL,
    ended_at       timestamptz,
    actions        bigint      NOT NULL DEFAULT 0,
    model_calls    bigint      NOT NULL DEFAULT 0,
    audit_path     text        NOT NULL
);
CREATE INDEX sessions_created_at_idx ON sessions (created_at DESC);

-- Upstream LLM providers. API keys are encrypted with AES-256-GCM using the
-- agentcore master key; the provider name is bound as associated data so a
-- ciphertext cannot be moved to another provider.
CREATE TABLE model_providers (
    name               text PRIMARY KEY CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,62}$'),
    kind               text        NOT NULL CHECK (kind IN ('anthropic', 'openai')),
    base_url           text        NOT NULL,
    api_key_ciphertext bytea       NOT NULL,
    api_key_nonce      bytea       NOT NULL,
    api_key_hint       text        NOT NULL,
    allowed_models     text[]      NOT NULL DEFAULT '{}',
    enabled            boolean     NOT NULL DEFAULT true,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    updated_by         text        NOT NULL
);

-- Every LLM call made through the model gateway (EU AI Act Art. 12).
CREATE TABLE model_calls (
    id               uuid PRIMARY KEY,
    session_id       uuid        NOT NULL REFERENCES sessions (id),
    provider         text        NOT NULL,
    model            text,
    method           text        NOT NULL,
    path             text        NOT NULL,
    http_status      integer,
    outcome          text        NOT NULL,
    detail           text,
    input_tokens     bigint,
    output_tokens    bigint,
    started_at       timestamptz NOT NULL,
    duration_ms      bigint      NOT NULL,
    request_body     text,
    response_body    text,
    bodies_truncated boolean     NOT NULL DEFAULT false,
    request_sha256   text        NOT NULL,
    response_sha256  text
);
CREATE INDEX model_calls_session_idx ON model_calls (session_id, started_at);

-- Who changed configuration, and when.
CREATE TABLE admin_events (
    id       bigserial PRIMARY KEY,
    at       timestamptz NOT NULL DEFAULT now(),
    actor    text        NOT NULL,
    action   text        NOT NULL,
    target   text        NOT NULL,
    details  jsonb       NOT NULL DEFAULT '{}'
);
