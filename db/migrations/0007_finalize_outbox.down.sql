-- Reverses 0007. Dropping the table discards any finalize messages that have not
-- yet been published, so a down-migration on a running floor loses the calls those
-- rows represent from the pipeline's point of view. Drain the queue first —
-- `SELECT * FROM sentinel_outbox_depth()` returning zero pending — or accept that
-- those calls will need re-finalizing by hand.

BEGIN;

DROP FUNCTION IF EXISTS sentinel_outbox_depth();
DROP FUNCTION IF EXISTS sentinel_outbox_failed(uuid, text);
DROP FUNCTION IF EXISTS sentinel_outbox_published(uuid, timestamptz);
DROP FUNCTION IF EXISTS sentinel_outbox_claim(int, timestamptz);

DROP POLICY IF EXISTS finalize_outbox_scope ON call_finalize_outbox;
DROP TABLE IF EXISTS call_finalize_outbox;

COMMIT;
