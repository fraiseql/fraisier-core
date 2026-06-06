#!/usr/bin/env bash
#
# Database-ops checkpoint — the generic-Postgres lifecycle proof.
#
# `db migrate` is delegated to the migration adapter (the Confiture model); the
# lifecycle ops `backup` / `db restore` / `db reset` are generic Postgres
# (pg_dump / pg_restore / psql) and work against ANY Postgres regardless of the
# migration adapter. This drives all four against a real Postgres, end-to-end:
#
#   1. db migrate          → the command adapter creates `items` + a seed row
#   2. (seed two more rows directly, and an extra `junk` schema)
#   3. backup              → pg_dump -Fc to an archive
#   4. (DROP TABLE items)  → simulate data loss
#   5. db restore --yes    → pg_restore brings `items` (3 rows) back
#   6. db reset --yes      → drop ALL user schemas, then re-migrate (1 row, no junk)
#
# Plus the safety surface: restore/reset WITHOUT --yes only print a plan and
# leave the DB untouched, and `backup` refuses to clobber an existing archive.
#
# Postgres runs as a throwaway container (no host install) unless --db-dsn is
# given. fraisier shells out to pg_dump/pg_restore/psql on the HOST, so the
# Postgres CLIENT tools must be installed locally (and recent enough for the
# server: pg_dump refuses a newer server major).
#
# Usage:
#   scripts/checkpoint-db.sh [--db-dsn <url>]
#
#   --db-dsn   use an existing Postgres instead of the throwaway container. The
#              database must be empty (the run creates, drops, and resets it).
#
set -euo pipefail

PG_PORT="${PG_PORT:-55434}"
PG_NAME="fraisier-dbops-pg"
DB_DSN=""

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

