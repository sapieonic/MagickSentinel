-- Row-level security.
--
-- The gateway connects as `sentinel_app`, a NOBYPASSRLS role, and sets three
-- transaction-local settings from the *verified* token claims before every query:
--
--   SET LOCAL sentinel.tenant_id = '<uuid>';
--   SET LOCAL sentinel.user_uid  = '<firebase uid>';
--   SET LOCAL sentinel.role      = 'agent' | 'supervisor' | ... ;
--
-- Nothing in a request body or path can influence these. If the settings are absent
-- the predicates evaluate false and the queries return nothing, which is the correct
-- failure mode: a bug that forgets to set the context leaks zero rows rather than all
-- of them.

BEGIN;

CREATE OR REPLACE FUNCTION sentinel_tenant() RETURNS uuid
  LANGUAGE sql STABLE AS
$$ SELECT nullif(current_setting('sentinel.tenant_id', true), '')::uuid $$;

CREATE OR REPLACE FUNCTION sentinel_uid() RETURNS text
  LANGUAGE sql STABLE AS
$$ SELECT nullif(current_setting('sentinel.user_uid', true), '') $$;

CREATE OR REPLACE FUNCTION sentinel_role() RETURNS text
  LANGUAGE sql STABLE AS
$$ SELECT coalesce(nullif(current_setting('sentinel.role', true), ''), 'none') $$;

-- Team of the calling user, resolved once per statement.
CREATE OR REPLACE FUNCTION sentinel_team() RETURNS uuid
  LANGUAGE sql STABLE SECURITY DEFINER AS
$$ SELECT team_id FROM users
   WHERE firebase_uid = sentinel_uid() AND tenant_id = sentinel_tenant() $$;

-- Visibility of a call, per the role matrix in section 13.4 of the spec.
--   agent      -> own calls only
--   supervisor -> own team
--   qa/compliance/admin -> the whole tenant
--   client     -> flagged calls only
CREATE OR REPLACE FUNCTION sentinel_can_see_call(p_user_uid text, p_team_id uuid, p_has_flags boolean)
  RETURNS boolean LANGUAGE sql STABLE AS
$$
  SELECT CASE sentinel_role()
    WHEN 'agent'      THEN p_user_uid = sentinel_uid()
    WHEN 'supervisor' THEN p_user_uid = sentinel_uid()
                        OR (p_team_id IS NOT NULL AND p_team_id = sentinel_team())
    WHEN 'qa'         THEN true
    WHEN 'compliance' THEN true
    WHEN 'admin'      THEN true
    WHEN 'client'     THEN p_has_flags
    ELSE false
  END
$$;

-- Helper for child tables that only carry call_id.
CREATE OR REPLACE FUNCTION sentinel_call_visible(p_call_id uuid)
  RETURNS boolean LANGUAGE sql STABLE AS
$$
  SELECT EXISTS (
    SELECT 1 FROM calls c
    WHERE c.id = p_call_id
      AND c.tenant_id = sentinel_tenant()
      AND sentinel_can_see_call(c.user_uid, c.team_id, c.has_flags)
  )
$$;

-- ---------------------------------------------------------------- tenant scoping

DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY[
    'teams','users','enrollment_tokens','devices','calls','media_segments',
    'transcripts','analyses','ptps','rule_sets','prompt_templates','flags',
    'coverage_daily','device_events','audit_log'
  ] LOOP
    EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
  END LOOP;
END $$;

-- tenants: a caller sees only their own tenant row.
ALTER TABLE tenants ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenants FORCE ROW LEVEL SECURITY;
CREATE POLICY tenants_self ON tenants
  USING (id = sentinel_tenant());

-- Plain tenant-scoped tables: everything in my tenant, nothing outside it.
CREATE POLICY teams_tenant ON teams USING (tenant_id = sentinel_tenant());
CREATE POLICY enrollment_tokens_tenant ON enrollment_tokens
  USING (tenant_id = sentinel_tenant()) WITH CHECK (tenant_id = sentinel_tenant());
CREATE POLICY devices_tenant ON devices
  USING (tenant_id = sentinel_tenant()) WITH CHECK (tenant_id = sentinel_tenant());
CREATE POLICY coverage_tenant ON coverage_daily
  USING (tenant_id = sentinel_tenant()
         AND (sentinel_role() <> 'agent' OR user_uid = sentinel_uid()))
  WITH CHECK (tenant_id = sentinel_tenant());
CREATE POLICY device_events_tenant ON device_events
  USING (tenant_id = sentinel_tenant()) WITH CHECK (tenant_id = sentinel_tenant());
CREATE POLICY audit_tenant ON audit_log
  USING (tenant_id = sentinel_tenant()) WITH CHECK (tenant_id = sentinel_tenant());

-- users: an agent must not see other agents' rows (13.4: no other agents' scores).
CREATE POLICY users_tenant ON users
  USING (tenant_id = sentinel_tenant()
         AND (sentinel_role() <> 'agent' OR firebase_uid = sentinel_uid()))
  WITH CHECK (tenant_id = sentinel_tenant());

-- Rule definitions are not agent-visible at all.
CREATE POLICY rule_sets_scope ON rule_sets
  USING (tenant_id = sentinel_tenant()
         AND sentinel_role() IN ('compliance','admin','qa'))
  WITH CHECK (tenant_id = sentinel_tenant()
              AND sentinel_role() IN ('compliance','admin'));
CREATE POLICY prompt_templates_scope ON prompt_templates
  USING ((tenant_id IS NULL OR tenant_id = sentinel_tenant())
         AND sentinel_role() IN ('compliance','admin'))
  WITH CHECK (tenant_id = sentinel_tenant() AND sentinel_role() = 'admin');

-- ------------------------------------------------------------- call-scoped tables

CREATE POLICY calls_scope ON calls
  USING (tenant_id = sentinel_tenant()
         AND sentinel_can_see_call(user_uid, team_id, has_flags))
  WITH CHECK (tenant_id = sentinel_tenant());

CREATE POLICY media_scope ON media_segments
  USING (tenant_id = sentinel_tenant() AND sentinel_call_visible(call_id))
  WITH CHECK (tenant_id = sentinel_tenant());

CREATE POLICY transcripts_scope ON transcripts
  USING (tenant_id = sentinel_tenant() AND sentinel_call_visible(call_id))
  WITH CHECK (tenant_id = sentinel_tenant());

CREATE POLICY analyses_scope ON analyses
  USING (tenant_id = sentinel_tenant() AND sentinel_call_visible(call_id))
  WITH CHECK (tenant_id = sentinel_tenant());

CREATE POLICY ptps_scope ON ptps
  USING (tenant_id = sentinel_tenant() AND sentinel_call_visible(call_id))
  WITH CHECK (tenant_id = sentinel_tenant());

-- The bank client sees every flag in the tenant (that is the product), so this
-- predicate deliberately does not go back through sentinel_call_visible for that
-- role: doing so would recurse into the calls policy, which consults flags.
CREATE POLICY flags_scope ON flags
  USING (tenant_id = sentinel_tenant()
         AND (sentinel_role() = 'client' OR sentinel_call_visible(call_id)))
  WITH CHECK (tenant_id = sentinel_tenant());

COMMIT;
