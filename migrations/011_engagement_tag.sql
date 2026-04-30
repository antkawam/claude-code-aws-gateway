-- Configurable project attribution: client_tag extracted from working directory path
-- using an admin-configured marker segment (e.g. "clients" → extracts the next segment).
-- project_key depth is also configurable via proxy_settings (project_key_depth).
ALTER TABLE spend_log ADD COLUMN IF NOT EXISTS client_tag TEXT;

CREATE INDEX IF NOT EXISTS idx_spend_log_client_tag
    ON spend_log(client_tag)
    WHERE client_tag IS NOT NULL;
