#!/usr/bin/env bash
# Row-level security acceptance tests.
#
# These assert the property the spec calls out as gating: a query that forgets its
# tenant filter still cannot read another tenant's rows, and an agent cannot read
# another agent's calls. Every check runs as sentinel_app (NOBYPASSRLS), the role the
# gateway actually connects as.
set -euo pipefail
cd "$(cd "$(dirname "$0")" && pwd)"
export PGTEST_ENTRY="$PWD/rls_test.sh"

# shellcheck disable=SC1091
source ./pgtest.sh

Q() { psql -h "$PGHOST" -p "$PGPORT" -d "$PGDATABASE" -qtAX -v ON_ERROR_STOP=1 -c "$1"; }
APP() { PGOPTIONS='' psql -h "$PGHOST" -p "$PGPORT" -d "$PGDATABASE" -U sentinel_app -qtAX -v ON_ERROR_STOP=1 -c "$1"; }

fail=0
expect() { # expect <label> <want> <got>
  if [ "$2" = "$3" ]; then printf 'ok   %s\n' "$1"
  else printf 'FAIL %s: want %q got %q\n' "$1" "$2" "$3"; fail=1; fi
}

# ------------------------------------------------------------------ fixtures
Q "
INSERT INTO tenants (id, name, idp_tenant_id) VALUES
  ('11111111-1111-1111-1111-111111111111','Acme BPO','acme-tenant'),
  ('22222222-2222-2222-2222-222222222222','Rival BPO','rival-tenant');
INSERT INTO teams (id, tenant_id, name) VALUES
  ('aaaaaaaa-0000-0000-0000-000000000001','11111111-1111-1111-1111-111111111111','Team North'),
  ('aaaaaaaa-0000-0000-0000-000000000002','11111111-1111-1111-1111-111111111111','Team South');
INSERT INTO users (firebase_uid, tenant_id, role, team_id, display_name) VALUES
  ('agent-a','11111111-1111-1111-1111-111111111111','agent','aaaaaaaa-0000-0000-0000-000000000001','Agent A'),
  ('agent-b','11111111-1111-1111-1111-111111111111','agent','aaaaaaaa-0000-0000-0000-000000000001','Agent B'),
  ('agent-c','11111111-1111-1111-1111-111111111111','agent','aaaaaaaa-0000-0000-0000-000000000002','Agent C'),
  ('sup-north','11111111-1111-1111-1111-111111111111','supervisor','aaaaaaaa-0000-0000-0000-000000000001','Sup North'),
  ('qa-1','11111111-1111-1111-1111-111111111111','qa',NULL,'QA One'),
  ('client-1','11111111-1111-1111-1111-111111111111','client',NULL,'Bank Client'),
  ('rival-admin','22222222-2222-2222-2222-222222222222','admin',NULL,'Rival Admin');
INSERT INTO devices (id, tenant_id, machine_guid, hw_fingerprint, cert_fingerprint, os_build, capture_tier, agent_version) VALUES
  ('dddddddd-0000-0000-0000-000000000001','11111111-1111-1111-1111-111111111111','mg-1','hw-1','cf-1','10.0.22631','A','1.0.0'),
  ('dddddddd-0000-0000-0000-000000000002','22222222-2222-2222-2222-222222222222','mg-2','hw-2','cf-2','10.0.19045','B','1.0.0');
INSERT INTO calls (id, tenant_id, device_id, user_uid, team_id, started_at, capture_tier) VALUES
  ('c0000000-0000-0000-0000-00000000000a','11111111-1111-1111-1111-111111111111','dddddddd-0000-0000-0000-000000000001','agent-a','aaaaaaaa-0000-0000-0000-000000000001', now(), 'A'),
  ('c0000000-0000-0000-0000-00000000000b','11111111-1111-1111-1111-111111111111','dddddddd-0000-0000-0000-000000000001','agent-b','aaaaaaaa-0000-0000-0000-000000000001', now(), 'A'),
  ('c0000000-0000-0000-0000-00000000000c','11111111-1111-1111-1111-111111111111','dddddddd-0000-0000-0000-000000000001','agent-c','aaaaaaaa-0000-0000-0000-000000000002', now(), 'A'),
  ('c0000000-0000-0000-0000-0000000000ff','22222222-2222-2222-2222-222222222222','dddddddd-0000-0000-0000-000000000002','rival-admin',NULL, now(), 'B');
