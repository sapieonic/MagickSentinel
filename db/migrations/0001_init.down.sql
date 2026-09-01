BEGIN;
DROP TABLE IF EXISTS audit_log, device_events, coverage_daily, flags, prompt_templates,
  rule_sets, ptps, analyses, transcripts, ingest_watermarks, media_segments, calls,
  devices, enrollment_tokens, users, teams, tenants CASCADE;
DROP FUNCTION IF EXISTS calls_sync_has_flags();
COMMIT;
