BEGIN;
DROP POLICY IF EXISTS teams_scope ON teams;
CREATE POLICY teams_tenant ON teams USING (tenant_id = sentinel_tenant());
DROP POLICY IF EXISTS memberships_scope ON team_memberships;
DROP TABLE IF EXISTS team_memberships;
DROP FUNCTION IF EXISTS sentinel_teams();
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
COMMIT;
