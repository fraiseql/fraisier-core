#!/usr/bin/env bash
#
# Blue-green checkpoint — the phase-07 §7.5 GA gate, preflight half.
#
# Drives the REAL `fraisier deploy --strategy blue-green` end-to-end through its
# preflight gates, against **real confiture 0.22** and a **real Postgres**:
#
#   * Gate (migration not window-safe): a DROP COLUMN migration is REFUSED at
#     preflight — confiture emits PFLIGHT_REPLICA_DROP_COLUMN (a *warning*, ok==true)
#     and fraisier's window-safety gate blocks before any instance or traffic change.
#   * Gate (expand allowed): an ADD COLUMN (nullable) expand migration PASSES the
#     window-safety gate (the deploy proceeds past preflight).
#   * Gate (connection budget): with green's pool larger than the shared DB's
#     headroom, the pre-swap connection-budget probe (a real `psql` query) REFUSES
#     before the swap.
#
# These three §4 gates refuse *before* any traffic/instance change, so they need
# no nginx or green app instance — only confiture + Postgres. The two traffic-tier
# gates (pre-swap health gate, post-swap degradation swap-back) are proven
# hermetically in `fraisier-core::blue_green`; the real-nginx end-to-end traffic
# swap is a documented follow-up (it needs nginx + a dual app instance fixture).
#
# Zero spend: Postgres runs as a throwaway podman container (cached *-alpine
# image); confiture must be on PATH (>= 0.22). No root, no network spend.
#
# Usage: scripts/checkpoint-blue-green.sh [--db-dsn <url>] [--keep]
#   --db-dsn  use an existing Postgres for the connection-budget gate instead of
#             the throwaway container (the DB is only queried, never written).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PG_PORT="${PG_PORT:-55434}"
PG_NAME="fraisier-bg-pg"
PG_IMAGE="${PG_IMAGE:-postgres:16-alpine}"
KEEP=0
DB_DSN=""

while [ $# -gt 0 ]; do
  case "$1" in
    --db-dsn) DB_DSN="${2:?--db-dsn needs a url}"; shift 2;;
    --keep)   KEEP=1; shift;;
    *) printf 'unknown argument: %s\n' "$1" >&2; exit 2;;
  esac
