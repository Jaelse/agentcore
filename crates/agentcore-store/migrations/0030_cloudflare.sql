-- A Cloudflare connection (API token + account), so agents in the
-- `cloudflare-deployer` role can provision Workers, Pages, D1, KV, R2,
-- Queues, Hyperdrive, DNS and secrets, and wire the repository's CI to
-- deploy, without ever seeing the token. The config holds the account id and
-- name; the token is encrypted like the others (AAD = "integration:cloudflare").
ALTER TABLE integrations DROP CONSTRAINT integrations_kind_check;
ALTER TABLE integrations ADD CONSTRAINT integrations_kind_check
    CHECK (kind IN ('github', 'vercel', 'cloudflare'));
