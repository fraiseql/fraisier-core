#!/usr/bin/env bash
#
# §10.3 production matrix — the operator sign-off that gates the rename.
#
# Two parts, both against a real engine (real systemd, real symlink activation,
# the migration adapter over real IPC, real OTLP→Jaeger). Neither fakes a green:
# every outcome is asserted, so a mis-injected failure fails the script loudly.
#
#   PART A — per-phase rollback + 3-consecutive deploys (always runs)
#     A controlled fixture proves the saga rolls back at each *forceable* phase
#     and commits three times in a row. The failures are real and deterministic:
#       * migrate  — a deploy carrying an invalid migration; `up` fails and the
#                    saga runs `down_to(previous)` (DB returns to the baseline).
#       * release  — a deploy whose app fails to start; `systemctl restart`
#                    returns non-zero and the saga re-activates + restarts the
#                    prior release (a Type=notify unit whose readiness is a
#                    function of the active version, so the rollback restart of
#                    the healthy baseline still succeeds → clean rolled_back).
#       * health   — a deploy whose app starts but reports unhealthy; the saga
#                    re-activates the prior release.
#     `verify` is asserted to PASS on a healthy deploy. A verify-phase *failure*
#     cannot be induced by natural config (verify is a post-migration success
#     report — sqlx reads `_sqlx_migrations.success`, confiture reflects its
#     `failed_count`; a failed migration aborts at *migrate*, never reaching a
#     verify-with-a-failed-check). Verify-failure rollback is unit-proven instead
#     — see crates/fraisier-saga/tests/rollback.rs and the single_host.rs tests.
#
#   PART B — criterion 1: fraiseql v2 deploys successfully 3× in production
#     Runs only with --real-config. Drives N consecutive deploys of YOUR real
#     fraisier.toml (real artifact + real Postgres via Confiture or sqlx) and
#     asserts each commits. The script never synthesises migrations here — it
#     drives your config as-is, so "provide artifact + DSN and run it" means:
#     point --real-config at your fraiseql deploy config and export the DSN env
#     var its [migration].database_url_env names.
#
# Part A's migration store is sqlx (SQLite by default; point --matrix-dsn at a
# throwaway Postgres URL to run the forced-failure matrix against real Postgres —
# the fixture migrations are dialect-neutral). Part A is what runs green locally
# with zero spend; on the --keep'd Hetzner host use --systemd system to confirm
# the same matrix under the pid-1 manager.
#
# Usage:
#   scripts/checkpoint-matrix.sh [--systemd user|system]
#                                [--matrix-dsn <url>] [--deploys N]
#                                [--real-config <fraisier.toml> --real-version V]...
#                                [--keep-jaeger]
#
# Env mirrors checkpoint-local.sh: CONTAINER, *_PORT overrides, FRAISIER_SQLX_REPO.
#
set -euo pipefail

# --------------------------------------------------------------------------
# Configuration
# --------------------------------------------------------------------------
ARTIFACT_PORT="${ARTIFACT_PORT:-8741}"
HEALTH_PORT="${HEALTH_PORT:-8742}"
JAEGER_UI_PORT="${JAEGER_UI_PORT:-16687}"
OTLP_HTTP_PORT="${OTLP_HTTP_PORT:-4319}"
UNIT="fraisier-matrix.service"
JAEGER_NAME="fraisier-matrix-jaeger"
KEEP_JAEGER="${KEEP_JAEGER:-0}"

MODE="user"
MATRIX_DSN=""
HAPPY_DEPLOYS=3
REAL_CONFIG=""
REAL_VERSIONS=()

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SQLX_REPO="${FRAISIER_SQLX_REPO:-$(cd "$REPO_ROOT/.." && pwd)/fraisier-adapter-sqlx}"

while [ $# -gt 0 ]; do
  case "$1" in
    --systemd)      MODE="$2"; shift 2;;
    --matrix-dsn)   MATRIX_DSN="$2"; shift 2;;
    --deploys)      HAPPY_DEPLOYS="$2"; shift 2;;
    --real-config)  REAL_CONFIG="$2"; shift 2;;
    --real-version) REAL_VERSIONS+=("$2"); shift 2;;
    --keep-jaeger)  KEEP_JAEGER=1; shift;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# --------------------------------------------------------------------------
# systemd mode wiring (user vs system) — same contract as checkpoint-local.sh
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

CONTAINER="${CONTAINER:-}"
if [ -z "$CONTAINER" ]; then
  if command -v docker >/dev/null; then CONTAINER=docker
  elif command -v podman >/dev/null; then CONTAINER=podman
  else die "need docker or podman for the Jaeger container"; fi