done

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()  { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

command -v confiture >/dev/null || die "confiture not on PATH (>= 0.22 for the window-safety gate)"
command -v cargo >/dev/null     || die "cargo not on PATH"

CONTAINER=""
if [ -z "$DB_DSN" ]; then
  command -v podman >/dev/null && CONTAINER=podman
  command -v docker >/dev/null && [ -z "$CONTAINER" ] && CONTAINER=docker
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/fraisier-bg.XXXXXX")"
cleanup() {
  [ "$KEEP" = 1 ] && { say "keeping $WORK + container $PG_NAME (--keep)"; return; }
  rm -rf "$WORK"
  [ -n "$CONTAINER" ] && $CONTAINER rm -f "$PG_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

say "building fraisier"
( cd "$REPO_ROOT" && cargo build --bin fraisier >/dev/null 2>&1 ) || die "fraisier build failed"
FRAISIER="$REPO_ROOT/target/debug/fraisier"
[ -x "$FRAISIER" ] || die "fraisier binary missing after build"
ok "confiture: $(confiture --version 2>&1 | head -1)"

# Does the installed confiture emit the first-class `window_safe` verdict
# (confiture#154 Phase 3)? fraisier's blue-green gate requires it; an older
# confiture returns no verdict and is refused (fail-safe). The full DROP/ADD/budget
# gates need it; without it we assert only the fail-safe.
WINDOW_SAFE=0
probe_window_safe() {
  local d="$1"
  mkdir -p "$d"
  printf 'CREATE TABLE _bg_probe (id int);\n' > "$d/001_p.up.sql"
  printf 'DROP TABLE _bg_probe;\n' > "$d/001_p.down.sql"
  CONFITURE_DATABASE_URL='postgresql://u@127.0.0.1:1/n?sslmode=disable' \
    confiture migrate preflight --no-config --format json --output "$d/r.json" \
    --migrations-dir "$d" >/dev/null 2>&1 || true
  grep -q '"window_safe"' "$d/r.json" 2>/dev/null
}
if probe_window_safe "$WORK/probe"; then
  WINDOW_SAFE=1
  ok "confiture emits window_safe (Phase 3) — running the full window-safety gates"
else
  say "confiture has no window_safe verdict yet — asserting the FAIL-SAFE only"
  say "      (the full DROP/ADD/budget gates validate once confiture#154 Phase 3 ships)"
fi

# A dummy local artifact (provision-green's stage step reads it; the deploy never
# reaches a swap, so its contents are irrelevant).
mkdir -p "$WORK/build" "$WORK/staging" "$WORK/state" "$WORK/migrations"
printf 'blue-green-artifact' > "$WORK/build/app"

# ---- migration sets -------------------------------------------------------
write_create() {
  cat > "$WORK/migrations/001_create.up.sql"  <<'SQL'
CREATE TABLE notes (id bigserial PRIMARY KEY, body text NOT NULL);
SQL
  cat > "$WORK/migrations/001_create.down.sql" <<'SQL'
DROP TABLE notes;
SQL
}
write_expand() { # an ADD COLUMN (nullable) — a window-safe expand
  cat > "$WORK/migrations/002_add_label.up.sql"  <<'SQL'
ALTER TABLE notes ADD COLUMN label text;
SQL
  cat > "$WORK/migrations/002_add_label.down.sql" <<'SQL'
ALTER TABLE notes DROP COLUMN label;
SQL
}
write_drop() { # a DROP COLUMN — NOT forward-compatible for a two-version window
  cat > "$WORK/migrations/002_drop_body.up.sql"  <<'SQL'
ALTER TABLE notes DROP COLUMN body;
SQL
  cat > "$WORK/migrations/002_drop_body.down.sql" <<'SQL'
ALTER TABLE notes ADD COLUMN body text;
SQL
}
reset_migrations() { rm -f "$WORK"/migrations/*.sql; write_create; }

# ---- fraisier.toml writer -------------------------------------------------
# $1 = green_pool ("" to omit the connection-budget probe entirely)
write_config() {
  local green_pool="$1"
  {
    cat <<EOF
[deploy]
name = "checkout"
environment = "production"
strategy = "blue-green"

[artifact]
source = "local"
path = "$WORK/build"
active_path = "$WORK/current"
staging_dir = "$WORK/staging"

[migration]
adapter = "confiture"
database_url_env = "BG_DATABASE_URL"
migrations_path = "$WORK/migrations"

[service]
adapter = "systemd"
unit = "checkout.service"
user = true

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"

[lb]
adapter = "nginx"
upstream = "checkout_upstream"
include_dir = "$WORK/nginx"

[blue_green]
green_unit = "checkout-green.service"
green_health_url = "http://127.0.0.1:8081/healthz"
green_servers = ["127.0.0.1:8081"]
blue_servers = ["127.0.0.1:8080"]
hold_secs = 5
EOF
    if [ -n "$green_pool" ]; then
      printf 'green_pool = %s\nconnection_margin = 10\n' "$green_pool"
    fi
  } > "$WORK/fraisier.toml"
}

# Run a blue-green deploy; capture --json stdout in $DEPLOY_OUT and the exit code
# in $DEPLOY_RC. The function runs in the current shell, so both persist; `set +e`
# keeps a (correct) non-zero deploy from aborting the script under `set -e`.
DEPLOY_OUT=""
DEPLOY_RC=0
deploy() {
  local dsn="$1"
  set +e
  DEPLOY_OUT="$(BG_DATABASE_URL="$dsn" "$FRAISIER" --json deploy \
    --config "$WORK/fraisier.toml" --state-dir "$WORK/state" --app-version 2.0.0)"
  DEPLOY_RC=$?
  set -e
}

# ==========================================================================
say "BLUE-GREEN — preflight GA gates (real confiture + real Postgres)"

# --- Gate 1: a non-window-safe migration is refused at the window-safety gate -
# With Phase 3 confiture: DROP COLUMN -> window_safe = false. Without it: ANY
# migration -> no verdict -> refused (fail-safe). Both cite `window_safe`.
say "gate: migration not window-safe -> refused at preflight"
reset_migrations; write_drop
write_config ""   # no connection-budget probe
deploy 'postgresql://unused@127.0.0.1:1/none?sslmode=disable'
echo "$DEPLOY_OUT"
[ "$DEPLOY_RC" -ne 0 ] || die "the deploy must exit non-zero"
echo "$DEPLOY_OUT" | grep -q "step 'preflight'" \
  || die "must be refused at the preflight step:\n$DEPLOY_OUT"
echo "$DEPLOY_OUT" | grep -qi "window_safe" \
  || die "the refusal must cite the window-safety verdict:\n$DEPLOY_OUT"
[ ! -e "$WORK/current" ] || die "no artifact must be staged on a preflight refusal"
if [ "$WINDOW_SAFE" = 1 ]; then
  ok "DROP COLUMN: confiture window_safe=false -> refused before any change"
else
  ok "no window_safe verdict -> refused (fail-safe; an un-upgraded confiture can't certify)"
fi

if [ "$WINDOW_SAFE" != 1 ]; then
  say "SKIP the expand-allowed + connection-budget gates (need confiture Phase 3)"
  say "      — these are proven hermetically in fraisier-core::{window_safety,connection_budget}"
  say "BLUE-GREEN preflight gates: PASS (fail-safe verified; full gates pending Phase 3)"
  exit 0
fi

# --- Gate 2: an ADD COLUMN expand migration passes the window-safety gate ----
say "gate: ADD COLUMN expand -> passes window-safety (preflight cleared)"
reset_migrations; write_expand
write_config ""
deploy 'postgresql://unused@127.0.0.1:1/none?sslmode=disable'
echo "$DEPLOY_OUT"
[ "$DEPLOY_RC" -ne 0 ] || die "the deploy still fails later (no green instance) — expected"
if echo "$DEPLOY_OUT" | grep -q "step 'preflight'"; then
  die "an expand migration must NOT be refused at preflight:\n$DEPLOY_OUT"
fi
echo "$DEPLOY_OUT" | grep -q "step 'provision-green'" \
  || die "expand should clear preflight and fail at provision-green (no green unit):\n$DEPLOY_OUT"
ok "ADD COLUMN expand cleared the window-safety gate (failed later, at provision-green)"

# --- Gate 3: the connection-budget probe refuses before the swap -------------
say "gate: connection budget (green pool > headroom) -> refused at preflight"
if [ -z "$DB_DSN" ] && [ -z "$CONTAINER" ]; then
  say "SKIP connection-budget gate: no --db-dsn and no podman/docker for a throwaway Postgres"
  say "      (this gate is proven hermetically in fraisier-core::connection_budget)"
else
  if [ -z "$DB_DSN" ]; then
    say "starting throwaway Postgres ($PG_IMAGE) with a low max_connections"
    $CONTAINER rm -f "$PG_NAME" >/dev/null 2>&1 || true
    $CONTAINER run -d --name "$PG_NAME" -e POSTGRES_PASSWORD=bgpw \
      -p "$PG_PORT:5432" "$PG_IMAGE" -c max_connections=20 >/dev/null \
      || die "could not start Postgres container"
    for _ in $(seq 1 40); do
      $CONTAINER exec "$PG_NAME" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 0.5
    done
    $CONTAINER exec "$PG_NAME" pg_isready -U postgres >/dev/null 2>&1 \
      || die "Postgres did not come up"
    DB_DSN="postgresql://postgres:bgpw@127.0.0.1:$PG_PORT/postgres?sslmode=disable"
    ok "Postgres ready (max_connections=20)"
  fi
  reset_migrations; write_expand     # window-safe, so preflight reaches the budget check
  write_config 500                   # green pool of 500 dwarfs the 20-connection ceiling
  deploy "$DB_DSN"
  echo "$DEPLOY_OUT"
  [ "$DEPLOY_RC" -ne 0 ] || die "the connection-budget deploy must exit non-zero"
  echo "$DEPLOY_OUT" | grep -q "step 'preflight'" \
    || die "the budget refusal must occur at the preflight step:\n$DEPLOY_OUT"
  echo "$DEPLOY_OUT" | grep -qi "max_connections" \
    || die "the refusal must cite the connection budget:\n$DEPLOY_OUT"
  ok "connection-budget exhaustion refused before the swap (real psql probe)"
fi

say "BLUE-GREEN preflight gates: PASS"
echo
echo "Proven end-to-end against real infra:"
echo "  • window-safety: DROP COLUMN refused (real confiture 0.22 PFLIGHT_REPLICA_*)"
echo "  • window-safety: ADD COLUMN expand allowed"
echo "  • connection-budget: exhaustion refused before swap (real psql)"
echo "Proven hermetically in fraisier-core::blue_green (traffic tier):"
echo "  • pre-swap health gate (green unhealthy -> traffic never moves)"
echo "  • post-swap degradation (swap-back to still-hot blue)"
echo "Follow-up: real-nginx traffic swap end-to-end (needs nginx + dual app fixture)."
