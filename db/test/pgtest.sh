#!/usr/bin/env bash
# Boot a throwaway PostgreSQL 16 cluster, apply every migration in order, and export
# PGHOST/PGPORT/PGDATABASE for the caller. Source this from a test script.
#
# Two accommodations for container images:
#
#   * initdb refuses to run as root, so when we are root we re-exec the *calling*
#     script as an unprivileged user -- $SUDO_USER if we got here through `sudo`,
#     otherwise `postgres`. See the probe below for why the choice is not free.
#   * pgvector is not in every dev image. When it is missing we install a stub
#     extension so the schema still applies; embedding columns then behave as opaque
#     text and vector-search tests skip. Production images MUST carry the real one.
set -euo pipefail

PGBIN=${PGBIN:-/usr/lib/postgresql/16/bin}
PGTEST_ENTRY=${PGTEST_ENTRY:-${BASH_SOURCE[1]:-$0}}

if [ "$(id -u)" = 0 ] && [ -z "${PGTEST_DROPPED_PRIV:-}" ]; then
  # Install the pgvector stub while we still have write access to the share dir.
  extdir=$("$PGBIN/pg_config" --sharedir)/extension
  if [ ! -f "$extdir/vector.control" ]; then
    echo "pgtest: pgvector missing, installing stub" >&2
    printf "comment = 'stub vector type for tests without pgvector'\ndefault_version = '0.0.0-stub'\nrelocatable = true\n" > "$extdir/vector.control"
    # A base type, not a domain: the schema declares vector(1024) and only a real
    # type accepts a type modifier.
    cat > "$extdir/vector--0.0.0-stub.sql" <<'SQL'
CREATE TYPE vector;
CREATE FUNCTION vector_in(cstring, oid, integer) RETURNS vector
  LANGUAGE internal IMMUTABLE STRICT AS 'textin';
CREATE FUNCTION vector_out(vector) RETURNS cstring
  LANGUAGE internal IMMUTABLE STRICT AS 'textout';
CREATE FUNCTION vector_modin(cstring[]) RETURNS integer
  LANGUAGE internal IMMUTABLE STRICT AS 'varchartypmodin';
CREATE FUNCTION vector_modout(integer) RETURNS cstring
  LANGUAGE internal IMMUTABLE STRICT AS 'varchartypmodout';
CREATE TYPE vector (
  INPUT = vector_in, OUTPUT = vector_out,
  TYPMOD_IN = vector_modin, TYPMOD_OUT = vector_modout,
  INTERNALLENGTH = VARIABLE, STORAGE = extended
);
SQL
  fi

  # Choosing who to become is not just "any non-root user": that account also has
  # to be able to *read* the checkout. $SUDO_USER is tried first because under
  # `sudo` it owns the checkout by construction. `postgres` is the fallback for a
  # plain root shell, and it is the one that bites: on a GitHub Actions runner
  # /home/runner is not world-traversable, so `bash /home/runner/work/.../x.sh`
  # dies with a bare "Permission denied" and exit 126 — no mention of which
  # directory refused, or that a uid change was involved at all. So probe for
  # readability before committing to a user, and say so plainly if none works.
  if [ -n "${PGTEST_USER:-}" ]; then
    candidates=("$PGTEST_USER")
  else
    candidates=(${SUDO_USER:+"$SUDO_USER"} postgres)
  fi

  target=
  for candidate in "${candidates[@]}"; do
    id -u "$candidate" >/dev/null 2>&1 || continue
    [ "$(id -u "$candidate")" != 0 ] || continue
    setpriv --reuid "$candidate" --regid "$(id -g "$candidate")" --init-groups \
      test -r "$PGTEST_ENTRY" 2>/dev/null || continue
    target=$candidate
    break
  done

  if [ -z "$target" ]; then
    echo "pgtest: no unprivileged user can read $PGTEST_ENTRY (tried: ${candidates[*]})" >&2
    exit 1
  fi

  workdir=$(mktemp -d)
  chown -R "$target" "$workdir"
  exec setpriv --reuid "$target" --regid "$(id -g "$target")" --init-groups \
    env PGTEST_DROPPED_PRIV=1 HOME="$workdir" PGDATA="$workdir/pgdata" \
        PGHOST="$workdir" PGTEST_ENTRY="$PGTEST_ENTRY" PATH="$PATH" \
    bash "$PGTEST_ENTRY"
fi

export PGDATA=${PGDATA:-$(mktemp -d)/pgdata}
export PGPORT=${PGPORT:-55432}
export PGHOST=${PGHOST:-$(dirname "$PGDATA")}
export PGDATABASE=${PGDATABASE:-sentinel_test}
export PGUSER=${PGUSER:-$(id -un)}

"$PGBIN/initdb" -D "$PGDATA" -U "$PGUSER" --auth=trust -E UTF8 >/dev/null
"$PGBIN/pg_ctl" -D "$PGDATA" -o "-p $PGPORT -k $PGHOST -c listen_addresses=''" -w start >/dev/null
trap '"$PGBIN/pg_ctl" -D "$PGDATA" -m immediate stop >/dev/null 2>&1 || true' EXIT

"$PGBIN/createdb" -h "$PGHOST" -p "$PGPORT" "$PGDATABASE"

migrations=$(cd "$(dirname "${BASH_SOURCE[0]}")/../migrations" && pwd)
for f in "$migrations"/*.up.sql; do
  psql -h "$PGHOST" -p "$PGPORT" -d "$PGDATABASE" -v ON_ERROR_STOP=1 -q -f "$f"
done
echo "pgtest: migrations applied to $PGDATABASE on $PGHOST:$PGPORT" >&2