fi
say "container runtime: $CONTAINER"

$CONTAINER rm -f "$JAEGER_NAME" >/dev/null 2>&1 || true
port_free() { ! { ss -ltn 2>/dev/null || netstat -ltn 2>/dev/null; } | grep -q ":$1 "; }
for p in "$ARTIFACT_PORT" "$HEALTH_PORT" "$JAEGER_UI_PORT" "$OTLP_HTTP_PORT"; do
  port_free "$p" || die "port $p is already in use (a leftover run?). Free it and retry."
done

[ -d "$SQLX_REPO" ] || die "sqlx adapter repo not found at $SQLX_REPO (set FRAISIER_SQLX_REPO)"
if [ -n "$REAL_CONFIG" ]; then
  [ -f "$REAL_CONFIG" ] || die "--real-config '$REAL_CONFIG' is not a readable file"
  [ "${#REAL_VERSIONS[@]}" -gt 0 ] || die "--real-config needs at least one --real-version <V>"
fi

# --------------------------------------------------------------------------
# Build
# --------------------------------------------------------------------------
say "building fraisier (--features otel) and the sqlx adapter"
( cd "$REPO_ROOT" && cargo build --features otel --bin fraisier )
( cd "$SQLX_REPO" && cargo build )
FRAISIER="$REPO_ROOT/target/debug/fraisier"
SQLX_DIR="$SQLX_REPO/target/debug"
[ -x "$FRAISIER" ] || die "fraisier binary missing after build"
[ -x "$SQLX_DIR/fraisier-adapter-sqlx" ] || die "sqlx adapter binary missing after build"

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
  if [ "$KEEP_JAEGER" = "1" ]; then
    say "leaving Jaeger up at http://localhost:$JAEGER_UI_PORT"
  else
    $CONTAINER rm -f "$JAEGER_NAME" >/dev/null 2>&1
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT
"${SC[@]}" stop "$UNIT" >/dev/null 2>&1 || true
$CONTAINER rm -f "$JAEGER_NAME" >/dev/null 2>&1 || true

mkdir -p "$WORK/www" "$WORK/migrations" "$WORK/staging" "$WORK/state" "$WORK/real-state" "$UNIT_DIR"

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
# The "app": a Type=notify unit whose behaviour is a function of the active
# release's version name (read once at start, like a real app reading its build):
#   *crash* → exit before readiness  ⇒ `systemctl restart` fails (release phase)
#   *sick*  → start, but serve 500    ⇒ health probe fails (health phase)
#   else    → start, serve 200        ⇒ healthy
# It signals readiness via sd_notify from the main process (NotifyAccess=main),
# so a Type=notify start only succeeds once the server is actually listening.
# --------------------------------------------------------------------------
cat > "$WORK/app.py" <<'PY'
import http.server, os, socket, sys
active, port = sys.argv[1], int(sys.argv[2])
try:
    version = os.path.basename(os.readlink(active))
except OSError:
    version = "none"

# Release-phase failure: the activated build fails to come up at all.
if "crash" in version:
    sys.exit(1)

def notify_ready():
    addr = os.environ.get("NOTIFY_SOCKET")
    if not addr:
        return
    if addr.startswith("@"):
        addr = "\0" + addr[1:]
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
    try:
        sock.connect(addr)
        sock.sendall(b"READY=1")
    finally:
        sock.close()

healthy = "sick" not in version
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200 if healthy else 500)
        self.end_headers()
        self.wfile.write(version.encode())
    def log_message(self, *a):
        pass

server = http.server.HTTPServer(("127.0.0.1", port), H)
notify_ready()
server.serve_forever()
PY

cat > "$UNIT_FILE" <<EOF
[Unit]
Description=fraisier §10.3 matrix app
# The matrix deploys many times within seconds; disable systemd's start rate
# limit (default 5/10s) so a rollback restart isn't refused as "started too
# often". A real deploy cadence never approaches this; this is a fixture concern.
StartLimitIntervalSec=0

[Service]
Type=notify
NotifyAccess=main
TimeoutStartSec=10
ExecStart=$(command -v python3) $WORK/app.py $WORK/current $HEALTH_PORT
Restart=no

[Install]
WantedBy=default.target
EOF
"${SC[@]}" daemon-reload
ok "installed $MODE unit $UNIT (Type=notify)"

