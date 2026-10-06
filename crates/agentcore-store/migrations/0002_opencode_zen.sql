-- Allow OpenCode Zen providers.
ALTER TABLE model_providers DROP CONSTRAINT model_providers_kind_check;
ALTER TABLE model_providers ADD CONSTRAINT model_providers_kind_check
    CHECK (kind IN ('anthropic', 'openai', 'opencode_zen'));
