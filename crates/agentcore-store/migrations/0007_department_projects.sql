-- A department can work on a project (a GitHub repository): its workers get
-- a checkout and GitHub tools for `role` (default: the project's role).
ALTER TABLE departments ADD COLUMN project_id uuid REFERENCES projects (id) ON DELETE SET NULL;
ALTER TABLE departments ADD COLUMN role text;
