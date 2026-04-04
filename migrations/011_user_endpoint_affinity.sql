-- User endpoint affinity for distributed sticky routing.
-- Persists sticky_user routing decisions across gateway instances.
CREATE TABLE IF NOT EXISTS user_endpoint_affinity (
    user_identity TEXT PRIMARY KEY,
    endpoint_id UUID NOT NULL,
    last_used_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_affinity_last_used ON user_endpoint_affinity(last_used_at);
