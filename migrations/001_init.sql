-- CCAG schema — single initial migration for fresh installs.

-- Teams
CREATE TABLE IF NOT EXISTS teams (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL UNIQUE,
    budget_amount_usd DOUBLE PRECISION,
    budget_period TEXT NOT NULL DEFAULT 'monthly',
    budget_policy JSONB,
    default_user_budget_usd DOUBLE PRECISION,
    notify_recipients TEXT NOT NULL DEFAULT 'both',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Users
CREATE TABLE IF NOT EXISTS users (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email TEXT NOT NULL UNIQUE,
    team_id UUID REFERENCES teams(id),
    role TEXT NOT NULL DEFAULT 'member',
    spend_limit_monthly_usd DOUBLE PRECISION,
    budget_period TEXT NOT NULL DEFAULT 'monthly',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Virtual API keys
CREATE TABLE IF NOT EXISTS virtual_keys (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    key_hash TEXT NOT NULL UNIQUE,
    key_prefix TEXT NOT NULL,
    name TEXT,
    user_id UUID REFERENCES users(id),
    team_id UUID REFERENCES teams(id),
    is_active BOOLEAN NOT NULL DEFAULT true,
    rate_limit_rpm INTEGER,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ
);

-- Spend log (request-level telemetry)
CREATE TABLE IF NOT EXISTS spend_log (
    id BIGSERIAL PRIMARY KEY,
    key_id UUID REFERENCES virtual_keys(id),
    user_identity TEXT,
    request_id TEXT,
    model TEXT NOT NULL,
    streaming BOOLEAN NOT NULL DEFAULT false,
    duration_ms INTEGER,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    stop_reason TEXT,
    tool_count SMALLINT NOT NULL DEFAULT 0,
    tool_names TEXT[],
    turn_count SMALLINT NOT NULL DEFAULT 0,
    thinking_enabled BOOLEAN NOT NULL DEFAULT false,
    has_system_prompt BOOLEAN NOT NULL DEFAULT false,
    session_id TEXT,
    project_key TEXT,
    tool_errors JSONB,
    has_correction BOOLEAN NOT NULL DEFAULT false,
    content_block_types TEXT[],
    system_prompt_hash TEXT,
    detection_flags JSONB,
    endpoint_id UUID,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_spend_log_key_id ON spend_log(key_id);
CREATE INDEX IF NOT EXISTS idx_spend_log_recorded_at ON spend_log(recorded_at);
CREATE INDEX IF NOT EXISTS idx_spend_log_user_identity ON spend_log(user_identity);
CREATE INDEX IF NOT EXISTS idx_spend_log_model ON spend_log(model);
CREATE INDEX IF NOT EXISTS idx_spend_log_session ON spend_log(session_id) WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_spend_log_project ON spend_log(project_key) WHERE project_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_spend_log_flagged ON spend_log USING GIN (detection_flags) WHERE detection_flags IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_virtual_keys_hash ON virtual_keys(key_hash);

-- Identity providers (DB-managed OIDC configs)
CREATE TABLE IF NOT EXISTS identity_providers (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    issuer_url TEXT NOT NULL UNIQUE,
    client_id TEXT,
    audience TEXT,
    jwks_url TEXT,
    flow_type TEXT NOT NULL DEFAULT 'device_code',
    auto_provision BOOLEAN NOT NULL DEFAULT false,
    default_role TEXT NOT NULL DEFAULT 'member',
    allowed_domains TEXT[],
    enabled BOOLEAN NOT NULL DEFAULT true,
    source TEXT NOT NULL DEFAULT 'admin',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Gateway settings (key-value store)
CREATE TABLE IF NOT EXISTS proxy_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Cache version for polling-based invalidation
CREATE TABLE IF NOT EXISTS cache_version (
    id INTEGER PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    version BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO cache_version (id, version) VALUES (1, 0) ON CONFLICT DO NOTHING;
INSERT INTO proxy_settings (key, value) VALUES ('virtual_keys_enabled', 'true') ON CONFLICT DO NOTHING;

-- Model mappings (Anthropic <-> Bedrock)
CREATE TABLE IF NOT EXISTS model_mappings (
    anthropic_prefix TEXT PRIMARY KEY,
    bedrock_suffix TEXT NOT NULL,
    anthropic_display TEXT,
    source TEXT NOT NULL DEFAULT 'seed',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO model_mappings (anthropic_prefix, bedrock_suffix, anthropic_display, source) VALUES
    ('claude-opus-4-6',    'anthropic.claude-opus-4-6-v1',              'claude-opus-4-6-20250605',    'seed'),
    ('claude-sonnet-4-6',  'anthropic.claude-sonnet-4-6',               'claude-sonnet-4-6-20250514',  'seed'),
    ('claude-sonnet-4-5',  'anthropic.claude-sonnet-4-5-20250929-v1:0', 'claude-sonnet-4-5-20250929',  'seed'),
    ('claude-sonnet-4-',   'anthropic.claude-sonnet-4-20250514-v1:0',   'claude-sonnet-4-20250514',    'seed'),
    ('claude-haiku-4-5',   'anthropic.claude-haiku-4-5-20251001-v1:0',  'claude-haiku-4-5-20251001',   'seed')
ON CONFLICT (anthropic_prefix) DO NOTHING;

-- Sessions (materialized from spend_log)
CREATE TABLE IF NOT EXISTS sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id TEXT UNIQUE NOT NULL,
    user_identity TEXT NOT NULL,
    project_key TEXT,
    start_time TIMESTAMPTZ NOT NULL,
    end_time TIMESTAMPTZ NOT NULL,
    duration_minutes DOUBLE PRECISION,
    request_count INT NOT NULL,
    turn_count INT,
    total_cost_usd DOUBLE PRECISION,
    models_used TEXT[],
    tools_used JSONB,
    correction_count INT DEFAULT 0,
    error_count INT DEFAULT 0,
    cache_hit_rate DOUBLE PRECISION,
    facets JSONB,
    analyzed_at TIMESTAMPTZ,
    aggregated_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ DEFAULT now(),
    flag_categories TEXT[],
    flag_count INT DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_identity);
CREATE INDEX IF NOT EXISTS idx_sessions_project ON sessions(project_key) WHERE project_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_sessions_unanalyzed ON sessions(end_time) WHERE analyzed_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_sessions_flagged ON sessions(end_time) WHERE flag_count > 0;

-- Project insights (cross-session analysis)
CREATE TABLE IF NOT EXISTS project_insights (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_key TEXT NOT NULL,
    tags TEXT[] NOT NULL DEFAULT '{}',
    title TEXT NOT NULL,
    description TEXT NOT NULL,
    problem TEXT NOT NULL DEFAULT '',
    cc_prompt TEXT,
    evidence JSONB NOT NULL DEFAULT '{}',
    recurrence_score DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    priority_score DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    session_ids TEXT[] NOT NULL DEFAULT '{}',
    generated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    stale_at TIMESTAMPTZ NOT NULL DEFAULT (now() + interval '7 days'),
    dismissed BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_project_insights_project ON project_insights (project_key);
CREATE INDEX IF NOT EXISTS idx_project_insights_active ON project_insights (project_key, dismissed, stale_at)
    WHERE NOT dismissed;

-- Cost estimation function
CREATE OR REPLACE FUNCTION estimate_cost_usd(
    p_model TEXT,
    p_input_tokens INTEGER,
    p_output_tokens INTEGER,
    p_cache_read_tokens INTEGER DEFAULT 0,
    p_cache_write_tokens INTEGER DEFAULT 0
) RETURNS DOUBLE PRECISION AS $$
DECLARE
    input_rate DOUBLE PRECISION;
    output_rate DOUBLE PRECISION;
    cache_read_rate DOUBLE PRECISION;
    cache_write_rate DOUBLE PRECISION;
BEGIN
    IF p_model LIKE '%opus-4-5%' OR p_model LIKE '%opus-4-6%'
       OR p_model LIKE '%opus_4_5%' OR p_model LIKE '%opus_4_6%' THEN
        input_rate := 5.0; output_rate := 25.0;
        cache_read_rate := 0.50; cache_write_rate := 6.25;
    ELSIF p_model LIKE '%opus%' THEN
        input_rate := 15.0; output_rate := 75.0;
        cache_read_rate := 1.50; cache_write_rate := 18.75;
    ELSIF p_model LIKE '%haiku-4-5%' OR p_model LIKE '%haiku_4_5%' THEN
        input_rate := 1.0; output_rate := 5.0;
        cache_read_rate := 0.10; cache_write_rate := 1.25;
    ELSIF p_model LIKE '%haiku%' THEN
        input_rate := 0.25; output_rate := 1.25;
        cache_read_rate := 0.03; cache_write_rate := 0.30;
    ELSE
        input_rate := 3.0; output_rate := 15.0;
        cache_read_rate := 0.30; cache_write_rate := 3.75;
    END IF;

    RETURN (p_input_tokens * input_rate
          + p_output_tokens * output_rate
          + p_cache_read_tokens * cache_read_rate
          + p_cache_write_tokens * cache_write_rate) / 1000000.0;
END;
$$ LANGUAGE plpgsql IMMUTABLE;

-- Budget events
CREATE TABLE IF NOT EXISTS budget_events (
    id BIGSERIAL PRIMARY KEY,
    user_identity TEXT,
    team_id UUID REFERENCES teams(id) ON DELETE SET NULL,
    event_type TEXT NOT NULL,
    threshold_percent INTEGER NOT NULL,
    spend_usd DOUBLE PRECISION NOT NULL,
    limit_usd DOUBLE PRECISION NOT NULL,
    percent DOUBLE PRECISION NOT NULL,
    period TEXT NOT NULL,
    period_start TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    delivered_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_budget_events_user ON budget_events (user_identity, period_start);
CREATE INDEX IF NOT EXISTS idx_budget_events_team ON budget_events (team_id, period_start);
CREATE INDEX IF NOT EXISTS idx_budget_events_undelivered ON budget_events (delivered_at) WHERE delivered_at IS NULL;

-- Bedrock endpoints (multi-account/region routing)
CREATE TABLE IF NOT EXISTS endpoints (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    role_arn TEXT,
    region TEXT NOT NULL,
    routing_prefix TEXT NOT NULL,
    priority INTEGER NOT NULL DEFAULT 0,
    enabled BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS team_endpoints (
    team_id UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    endpoint_id UUID NOT NULL REFERENCES endpoints(id) ON DELETE CASCADE,
    priority INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (team_id, endpoint_id)
);

-- User search provider configurations
CREATE TABLE IF NOT EXISTS user_search_providers (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider_type TEXT NOT NULL,
    api_key TEXT,
    api_url TEXT,
    max_results INTEGER NOT NULL DEFAULT 5,
    enabled BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT usp_unique_user_provider UNIQUE (user_id, provider_type)
);

CREATE INDEX IF NOT EXISTS idx_usp_user ON user_search_providers(user_id);
