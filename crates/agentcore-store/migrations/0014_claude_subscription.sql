-- Allow Claude subscription providers (OAuth tokens from `claude setup-token`).
ALTER TABLE model_providers DROP CONSTRAINT model_providers_kind_check;
ALTER TABLE model_providers ADD CONSTRAINT model_providers_kind_check
    CHECK (kind IN ('anthropic', 'openai', 'opencode_zen', 'claude_subscription'));
