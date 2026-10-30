-- The communicator agents are replaced by a message router: a deterministic
-- policy per department decides what crosses between departments.

-- What the router lets through to and from the department (see
-- agentcore_core::RoutingPolicy). `{}` = everything.
ALTER TABLE departments ADD COLUMN routing_policy jsonb NOT NULL DEFAULT '{}';
ALTER TABLE departments DROP COLUMN communicator_agent;

-- Communicators are gone. Their messages stay (the sender becomes unknown but
-- `from_name` keeps reading `communicator (Dept)`); their inbox entries go.
DELETE FROM org_agents WHERE kind = 'communicator';
DROP INDEX org_agents_one_communicator;
ALTER TABLE org_agents DROP CONSTRAINT org_agents_kind_check;
ALTER TABLE org_agents ADD CONSTRAINT org_agents_kind_check CHECK (kind = 'worker');

-- What became of a message at the router: delivered, held for a person to
-- approve, or rejected (by a policy or a person), with the reason.
ALTER TABLE org_messages
    ADD COLUMN status   text NOT NULL DEFAULT 'delivered'
                        CHECK (status IN ('delivered', 'held', 'rejected')),
    ADD COLUMN decision text NOT NULL DEFAULT '';
CREATE INDEX org_messages_held_idx ON org_messages (created_at) WHERE status = 'held';
