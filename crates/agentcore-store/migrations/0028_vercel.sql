-- A Vercel connection (read-only token), so agentcore can read the build log
-- of a failed preview deployment and hand it to the agent that made the
-- change. Encrypted like the GitHub token (AAD = "integration:vercel").
ALTER TABLE integrations DROP CONSTRAINT integrations_kind_check;
ALTER TABLE integrations ADD CONSTRAINT integrations_kind_check
    CHECK (kind IN ('github', 'vercel'));
