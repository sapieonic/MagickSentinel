BEGIN;
REVOKE ALL ON ALL TABLES IN SCHEMA public FROM sentinel_app, sentinel_pipeline;
REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM sentinel_app, sentinel_pipeline;
REVOKE USAGE ON SCHEMA public FROM sentinel_app, sentinel_pipeline;
COMMIT;
