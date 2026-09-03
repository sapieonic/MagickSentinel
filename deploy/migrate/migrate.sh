#!/usr/bin/env bash
# Applies db/migrations/*.up.sql in order, once each, and then sets the login
# passwords the roles created by 0003 do not have.
#
# ==============================================================================
# WHY THIS EXISTS, GIVEN THAT db/test/pgtest.sh ALREADY APPLIES MIGRATIONS
#
# pgtest.sh boots a throwaway cluster, applies everything, and throws it away. That is
# exactly right for a test and exactly wrong for a deployment: it has no notion of a
# database that already has some migrations applied, it installs a *stub* pgvector when
# the real one is absent, and it re-applies from scratch every time.
#
# This runner is the deployment-side counterpart. It is idempotent, it records what it
# applied, and it refuses to touch a database in a state it does not understand.
#
# ==============================================================================
# THINGS IT DOES THAT ARE NOT OBVIOUS, AND WHY
#
# **It does not edit db/.** Another work stream owns db/migrations and is adding 0007
# and 0008. The runner discovers `*.up.sql` by glob and orders by the four-digit
# prefix, so new migrations are picked up with no change here. It validates the
# prefix format rather than trusting it: a file named `7_thing.up.sql` sorts after
# `0010_...` lexically, which is how a migration gets skipped or applied out of order,
# and both failures are silent.
#
# **The ledger table is `deploy_schema_migrations`, not `schema_migrations`.** Named
# out of db/'s way on purpose: a future migration is entitled to create a table called
# `schema_migrations`, and a collision between a migration and the thing that applies
# migrations is a bad afternoon. The `deploy_` prefix says which work stream owns it.
#
# **It records a SHA-256 per file and refuses if a previously applied file changed.**
# `.github/instructions/tenant-isolation.instructions.md` states the rule — "migrations
# are applied in filename order and are never edited once merged — add a new pair" —
# and this enforces it. An edited migration produces two databases with the same
# recorded version and different schemas, and nothing else in the system would ever
# notice.
#
# **It does NOT pass --single-transaction.** Every migration already wraps itself in
# `BEGIN; … COMMIT;` (the instructions file requires it). Adding an outer transaction
# means the file's own COMMIT commits psql's wrapper and the remaining statements run
# outside any transaction, so a failure half-way through leaves a partly applied
# migration that the ledger has not recorded. Each file is its own transaction, which
# is what the files were written for.
#
# **It needs the real pgvector.** 0001 does `CREATE EXTENSION vector` for the
# `vector(1024)` transcript embedding column, and stock postgres:16 does not carry it.
# pgtest.sh's stub exists so tests can run without pgvector; a deployment must use
# `pgvector/pgvector:pg16`. This runner checks and says so explicitly, because the
# failure otherwise arrives as an opaque "type vector does not exist" from inside 0001.
#
# **The sentinel_app / sentinel_pipeline passwords are set here, not in a migration.**
# 0003 creates both roles with LOGIN and no password, which is correct for a migration
# — a password committed to db/migrations would be a credential in git — and useless
# for a TCP-connecting deployment, which cannot authenticate without one. So the
# passwords come from the environment. They are never echoed, never passed on a command
# line, and never written into a SQL file.
# ==============================================================================
set -euo pipefail

MIGRATIONS_DIR=${MIGRATIONS_DIR:-/migrations}
LEDGER=${LEDGER_TABLE:-deploy_schema_migrations}

log() { printf 'migrate: %s\n' "$*" >&2; }
die() { printf 'migrate: ERROR: %s\n' "$*" >&2; exit 1; }

# ------------------------------------------------------------------ connection
# psql reads PGHOST/PGPORT/PGUSER/PGPASSWORD/PGDATABASE from the environment, so the
# runner takes no DSN of its own. One fewer place for a credential to be assembled by
# string concatenation, and it matches how ci.yml's go job talks to Postgres.
: "${PGHOST:?PGHOST is required}"
: "${PGDATABASE:?PGDATABASE is required}"
: "${PGUSER:?PGUSER is required (the schema owner, not sentinel_app)}"

