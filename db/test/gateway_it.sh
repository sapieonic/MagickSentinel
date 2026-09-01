#!/usr/bin/env bash
# Run the gateway's integration tests against a throwaway migrated Postgres.
set -euo pipefail
cd "$(cd "$(dirname "$0")" && pwd)"
export PGTEST_ENTRY="$PWD/gateway_it.sh"

# shellcheck disable=SC1091
source ./pgtest.sh

psql -h "$PGHOST" -p "$PGPORT" -d "$PGDATABASE" -qtAX -c \
  "ALTER ROLE sentinel_app WITH PASSWORD 'sentinel_test'" >/dev/null

# The gateway connects as sentinel_app precisely because that role cannot bypass RLS.
export SENTINEL_TEST_DATABASE_URL="postgres://sentinel_app@/${PGDATABASE}?host=${PGHOST}&port=${PGPORT}&sslmode=disable"
# Seeding and read-back use the schema owner: TRUNCATE needs ownership, and several
# assertions deliberately read rows the application role must not be able to see.
export SENTINEL_TEST_ADMIN_DATABASE_URL="postgres://${PGUSER}@/${PGDATABASE}?host=${PGHOST}&port=${PGPORT}&sslmode=disable"

cd ../../server/gateway
exec go test "$@" ./...
