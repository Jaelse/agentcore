-- Finished sessions can be archived: hidden from the default list, nothing deleted.
ALTER TABLE sessions ADD COLUMN archived_at timestamptz;
CREATE INDEX sessions_archived_idx ON sessions (archived_at) WHERE archived_at IS NOT NULL;