# --------------------------------------------------------------------------
# Artifact server: one opaque release per version + sha256 sidecar, minted on
# demand. The version name is the only thing that matters (the app reads it off
# the active symlink), so a release is just its name.
# --------------------------------------------------------------------------
mint_release() { # <version>
  local v="$1"
  printf 'fraiseql-%s-payload' "$v" > "$WORK/www/app-$v.tar.gz"
  ( cd "$WORK/www" && sha256sum "app-$v.tar.gz" > "app-$v.tar.gz.sha256" )
}
python3 -m http.server --directory "$WORK/www" "$ARTIFACT_PORT" >/dev/null 2>&1 &
ARTIFACT_PID=$!
sleep 0.3
kill -0 "$ARTIFACT_PID" 2>/dev/null || die "artifact server failed to start on :$ARTIFACT_PORT"
ok "artifact server on :$ARTIFACT_PORT (pid $ARTIFACT_PID)"

# --------------------------------------------------------------------------
# Migrations (dialect-neutral, so the same fixture works on SQLite or Postgres)
# --------------------------------------------------------------------------
cat > "$WORK/migrations/0001_create_widgets.up.sql"   <<'SQL'
CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
SQL
cat > "$WORK/migrations/0001_create_widgets.down.sql" <<'SQL'
DROP TABLE widgets;
SQL
cat > "$WORK/migrations/0002_add_color.up.sql"   <<'SQL'
ALTER TABLE widgets ADD COLUMN color TEXT;
SQL
cat > "$WORK/migrations/0002_add_color.down.sql" <<'SQL'
ALTER TABLE widgets DROP COLUMN color;
SQL

# --------------------------------------------------------------------------
# fraisier.toml — sqlx migration adapter over IPC; everything else real
# --------------------------------------------------------------------------
DSN="${MATRIX_DSN:-sqlite://$WORK/app.db?mode=rwc}"
cat > "$WORK/fraisier.toml" <<EOF
[deploy]
name = "fraiseql"
environment = "matrix"

[artifact]
source = "release"
release_url = "http://127.0.0.1:$ARTIFACT_PORT/app-{version}.tar.gz"
checksum_url = "http://127.0.0.1:$ARTIFACT_PORT/app-{version}.tar.gz.sha256"
staging_dir = "$WORK/staging"
active_path = "$WORK/current"

[migration]
adapter = "sqlx"
database_url_env = "FRAISIER_MATRIX_DSN"
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
  PATH="$SQLX_DIR:$PATH" \
  FRAISIER_MATRIX_DSN="$DSN" \
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
applied_count() {
  command -v sqlite3 >/dev/null || { echo skip; return; }
  case "$DSN" in
    sqlite://*) sqlite3 "$WORK/app.db" 'SELECT COUNT(*) FROM _sqlx_migrations WHERE success=1' 2>/dev/null || echo skip;;
    *) echo skip;;
  esac
}

assert_committed() { # <version> <output>
  echo "$2" | grep -q '"outcome": *"committed"' || die "deploy $1 did not commit:\n$2"
}
assert_rolled_back() { # <output>
  echo "$1" | grep -q '"outcome": *"rolled_back"' || die "deploy did not roll back:\n$1"
}

# ==========================================================================
# PART A — per-phase rollback + 3-consecutive deploys
# ==========================================================================
say "PART A — forced-failure matrix on the real engine ($MODE systemd, sqlx/IPC)"

# --- Baseline: one healthy deploy so rollbacks have a prior release ----------
mint_release ok1
out="$(deploy ok1)"; assert_committed ok1 "$out"
[ "$(active_release)" = "ok1" ] || die "ok1 is not the active release"
wait_health 200 || die "baseline ok1 is not healthy"
base="$(applied_count)"
ok "baseline ok1 committed, live, healthy (migrations applied: $base)"

# --- A1: three consecutive successful deploys (criterion-1 mechanic) ---------
say "A1 — $HAPPY_DEPLOYS consecutive successful deploys"
for i in $(seq 1 "$HAPPY_DEPLOYS"); do
  v="ok-run$i"; mint_release "$v"
  out="$(deploy "$v")"; assert_committed "$v" "$out"
  [ "$(active_release)" = "$v" ] || die "$v is not active after commit"
  wait_health 200 || die "$v is not healthy after commit"
  ok "deploy $i/$HAPPY_DEPLOYS ($v) committed and healthy"
done
baseline_rel="$(active_release)"

