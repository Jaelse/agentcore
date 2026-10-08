-- Agents added from the agent catalogue in the web UI (in addition to the
-- `[[agents]]` of the configuration file). `spec` is the AgentSpec.
CREATE TABLE installed_agents (
    name       text PRIMARY KEY CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,62}$'),
    catalog    text        NOT NULL,
    spec       jsonb       NOT NULL,
    enabled    boolean     NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    updated_by text        NOT NULL
);
