-- Transactional outbox for `sentinel.call.finalize`.
--
-- The gateway is the producer for the one subject the Python pipeline consumes
-- (`server/pipeline/sentinel_pipeline/consumer.py`). The obvious implementation is a
-- publish call at the end of the finalize handler, and it is wrong in a way that
-- cannot be detected afterwards:
--
--   * The database commit and the NATS publish are two different systems. If the
--     publish fails after the commit — broker restarting, network partition, the
--     gateway pod evicted between the two lines — the call is captured, its audio is
--     in object storage, its row says `transcribing`, the customer is billed for the
--     minutes, and nothing will ever transcribe it. There is no error anywhere: the
--     request succeeded.
--   * Nobody notices. The call does not appear in the compliance queue, but neither
--     do the thousands of calls that legitimately carry no findings. The floor's
--     coverage figure — the number that backs the "100% of calls monitored" claim
--     this product is bought for — is quietly wrong, and it stays wrong.
--
-- A retry loop around the publish narrows the window without closing it, because the
-- process can still die inside the loop. The only construction that closes it is to
-- make the intent to publish part of the same transaction as the finalize itself: one
-- commit, so either the call is finalized and the message is queued or neither
-- happened. A separate publisher goroutine then drains this table into JetStream and
-- deletes nothing until the broker has acked.
--
-- The cost is at-least-once delivery rather than exactly-once: a publish that
-- succeeds and whose row we then fail to mark will be published again. That is the
-- safe direction and the consumer is built for it — consumer.py's module docstring
-- records that a redelivered finalize re-runs harmlessly because transcripts,
-- analyses and flags are all keyed by call id. Duplicate work costs model tokens;
-- a dropped finalize costs a compliance record.

BEGIN;

CREATE TABLE call_finalize_outbox (
  -- One row per call, not per attempt. The primary key is what makes the enqueue
  -- idempotent: a reconnect that replays call.end cannot queue a second message,
  -- and neither can a retried request.
  call_id         uuid PRIMARY KEY REFERENCES calls(id) ON DELETE CASCADE,
  tenant_id       uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  -- The instant the call was finalized, carried into the message payload so the
  -- pipeline's own latency measurements are against the call rather than against
  -- whenever the broker happened to be reachable.
  finalized_at    timestamptz NOT NULL,
  -- Publish attempts made so far, incremented when a row is claimed rather than
  -- when it fails. It travels in the payload: a consumer that sees attempt > 1
  -- knows the message was delayed on our side, which is otherwise indistinguishable
  -- from a call that simply ran long.
  attempt         int NOT NULL DEFAULT 0,
  -- Set once the broker has acked. Rows are kept rather than deleted so that
  -- "was this call ever handed to the pipeline, and when" is answerable during an
  -- incident; the retention sweep removes them with the call.
  published_at    timestamptz,
  -- Exponential backoff. A broker outage must not become a spin loop against a
  -- broker that is trying to come back up.
  next_attempt_at timestamptz NOT NULL DEFAULT now(),
  -- The last transport failure, for the operator. Never call content: this is a
  -- broker error string, and the payload it refers to carries four identifiers and
  -- nothing else.
  last_error      text,
  created_at      timestamptz NOT NULL DEFAULT now()
);

-- The drainer's only query shape: unpublished rows that are due, oldest first.
-- Partial, so the index stays small once a floor has been running for a year and
-- almost every row is published.
CREATE INDEX call_finalize_outbox_due
  ON call_finalize_outbox (next_attempt_at, call_id)
  WHERE published_at IS NULL;

-- Tenant-scoped like everything else, and FORCE so the schema owner does not
-- accidentally become an exception to it. The enqueue happens inside the ingest
-- transaction, which has already set sentinel.tenant_id, so it goes through the
-- policy like any other write.
ALTER TABLE call_finalize_outbox ENABLE ROW LEVEL SECURITY;
ALTER TABLE call_finalize_outbox FORCE ROW LEVEL SECURITY;

-- No role needs to read another tenant's finalize queue through a normal query
-- path: an admin has no screen for it and an agent certainly does not. The write
-- side is deliberately restricted to the ingest context, which runs as `admin`
-- under Store.AsSystem.
CREATE POLICY finalize_outbox_scope ON call_finalize_outbox
  USING (tenant_id = sentinel_tenant() AND sentinel_role() = 'admin')
  WITH CHECK (tenant_id = sentinel_tenant() AND sentinel_role() = 'admin');

-- ------------------------------------------------------------------ the drainer
--
-- The drainer is the one part of the gateway that legitimately works across
-- tenants: it is a queue of work for a single process, and asking it to iterate
-- tenants would require it to first enumerate `tenants`, which is itself a
-- cross-tenant read. Rather than loosen the policy above, it gets narrow
-- SECURITY DEFINER functions in the same style as
-- db/migrations/0005_bootstrap_functions.up.sql.
--
-- docs/security.md says the set of tenant-crossing operations is exactly three and
-- is greppable. It is now those three plus this queue, and the reason the addition
-- is acceptable is that these functions cannot be used to browse: the claim
-- returns only the four identifiers that go into a message payload — no transcript,
-- no summary, no account reference, nothing borrower-related — and the other two
-- take a call id the caller must already have been handed by a claim.

