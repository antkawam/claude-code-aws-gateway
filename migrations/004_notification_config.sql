-- App notification configuration: admin-managed destination with draft/active workflow.

CREATE TABLE IF NOT EXISTS notification_config (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    slot            TEXT NOT NULL CHECK (slot IN ('active', 'draft')),
    destination_type  TEXT NOT NULL CHECK (destination_type IN ('webhook', 'sns', 'eventbridge')),
    destination_value TEXT NOT NULL,
    event_categories  JSONB NOT NULL DEFAULT '["budget"]',
    last_tested_at    TIMESTAMPTZ,
    last_test_success BOOLEAN,
    last_test_error   TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- At most one active and one draft config.
CREATE UNIQUE INDEX IF NOT EXISTS notification_config_slot_unique ON notification_config (slot);

-- Delivery audit log (pruned to last 500 entries).
CREATE TABLE IF NOT EXISTS notification_delivery_log (
    id                BIGSERIAL PRIMARY KEY,
    event_id          BIGINT REFERENCES budget_events(id) ON DELETE SET NULL,
    destination_type  TEXT NOT NULL,
    destination_value TEXT NOT NULL,
    event_type        TEXT NOT NULL,
    payload           JSONB NOT NULL,
    status            TEXT NOT NULL CHECK (status IN ('success', 'failure')),
    error_message     TEXT,
    duration_ms       INT NOT NULL DEFAULT 0,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS notification_delivery_log_created_at ON notification_delivery_log (created_at);