# --- A2: forced MIGRATE failure ---------------------------------------------
say "A2 — forced migrate failure (invalid migration → down_to rollback)"
cat > "$WORK/migrations/0003_break.up.sql"   <<'SQL'
THIS IS NOT VALID SQL;
SQL
cat > "$WORK/migrations/0003_break.down.sql" <<'SQL'
DROP TABLE IF EXISTS never_created;
SQL
mint_release ok-migfail
set +e; out="$(deploy ok-migfail)"; code=$?; set -e
echo "$out"
[ "$code" -ne 0 ] || die "migrate-failure deploy should have failed"
assert_rolled_back "$out"
echo "$out" | grep -q "step 'migrate'" || die "expected the failure at the migrate step:\n$out"
[ "$(active_release)" = "$baseline_rel" ] || die "migrate failure must not change the active release (got $(active_release))"
now="$(applied_count)"
[ "$now" = skip ] || [ "$now" = "$base" ] || die "DB not back at baseline after migrate rollback (was $base, now $now)"
rm -f "$WORK/migrations/0003_break.up.sql" "$WORK/migrations/0003_break.down.sql"
ok "migrate failed at 'migrate', rolled back; active still $baseline_rel; DB at baseline ($now)"

# --- A3: forced RELEASE failure (the restart step) --------------------------
say "A3 — forced release failure (app fails to start → re-activate prior)"
mint_release crashrel
set +e; out="$(deploy crashrel)"; code=$?; set -e
echo "$out"
[ "$code" -ne 0 ] || die "release-failure deploy should have failed"
assert_rolled_back "$out"
# The release phase is the activate→restart pair; a start failure surfaces at
# 'restart', and the saga compensates the completed 'activate' step.
echo "$out" | grep -q "step 'restart'" || die "expected the failure at the restart step:\n$out"
[ "$(active_release)" = "$baseline_rel" ] || die "release rollback did not re-activate $baseline_rel (got $(active_release))"
wait_health 200 || die "$baseline_rel not healthy again after release rollback"
ok "release failed at 'restart', rolled back to $baseline_rel, healthy"

# --- A4: forced HEALTH failure ----------------------------------------------
say "A4 — forced health failure (app starts but unhealthy → re-activate prior)"
mint_release sickver
set +e; out="$(deploy sickver)"; code=$?; set -e
echo "$out"
[ "$code" -ne 0 ] || die "health-failure deploy should have failed"
assert_rolled_back "$out"
echo "$out" | grep -q "step 'health'" || die "expected the failure at the health step:\n$out"
[ "$(active_release)" = "$baseline_rel" ] || die "health rollback did not re-activate $baseline_rel (got $(active_release))"
wait_health 200 || die "$baseline_rel not healthy again after health rollback"
ok "health failed at 'health', rolled back to $baseline_rel, healthy"

# --- A5: verify passes on a healthy deploy ----------------------------------
say "A5 — verify passes on a healthy deploy (verify-failure rollback is unit-proven)"
mint_release ok-verify
out="$(deploy ok-verify)"; assert_committed ok-verify "$out"
wait_health 200 || die "ok-verify not healthy"
ok "healthy deploy committed (verify ran and passed)"
say "  note: a verify-phase *failure* is not inducible by natural config; its"
say "  rollback is proven in fraisier-saga/tests/rollback.rs + single_host.rs."

# ==========================================================================
# PART B — criterion 1: real artifact + real Postgres, 3 consecutive deploys
# ==========================================================================
if [ -n "$REAL_CONFIG" ]; then
  say "PART B — ${#REAL_VERSIONS[@]} consecutive production deploys of $REAL_CONFIG"
  say "  (driving your real config as-is; export the DSN env its [migration] names)"
  i=0
  for v in "${REAL_VERSIONS[@]}"; do
    i=$((i + 1))
    set +e
    out="$(OTEL_EXPORTER_OTLP_ENDPOINT="http://localhost:$OTLP_HTTP_PORT" \
      "$FRAISIER" --json deploy --config "$REAL_CONFIG" \
        --state-dir "$WORK/real-state" --app-version "$v")"
    code=$?
    set -e
    echo "$out"
    [ "$code" -eq 0 ] || die "production deploy $i ($v) failed (exit $code)"
    assert_committed "$v" "$out"
    ok "production deploy $i/${#REAL_VERSIONS[@]} ($v) committed"
  done
else
  say "PART B skipped (pass --real-config <fraisier.toml> --real-version <V>... to"
  say "  run the real-artifact / real-Postgres criterion-1 deploys)"
fi

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

say "MATRIX PASSED [$MODE systemd] — 3 consecutive commits; forced migrate/release/"
say "health failures each rolled back to the healthy baseline; verify passed; spans"
say "exported. $( [ -n "$REAL_CONFIG" ] && echo "Real config committed ${#REAL_VERSIONS[@]}×." || echo "Run with --real-config for the criterion-1 production deploys." )"
if [ "$KEEP_JAEGER" = "1" ]; then
  say "Jaeger left up — re-inspect headless with:"
  say "  scripts/show-trace.sh --jaeger http://localhost:$JAEGER_UI_PORT"
fi
exit 0
