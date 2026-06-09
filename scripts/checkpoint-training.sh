#!/usr/bin/env bash
#
# Training-field checkpoint — the Confiture-on-Postgres deploy proof.
#
# The matrix and checkpoint-local.sh drive the *sqlx* migration adapter over IPC
# against SQLite. This drives the **in-process Confiture adapter against a real
# Postgres**, inside the real deploy saga, end-to-end:
#   * three consecutive successful deploys of the tiny DB-backed training app
#     (examples/training-field), each asserted to commit + serve health 200 (the
#     app's /health is 200 only when the Confiture-migrated `notes` table exists);
#   * a forced failure at each forceable phase, each asserted to roll back to the
#     healthy baseline:
#       - migrate — a deploy carrying an invalid Confiture migration; `up` fails
#                   and the saga runs `down_to(previous)`;
#       - restart — a deploy whose app ("crash" build) won't start;
#       - health  — a deploy whose app ("sick" build) serves 500.
#   * verify is asserted to PASS on a healthy deploy.
#
# Postgres and Jaeger both run as throwaway containers (no host install), so the
# script is self-contained in either systemd mode. It is the Confiture-flavoured
# sibling of scripts/checkpoint-matrix.sh and reuses its structure.
#
# Usage:
#   scripts/checkpoint-training.sh [--systemd user|system] [--deploys N]
#                                  [--db-dsn <url>] [--keep-jaeger]
#
#   --db-dsn   use an existing Postgres instead of the throwaway container (the
#              DB must be empty; the run applies + rolls back migrations).
#
set -euo pipefail

# --------------------------------------------------------------------------
# Configuration
# --------------------------------------------------------------------------
ARTIFACT_PORT="${ARTIFACT_PORT:-8751}"
HEALTH_PORT="${HEALTH_PORT:-8752}"
JAEGER_UI_PORT="${JAEGER_UI_PORT:-16688}"
OTLP_HTTP_PORT="${OTLP_HTTP_PORT:-4320}"
PG_PORT="${PG_PORT:-55433}"
UNIT="fraisier-training.service"
JAEGER_NAME="fraisier-training-jaeger"
PG_NAME="fraisier-training-pg"
KEEP_JAEGER="${KEEP_JAEGER:-0}"

MODE="user"
HAPPY_DEPLOYS=3
DB_DSN=""

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_DIR="$REPO_ROOT/examples/training-field"

while [ $# -gt 0 ]; do
  case "$1" in
    --systemd)     MODE="$2"; shift 2;;
    --deploys)     HAPPY_DEPLOYS="$2"; shift 2;;
    --db-dsn)      DB_DSN="$2"; shift 2;;
    --keep-jaeger) KEEP_JAEGER=1; shift;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# --------------------------------------------------------------------------
# systemd mode wiring (user vs system) — same contract as checkpoint-matrix.sh
# --------------------------------------------------------------------------
case "$MODE" in
  user)
    SC=(systemctl --user)
    UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    "${SC[@]}" show-environment >/dev/null 2>&1 \
      || die "no user systemd manager (need a logged-in session). Use --systemd system as root."
    SERVICE_USER="user = true"
    ;;
  system)
    SC=(systemctl)
    UNIT_DIR="/etc/systemd/system"
    [ "$(id -u)" = 0 ]         || die "--systemd system needs root"
    [ -d /run/systemd/system ] || die "pid 1 is not systemd"
    SERVICE_USER=""
    ;;
  *) die "--systemd must be 'user' or 'system' (got '$MODE')";;
esac
say "systemd mode: $MODE"

# --------------------------------------------------------------------------
# Preconditions
# --------------------------------------------------------------------------
command -v cargo     >/dev/null || die "cargo not found"
command -v python3   >/dev/null || die "python3 not found"
command -v sha256sum >/dev/null || die "sha256sum not found"
command -v curl      >/dev/null || die "curl not found"
command -v confiture >/dev/null || die "confiture not on PATH (>= 0.22 for the preflight step)"
[ -d "$APP_DIR" ] || die "training-field app not found at $APP_DIR"

CONTAINER="${CONTAINER:-}"
if [ -z "$CONTAINER" ]; then
  if command -v docker >/dev/null; then CONTAINER=docker
  elif command -v podman >/dev/null; then CONTAINER=podman
  else die "need docker or podman for the Jaeger + Postgres containers"; fi
fi
say "container runtime: $CONTAINER"
say "confiture: $(confiture --version 2>&1 | head -1)"

