-- A department whose work is on a repository (developers, QA, ...) cannot work
-- without one: its agents are held until a project is linked.
ALTER TABLE departments ADD COLUMN requires_repository boolean NOT NULL DEFAULT false;