while [ $# -gt 0 ]; do
  case "$1" in
    --db-dsn) DB_DSN="$2"; shift 2;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()  { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# --------------------------------------------------------------------------
# Preconditions — the host needs the Postgres client tools fraisier shells to.
# --------------------------------------------------------------------------
command -v cargo      >/dev/null || die "cargo not found"
command -v pg_dump    >/dev/null || die "pg_dump not on PATH (Postgres client tools required)"
command -v pg_restore >/dev/null || die "pg_restore not on PATH (Postgres client tools required)"
command -v psql       >/dev/null || die "psql not on PATH (Postgres client tools required)"

CONTAINER="${CONTAINER:-}"
if [ -z "$DB_DSN" ] && [ -z "$CONTAINER" ]; then
  if command -v docker >/dev/null; then CONTAINER=docker
  elif command -v podman >/dev/null; then CONTAINER=podman
  else die "need docker or podman for the throwaway Postgres (or pass --db-dsn)"; fi
fi

# --------------------------------------------------------------------------
# Build fraisier
# --------------------------------------------------------------------------
say "building fraisier"
( cd "$REPO_ROOT" && cargo build --bin fraisier )
FRAISIER="$REPO_ROOT/target/debug/fraisier"
[ -x "$FRAISIER" ] || die "fraisier binary missing after build"

# --------------------------------------------------------------------------
# Workspace + teardown
# --------------------------------------------------------------------------
WORK="$(mktemp -d)"

# shellcheck disable=SC2329  # invoked indirectly via `trap cleanup EXIT`
cleanup() {
  set +e
  say "tearing down"
  [ -n "$CONTAINER" ] && $CONTAINER rm -f "$PG_NAME" >/dev/null 2>&1
  rm -rf "$WORK"
}
trap cleanup EXIT

# --------------------------------------------------------------------------
# Postgres (throwaway container unless --db-dsn given)
# --------------------------------------------------------------------------
if [ -n "$DB_DSN" ]; then
  DSN="$DB_DSN"
  say "using supplied Postgres DSN"
else
  [ -z "$CONTAINER" ] || $CONTAINER rm -f "$PG_NAME" >/dev/null 2>&1 || true
  port_free() { ! { ss -ltn 2>/dev/null || netstat -ltn 2>/dev/null; } | grep -q ":$1 "; }
  port_free "$PG_PORT" || die "port $PG_PORT is in use (a leftover run?). Free it and retry."
  say "starting Postgres ($PG_NAME) on :$PG_PORT via $CONTAINER"
  $CONTAINER run -d --name "$PG_NAME" \
    -e POSTGRES_PASSWORD=dbopspw -e POSTGRES_DB=dbopsdb \
    -p "$PG_PORT:5432" postgres:16 >/dev/null
  for _ in $(seq 1 30); do
    $CONTAINER exec "$PG_NAME" pg_isready -U postgres >/dev/null 2>&1 && break
    sleep 1
  done
  $CONTAINER exec "$PG_NAME" pg_isready -U postgres >/dev/null 2>&1 || die "Postgres did not come up"
  DSN="postgresql://postgres:dbopspw@127.0.0.1:$PG_PORT/dbopsdb?sslmode=disable"
  ok "Postgres ready (database dbopsdb)"
fi

# A scalar query against the target DB (test harness — uses the DSN directly).
q() { psql "$DSN" -tAqc "$1"; }

# --------------------------------------------------------------------------
# fraisier.toml — generic-Postgres db ops over the command migration adapter
# (so the script needs no confiture; the lifecycle ops are adapter-agnostic).
# --------------------------------------------------------------------------
UP_SQL="CREATE TABLE IF NOT EXISTS items(id serial primary key, name text); INSERT INTO items(name) SELECT 'seed' WHERE NOT EXISTS (SELECT 1 FROM items);"
cat > "$WORK/fraisier.toml" <<EOF
[deploy]
name = "dbops"
environment = "checkpoint"

[migration]
adapter = "command"
database_url_env = "DBOPS_DATABASE_URL"

[migration.settings.commands]
up = "psql \"\$DATABASE_URL\" -v ON_ERROR_STOP=1 -c \"$UP_SQL\""
current_revision = "echo v1"
EOF

fr() { DBOPS_DATABASE_URL="$DSN" "$FRAISIER" --json "$@" --config "$WORK/fraisier.toml"; }

# ==========================================================================
# Round-trip
# ==========================================================================
say "DB OPS — generic-Postgres lifecycle round-trip"

# --- 1. db migrate: the adapter creates `items` + a seed row ----------------
fr db migrate --state-dir "$WORK/state" >/dev/null || die "db migrate failed"
[ "$(q 'SELECT count(*) FROM items')" = "1" ] || die "db migrate did not create the seeded items table"
ok "db migrate: items created with 1 seed row"

# --- 2. seed more rows + an extra schema to prove restore/reset semantics ---
q "INSERT INTO items(name) VALUES ('alpha'), ('beta');" >/dev/null
q "CREATE SCHEMA junk; CREATE TABLE junk.t(x int);" >/dev/null
[ "$(q 'SELECT count(*) FROM items')" = "3" ] || die "seeding failed"
ok "seeded items to 3 rows and created an extra schema 'junk'"

# --- 3. backup --------------------------------------------------------------
ARCHIVE="$WORK/dbops.pgdump"
fr backup --output "$ARCHIVE" >/dev/null || die "backup failed"
[ -s "$ARCHIVE" ] || die "backup produced no archive"
ok "backup wrote $(wc -c < "$ARCHIVE") bytes to the archive"

# backup refuses to clobber the existing archive without --force.
if fr backup --output "$ARCHIVE" >/dev/null 2>&1; then
  die "backup clobbered an existing archive without --force"
fi
ok "backup refuses to overwrite an existing archive without --force"

# --- 4. simulate data loss --------------------------------------------------
q "DROP TABLE items;" >/dev/null
[ "$(q "SELECT to_regclass('public.items') IS NULL")" = "t" ] || die "items should be gone"
ok "dropped the items table (simulated data loss)"

# --- 5. db restore: plan first (no change), then --yes restores -------------
fr db restore --input "$ARCHIVE" >/dev/null || die "restore plan failed"
[ "$(q "SELECT to_regclass('public.items') IS NULL")" = "t" ] || die "restore plan must not change the DB"
ok "db restore (no --yes): plan only, DB untouched"

fr db restore --input "$ARCHIVE" --yes >/dev/null || die "restore failed"
[ "$(q 'SELECT count(*) FROM items')" = "3" ] || die "restore did not bring items back to 3 rows"
ok "db restore --yes: items restored to 3 rows"

# --- 6. db reset: plan first (no change), then --yes wipes + re-migrates -----
fr db reset --state-dir "$WORK/state" >/dev/null || die "reset plan failed"
[ "$(q 'SELECT count(*) FROM items')" = "3" ] || die "reset plan must not change the DB"
[ "$(q "SELECT count(*) FROM information_schema.schemata WHERE schema_name='junk'")" = "1" ] \
  || die "reset plan must not drop schemas"
ok "db reset (no --yes): plan only, DB untouched"

fr db reset --state-dir "$WORK/state" --yes >/dev/null || die "reset failed"
[ "$(q 'SELECT count(*) FROM items')" = "1" ] \
  || die "reset did not drop+re-migrate to the seeded baseline (1 row)"
[ "$(q "SELECT count(*) FROM information_schema.schemata WHERE schema_name='junk'")" = "0" ] \
  || die "reset did not drop the extra 'junk' schema"
ok "db reset --yes: all user schemas dropped, re-migrated to 1 seed row, 'junk' gone"

say "DB OPS CHECKPOINT PASSED — real Postgres, generic lifecycle:"
say "  migrate (adapter) → backup → restore → reset, with plan/clobber guards proven."
exit 0
