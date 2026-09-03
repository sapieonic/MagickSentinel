BEGIN;

DROP INDEX IF EXISTS transcripts_tenant_created_idx;
DROP INDEX IF EXISTS media_segments_tenant_received_idx;

-- Dropping the function leaves the pipeline's nightly jobs unable to enumerate
-- tenants, which is the correct consequence of reverting this migration: they fail
-- loudly rather than silently purging or reconciling nothing.
DROP FUNCTION IF EXISTS sentinel_pipeline_tenants();

COMMIT;