$CONTAINER rm -f "$JAEGER_NAME" "$PG_NAME" >/dev/null 2>&1 || true
port_free() { ! { ss -ltn 2>/dev/null || netstat -ltn 2>/dev/null; } | grep -q ":$1 "; }
CHECK_PORTS=("$ARTIFACT_PORT" "$HEALTH_PORT" "$JAEGER_UI_PORT" "$OTLP_HTTP_PORT")
[ -n "$DB_DSN" ] || CHECK_PORTS+=("$PG_PORT")
for p in "${CHECK_PORTS[@]}"; do
  port_free "$p" || die "port $p is already in use (a leftover run?). Free it and retry."
done

# --------------------------------------------------------------------------
# Build the engine (--features otel) and the training app
# --------------------------------------------------------------------------
say "building fraisier (--features otel) and the training app"
( cd "$REPO_ROOT" && cargo build --features otel --bin fraisier )
( cd "$APP_DIR" && cargo build )
FRAISIER="$REPO_ROOT/target/debug/fraisier"
TRAINING_APP="$APP_DIR/target/debug/fraisier-training-app"
[ -x "$FRAISIER" ]     || die "fraisier binary missing after build"
[ -x "$TRAINING_APP" ] || die "training app binary missing after build"

# --------------------------------------------------------------------------
# Workspace + teardown
# --------------------------------------------------------------------------
WORK="$(mktemp -d)"
UNIT_FILE="$UNIT_DIR/$UNIT"
ARTIFACT_PID=""

