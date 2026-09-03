#!/usr/bin/env bash
# Writes deploy/.env with random credentials, for a local stack.
#
# WHY THIS EXISTS. deploy/compose.yaml references every credential as `${VAR:?...}`
# with no default, on purpose: a working dev default is how `postgres/postgres` reaches
# a host that is not a laptop. The cost of that decision is friction on first run, and
# the answer to friction is one command rather than an argument — so this generates
# values instead of the repository shipping them.
#
# WHAT IT DELIBERATELY DOES NOT DO.
#
#   * It does not overwrite an existing .env. A stack whose Postgres volume was
#     initialised with one password and whose .env now holds another is a stack that
#     fails to authenticate with a message about the password being wrong, and the fix
#     — `down -v` — discards the database. Pass --force if that is what you want.
#   * It does not generate anything for production. These are 32 random characters from
#     /dev/urandom, which is fine for a laptop and is not a secret-management strategy.
#     Real deployments take credentials from whatever the customer's secret store is;
#     the reason compose reads them from the environment rather than from a file in the
#     repository is precisely so that substitution is possible without editing anything.
#   * It does not touch the API keys. SENTINEL_GOOGLE_API_KEY and
#     SENTINEL_SARVAM_API_KEY are real third-party credentials with real spend attached
#     and cannot be invented; they are left empty for you to fill in.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
target="$here/.env"
example="$here/.env.example"
force=0

for arg in "$@"; do
  case "$arg" in
    --force) force=1 ;;
    -h|--help)
      sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

if [ -f "$target" ] && [ "$force" -eq 0 ]; then
  cat >&2 <<'MSG'
deploy/.env already exists; refusing to overwrite it.

Rotating these values against a stack that is already running does not work: the
Postgres volume was initialised with the old POSTGRES_PASSWORD and keeps it, so the new
one just fails to authenticate. The only way through is `docker compose -f
deploy/compose.yaml down -v`, which discards the database.

Pass --force if that is genuinely what you want.
MSG
  exit 1
fi

[ -f "$example" ] || { echo "deploy/.env.example is missing" >&2; exit 1; }

# base64 of 24 random bytes: 32 characters, no shell metacharacters, no quoting
# problems in a compose env file, and no `=` padding to be mistaken for a separator.
# `tr -d` removes the base64 characters that need escaping in a URL, because several of
# these values end up inside a postgres:// or nats:// DSN in compose.yaml, where a `/`
# or `+` in a password changes where the host part starts.
rand() { LC_ALL=C tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 32; }

umask 077   # the file holds credentials; it should not be group- or world-readable

# Start from the example so every comment in it survives into the generated file. The
# comments are where the reasoning lives — why both roles are NOBYPASSRLS, why the
# three NATS credentials are separate, what the ap-south-1 default means for OPEN-4 —
# and a generated .env stripped of them is a file nobody can reason about later.
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
cp "$example" "$tmp"

replace() {
  local key="$1" value="$2"
  # The value is written with a shell here-doc rather than through sed's replacement
  # text, so a generated character that means something to sed cannot corrupt the file.
  # `rand` already excludes those, and relying on that would be relying on a detail of
  # a different function.
  awk -v k="$key" -v v="$value" -F= '
    $1 == k { printf "%s=%s\n", k, v; next }
    { print }
  ' "$tmp" > "$tmp.new" && mv "$tmp.new" "$tmp"
}

for key in \
  POSTGRES_PASSWORD \
  SENTINEL_APP_PASSWORD \
  SENTINEL_PIPELINE_PASSWORD \
  NATS_GATEWAY_PASSWORD \
  NATS_PIPELINE_PASSWORD \
  NATS_SYS_PASSWORD \
  MINIO_ROOT_PASSWORD \
  SENTINEL_S3_SECRET_KEY \
  GRAFANA_ADMIN_PASSWORD
do
  replace "$key" "$(rand)"
done

mv "$tmp" "$target"
chmod 600 "$target"
trap - EXIT

cat <<MSG
Wrote $target with random credentials (mode 600).

Still empty, and not inventable:
  SENTINEL_GOOGLE_API_KEY   needed if SENTINEL_ASR_PROVIDER resolves to the default
  SENTINEL_SARVAM_API_KEY   needed for the India-hosted ASR exit (see OPEN-4)

Next:
  docker compose -f deploy/compose.yaml up -d
  docker compose -f deploy/compose.yaml run --rm migrate
  docker compose -f deploy/compose.yaml logs -f pipeline   # the selftest verdict
MSG
