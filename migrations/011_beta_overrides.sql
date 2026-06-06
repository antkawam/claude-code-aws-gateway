-- Admin-managed capability overrides: operator-set (profile, beta) supported flags.
-- These override the learned (seed-probe, request-path) cache entries and ignore TTL.

CREATE TABLE beta_overrides (
    endpoint_id UUID NOT NULL REFERENCES endpoints(id) ON DELETE CASCADE,
    profile_id  TEXT NOT NULL,
    beta_name   TEXT NOT NULL,
    supported   BOOLEAN NOT NULL,
    set_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    set_by      TEXT NOT NULL,
    reason      TEXT,
    PRIMARY KEY (endpoint_id, profile_id, beta_name)
);
