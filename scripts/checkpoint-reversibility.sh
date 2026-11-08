#!/usr/bin/env bash
#
# Local real-PostgreSQL checkpoint: `db migrate` does not apply a migration no
# `down` can undo unless `[migration].allow_irreversible = true` says so.
#
# A confiture project holds a SQL migration with a `.down.sql` and a `.py`
# migration whose `down()` is `pass`. Rolling back past the second would rewind
# confiture's ledger and leave its table, so:
#
#   1. `db migrate` refuses, names the migration, and applies nothing;
#   2. with `allow_irreversible = true` it applies both;
#   3. a project whose every migration has a down applies without the key;
#   4. (with --old-confiture) a confiture older than 1.40.0, which cannot tell,
#      is refused with that reason.
#
# Uses a server the developer already has (no container, no host); the
# databases it creates are named `fraisier_t_<pid>_rev*` and dropped again.
#
# Usage:
#   scripts/checkpoint-reversibility.sh [--super-dsn <url>] [--fraisier <binary>]
#                                       [--confiture <binary>]
#                                       [--old-confiture <binary>]
#
#   --super-dsn      a superuser over the default Unix socket, used to create and
#                    drop the databases (default: the current user)
#   --fraisier       run this binary instead of building one (to compare releases)
#   --confiture      confiture 1.40.0 or later (default: `confiture` on PATH)
#   --old-confiture  a confiture older than 1.40.0, for case 4 (skipped if unset)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SUPER_DSN="${FRAISIER_TEST_PG_SUPER_DSN:-postgres://${USER}@%2Frun%2Fpostgresql/postgres}"
FRAISIER=""
CONFITURE="confiture"
OLD_CONFITURE=""

while [ $# -gt 0 ]; do
  case "$1" in
    --super-dsn) SUPER_DSN="$2"; shift 2;;
    --fraisier) FRAISIER="$2"; shift 2;;
    --confiture) CONFITURE="$2"; shift 2;;
    --old-confiture) OLD_CONFITURE="$2"; shift 2;;
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
PREFIX="fraisier_t_$$_rev"
DATABASES=()
super_psql() { psql -X -q -v ON_ERROR_STOP=1 -tA "$@"; }
# shellcheck disable=SC2329  # invoked indirectly via `trap cleanup EXIT`
cleanup() {
  for db in "${DATABASES[@]}"; do
    super_psql -d "$SUPER_DSN" -c "DROP DATABASE IF EXISTS \"$db\" WITH (FORCE)" >/dev/null 2>&1
  done
  rm -rf "$WORK"
}
trap cleanup EXIT

# A fresh database and a project in $WORK/<name>; the last argument says
# whether the `.py` migration with an empty down is part of it.
project() {
  local name="$1" with_empty_down="$2" extra="${3:-}"
  local db="${PREFIX}_$name" dir="$WORK/$name"
  DATABASES+=("$db")
  super_psql -d "$SUPER_DSN" -c "CREATE DATABASE \"$db\""
  mkdir -p "$dir/db/migrations"
  echo "CREATE TABLE base (id int);" >"$dir/db/migrations/20261107000001_base.up.sql"
  echo "DROP TABLE base;" >"$dir/db/migrations/20261107000001_base.down.sql"
  if [ "$with_empty_down" = yes ]; then
    cat >"$dir/db/migrations/20261107000002_add_widgets.py" <<'EOF'
from confiture.models.migration import Migration


class AddWidgets(Migration):
    version = "20261107000002"
    name = "add_widgets"

    def up(self) -> None:
        self.execute("CREATE TABLE widgets (id int)")

    def down(self) -> None:
        pass
EOF
  fi
  cat >"$dir/fraisier.toml" <<EOF
[deploy]
name = "rev"
environment = "test"

[migration]
adapter = "confiture"
database_url_env = "FRAISIER_T_REV_DSN"
migrations_path = "$dir/db/migrations"
$extra
EOF
}

# Run `db migrate` for project $1 with confiture $2; the exit code is returned
# and the output left in $WORK/out.
migrate() {
  local name="$1" confiture="$2"
  (cd "$WORK/$name" && env FRAISIER_T_REV_DSN="postgresql:///${PREFIX}_$name" \
    FRAISIER_CONFITURE_BIN="$confiture" \
    "$FRAISIER" db migrate --config fraisier.toml --state-dir state --json) \
    >"$WORK/out" 2>&1
}
tables() {
  super_psql -d "${SUPER_DSN%/*}/${PREFIX}_$1" -c \
    "SELECT (to_regclass('public.base') IS NOT NULL)::text || '|' || (to_regclass('public.widgets') IS NOT NULL)::text"
}

say "fraisier ($FRAISIER) with confiture $("$CONFITURE" --version 2>&1 | head -1)"

project refused yes
code=0; migrate refused "$CONFITURE" || code=$?
seen="$(tables refused)"
if [ "$code" -ne 0 ] && grep -q '20261107000002' "$WORK/out" \
  && grep -q 'allow_irreversible = true' "$WORK/out" && [ "$seen" = "false|false" ]; then
  pass "an empty down() is refused by name and nothing is applied"
else
  fail "refusal: exit $code, tables '$seen': $(head -c 600 "$WORK/out")"
fi

project allowed yes "allow_irreversible = true"
code=0; migrate allowed "$CONFITURE" || code=$?
seen="$(tables allowed)"
if [ "$code" -eq 0 ] && [ "$seen" = "true|true" ]; then
  pass "allow_irreversible = true applies it"
else
  fail "allowed: exit $code, tables '$seen': $(head -c 600 "$WORK/out")"
fi

project reversible no
code=0; migrate reversible "$CONFITURE" || code=$?
seen="$(tables reversible)"
if [ "$code" -eq 0 ] && [ "$seen" = "true|false" ]; then
  pass "a project whose every migration has a down applies without the key"
else
  fail "reversible: exit $code, tables '$seen': $(head -c 600 "$WORK/out")"
fi

if [ -n "$OLD_CONFITURE" ]; then
  project old yes
  code=0; migrate old "$OLD_CONFITURE" || code=$?
  seen="$(tables old)"
  if [ "$code" -ne 0 ] && grep -q 'older than 1.40.0' "$WORK/out" && [ "$seen" = "false|false" ]; then
    pass "a confiture that cannot tell is refused with its reason"
  else
    fail "old confiture: exit $code, tables '$seen': $(head -c 600 "$WORK/out")"
  fi
fi

echo
echo "checkpoint-reversibility: $PASSED/$((PASSED + FAILED)) passed"
[ "$FAILED" -eq 0 ]
