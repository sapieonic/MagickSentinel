#!/usr/bin/env bash
# Run the pipeline's Postgres integration tests against a throwaway cluster.
#
# Boots PostgreSQL 16, applies every migration in db/migrations (including
# 0008_pipeline_jobs, which the pipeline's scheduled jobs need), and points the two
# test DSNs at it:
#
#   SENTINEL_PIPELINE_TEST_DATABASE_URL        connects as sentinel_pipeline, the
#                                              NOBYPASSRLS role the pipeline uses
#   SENTINEL_PIPELINE_TEST_ADMIN_DATABASE_URL  the schema owner, for seeding and for
#                                              read-backs of rows the pipeline role
#                                              must not be able to see
#
# Without those variables `python -m pytest` skips these tests, which is what keeps
# the default suite runnable with no database — the same arrangement the gateway's Go
# integration tests use.
#
#   bash tests/pg_integration.sh              # from server/pipeline
#   PYTHON=/path/to/python bash tests/pg_integration.sh
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
export PGTEST_ENTRY="$here/pg_integration.sh"

# shellcheck disable=SC1091
source "$here/../../../db/test/pgtest.sh"

export SENTINEL_PIPELINE_TEST_DATABASE_URL="postgresql://sentinel_pipeline@/${PGDATABASE}?host=${PGHOST}&port=${PGPORT}"
export SENTINEL_PIPELINE_TEST_ADMIN_DATABASE_URL="postgresql://${PGUSER}@/${PGDATABASE}?host=${PGHOST}&port=${PGPORT}"

cd "$here/.."
# The cache is disabled because pgtest.sh re-execs this script as an unprivileged
# user, which usually cannot write .pytest_cache in a checkout owned by someone else.
exec "${PYTHON:-python3}" -m pytest -q -p no:cacheprovider tests/test_postgres_integration.py "$@"
