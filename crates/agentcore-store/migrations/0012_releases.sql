-- Releases and distribution: products the organisation ships, releases
-- agents propose and people approve, staged rollouts with health gates,
-- what update clients report, and the keys that sign what is served.

CREATE TABLE release_products (
    id                        uuid PRIMARY KEY,
    name                      text        NOT NULL UNIQUE CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,62}$'),
    description               text        NOT NULL DEFAULT '',
    -- Departments whose agents may propose releases.
    departments               uuid[]      NOT NULL DEFAULT '{}',
    repository                text,
    plan                      jsonb       NOT NULL,
    gates                     jsonb       NOT NULL DEFAULT '[]',
    approvals_required        integer     NOT NULL DEFAULT 1 CHECK (approvals_required BETWEEN 1 AND 5),
    artifact_hosts            text[]      NOT NULL DEFAULT '{}',
    webhook_url               text,
    webhook_secret_ciphertext bytea,
    webhook_secret_nonce      bytea,
    webhook_secret_hint       text,
    -- Update clients authenticate with a token; only its hash is kept.
    client_token_sha256       text,
    client_token_hint         text,
    frozen                    text,
    frozen_by                 text,
    enabled                   boolean     NOT NULL DEFAULT true,
    created_at                timestamptz NOT NULL DEFAULT now(),
    updated_at                timestamptz NOT NULL DEFAULT now(),
    updated_by                text        NOT NULL
);

CREATE TABLE releases (
    id                 uuid PRIMARY KEY,
    product_id         uuid        NOT NULL REFERENCES release_products (id) ON DELETE CASCADE,
    version            text        NOT NULL,
    commit_sha         text,
    notes              text        NOT NULL,
    artifacts          jsonb       NOT NULL,
    sbom               jsonb,
    checks             jsonb       NOT NULL DEFAULT '[]',
    changes            jsonb       NOT NULL DEFAULT '{}',
    risk               jsonb       NOT NULL,
    plan               jsonb       NOT NULL,
    approvals_required integer     NOT NULL CHECK (approvals_required >= 1),
    approvals          jsonb       NOT NULL DEFAULT '[]',
    status             text        NOT NULL DEFAULT 'proposed'
                                   CHECK (status IN ('proposed', 'changes_requested', 'rolling_out',
                                                     'paused', 'released', 'rolled_back', 'rejected')),
    stage              integer     NOT NULL DEFAULT 0,
    stage_started_at   timestamptz,
    gates              jsonb       NOT NULL DEFAULT '[]',
    gates_checked_at   timestamptz,
    department_id      uuid REFERENCES departments (id) ON DELETE SET NULL,
    agent_id           uuid REFERENCES org_agents (id) ON DELETE SET NULL,
    proposed_by        text        NOT NULL,
    revision           integer     NOT NULL DEFAULT 1,
    history            jsonb       NOT NULL DEFAULT '[]',
    feedback           text,
    reason             text,
    decided_by         text,
    started_at         timestamptz,
    released_at        timestamptz,
    attestation        jsonb,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now()
);
-- A version is used once; only a rejected release (never shipped) frees it.
CREATE UNIQUE INDEX releases_version_idx ON releases (product_id, version)
    WHERE status <> 'rejected';
CREATE INDEX releases_product_idx ON releases (product_id, created_at DESC);
CREATE INDEX releases_status_idx ON releases (status, created_at DESC);
-- One rollout at a time per product.
CREATE UNIQUE INDEX releases_one_active_idx ON releases (product_id)
    WHERE status IN ('rolling_out', 'paused');

-- What update clients report, one row per release and unit. Units are
-- stored as a hash: the raw installation id never reaches the database.
CREATE TABLE release_reports (
    release_id    uuid        NOT NULL REFERENCES releases (id) ON DELETE CASCADE,
    unit_hash     text        NOT NULL,
    installed     boolean     NOT NULL DEFAULT false,
    install_failed boolean    NOT NULL DEFAULT false,
    crashed       boolean     NOT NULL DEFAULT false,
    crashes       integer     NOT NULL DEFAULT 0,
    first_seen    timestamptz NOT NULL DEFAULT now(),
    last_seen     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (release_id, unit_hash)
);

-- Ed25519 keys that sign update manifests and attestations. The private
-- key (PKCS#8) is encrypted with the master key; retired keys stay
-- published so earlier signatures still verify.
CREATE TABLE release_keys (
    id                uuid PRIMARY KEY,
    key_id            text        NOT NULL UNIQUE,
    public_key        bytea       NOT NULL,
    pkcs8_ciphertext  bytea       NOT NULL,
    pkcs8_nonce       bytea       NOT NULL,
    created_at        timestamptz NOT NULL DEFAULT now(),
    created_by        text        NOT NULL,
    retired_at        timestamptz
);
CREATE UNIQUE INDEX release_keys_one_active_idx ON release_keys ((true)) WHERE retired_at IS NULL;
