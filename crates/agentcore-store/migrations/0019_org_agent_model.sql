-- A department agent can run its agent definition on another model provider
-- and model than the definition names. NULL keeps the definition's own.
ALTER TABLE org_agents ADD COLUMN provider text;
ALTER TABLE org_agents ADD COLUMN model text;
