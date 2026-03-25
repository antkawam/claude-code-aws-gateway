-- Add scopes column to identity_providers.
-- Controls which OIDC scopes are requested during the login redirect.
-- NULL or empty = 'openid' (safe default, works with all IDPs).
-- Example: 'openid email profile' for Entra/Okta when email claim is needed.
ALTER TABLE identity_providers ADD COLUMN IF NOT EXISTS scopes TEXT;
