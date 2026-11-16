-- Reviewing a pull request with the agent on its preview: how agentcore's
-- browser gets into a project's previews, and the screenshots it shows.

-- Per project: a Vercel "Protection Bypass for Automation" secret and a
-- reviewer login, encrypted together (the agent never sees them), plus
-- other sites the preview may send the browser to (e.g. a sign-in page).
CREATE TABLE preview_access (
    project_id        uuid PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    username          text        NOT NULL DEFAULT '',
    extra_hosts       text[]      NOT NULL DEFAULT '{}',
    secret_ciphertext bytea       NOT NULL,
    secret_nonce      bytea       NOT NULL,
    bypass_hint       text        NOT NULL DEFAULT '',
    has_password      boolean     NOT NULL DEFAULT false,
    updated_at        timestamptz NOT NULL DEFAULT now(),
    updated_by        text        NOT NULL
);

CREATE TABLE screenshots (
    id         uuid PRIMARY KEY,
    session_id uuid        NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    mime       text        NOT NULL,
    image      bytea       NOT NULL
);
CREATE INDEX screenshots_session ON screenshots (session_id);
