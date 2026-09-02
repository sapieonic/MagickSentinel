BEGIN;
DROP FUNCTION IF EXISTS sentinel_device_by_cert(text);
DROP FUNCTION IF EXISTS sentinel_register_device(uuid, text, text, text, timestamptz, text, char, text);
DROP FUNCTION IF EXISTS sentinel_consume_enrollment_token(text, timestamptz);
COMMIT;
