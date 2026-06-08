ALTER TABLE model_mappings
  ADD COLUMN IF NOT EXISTS created_via TEXT NOT NULL DEFAULT 'unknown';

ALTER TABLE model_mappings
  ADD COLUMN IF NOT EXISTS last_used_at TIMESTAMPTZ;

-- Clean up inert legacy rows that were keyed as starts_with prefixes
-- and have been unreachable since the exact-match cutover (v1.7.0, PR #77).
DELETE FROM model_mappings
WHERE anthropic_prefix IN ('claude-sonnet-4-', 'opus');