-- Claim a batch of due rows.
--
-- The attempt counter and the backoff advance inside the claim, not after a failure.
-- That is what makes a crash between claiming and publishing safe: the row is not
-- lost (it is still unpublished) and it does not spin (its next attempt is already
-- pushed out). SKIP LOCKED lets a second gateway replica drain the same table
-- without the two blocking on each other or claiming the same row.
CREATE OR REPLACE FUNCTION sentinel_outbox_claim(p_limit int, p_now timestamptz)
RETURNS TABLE (call_id uuid, tenant_id uuid, finalized_at timestamptz, attempt int)
LANGUAGE sql SECURITY DEFINER SET search_path = public AS
$$
  WITH due AS (
    SELECT o.call_id
      FROM call_finalize_outbox o
     WHERE o.published_at IS NULL
       AND o.next_attempt_at <= p_now
     ORDER BY o.next_attempt_at, o.call_id
     LIMIT greatest(1, least(p_limit, 500))
       FOR UPDATE SKIP LOCKED
  )
  UPDATE call_finalize_outbox o
     SET attempt = o.attempt + 1,
         -- 2s, 4s, 8s ... capped at five minutes. The cap matters more than the
         -- curve: a message that has been stuck for an hour should still be
         -- retried every five minutes, because the fix for a broker outage is
         -- usually someone restarting the broker.
         next_attempt_at = p_now + least(
           interval '5 minutes',
           interval '2 seconds' * power(2, least(o.attempt, 8))
         )
    FROM due
   WHERE o.call_id = due.call_id
  RETURNING o.call_id, o.tenant_id, o.finalized_at, o.attempt
$$;

-- Mark a row delivered. Idempotent, and it refuses to move a published_at that is
-- already set, so a duplicate publish does not rewrite the delivery time recorded
-- for the first one.
CREATE OR REPLACE FUNCTION sentinel_outbox_published(p_call_id uuid, p_now timestamptz)
RETURNS void
LANGUAGE sql SECURITY DEFINER SET search_path = public AS
$$
  UPDATE call_finalize_outbox
     SET published_at = p_now, last_error = NULL
   WHERE call_id = p_call_id AND published_at IS NULL
$$;

-- Record why a publish failed. Deliberately does not touch attempt or
-- next_attempt_at — the claim already advanced both — and deliberately does not
-- give up after N attempts. A row abandoned here is a call that is captured,
-- stored, billed and never analysed, which is the exact failure this table exists
-- to prevent. The alarm is the queue depth, not a dead-letter column.
CREATE OR REPLACE FUNCTION sentinel_outbox_failed(p_call_id uuid, p_error text)
RETURNS void
LANGUAGE sql SECURITY DEFINER SET search_path = public AS
$$
  UPDATE call_finalize_outbox
     SET last_error = left(p_error, 500)
   WHERE call_id = p_call_id AND published_at IS NULL
$$;

-- Unpublished depth and the age of the oldest unpublished row, for the gateway's
-- OpenTelemetry gauges. Depth alone is not enough to alarm on: a busy floor's
-- queue is briefly non-empty all the time, whereas a queue whose oldest row is
-- twenty minutes old means the pipeline has stopped receiving work.
CREATE OR REPLACE FUNCTION sentinel_outbox_depth()
RETURNS TABLE (pending bigint, oldest_unpublished timestamptz)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS
$$
  SELECT count(*)::bigint, min(finalized_at)
    FROM call_finalize_outbox
   WHERE published_at IS NULL
$$;

REVOKE ALL ON FUNCTION sentinel_outbox_claim(int, timestamptz) FROM PUBLIC;
REVOKE ALL ON FUNCTION sentinel_outbox_published(uuid, timestamptz) FROM PUBLIC;
REVOKE ALL ON FUNCTION sentinel_outbox_failed(uuid, text) FROM PUBLIC;
REVOKE ALL ON FUNCTION sentinel_outbox_depth() FROM PUBLIC;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sentinel_app') THEN
    GRANT SELECT, INSERT, UPDATE ON call_finalize_outbox TO sentinel_app;
    GRANT EXECUTE ON FUNCTION sentinel_outbox_claim(int, timestamptz) TO sentinel_app;
    GRANT EXECUTE ON FUNCTION sentinel_outbox_published(uuid, timestamptz) TO sentinel_app;
    GRANT EXECUTE ON FUNCTION sentinel_outbox_failed(uuid, text) TO sentinel_app;
    GRANT EXECUTE ON FUNCTION sentinel_outbox_depth() TO sentinel_app;
  END IF;
  -- The pipeline reads the queue for nothing; it is handed work by the broker. A
  -- read grant is given anyway so an operator debugging a stalled floor from the
  -- pipeline's credentials can see the depth without the application role.
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sentinel_pipeline') THEN
    GRANT SELECT ON call_finalize_outbox TO sentinel_pipeline;
    GRANT EXECUTE ON FUNCTION sentinel_outbox_depth() TO sentinel_pipeline;
  END IF;
END $$;

COMMIT;
