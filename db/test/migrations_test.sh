#!/usr/bin/env bash
# Migrations must be reversible and re-appliable.
#
# A down migration nobody has ever run is not a rollback plan, it is a file. This
# applies every migration, rolls the whole set back, and applies it again — which is
# what a failed deploy actually does at three in the morning.
set -euo pipefail
cd "$(cd "$(dirname "$0")" && pwd)"
export PGTEST_ENTRY="$PWD/migrations_test.sh"

# shellcheck disable=SC1091
source ./pgtest.sh   # applies every *.up.sql once

Q() { psql -h "$PGHOST" -p "$PGPORT" -d "$PGDATABASE" -qtAX -v ON_ERROR_STOP=1 "$@"; }
fail=0
check() { if [ "$2" = "$3" ]; then printf 'ok   %s\n' "$1"
          else printf 'FAIL %s: want %q got %q\n' "$1" "$2" "$3"; fail=1; fi }

migrations="$PWD/../migrations"
ups=$(ls "$migrations"/*.up.sql | sort)
downs=$(ls "$migrations"/*.down.sql | sort -r)

# Every up must have a down. A missing one is only noticed during the rollback that
# needed it.
for up in $ups; do
  down="${up%.up.sql}.down.sql"
  [ -f "$down" ] || { printf 'FAIL %s has no down migration\n' "$(basename "$up")"; fail=1; }
done

before=$(Q -c "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'")
check "schema created tables" "true" "$([ "$before" -gt 10 ] && echo true || echo false)"

echo "--- rolling back ---"
for down in $downs; do
  Q -q -f "$down" || { printf 'FAIL %s did not apply\n' "$(basename "$down")"; fail=1; }
done

remaining=$(Q -c "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'")
check "rollback removed every table" "0" "$remaining"

leftover=$(Q -c "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
                  WHERE n.nspname = 'public' AND p.proname LIKE 'sentinel%'")
check "rollback removed the sentinel functions" "0" "$leftover"

echo "--- re-applying ---"
for up in $ups; do
  Q -q -f "$up" || { printf 'FAIL %s did not re-apply\n' "$(basename "$up")"; fail=1; }
done

after=$(Q -c "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'")
check "re-apply restored the schema" "$before" "$after"

policies=$(Q -c "SELECT count(*) FROM pg_policies WHERE schemaname='public'")
check "row-level security policies came back" "true" "$([ "$policies" -gt 10 ] && echo true || echo false)"

# Every tenant-scoped table must have RLS forced, not merely enabled: enabled alone
# exempts the table owner, and the owner is who the migrations run as.
unforced=$(Q -c "
  SELECT COALESCE(string_agg(c.relname, ', '), '')
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
   WHERE n.nspname = 'public' AND c.relkind = 'r'
     AND EXISTS (SELECT 1 FROM pg_attribute a
                  WHERE a.attrelid = c.oid AND a.attname = 'tenant_id' AND NOT a.attisdropped)
     AND NOT (c.relrowsecurity AND c.relforcerowsecurity)")
check "every tenant-scoped table forces RLS" "" "$unforced"

echo
if [ "$fail" = 0 ]; then echo "migrations: all checks passed"; else echo "migrations: FAILURES"; fi
exit $fail
