-- Routing rules: per-team endpoint assignment strategy + designated default endpoint

-- One endpoint can be designated as the default: receives traffic from teams/users
-- with no specific endpoint assignment.
ALTER TABLE endpoints
    ADD COLUMN IF NOT EXISTS is_default BOOLEAN NOT NULL DEFAULT false;

-- Enforce at most one default endpoint via partial unique index.
CREATE UNIQUE INDEX IF NOT EXISTS endpoints_one_default
    ON endpoints (is_default)
    WHERE is_default = true;

-- Per-team routing strategy: how requests are distributed across assigned endpoints.
-- Values: primary_fallback | sticky_user | round_robin | least_latency
ALTER TABLE teams
    ADD COLUMN IF NOT EXISTS routing_strategy TEXT NOT NULL DEFAULT 'primary_fallback';
