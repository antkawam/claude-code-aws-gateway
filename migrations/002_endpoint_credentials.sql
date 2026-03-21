-- Add inference_profile_arn and external_id to endpoints
-- inference_profile_arn: optional application inference profile ARN to invoke directly
--   (instead of constructing prefix.model_id from routing_prefix)
-- external_id: optional STS AssumeRole external ID for cross-account security

ALTER TABLE endpoints
    ADD COLUMN IF NOT EXISTS inference_profile_arn TEXT,
    ADD COLUMN IF NOT EXISTS external_id TEXT;