# ON_ERROR_STOP=1 on every invocation. Without it psql reports a failed statement and
# carries on to the next one, so a broken migration produces a zero exit code and a
# half-built schema.
psql_q() { psql -v ON_ERROR_STOP=1 -q -X --no-psqlrc "$@"; }
psql_v() { psql -v ON_ERROR_STOP=1 -t -A -X --no-psqlrc "$@"; }

log "waiting for ${PGHOST}:${PGPORT:-5432}/${PGDATABASE} as ${PGUSER}"
# A bounded wait rather than an unbounded one: compose's `depends_on: service_healthy`
# should already have handled ordering, so if this loop ever runs long, something is
# wrong and hanging forever hides it.
for attempt in $(seq 1 60); do
  if pg_isready -q; then break; fi
  if [ "$attempt" -eq 60 ]; then die "database did not become ready within 60 attempts"; fi
  sleep 1
done
log "database is accepting connections"

# ---------------------------------------------------------------- pgvector check
# Two checks, and the second is the one that matters.
#
# First, availability. 0001 does `CREATE EXTENSION IF NOT EXISTS vector` for the
# transcript embedding column, and stock postgres:16 does not carry pgvector, so
# without this the failure arrives as an opaque "type vector does not exist" from
# inside a migration.
if [ "$(psql_v -c "SELECT count(*) FROM pg_available_extensions WHERE name = 'vector'")" = "0" ]; then
  die "$(cat <<'MSG'
This PostgreSQL server does not have pgvector available.

db/migrations/0001_init.up.sql does CREATE EXTENSION IF NOT EXISTS vector for the
transcript embedding column (vector(1024)), and the stock postgres:16 image does not
carry it. Use pgvector/pgvector:pg16.
MSG
)"
fi

# Second, and this is the check availability alone cannot make: is it the REAL
# pgvector, or the stub?
#
# db/test/pgtest.sh installs a stub `vector` extension when pgvector is absent, so the
# RLS tests can run in a dev image without it. The stub defines a base type backed by
# textin/textout and nothing else. It appears in pg_available_extensions exactly like
# the real thing, so counting rows there does not distinguish them — which is why this
# probes for capability instead.
#
# `<->` (L2 distance) is the operator the stub does not have. If it is missing, the
# schema still applies, embedding columns behave as opaque text, no vector index is
# built, and similarity search returns nothing while reporting no error at all. That is
# the shape of failure this product cannot afford, so it is a hard stop rather than a
# warning.
psql_q -c "CREATE EXTENSION IF NOT EXISTS vector"
if ! psql_q -c "SELECT ('[1,2,3]'::vector(3)) <-> ('[3,2,1]'::vector(3))" >/dev/null 2>&1; then
  die "$(cat <<'MSG'
The `vector` extension is present but does not implement the <-> distance operator,
which means this is db/test/pgtest.sh's STUB extension, not pgvector.

The stub exists so the row-level-security tests can run in a dev image without
pgvector; pgtest.sh says so in its own header, and it says production images must
carry the real one. It must never be used for a deployment:

  * embedding columns become opaque text,
  * no vector index is created,
  * similarity search returns nothing,
  * and none of that raises an error.

Use pgvector/pgvector:pg16, or install pgvector on this server.
MSG
)"
fi
log "pgvector is available and functional (<-> resolves)"

