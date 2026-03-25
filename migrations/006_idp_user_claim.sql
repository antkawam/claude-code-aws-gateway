-- Add user_claim column to identity_providers.
-- Controls which JWT claim is used as the user identifier.
-- NULL or 'auto' = fallback chain: email > preferred_username > upn > name > sub
-- Other values: 'email', 'preferred_username', 'upn', 'oid', 'name', 'sub'
ALTER TABLE identity_providers ADD COLUMN IF NOT EXISTS user_claim TEXT;
