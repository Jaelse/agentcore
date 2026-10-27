-- Budgets in tokens as well as money, and working hours per department.

-- A budget counts money (limit_micros in millionths of the currency) or
-- tokens (limit_micros is then a token count). A scope can have one budget
-- per period and unit.
ALTER TABLE budgets ADD COLUMN unit text NOT NULL DEFAULT 'money'
    CHECK (unit IN ('money', 'tokens'));
DROP INDEX budgets_scope_period_idx;
CREATE UNIQUE INDEX budgets_scope_period_idx
    ON budgets (COALESCE(department_id, '00000000-0000-0000-0000-000000000000'::uuid), period, unit);

-- When a department works. Outside the windows it is paused (by `hours`)
-- and its model calls are refused; no row, or `enabled = false`, means any
-- time. `windows` is a list of {days: [1..7], start: "HH:MM", end: "HH:MM"}.
CREATE TABLE department_hours (
    department_id uuid PRIMARY KEY REFERENCES departments (id) ON DELETE CASCADE,
    enabled       boolean     NOT NULL DEFAULT true,
    timezone      text        NOT NULL DEFAULT 'UTC',
    windows       jsonb       NOT NULL DEFAULT '[]',
    updated_at    timestamptz NOT NULL DEFAULT now(),
    updated_by    text        NOT NULL
);
