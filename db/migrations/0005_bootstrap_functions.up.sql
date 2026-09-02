-- Bootstrap lookups.
--
-- Three operations legitimately happen before any tenant context exists, because
-- they are how the tenant is *established*:
--
--   1. consuming an enrollment token — the token names the tenant,
--   2. registering the enrolled device,
--   3. resolving a presented client certificate back to a device.
--
-- Everything else in the gateway runs under row-level security. Rather than punch a
-- hole in the policies for these, they are exposed as three narrow SECURITY DEFINER
-- functions. The application role gets EXECUTE on them and nothing more, so the set
-- of tenant-crossing operations is exactly these three and is greppable.
--
-- The owning role must be able to bypass RLS (a superuser, or a role with BYPASSRLS).
-- That is the schema owner, not the role the gateway connects as.

BEGIN;

-- 1. Claim an enrollment token. The WHERE clause is what makes it single-use: two
--    racing enrollments cannot both affect the row.
CREATE OR REPLACE FUNCTION sentinel_consume_enrollment_token(p_token_hash text, p_now timestamptz)
RETURNS uuid
LANGUAGE sql SECURITY DEFINER SET search_path = public AS
$$
  UPDATE enrollment_tokens
     SET consumed_at = p_now
   WHERE token_hash = p_token_hash
     AND consumed_at IS NULL
     AND expires_at > p_now
  RETURNING tenant_id
$$;

-- 2. Register the device the token authorised. Constrained to the tenant the caller
--    passes, which is the one the consumed token returned.
CREATE OR REPLACE FUNCTION sentinel_register_device(
  p_tenant_id uuid, p_machine_guid text, p_hw_fingerprint text, p_cert_fingerprint text,
  p_not_after timestamptz, p_os_build text, p_capture_tier char(1), p_agent_version text)
RETURNS uuid
LANGUAGE sql SECURITY DEFINER SET search_path = public AS
$$
  INSERT INTO devices (tenant_id, machine_guid, hw_fingerprint, cert_fingerprint,
                       cert_not_after, os_build, capture_tier, agent_version)
  VALUES (p_tenant_id, p_machine_guid, p_hw_fingerprint, p_cert_fingerprint,
          p_not_after, p_os_build, p_capture_tier, p_agent_version)
  ON CONFLICT (tenant_id, machine_guid) DO UPDATE
     SET hw_fingerprint   = excluded.hw_fingerprint,
         cert_fingerprint = excluded.cert_fingerprint,
         cert_not_after   = excluded.cert_not_after,
         os_build         = excluded.os_build,
         capture_tier     = excluded.capture_tier,
         agent_version    = excluded.agent_version,
         status           = 'active',
         revoked_at       = NULL,
         revoked_reason   = NULL
  RETURNING id
$$;

-- 3. Resolve a client certificate to a device. Returns only what the gateway needs
--    to build an identity: no machine details, no fleet enumeration. Keyed on a
--    unique index over a SHA-256, so it cannot be used to browse.
CREATE OR REPLACE FUNCTION sentinel_device_by_cert(p_fingerprint text)
RETURNS TABLE (device_id uuid, tenant_id uuid, status text)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS
$$
  SELECT id, tenant_id, status FROM devices WHERE cert_fingerprint = p_fingerprint
$$;

REVOKE ALL ON FUNCTION sentinel_consume_enrollment_token(text, timestamptz) FROM PUBLIC;
REVOKE ALL ON FUNCTION sentinel_register_device(uuid, text, text, text, timestamptz, text, char, text) FROM PUBLIC;
REVOKE ALL ON FUNCTION sentinel_device_by_cert(text) FROM PUBLIC;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sentinel_app') THEN
    GRANT EXECUTE ON FUNCTION sentinel_consume_enrollment_token(text, timestamptz) TO sentinel_app;
    GRANT EXECUTE ON FUNCTION sentinel_register_device(uuid, text, text, text, timestamptz, text, char, text) TO sentinel_app;
    GRANT EXECUTE ON FUNCTION sentinel_device_by_cert(text) TO sentinel_app;
  END IF;
END $$;

COMMIT;
