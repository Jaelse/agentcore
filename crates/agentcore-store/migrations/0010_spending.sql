-- What model calls cost, and budgets that warn or pause before money runs out.

-- Prices per million tokens, per provider and model. `model` is an exact
-- model name, a prefix ending in `*` (`claude-sonnet-*`), or `*` for every
-- model of the provider; the most specific match wins.
CREATE TABLE model_prices (
    provider        text          NOT NULL REFERENCES model_providers (name) ON DELETE CASCADE,
    model           text          NOT NULL CHECK (model <> ''),
    input_per_mtok  numeric(14,6) NOT NULL CHECK (input_per_mtok >= 0),
    output_per_mtok numeric(14,6) NOT NULL CHECK (output_per_mtok >= 0),
    updated_at      timestamptz   NOT NULL DEFAULT now(),
    updated_by      text          NOT NULL,
    PRIMARY KEY (provider, model)
);

-- Cost of a call in millionths of the currency, priced when it is
-- recorded (NULL: no price known then).
ALTER TABLE model_calls ADD COLUMN cost_micros bigint;
CREATE INDEX model_calls_started_idx ON model_calls (started_at);

ALTER TABLE org_settings ADD COLUMN currency text NOT NULL DEFAULT 'USD';

-- A spending limit for the whole organisation (department_id NULL) or one
-- department, per calendar day, week or month (UTC).
CREATE TABLE budgets (
    id                 uuid PRIMARY KEY,
    department_id      uuid REFERENCES departments (id) ON DELETE CASCADE,
    period             text        NOT NULL CHECK (period IN ('day', 'week', 'month')),
    limit_micros       bigint      NOT NULL CHECK (limit_micros > 0),
    -- warn: tell people; pause: also pause the work and block model calls.
    action             text        NOT NULL CHECK (action IN ('warn', 'pause')),
    warn_percent       integer     NOT NULL DEFAULT 80 CHECK (warn_percent BETWEEN 1 AND 100),
    -- Start of the period in which people were warned / the budget ran out.
    warned_period      timestamptz,
    exhausted_period   timestamptz,
    -- Departments this budget paused (resumed when the next period starts).
    paused_departments uuid[]      NOT NULL DEFAULT '{}',
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    updated_by         text        NOT NULL
);
CREATE UNIQUE INDEX budgets_scope_period_idx
    ON budgets (COALESCE(department_id, '00000000-0000-0000-0000-000000000000'::uuid), period);