INSERT INTO rule_sets (tenant_id, version, definition, active, created_by)
SELECT t.id, 1, d.definition, true, 'system' FROM tenants t CROSS JOIN default_rule_set d;
INSERT INTO flags (tenant_id, call_id, rule_id, rule_set_version, severity, tier) VALUES
  ('11111111-1111-1111-1111-111111111111','c0000000-0000-0000-0000-00000000000b','false_legal_threat',1,'critical',1);
" >/dev/null

ctx() { # ctx <tenant> <uid> <role> <query>
  APP "SET ROLE sentinel_app;
       SELECT set_config('sentinel.tenant_id','$1',false),
              set_config('sentinel.user_uid','$2',false),
              set_config('sentinel.role','$3',false);
       $4" | tail -1
}
ACME=11111111-1111-1111-1111-111111111111
RIVAL=22222222-2222-2222-2222-222222222222

# ------------------------------------------------------------------- assertions
expect "agent sees only own calls" \
  "1" "$(ctx $ACME agent-a agent 'SELECT count(*) FROM calls;')"

expect "agent cannot read a named other-agent call" \
  "0" "$(ctx $ACME agent-a agent "SELECT count(*) FROM calls WHERE id='c0000000-0000-0000-0000-00000000000b';")"

expect "supervisor sees own team only" \
  "2" "$(ctx $ACME sup-north supervisor 'SELECT count(*) FROM calls;')"

expect "qa sees whole tenant" \
  "3" "$(ctx $ACME qa-1 qa 'SELECT count(*) FROM calls;')"

expect "client sees flagged calls only" \
  "1" "$(ctx $ACME client-1 client 'SELECT count(*) FROM calls;')"

expect "cross-tenant read returns nothing even when asked for by id" \
  "0" "$(ctx $ACME qa-1 qa "SELECT count(*) FROM calls WHERE id='c0000000-0000-0000-0000-0000000000ff';")"

expect "rival tenant sees only its own call" \
  "1" "$(ctx $RIVAL rival-admin admin 'SELECT count(*) FROM calls;')"

expect "missing context leaks zero rows" \
  "0" "$(APP "SET ROLE sentinel_app; SELECT count(*) FROM calls;" | tail -1)"

expect "agent cannot enumerate other users" \
  "1" "$(ctx $ACME agent-a agent 'SELECT count(*) FROM users;')"

expect "agent cannot read raw rule definitions" \
  "0" "$(ctx $ACME agent-a agent 'SELECT count(*) FROM rule_sets;')"

expect "compliance can read rule definitions" \
  "1" "$(ctx $ACME qa-1 compliance 'SELECT count(*) FROM rule_sets;')"

expect "agent cannot see another agent's device events across tenants" \
  "1" "$(ctx $ACME agent-a agent 'SELECT count(*) FROM devices;')"

expect "insert into another tenant is rejected" \
  "rejected" "$(ctx $ACME qa-1 admin "SELECT 'inserted' FROM (INSERT INTO teams (tenant_id, name) VALUES ('$RIVAL','sneaky') RETURNING 1) x;" 2>/dev/null || echo rejected)"

expect "transcript of another agent's call is invisible" \
  "0" "$(ctx $ACME agent-a agent "SELECT count(*) FROM transcripts WHERE call_id='c0000000-0000-0000-0000-00000000000b';")"

echo
if [ "$fail" = 0 ]; then echo "RLS: all checks passed"; else echo "RLS: FAILURES"; fi
exit $fail
