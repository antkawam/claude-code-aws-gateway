-- estimate_cost_usd: ignore a Bedrock region prefix when matching model_pricing.
--
-- model_pricing rows hold bare prefixes ("claude-sonnet-4-6"), but a client may send
-- a cross-region inference profile id ("us.anthropic.claude-sonnet-4-6"), and that
-- string is what spend_log records. The old `p_model LIKE model_prefix || '%'` never
-- matched it, so the cost was NULL: $0.00 in the portal, and nothing counted against
-- budgets, which call this same function.
--
-- Strip a leading "<region>.anthropic." first. "<region>" is any lowercase alpha token
-- (us, eu, apac, global, ...), so new routing prefixes need no change here. Cost is
-- computed on read, so this also re-prices rows already logged.

CREATE OR REPLACE FUNCTION estimate_cost_usd(
    p_model TEXT,
    p_input_tokens INTEGER,
    p_output_tokens INTEGER,
    p_cache_read_tokens INTEGER DEFAULT 0,
    p_cache_write_tokens INTEGER DEFAULT 0
) RETURNS DOUBLE PRECISION AS $$
DECLARE
    r RECORD;
    v_model TEXT;
BEGIN
    v_model := regexp_replace(p_model, '^[a-z]+\.anthropic\.', '');

    SELECT input_rate, output_rate, cache_read_rate, cache_write_rate
    INTO r FROM model_pricing
    WHERE v_model LIKE model_prefix || '%'
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
