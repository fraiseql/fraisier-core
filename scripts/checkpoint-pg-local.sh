#!/usr/bin/env bash
#
# Local real-PostgreSQL checkpoint: the database refusals that must hold against a
# real server, run against one the developer already has (no container, no host).
#
#   1. the ignored `fraisier-db` integration tests that need a superuser
#      (`FRAISIER_TEST_PG_SUPER_DSN`): a refresh that could not create its target
#      again is refused before the drop, and a failure after the drop names a
#      recovery that works;
#   2. `fraisier db reset --yes` with a migration tool that cannot run refuses
#      and leaves the database's table in place.
#
# The server's pg_hba.conf must trust local connections for any role: the tests
# connect as the roles they create. Everything created is named
# `fraisier_t_<pid>_*` and dropped again.
#
# Usage:
#   scripts/checkpoint-pg-local.sh [--super-dsn postgres://<user>@<host>/<db>]
#
# The default DSN is the current user over the default Unix socket.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SUPER_DSN="${FRAISIER_TEST_PG_SUPER_DSN:-postgres://${USER}@%2Frun%2Fpostgresql/postgres}"

while [ $# -gt 0 ]; do
  case "$1" in
    --super-dsn) SUPER_DSN="$2"; shift 2;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
pass() { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; PASSED=$((PASSED + 1)); }
fail() { printf '\033[1;31m  ✗\033[0m %s\n' "$*"; FAILED=$((FAILED + 1)); }
PASSED=0
FAILED=0

command -v psql >/dev/null || { echo "psql not on PATH" >&2; exit 1; }
cd "$REPO_ROOT"

# --------------------------------------------------------------------------
say "fraisier-db integration tests against ${SUPER_DSN%%@*}@…"
if FRAISIER_TEST_PG_SUPER_DSN="$SUPER_DSN" cargo test -q -p fraisier-db \
    --test refresh_create_privilege -- --ignored --test-threads=1; then
  pass "refresh_create_privilege (3 cases)"
else
  fail "refresh_create_privilege"
fi

# --------------------------------------------------------------------------
say "db reset with a migration tool that cannot run"
cargo build -q -p fraisier
FRAISIER="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/debug/fraisier"
WORK="$(mktemp -d)"
DB="fraisier_t_$$_reset"
SOCKET_DSN="${SUPER_DSN%/*}/$DB"
cleanup() {
  psql -X -q -d "${SUPER_DSN}" -c "DROP DATABASE IF EXISTS \"$DB\" WITH (FORCE)" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

super_psql() { psql -X -q -v ON_ERROR_STOP=1 -tA "$@"; }
super_psql -d "$SUPER_DSN" -c "CREATE DATABASE \"$DB\""
super_psql -d "$SOCKET_DSN" -c "CREATE TABLE kept (id int); INSERT INTO kept VALUES (1)"

cat >"$WORK/fraisier.toml" <<EOF
[deploy]
name = "demo"
environment = "test"

[migration]
adapter = "confiture"
database_url_env = "FRAISIER_T_RESET_DSN"
EOF
set +e
FRAISIER_T_RESET_DSN="$SOCKET_DSN" FRAISIER_CONFITURE_BIN=/nonexistent/confiture \
  "$FRAISIER" db reset --config "$WORK/fraisier.toml" --state-dir "$WORK/state" --yes \
  >"$WORK/out" 2>&1
code=$?
set -e
rows="$(super_psql -d "$SOCKET_DSN" -c "SELECT count(*) FROM kept" 2>/dev/null || echo gone)"
if [ "$code" -ne 0 ] && [ "$rows" = "1" ] && grep -q "Nothing was changed" "$WORK/out"; then
  pass "refused (exit $code) and the table is still there"
else
  fail "exit $code, rows=$rows: $(cat "$WORK/out")"
fi

# --------------------------------------------------------------------------
echo
echo "checkpoint-pg-local: $PASSED/$((PASSED + FAILED)) passed"
[ "$FAILED" -eq 0 ]