# ------------------------------------------------------------------- the ledger
# `IF NOT EXISTS` so the runner is safe to run against an already-initialised database,
# which is the normal case. Not tenant-scoped and deliberately not row-level-security
# enabled: it holds no tenant data, and it is read and written by the schema owner
# rather than by sentinel_app.
psql_q <<SQL
CREATE TABLE IF NOT EXISTS ${LEDGER} (
  version     text        PRIMARY KEY,
  filename    text        NOT NULL,
  sha256      text        NOT NULL,
  applied_at  timestamptz NOT NULL DEFAULT now(),
  applied_by  text        NOT NULL DEFAULT current_user
);
COMMENT ON TABLE ${LEDGER} IS
  'Applied db/migrations/*.up.sql, recorded by deploy/migrate/migrate.sh. Owned by the deployment work stream; db/migrations must not reference it.';
SQL

# -------------------------------------------------------------- discover the set
shopt -s nullglob
ups=("$MIGRATIONS_DIR"/*.up.sql)
shopt -u nullglob
[ "${#ups[@]}" -gt 0 ] || die "no *.up.sql files found in $MIGRATIONS_DIR"

# Sort by filename. The four-digit zero-padded prefix makes a lexical sort identical to
# a numeric one, which is why the format is validated below rather than assumed: the
# moment one file is named `7_` or `10_`, lexical and numeric order diverge and a
# migration runs before its dependency.
#
# `mapfile -d ''` over a NUL-delimited sort rather than word-splitting on newlines: a
# filename containing a newline would otherwise be split into two entries, and the
# second would be applied as a path that does not exist. Unlikely in db/migrations,
# and the failure would be confusing enough to be worth six characters.
mapfile -d '' -t ups < <(printf '%s\0' "${ups[@]}" | LC_ALL=C sort -z)

for up in "${ups[@]}"; do
  base=$(basename "$up")
  if ! [[ "$base" =~ ^([0-9]{4})_[A-Za-z0-9_]+\.up\.sql$ ]]; then
    die "$base does not match NNNN_name.up.sql. Four zero-padded digits, because that is what makes filename order equal numeric order."
  fi
  down="${up%.up.sql}.down.sql"
  if [ ! -f "$down" ]; then
    # Not fatal here — the runner never rolls back, and refusing to deploy over a
    # missing down migration would block a deployment for a problem that belongs in
    # code review. db/test/migrations_test.sh is the gate for that.
    log "WARNING: $base has no matching .down.sql; a down migration nobody has is not a rollback plan"
  fi
done

# ------------------------------------------------------------------ apply, in order
applied=0
skipped=0

for up in "${ups[@]}"; do
  base=$(basename "$up")
  version="${base%%_*}"
  sha=$(sha256sum "$up" | cut -d' ' -f1)

  recorded=$(psql_v -c "SELECT sha256 FROM ${LEDGER} WHERE version = '${version}'")

  if [ -n "$recorded" ]; then
    if [ "$recorded" != "$sha" ]; then
      die "$(cat <<MSG
${base} has already been applied, and its contents have changed since.

  recorded sha256  ${recorded}
  current  sha256  ${sha}

Migrations are never edited once merged (see
.github/instructions/tenant-isolation.instructions.md) -- add a new NNNN pair instead.
Re-applying an edited migration is not possible and pretending it was applied is
worse: two databases would carry the same recorded version and different schemas, and
nothing downstream would ever notice.

If this file genuinely needs to change and has not yet reached any deployment you care
about, delete its row from ${LEDGER} and reset that database deliberately.
MSG
)"
    fi
    skipped=$((skipped + 1))
    continue
  fi

  log "applying $base"
  # No --single-transaction: see the header. Each file carries its own BEGIN/COMMIT.
  psql_q -f "$up"

  # The ledger row goes in after the migration commits, in its own statement, and is
  # deliberately NOT inside the migration's transaction -- it cannot be, because the
  # migration commits itself. The consequence is a narrow window in which a migration
  # is applied and unrecorded; the next run would try to re-apply it and fail on, say,
  # a duplicate CREATE TABLE. That is the right failure: loud, at the start of a
  # deployment, on a database a human can inspect. The alternative -- recording first
  # -- would skip a migration that never actually ran.
  psql_q -c "INSERT INTO ${LEDGER} (version, filename, sha256) VALUES ('${version}', '${base}', '${sha}')"
  applied=$((applied + 1))
done

log "migrations: ${applied} applied, ${skipped} already present"

# ------------------------------------------------------- role login passwords
# 0003_roles.up.sql creates sentinel_app and sentinel_pipeline with LOGIN and no
# password. That is correct for a file in git and insufficient for anything that
# connects over TCP, which is every deployment: PostgreSQL's scram-sha-256
# authentication has nothing to verify against for a passwordless role, so the
# connection is refused. ci.yml documents the same wrinkle and works around it with an
# inline ALTER ROLE.
#
# The password is taken from the environment and passed as a psql *variable*, quoted by
# psql with :'name' rather than interpolated into the SQL text by the shell. That
# matters: a password containing a single quote would otherwise either break the
# statement or, worse, terminate the string early and change what the statement does.
#
# Note the SQL arrives on stdin rather than through `-c`. psql performs variable
# interpolation only on input it reads and lexes -- a file or stdin -- and not on a
# `--command` string, where `:'pw'` reaches the server verbatim and fails with a syntax
# error at the colon. This was found by running it.
set_role_password() {
  local role="$1" value="$2" label="$3"
  if [ -z "$value" ]; then
    log "WARNING: ${label} is unset, so ${role} keeps its passwordless state and cannot log in over TCP."
    return 0
  fi
  # ALTER ROLE ... PASSWORD is redacted from the statement log by PostgreSQL, but
  # `log_statement = 'all'` on an older configuration can still capture it. Worth
  # knowing rather than worth working around; a deployment that logs all statements has
  # a bigger problem, since those statements carry account references.
  #
  # The role name is interpolated by the shell because an identifier cannot be a psql
  # variable in this position; it is a literal from this script, never from input.
  printf 'ALTER ROLE %s WITH PASSWORD :%s;\n' "$role" "'pw'" \
    | psql_q -v pw="$value" -f -
  log "set a login password for ${role} (from ${label}; not echoed)"
}

set_role_password sentinel_app      "${SENTINEL_APP_PASSWORD:-}"      SENTINEL_APP_PASSWORD
set_role_password sentinel_pipeline "${SENTINEL_PIPELINE_PASSWORD:-}" SENTINEL_PIPELINE_PASSWORD

# ---------------------------------------------------------------- post-conditions
# Assert the properties the whole tenant-isolation design rests on, rather than
# assuming the migrations produced them. Cheap, and each one has a failure mode that is
# invisible from the application: a BYPASSRLS application role returns every tenant's
# rows for every query and nothing anywhere reports an error.
for role in sentinel_app sentinel_pipeline; do
  bypass=$(psql_v -c "SELECT rolbypassrls FROM pg_roles WHERE rolname = '${role}'")
  [ -n "$bypass" ] || die "role ${role} does not exist after migrating; 0003_roles.up.sql did not run"
  [ "$bypass" = "f" ] || die "role ${role} has BYPASSRLS. Tenant isolation in this product lives in the database, not the application. Refusing to certify this database."
done
log "sentinel_app and sentinel_pipeline exist and are NOBYPASSRLS"

unforced=$(psql_v <<'SQL'
SELECT count(*)
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'public'
  AND c.relkind = 'r'
  AND c.relrowsecurity
  AND NOT c.relforcerowsecurity;
SQL
)
if [ "${unforced:-0}" != "0" ]; then
  # RLS without FORCE exempts the table owner, and the owner is who the migrations run
  # as. A table in that state looks protected in `\d` and is not protected against the
  # one role most likely to be used for an ad-hoc query.
  log "WARNING: ${unforced} table(s) have ROW LEVEL SECURITY enabled but not FORCED. FORCE is what makes a policy apply to the table owner too."
fi

log "done"
