-- SCIM 2.0 foundation: scim_tokens table + schema extensions for users, teams, identity_providers.

-- SCIM bearer tokens (per-IDP)
CREATE TABLE IF NOT EXISTS scim_tokens (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    idp_id UUID NOT NULL REFERENCES identity_providers(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    token_prefix TEXT NOT NULL,
    name TEXT,
    created_by TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT true,
    last_used_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_scim_tokens_hash ON scim_tokens(token_hash);

-- Extend users for SCIM
ALTER TABLE users
    ADD COLUMN IF NOT EXISTS active BOOLEAN NOT NULL DEFAULT true,
    ADD COLUMN IF NOT EXISTS external_id TEXT,
    ADD COLUMN IF NOT EXISTS display_name TEXT,
    ADD COLUMN IF NOT EXISTS given_name TEXT,
    ADD COLUMN IF NOT EXISTS family_name TEXT,
    ADD COLUMN IF NOT EXISTS scim_managed BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS idp_id UUID REFERENCES identity_providers(id) ON DELETE SET NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_external_id
    ON users(external_id) WHERE external_id IS NOT NULL;

-- Extend identity_providers for SCIM toggle
ALTER TABLE identity_providers
    ADD COLUMN IF NOT EXISTS scim_enabled BOOLEAN NOT NULL DEFAULT false;

-- Extend teams for SCIM Group mapping
ALTER TABLE teams
    ADD COLUMN IF NOT EXISTS external_id TEXT,
    ADD COLUMN IF NOT EXISTS display_name TEXT,
    ADD COLUMN IF NOT EXISTS scim_managed BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS idp_id UUID REFERENCES identity_providers(id) ON DELETE SET NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_teams_external_id
    ON teams(external_id) WHERE external_id IS NOT NULL;
