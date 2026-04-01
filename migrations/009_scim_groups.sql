-- SCIM Groups (separate from CCAG teams — role mapping only)
CREATE TABLE IF NOT EXISTS scim_groups (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    external_id TEXT,
    display_name TEXT NOT NULL,
    idp_id UUID NOT NULL REFERENCES identity_providers(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_scim_groups_external_id
    ON scim_groups(idp_id, external_id) WHERE external_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS scim_group_members (
    group_id UUID NOT NULL REFERENCES scim_groups(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (group_id, user_id)
);

-- Admin group mapping config: JSON array of group displayNames that map to admin role
ALTER TABLE identity_providers
    ADD COLUMN IF NOT EXISTS scim_admin_groups JSONB NOT NULL DEFAULT '[]'::jsonb;
