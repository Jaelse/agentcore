-- A pool of Claude subscription tokens (from `claude setup-token`) behind one
-- `claude_subscription` provider. Tokens are encrypted like provider keys
-- (AES-256-GCM, token id as associated data). A session leases one token for
-- as long as it lives; a token whose session has ended is free again.
CREATE TABLE claude_tokens (
    id         uuid PRIMARY KEY,
    provider   text        NOT NULL REFERENCES model_providers (name) ON DELETE CASCADE,
    label      text        NOT NULL,
    ciphertext bytea       NOT NULL,
    nonce      bytea       NOT NULL,
    hint       text        NOT NULL,
    enabled    boolean     NOT NULL DEFAULT true,
    session_id uuid        REFERENCES sessions (id) ON DELETE SET NULL,
    leased_at  timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    created_by text        NOT NULL
);
CREATE INDEX claude_tokens_provider_idx ON claude_tokens (provider);
