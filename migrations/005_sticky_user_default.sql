-- Change default routing strategy from primary_fallback to sticky_user.
-- Sticky user affinity maximizes Bedrock prompt cache hits, significantly reducing costs.
ALTER TABLE teams
    ALTER COLUMN routing_strategy SET DEFAULT 'sticky_user';

-- Update existing teams still on primary_fallback to sticky_user.
UPDATE teams SET routing_strategy = 'sticky_user' WHERE routing_strategy = 'primary_fallback';
