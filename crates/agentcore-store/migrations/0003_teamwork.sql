-- GitHub connection: one per deployment. The token is encrypted like
-- provider API keys (AAD = "integration:github").
CREATE TABLE integrations (
    kind              text PRIMARY KEY CHECK (kind IN ('github')),
    config            jsonb       NOT NULL DEFAULT '{}',
    secret_ciphertext bytea       NOT NULL,
    secret_nonce      bytea       NOT NULL,
    secret_hint       text        NOT NULL,
    updated_at        timestamptz NOT NULL DEFAULT now(),
    updated_by        text        NOT NULL
);

-- A project = a GitHub repository (and optionally a GitHub Projects board)
-- that agents work on, with the role they work in by default.
CREATE TABLE projects (
    id             uuid PRIMARY KEY,
    name           text        NOT NULL UNIQUE,
    repo_owner     text        NOT NULL,
    repo_name      text        NOT NULL,
    default_branch text        NOT NULL DEFAULT 'main',
    agent          text        NOT NULL,
    role           text        NOT NULL,
    board          jsonb,
    notes          text        NOT NULL DEFAULT '',
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    updated_by     text        NOT NULL
);

ALTER TABLE sessions ADD COLUMN context jsonb NOT NULL DEFAULT '{}';

-- Latest snapshot of what an agent changed in its repository workspace.
CREATE TABLE session_changes (
    session_id uuid PRIMARY KEY REFERENCES sessions (id),
    changes    jsonb       NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
