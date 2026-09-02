-- Two corrections to team scoping, both found while wiring the portal.
--
-- 1. The `teams` policy was tenant-scoped only, so any authenticated caller — an
--    agent included — could enumerate the tenant's team roster. The role matrix says
--    an agent must not see other agents' calls or scores; the org chart is the same
--    kind of information and there is no screen that needs it.
--
-- 2. Supervisor visibility compared a call's team against `users.team_id`, a single
--    value. A supervisor responsible for more than one team could never see the
--    others' calls, and adding a teams listing only let them name the teams they
--    still could not read. Membership becomes a set.

BEGIN;

CREATE TABLE team_memberships (
  tenant_id  uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  user_uid   text NOT NULL REFERENCES users(firebase_uid) ON DELETE CASCADE,
  team_id    uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (user_uid, team_id)
);
CREATE INDEX ON team_memberships (tenant_id, team_id);

ALTER TABLE team_memberships ENABLE ROW LEVEL SECURITY;
ALTER TABLE team_memberships FORCE ROW LEVEL SECURITY;

-- Backfill from the single-team column so nothing regresses on upgrade.
-- users.team_id stays as the agent's primary team, which is what a call row stamps.
INSERT INTO team_memberships (tenant_id, user_uid, team_id)
SELECT tenant_id, firebase_uid, team_id FROM users WHERE team_id IS NOT NULL
ON CONFLICT DO NOTHING;

-- Every team the caller belongs to.
--
-- The union with users.team_id is what keeps this backward compatible:
-- users.team_id remains the primary team, stamped onto each call row and set by
-- whatever provisions users today, and team_memberships only adds the extras. A
-- one-time backfill would have covered the users that existed when this migration
-- ran and silently missed every one created afterwards.
--
-- SECURITY DEFINER because the policy on team_memberships would otherwise have to
-- consult itself.
CREATE OR REPLACE FUNCTION sentinel_teams() RETURNS uuid[]
  LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS
$$
  SELECT COALESCE(array_agg(DISTINCT t), ARRAY[]::uuid[]) FROM (
    SELECT team_id AS t FROM team_memberships
     WHERE user_uid = sentinel_uid() AND tenant_id = sentinel_tenant()
    UNION
    SELECT team_id FROM users
     WHERE firebase_uid = sentinel_uid() AND tenant_id = sentinel_tenant()
       AND team_id IS NOT NULL
  ) x
$$;

CREATE OR REPLACE FUNCTION sentinel_can_see_call(p_user_uid text, p_team_id uuid, p_has_flags boolean)
  RETURNS boolean LANGUAGE sql STABLE AS
$$
  SELECT CASE sentinel_role()
    WHEN 'agent'      THEN p_user_uid = sentinel_uid()
    WHEN 'supervisor' THEN p_user_uid = sentinel_uid()
                        OR (p_team_id IS NOT NULL AND p_team_id = ANY (sentinel_teams()))
    WHEN 'qa'         THEN true
    WHEN 'compliance' THEN true
    WHEN 'admin'      THEN true
    WHEN 'client'     THEN p_has_flags
    ELSE false
  END
$$;

-- An agent sees their own memberships and nothing else; the roster is for roles that
-- have a team scope to choose from.
CREATE POLICY memberships_scope ON team_memberships
  USING (tenant_id = sentinel_tenant()
         AND (sentinel_role() <> 'agent' OR user_uid = sentinel_uid()))
  WITH CHECK (tenant_id = sentinel_tenant() AND sentinel_role() = 'admin');

DROP POLICY IF EXISTS teams_tenant ON teams;
CREATE POLICY teams_scope ON teams
  USING (tenant_id = sentinel_tenant()
         AND (sentinel_role() <> 'agent' OR id = ANY (sentinel_teams())))
  WITH CHECK (tenant_id = sentinel_tenant() AND sentinel_role() = 'admin');

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sentinel_app') THEN
    GRANT SELECT, INSERT, UPDATE, DELETE ON team_memberships TO sentinel_app;
    GRANT EXECUTE ON FUNCTION sentinel_teams() TO sentinel_app;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sentinel_pipeline') THEN
    GRANT SELECT ON team_memberships TO sentinel_pipeline;
  END IF;
END $$;

COMMIT;
