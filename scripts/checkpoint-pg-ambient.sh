#!/usr/bin/env bash
#
# Local real-PostgreSQL checkpoint: an ambient `PG*` variable does not fill what
# a database URL leaves unset.
#
# With a hostile environment (`PGPORT=1`, `PGHOSTADDR=192.0.2.1`, `PGUSER=nobody`,
# `PGDATABASE=postgres`, `PGOPTIONS='-c search_path=nowhere'`,
# `PGSERVICE=nonexistent`), the built `fraisier` runs `db migrate` (confiture),
# `backup`, `db restore --yes` and `db reset --yes` against a socket URL that
# names only the database. Each must reach that database, and the probe
# migration records the `search_path` the session saw. `PGSSLMODE=verify-full`
# is set too: a socket URL is exempt from the TLS policy check.
#
# Then over TCP (`localhost`): with that `PGSSLMODE` and a URL that does not set
# `sslmode`, `db reset --yes` is refused, naming `?sslmode=`, and drops nothing;
# with `?sslmode=disable` in the URL, `backup` runs.
#
# Uses a server the developer already has (no container, no host); the database
# it creates is named `fraisier_t_<pid>_ambient` and dropped again.
#
# Usage:
#   scripts/checkpoint-pg-ambient.sh [--super-dsn <url>] [--fraisier <binary>]
#                                    [--confiture <binary>]
#
#   --super-dsn  a superuser over the default Unix socket, used to create and drop
#                the database (default: the current user)
#   --fraisier   run this binary instead of building one (to compare releases)
#   --confiture  the confiture binary (default: `confiture` on PATH)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SUPER_DSN="${FRAISIER_TEST_PG_SUPER_DSN:-postgres://${USER}@%2Frun%2Fpostgresql/postgres}"
FRAISIER=""
CONFITURE="confiture"

while [ $# -gt 0 ]; do
  case "$1" in
    --super-dsn) SUPER_DSN="$2"; shift 2;;
    --fraisier) FRAISIER="$2"; shift 2;;
    --confiture) CONFITURE="$2"; shift 2;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
pass() { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; PASSED=$((PASSED + 1)); }
fail() { printf '\033[1;31m  ✗\033[0m %s\n' "$*"; FAILED=$((FAILED + 1)); }
PASSED=0
FAILED=0

command -v psql >/dev/null || { echo "psql not on PATH" >&2; exit 1; }
command -v "$CONFITURE" >/dev/null || { echo "confiture not found: $CONFITURE" >&2; exit 1; }
if [ -z "$FRAISIER" ]; then
  say "building fraisier"
  (cd "$REPO_ROOT" && cargo build -q -p fraisier)
  FRAISIER="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/debug/fraisier"
fi

WORK="$(mktemp -d)"
DB="fraisier_t_$$_ambient"
# The superuser's connection is used by this script alone, never under the
# hostile environment.
super_psql() { env -u PGOPTIONS psql -X -q -v ON_ERROR_STOP=1 -tA "$@"; }
# shellcheck disable=SC2329  # invoked indirectly via `trap cleanup EXIT`
cleanup() {
  super_psql -d "$SUPER_DSN" -c "DROP DATABASE IF EXISTS \"$DB\" WITH (FORCE)" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

super_psql -d "$SUPER_DSN" -c "CREATE DATABASE \"$DB\""
probe() { super_psql -d "${SUPER_DSN%/*}/$DB" -c "$1" 2>/dev/null || echo "unreadable"; }

mkdir -p "$WORK/db/migrations"
cat >"$WORK/db/migrations/20261107000000_probe.up.sql" <<'EOF'
CREATE TABLE probe AS
  SELECT current_database() AS db, current_setting('search_path') AS search_path;
EOF
echo "DROP TABLE probe;" >"$WORK/db/migrations/20261107000000_probe.down.sql"
cat >"$WORK/fraisier.toml" <<EOF
[deploy]
name = "ambient"
environment = "test"

[migration]
adapter = "confiture"
database_url_env = "FRAISIER_T_AMBIENT_DSN"
migrations_path = "$WORK/db/migrations"
EOF

# The URL names only the database: host, port and user are libpq's defaults
# (the Unix socket, the OS user) unless something fills them.
DSN="postgresql:///$DB"
hostile() {
  env FRAISIER_T_AMBIENT_DSN="$DSN" FRAISIER_CONFITURE_BIN="$CONFITURE" \
    PGPORT=1 PGHOSTADDR=192.0.2.1 PGUSER=nobody PGDATABASE=postgres \
    PGOPTIONS='-c search_path=nowhere' PGSERVICE=nonexistent PGCONNECT_TIMEOUT=3 \
    PGSSLMODE=verify-full \
    "$FRAISIER" "$@" --config "$WORK/fraisier.toml" >"$WORK/out" 2>&1
}
step() {
  local name="$1" check="$2" expected="$3"
  shift 3
  local code=0 seen
  hostile "$@" || code=$?
  seen="$(probe "$check")"
  if [ "$code" -eq 0 ] && [ "$seen" = "$expected" ]; then
    pass "$name reached $DB"
  else
    fail "$name: exit $code, saw '$seen': $(head -c 400 "$WORK/out")"
  fi
}

say "fraisier with a hostile ambient PG* environment ($FRAISIER)"
step "db migrate" "SELECT db || '|' || search_path FROM probe" "$DB|\"\$user\", public" \
  db migrate --state-dir "$WORK/state"
super_psql -d "${SUPER_DSN%/*}/$DB" -c "CREATE TABLE kept AS SELECT 1 AS id"
step "backup" "SELECT count(*) FROM kept" "1" backup --output "$WORK/archive.pgdump"
super_psql -d "${SUPER_DSN%/*}/$DB" -c "DROP TABLE kept"
step "db restore --yes" "SELECT count(*) FROM kept" "1" \
  db restore --input "$WORK/archive.pgdump" --yes
step "db reset --yes" \
  "SELECT (to_regclass('public.kept') IS NULL)::text || '|' || db FROM probe" "true|$DB" \
  db reset --state-dir "$WORK/state" --yes

say "over TCP, with an ambient PGSSLMODE=verify-full"
super_psql -d "${SUPER_DSN%/*}/$DB" -c "CREATE TABLE tls_kept AS SELECT 1 AS id"
DSN="postgresql://${USER}@localhost:5432/$DB"
code=0
hostile db reset --state-dir "$WORK/state" --yes || code=$?
seen="$(probe "SELECT count(*) FROM tls_kept")"
if [ "$code" -ne 0 ] && [ "$seen" = "1" ] && grep -q '?sslmode=' "$WORK/out"; then
  pass "a TCP URL without sslmode is refused (exit $code), naming ?sslmode=, and nothing dropped"
else
  fail "TCP without sslmode: exit $code, tls_kept=$seen: $(head -c 400 "$WORK/out")"
fi
DSN="postgresql://${USER}@localhost:5432/$DB?sslmode=disable"
rm -f "$WORK/tls.pgdump"
step "backup over TCP with ?sslmode= in the URL" "SELECT count(*) FROM tls_kept" "1" \
  backup --output "$WORK/tls.pgdump"

echo
echo "checkpoint-pg-ambient: $PASSED/$((PASSED + FAILED)) passed"
[ "$FAILED" -eq 0 ]
