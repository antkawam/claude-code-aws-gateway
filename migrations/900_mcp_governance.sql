-- MCP / tool governance overlay.
--
-- Numbered 900 on purpose: upstream CCAG migrations advance from 013 upward, so a
-- high offset guarantees this file never collides with an upstream version number
-- when rebasing. Every table is prefixed `mcpgov_` for the same reason.
--
-- Three concerns:
--   1. Catalog  (mcpgov_servers, mcpgov_tools) — what MCP servers/tools exist, auto
--      discovered from live traffic, each with an approval status.
--   2. Policy   (mcpgov_policies) — per-scope rules resolved global -> team -> user.
--   3. Audit    (mcpgov_events) — every non-allow decision, for rollout visibility.

-- ---------------------------------------------------------------------------
-- Catalog
-- ---------------------------------------------------------------------------

-- One row per discovered MCP server, keyed on the `<server>` segment of the
-- `mcp__<server>__<tool>` convention Claude Code uses to namespace MCP tools.
CREATE TABLE IF NOT EXISTS mcpgov_servers (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    server_name TEXT NOT NULL UNIQUE,
    -- pending: discovered, no decision yet (policy default_action applies)
    -- approved / denied: explicit operator decision
    status      TEXT NOT NULL DEFAULT 'pending',
    notes       TEXT,
    first_seen  TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT mcpgov_servers_status_chk
        CHECK (status IN ('pending', 'approved', 'denied'))
);

-- One row per discovered tool. `tool_name` is the full name as it appears in the
-- request `tools` array (e.g. "mcp__github__create_issue", or "Bash" for builtins).
-- `server_name` is NULL for non-MCP builtin tools.
CREATE TABLE IF NOT EXISTS mcpgov_tools (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tool_name   TEXT NOT NULL UNIQUE,
    server_name TEXT,
    -- inherit: defer to the server's status
    -- approved / denied: explicit per-tool override
    status      TEXT NOT NULL DEFAULT 'inherit',
    first_seen  TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen   TIMESTAMPTZ NOT NULL DEFAULT now(),
    seen_count  BIGINT NOT NULL DEFAULT 0,
    CONSTRAINT mcpgov_tools_status_chk
        CHECK (status IN ('inherit', 'approved', 'denied'))
);

CREATE INDEX IF NOT EXISTS mcpgov_tools_server_idx ON mcpgov_tools (server_name);
CREATE INDEX IF NOT EXISTS mcpgov_tools_status_idx ON mcpgov_tools (status);

-- ---------------------------------------------------------------------------
-- Policy
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS mcpgov_policies (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- global | team | user
    scope          TEXT NOT NULL,
    -- NULL for global; team UUID (as text) for team; user email for user
    scope_ref      TEXT,
    -- observe: decide + log, never modify the request
    -- warn:    allow, but log and flag on the response
    -- enforce: actually strip denied tool definitions
    mode           TEXT NOT NULL DEFAULT 'observe',
    -- What to do with a tool the catalog has no decision for (status 'pending').
    default_action TEXT NOT NULL DEFAULT 'allow',
    -- mcp_only: only `mcp__*` tools are subject to policy (safe default — never
    --           touches builtins like Read/Write/Bash)
    -- all_tools: builtins are governed too
    applies_to     TEXT NOT NULL DEFAULT 'mcp_only',
    -- Glob patterns matched against the full tool name, e.g. "mcp__github__*".
    allow_patterns JSONB NOT NULL DEFAULT '[]'::jsonb,
    deny_patterns  JSONB NOT NULL DEFAULT '[]'::jsonb,
    enabled        BOOLEAN NOT NULL DEFAULT true,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by     TEXT,
    CONSTRAINT mcpgov_policies_scope_chk
        CHECK (scope IN ('global', 'team', 'user')),
    CONSTRAINT mcpgov_policies_mode_chk
        CHECK (mode IN ('observe', 'warn', 'enforce')),
    CONSTRAINT mcpgov_policies_default_action_chk
        CHECK (default_action IN ('allow', 'deny')),
    CONSTRAINT mcpgov_policies_applies_to_chk
        CHECK (applies_to IN ('mcp_only', 'all_tools')),
    -- A global policy must have no ref; team/user policies must have one.
    CONSTRAINT mcpgov_policies_ref_chk CHECK (
        (scope = 'global' AND scope_ref IS NULL)
        OR (scope <> 'global' AND scope_ref IS NOT NULL AND scope_ref <> '')
    )
);

-- Postgres treats NULLs as distinct in UNIQUE constraints, which would allow
-- several 'global' rows. COALESCE collapses that so there is exactly one policy
-- per (scope, ref).
CREATE UNIQUE INDEX IF NOT EXISTS mcpgov_policies_scope_uniq
    ON mcpgov_policies (scope, COALESCE(scope_ref, ''));

-- Seed a global policy in the safest possible posture: observe mode (nothing is
-- ever stripped) and allow-by-default. Operators tighten this once the catalog
-- has been populated from real traffic.
INSERT INTO mcpgov_policies (scope, scope_ref, mode, default_action, applies_to)
SELECT 'global', NULL, 'observe', 'allow', 'mcp_only'
WHERE NOT EXISTS (SELECT 1 FROM mcpgov_policies WHERE scope = 'global');

-- ---------------------------------------------------------------------------
-- Audit
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS mcpgov_events (
    id            BIGSERIAL PRIMARY KEY,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    tool_name     TEXT NOT NULL,
    server_name   TEXT,
    -- blocked: tool definition was stripped (enforce mode)
    -- would_block: policy said deny but mode was observe (dry run)
    -- warned: policy said deny but mode was warn (allowed + flagged)
    decision      TEXT NOT NULL,
    mode          TEXT NOT NULL,
    reason        TEXT NOT NULL,
    -- Which scope produced the decision: global | team | user | catalog
    decided_by    TEXT,
    user_identity TEXT,
    team_id       UUID,
    key_id        UUID,
    request_id    TEXT,
    CONSTRAINT mcpgov_events_decision_chk
        CHECK (decision IN ('blocked', 'would_block', 'warned'))
);

CREATE INDEX IF NOT EXISTS mcpgov_events_created_idx ON mcpgov_events (created_at DESC);
CREATE INDEX IF NOT EXISTS mcpgov_events_tool_idx ON mcpgov_events (tool_name);
CREATE INDEX IF NOT EXISTS mcpgov_events_user_idx ON mcpgov_events (user_identity);
