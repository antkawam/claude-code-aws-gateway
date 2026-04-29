-- Model pricing table and updated estimate_cost_usd function.
-- Rates are in USD per 1M tokens (Standard, Global CRIS, us-east-1, as of 2026-04-16).

CREATE TABLE IF NOT EXISTS model_pricing (
    model_prefix TEXT PRIMARY KEY,
    input_rate DOUBLE PRECISION NOT NULL,
    output_rate DOUBLE PRECISION NOT NULL,
    cache_read_rate DOUBLE PRECISION NOT NULL,
    cache_write_rate DOUBLE PRECISION NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('seed','price_list_api','admin_manual')),
    aws_sku TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO model_pricing (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source) VALUES
    ('claude-opus-4-7',   5.00, 25.00, 0.50, 6.25, 'seed'),
    ('claude-opus-4-6',   5.00, 25.00, 0.50, 6.25, 'seed'),
    ('claude-opus-4-5',   5.00, 25.00, 0.50, 6.25, 'seed'),
    ('claude-sonnet-4-6', 3.00, 15.00, 0.30, 3.75, 'seed'),
    ('claude-sonnet-4-5', 3.00, 15.00, 0.30, 3.75, 'seed'),
    ('claude-haiku-4-5',  1.00,  5.00, 0.10, 1.25, 'seed'),
    -- Shorter prefix catches legacy date-versioned model IDs like claude-sonnet-4-20250514
    -- that do not include a generation patch digit.
    ('claude-sonnet-4-',  3.00, 15.00, 0.30, 3.75, 'seed')
ON CONFLICT (model_prefix) DO NOTHING;

-- Rewrite estimate_cost_usd to look up rates from the model_pricing table.
-- Uses longest-prefix match so date-suffixed model IDs (e.g. claude-opus-4-7-20260401)
-- resolve correctly. Returns NULL for unknown models instead of silently falling back
-- to an arbitrary default rate.
-- Must be STABLE (not IMMUTABLE) because it reads from a table.
CREATE OR REPLACE FUNCTION estimate_cost_usd(
    p_model TEXT,
    p_input_tokens INTEGER,
    p_output_tokens INTEGER,
    p_cache_read_tokens INTEGER DEFAULT 0,
    p_cache_write_tokens INTEGER DEFAULT 0
) RETURNS DOUBLE PRECISION AS $$
DECLARE r RECORD;
BEGIN
    SELECT input_rate, output_rate, cache_read_rate, cache_write_rate
    INTO r FROM model_pricing
    WHERE p_model LIKE model_prefix || '%'
    ORDER BY length(model_prefix) DESC
    LIMIT 1;

    IF NOT FOUND THEN
        RETURN NULL;
    END IF;

    RETURN (p_input_tokens * r.input_rate
          + p_output_tokens * r.output_rate
          + p_cache_read_tokens * r.cache_read_rate
          + p_cache_write_tokens * r.cache_write_rate) / 1000000.0;
END;
$$ LANGUAGE plpgsql STABLE;
