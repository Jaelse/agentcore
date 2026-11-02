-- Nodes are ephemeral. A node that stops responding (or shuts down) is
-- forgotten, and the sessions it ran are carried on by another node from the
-- data volume, which every node mounts. `nodes` only lists the nodes that are
-- running now.
--
-- How to build a session again on another node: the request a person started
-- it with (department agents are built from their department instead).
ALTER TABLE sessions ADD COLUMN resume_spec jsonb;

-- Live sessions by node, to find the ones a node has to carry on.
DROP INDEX IF EXISTS sessions_node_live_idx;
CREATE INDEX sessions_node_live_idx ON sessions (node)
    WHERE status NOT IN ('stopped', 'completed', 'failed');
