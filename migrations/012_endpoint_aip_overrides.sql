-- Per-endpoint AIP override map.
-- Each row says "on this endpoint, when a user requests this model, invoke it
-- via this Application Inference Profile ARN."  Models without a row fall
-- through to the existing CRI path.

CREATE TABLE IF NOT EXISTS endpoint_aip_overrides (
    endpoint_id UUID        NOT NULL REFERENCES endpoints(id) ON DELETE CASCADE,
    model_id    TEXT        NOT NULL,
    aip_arn     TEXT        NOT NULL,
    set_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    set_by      TEXT        NOT NULL,
    reason      TEXT,
    PRIMARY KEY (endpoint_id, model_id)
);
