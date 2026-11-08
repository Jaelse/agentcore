-- Warnings and errors from every node's operational log. Nodes are ephemeral
-- and keep their full log in memory only; these rows let the log export show
-- what a node reported before it went away. Pruned after a retention period.
CREATE TABLE node_logs (
    id      uuid PRIMARY KEY,
    at      timestamptz NOT NULL,
    node    text        NOT NULL,
    level   text        NOT NULL,
    target  text        NOT NULL,
    message text        NOT NULL,
    fields  jsonb       NOT NULL DEFAULT '{}'
);
CREATE INDEX node_logs_at_idx ON node_logs (at);