# shellcheck disable=SC2329  # invoked indirectly via `trap cleanup EXIT`
cleanup() {
  set +e
  say "tearing down"
  "${SC[@]}" stop "$UNIT" >/dev/null 2>&1
  "${SC[@]}" disable "$UNIT" >/dev/null 2>&1
  rm -f "$UNIT_FILE"
  "${SC[@]}" daemon-reload >/dev/null 2>&1
  [ -n "$ARTIFACT_PID" ] && kill "$ARTIFACT_PID" >/dev/null 2>&1
  $CONTAINER rm -f "$PG_NAME" >/dev/null 2>&1
  if [ "$KEEP_JAEGER" = "1" ]; then
    say "leaving Jaeger up at http://localhost:$JAEGER_UI_PORT"
  else
    $CONTAINER rm -f "$JAEGER_NAME" >/dev/null 2>&1
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT
"${SC[@]}" stop "$UNIT" >/dev/null 2>&1 || true

mkdir -p "$WORK/www" "$WORK/migrations" "$WORK/staging" "$WORK/state" "$UNIT_DIR"

# --------------------------------------------------------------------------
# Jaeger
# --------------------------------------------------------------------------
say "starting Jaeger ($JAEGER_NAME)"
$CONTAINER run -d --name "$JAEGER_NAME" \
  -e COLLECTOR_OTLP_ENABLED=true \
  -p "$JAEGER_UI_PORT:16686" -p "$OTLP_HTTP_PORT:4318" \
  jaegertracing/all-in-one >/dev/null
for _ in $(seq 1 30); do
  curl -fsS "http://localhost:$JAEGER_UI_PORT/" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -fsS "http://localhost:$JAEGER_UI_PORT/" >/dev/null 2>&1 || die "Jaeger did not come up"
ok "Jaeger ready"

# --------------------------------------------------------------------------
# Postgres (throwaway container, unless --db-dsn given)
# --------------------------------------------------------------------------
if [ -n "$DB_DSN" ]; then
  DSN="$DB_DSN"
  say "using supplied Postgres DSN"
else
  say "starting Postgres ($PG_NAME) on :$PG_PORT"
  $CONTAINER run -d --name "$PG_NAME" \
    -e POSTGRES_PASSWORD=trainpw -e POSTGRES_DB=trainingdb \
    -p "$PG_PORT:5432" postgres:16 >/dev/null
  for _ in $(seq 1 30); do
    $CONTAINER exec "$PG_NAME" pg_isready -U postgres >/dev/null 2>&1 && break
    sleep 1
  done
  $CONTAINER exec "$PG_NAME" pg_isready -U postgres >/dev/null 2>&1 || die "Postgres did not come up"
  DSN="postgresql://postgres:trainpw@127.0.0.1:$PG_PORT/trainingdb?sslmode=disable"
  ok "Postgres ready (database trainingdb)"
fi

# --------------------------------------------------------------------------
# systemd unit for the training app. The app reads the active release's version
# off the `current` symlink and the DSN from TRAINING_DATABASE_URL; readiness is
# sd_notify (Type=notify), so a start only succeeds once it is listening and the
# DB is reachable.
# --------------------------------------------------------------------------
cat > "$UNIT_FILE" <<EOF
[Unit]
Description=fraisier training-field app

[Service]
Type=notify
NotifyAccess=main
TimeoutStartSec=15
Environment=TRAINING_DATABASE_URL=$DSN
ExecStart=$TRAINING_APP $WORK/current $HEALTH_PORT
Restart=no

[Install]
WantedBy=default.target
EOF
"${SC[@]}" daemon-reload
ok "installed $MODE unit $UNIT (Type=notify, DB-backed health)"

# --------------------------------------------------------------------------
# Artifact server + opaque per-version releases (the binary is fixed infra; the
# app reads its version off the symlink, exactly like checkpoint-matrix.sh).
# --------------------------------------------------------------------------
mint_release() { # <version>
  local v="$1"
  printf 'training-%s-payload' "$v" > "$WORK/www/app-$v.tar.gz"
  ( cd "$WORK/www" && sha256sum "app-$v.tar.gz" > "app-$v.tar.gz.sha256" )
}
python3 -m http.server --directory "$WORK/www" "$ARTIFACT_PORT" >/dev/null 2>&1 &
ARTIFACT_PID=$!
sleep 0.3
kill -0 "$ARTIFACT_PID" 2>/dev/null || die "artifact server failed to start on :$ARTIFACT_PORT"
ok "artifact server on :$ARTIFACT_PORT (pid $ARTIFACT_PID)"

# --------------------------------------------------------------------------
# Confiture migrations (copied from the example so we can inject a break)
# --------------------------------------------------------------------------
cp "$APP_DIR"/migrations/*.sql "$WORK/migrations/"
mig_count=$( set -- "$WORK"/migrations/*.up.sql; echo $# )
ok "copied $mig_count Confiture migrations"

# --------------------------------------------------------------------------
# fraisier.toml — Confiture migration adapter over a real Postgres
# --------------------------------------------------------------------------
cat > "$WORK/fraisier.toml" <<EOF
[deploy]
name = "training-field"
environment = "training"

[artifact]
source = "release"
release_url = "http://127.0.0.1:$ARTIFACT_PORT/app-{version}.tar.gz"
checksum_url = "http://127.0.0.1:$ARTIFACT_PORT/app-{version}.tar.gz.sha256"
staging_dir = "$WORK/staging"
active_path = "$WORK/current"

[migration]
adapter = "confiture"
database_url_env = "TRAINING_DATABASE_URL"
migrations_path = "$WORK/migrations"

[service]
adapter = "systemd"
unit = "$UNIT"
$SERVICE_USER

[health]
adapter = "http"
url = "http://127.0.0.1:$HEALTH_PORT/health"
expected_status = 200
EOF

deploy() { # <app-version>
  TRAINING_DATABASE_URL="$DSN" \
  OTEL_EXPORTER_OTLP_ENDPOINT="http://localhost:$OTLP_HTTP_PORT" \
    "$FRAISIER" --json deploy \
      --config "$WORK/fraisier.toml" \
      --state-dir "$WORK/state" \
      --app-version "$1"
}
active_release() { basename "$(readlink "$WORK/current")" | sed 's/^app-//; s/\.tar\.gz$//'; }
health_code()    { curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$HEALTH_PORT/health"; }
wait_health() { # <expected-code>
  for _ in $(seq 1 20); do
    [ "$(health_code)" = "$1" ] && return 0
    sleep 0.25
  done
  return 1
}
confiture_revision() {
  CONFITURE_DATABASE_URL="$DSN" confiture migrate current --no-config --format json 2>/dev/null \
    | python3 -c 'import json,sys; print((json.load(sys.stdin) or {}).get("revision") or "none")' 2>/dev/null \
    || echo "none"
}

assert_committed() { # <version> <output>
  echo "$2" | grep -q '"outcome": *"committed"' || die "deploy $1 did not commit:\n$2"
}
assert_rolled_back() { # <output>
  echo "$1" | grep -q '"outcome": *"rolled_back"' || die "deploy did not roll back:\n$1"
}

# ==========================================================================
# Forced-failure matrix on the real engine (Confiture + Postgres)
# ==========================================================================
say "TRAINING FIELD — Confiture-on-Postgres deploy matrix ($MODE systemd)"

# --- Baseline ---------------------------------------------------------------
mint_release ok1
out="$(deploy ok1)"; assert_committed ok1 "$out"
[ "$(active_release)" = "ok1" ] || die "ok1 is not the active release"
wait_health 200 || die "baseline ok1 is not healthy"
base_rev="$(confiture_revision)"
ok "baseline ok1 committed, live, healthy (confiture revision: $base_rev)"

# --- N consecutive successful deploys ---------------------------------------
say "A1 — $HAPPY_DEPLOYS consecutive successful deploys"
for i in $(seq 1 "$HAPPY_DEPLOYS"); do
  v="ok-run$i"; mint_release "$v"
  out="$(deploy "$v")"; assert_committed "$v" "$out"
  [ "$(active_release)" = "$v" ] || die "$v is not active after commit"
  wait_health 200 || die "$v is not healthy after commit"
  ok "deploy $i/$HAPPY_DEPLOYS ($v) committed and healthy"
done
baseline_rel="$(active_release)"

# --- A2: forced MIGRATE failure (invalid Confiture migration) ---------------
say "A2 — forced migrate failure (invalid migration → down_to rollback)"
cat > "$WORK/migrations/003_break.up.sql"   <<'SQL'
THIS IS NOT VALID SQL;
SQL
cat > "$WORK/migrations/003_break.down.sql" <<'SQL'
SELECT 1;
SQL
mint_release ok-migfail
set +e; out="$(deploy ok-migfail)"; code=$?; set -e
echo "$out"
[ "$code" -ne 0 ] || die "migrate-failure deploy should have failed"
assert_rolled_back "$out"
echo "$out" | grep -q "step 'migrate'" || die "expected the failure at the migrate step:\n$out"
[ "$(active_release)" = "$baseline_rel" ] || die "migrate failure must not change the active release (got $(active_release))"
[ "$(confiture_revision)" = "$base_rev" ] || die "DB not back at baseline after migrate rollback (was $base_rev, now $(confiture_revision))"
rm -f "$WORK/migrations/003_break.up.sql" "$WORK/migrations/003_break.down.sql"
ok "migrate failed at 'migrate', rolled back; active still $baseline_rel; DB at $base_rev"

# --- A3: forced RESTART failure (crash build won't start) -------------------
say "A3 — forced restart failure (app won't start → re-activate prior)"
mint_release crashbuild
set +e; out="$(deploy crashbuild)"; code=$?; set -e
echo "$out"
[ "$code" -ne 0 ] || die "restart-failure deploy should have failed"
assert_rolled_back "$out"
echo "$out" | grep -q "step 'restart'" || die "expected the failure at the restart step:\n$out"
[ "$(active_release)" = "$baseline_rel" ] || die "restart rollback did not re-activate $baseline_rel (got $(active_release))"
wait_health 200 || die "$baseline_rel not healthy again after restart rollback"
ok "restart failed at 'restart', rolled back to $baseline_rel, healthy"

# --- A4: forced HEALTH failure (sick build serves 500) ----------------------
say "A4 — forced health failure (app serves 500 → re-activate prior)"
mint_release sickbuild
set +e; out="$(deploy sickbuild)"; code=$?; set -e
echo "$out"
[ "$code" -ne 0 ] || die "health-failure deploy should have failed"
assert_rolled_back "$out"
echo "$out" | grep -q "step 'health'" || die "expected the failure at the health step:\n$out"
[ "$(active_release)" = "$baseline_rel" ] || die "health rollback did not re-activate $baseline_rel (got $(active_release))"
wait_health 200 || die "$baseline_rel not healthy again after health rollback"
ok "health failed at 'health', rolled back to $baseline_rel, healthy"

# --- A5: verify passes on a healthy deploy ----------------------------------
say "A5 — verify passes on a healthy deploy"
mint_release ok-verify
out="$(deploy ok-verify)"; assert_committed ok-verify "$out"
wait_health 200 || die "ok-verify not healthy"
ok "healthy deploy committed (Confiture verify ran and passed)"

# --------------------------------------------------------------------------
# Traces
# --------------------------------------------------------------------------
say "checking Jaeger for exported spans"
found=0
for _ in $(seq 1 20); do
  if curl -fsS "http://localhost:$JAEGER_UI_PORT/api/services" | grep -q '"fraisier"'; then
    found=1; break
  fi
  sleep 0.5
done
[ "$found" = "1" ] || die "the 'fraisier' service never appeared in Jaeger (OTLP export failed)"
ok "saga spans exported to Jaeger (service=fraisier)"
"$REPO_ROOT/scripts/show-trace.sh" --jaeger "http://localhost:$JAEGER_UI_PORT" || true

say "TRAINING FIELD PASSED [$MODE systemd] — Confiture + real Postgres, in-saga:"
say "$HAPPY_DEPLOYS consecutive commits; forced migrate/restart/health failures each"
say "rolled back to the healthy baseline; verify passed; spans exported."
if [ "$KEEP_JAEGER" = "1" ]; then
  say "Jaeger left up — re-inspect headless with:"
  say "  scripts/show-trace.sh --jaeger http://localhost:$JAEGER_UI_PORT"
fi
exit 0
