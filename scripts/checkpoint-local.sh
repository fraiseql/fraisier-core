#!/usr/bin/env bash
#
# §10.3 checkpoint, part (a) — a real fraisier deploy with no faked moving parts.
#
# Retires the biggest unvalidated foundation risk: the parts every automated test
# has had to fake. It runs a real deploy that exercises
#
#   * the real SystemdService adapter against a real systemd unit — real
#     `systemctl restart` / `is-active`, real exit codes;
#   * real filesystem symlink activation (the [artifact] active_path/staging_dir);
#   * the sqlx migration adapter over real IPC (SQLite — no DB server);
#   * forced-failure rollback (a bad release fails its health probe → the saga
#     re-activates the prior release and reverts the migration over IPC);
#   * real OpenTelemetry → Jaeger span export.
#
# Two deploys: v1 is healthy and commits; v2 ships a release whose app reports
# unhealthy, so the deploy rolls back. The app's health is a function of which
# release is *currently symlinked and (re)started*, so passing-v1 → failing-v2 →
# passing-again-after-rollback proves the symlink swap and the systemctl restart
# are both load-bearing.
#
# Modes (CHECKPOINT_SYSTEMD):
#   user   (default) — `systemctl --user`, no root, zero spend. Validates the
#                      adapter logic + observability on your own machine.
#   system           — a real /etc/systemd/system unit driven by the pid-1
#                      manager (needs root). This is what scripts/checkpoint-hetzner.sh
#                      runs on the provisioned host, so the genuinely-remote,
#                      system-level path reuses this exact, tested scenario.
#
# What neither mode covers (that is checkpoint-hetzner.sh's job): a genuinely
# remote host over the network, and three consecutive production deploys with a
# forced failure at each phase. This validates the adapter + pipeline; the remote
# matrix is its own gate.
#
# Usage:  scripts/checkpoint-local.sh
# Env:    CHECKPOINT_SYSTEMD=user|system   (default user)
#         KEEP_JAEGER=1                     leave Jaeger up for inspection
#         CONTAINER=podman                  force a runtime (default docker→podman)
#
set -euo pipefail

# --------------------------------------------------------------------------
# Configuration (override via the environment)
# --------------------------------------------------------------------------
ARTIFACT_PORT="${ARTIFACT_PORT:-8731}"
HEALTH_PORT="${HEALTH_PORT:-8732}"
JAEGER_UI_PORT="${JAEGER_UI_PORT:-16686}"
OTLP_HTTP_PORT="${OTLP_HTTP_PORT:-4318}"
UNIT="fraisier-checkpoint.service"
JAEGER_NAME="fraisier-checkpoint-jaeger"
KEEP_JAEGER="${KEEP_JAEGER:-0}"
MODE="${CHECKPOINT_SYSTEMD:-user}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SQLX_REPO="${FRAISIER_SQLX_REPO:-$(cd "$REPO_ROOT/.." && pwd)/fraisier-adapter-sqlx}"

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# --------------------------------------------------------------------------
# systemd mode wiring (user vs system)
# --------------------------------------------------------------------------
case "$MODE" in
  user)
    SC=(systemctl --user)
    UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    "${SC[@]}" show-environment >/dev/null 2>&1 \
      || die "no user systemd manager (need a logged-in session). Use CHECKPOINT_SYSTEMD=system as root."
    ;;
  system)
    SC=(systemctl)
    UNIT_DIR="/etc/systemd/system"
    [ "$(id -u)" = 0 ]        || die "CHECKPOINT_SYSTEMD=system needs root"
    [ -d /run/systemd/system ] || die "pid 1 is not systemd"
    ;;
  *) die "CHECKPOINT_SYSTEMD must be 'user' or 'system' (got '$MODE')";;
esac
say "systemd mode: $MODE"

# --------------------------------------------------------------------------
# Preconditions
# --------------------------------------------------------------------------
command -v cargo     >/dev/null || die "cargo not found"
command -v python3   >/dev/null || die "python3 not found (used for the local fixtures)"
command -v sha256sum >/dev/null || die "sha256sum not found"

CONTAINER="${CONTAINER:-}"
if [ -z "$CONTAINER" ]; then
  if command -v docker >/dev/null; then CONTAINER=docker
  elif command -v podman >/dev/null; then CONTAINER=podman
  else die "need docker or podman for the Jaeger container"; fi
fi
say "container runtime: $CONTAINER"

port_free() { ! { ss -ltn 2>/dev/null || netstat -ltn 2>/dev/null; } | grep -q ":$1 "; }
for p in "$ARTIFACT_PORT" "$HEALTH_PORT" "$JAEGER_UI_PORT" "$OTLP_HTTP_PORT"; do
  port_free "$p" || die "port $p is already in use (a leftover run?). Free it and retry."
done

[ -d "$SQLX_REPO" ] || die "sqlx adapter repo not found at $SQLX_REPO (set FRAISIER_SQLX_REPO)"

# --------------------------------------------------------------------------
# Build the binaries (fraisier with OTLP export; the sqlx IPC adapter)
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
    say "leaving Jaeger up at http://localhost:$JAEGER_UI_PORT ($CONTAINER rm -f $JAEGER_NAME to remove)"
  else
    $CONTAINER rm -f "$JAEGER_NAME" >/dev/null 2>&1
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT
# Clear leftovers from an interrupted prior run.
"${SC[@]}" stop "$UNIT" >/dev/null 2>&1 || true
$CONTAINER rm -f "$JAEGER_NAME" >/dev/null 2>&1 || true

mkdir -p "$WORK/www" "$WORK/migrations" "$WORK/staging" "$WORK/state" "$UNIT_DIR"

# --------------------------------------------------------------------------
# Jaeger (OTLP/HTTP on 4318, UI on 16686)
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
# Local artifact server: two opaque "releases" + their sha256 sidecars
# --------------------------------------------------------------------------
for v in v1 v2; do
  printf 'fraiseql-%s-payload' "$v" > "$WORK/www/app-$v.tar.gz"
  ( cd "$WORK/www" && sha256sum "app-$v.tar.gz" > "app-$v.tar.gz.sha256" )
done
# `--directory` (not a subshell) so $! is python's own PID — otherwise cleanup
# would kill the subshell and orphan the server, leaking the port.
python3 -m http.server --directory "$WORK/www" "$ARTIFACT_PORT" >/dev/null 2>&1 &
ARTIFACT_PID=$!
sleep 0.3
kill -0 "$ARTIFACT_PID" 2>/dev/null || die "artifact server failed to start on :$ARTIFACT_PORT"
ok "artifact server on :$ARTIFACT_PORT (pid $ARTIFACT_PID)"

# --------------------------------------------------------------------------
# The "app": a real systemd unit serving /health. It reads which release is
# active *once at startup*, so only a real restart after a symlink swap changes
# what it reports. v1 → 200 (healthy), anything else → 500.
# --------------------------------------------------------------------------
cat > "$WORK/health_server.py" <<'PY'
import http.server, os, sys
active, port = sys.argv[1], int(sys.argv[2])
try:
    version = os.path.basename(os.readlink(active))
except OSError:
    version = "none"
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200 if version == "v1" else 500)
        self.end_headers()
        self.wfile.write(version.encode())
    def log_message(self, *a):
        pass
http.server.HTTPServer(("127.0.0.1", port), H).serve_forever()
PY

# In user mode the adapter spawns bare `systemctl`, so redirect it to the user
# manager via a wrapper (the config schema does not yet expose `[service].user`).
# In system mode the real `systemctl` is exactly what we want.
if [ "$MODE" = user ]; then
  cat > "$WORK/systemctl-user" <<'SH'
#!/bin/sh
exec systemctl --user "$@"
SH
  chmod +x "$WORK/systemctl-user"
  SYSTEMCTL_BIN="$WORK/systemctl-user"
else
  SYSTEMCTL_BIN="$(command -v systemctl)"
fi

cat > "$UNIT_FILE" <<EOF
[Unit]
Description=fraisier local checkpoint app

[Service]
ExecStart=$(command -v python3) $WORK/health_server.py $WORK/current $HEALTH_PORT
Restart=no

[Install]
WantedBy=default.target
EOF
"${SC[@]}" daemon-reload
ok "installed $MODE unit $UNIT"

# --------------------------------------------------------------------------
# fraisier.toml — the sqlx IPC adapter drives migrations; everything real
# --------------------------------------------------------------------------
cat > "$WORK/fraisier.toml" <<EOF
[deploy]
name = "fraiseql"
environment = "checkpoint"

[artifact]
source = "release"
release_url = "http://127.0.0.1:$ARTIFACT_PORT/app-{version}.tar.gz"
checksum_url = "http://127.0.0.1:$ARTIFACT_PORT/app-{version}.tar.gz.sha256"
staging_dir = "$WORK/staging"
active_path = "$WORK/current"

[migration]
adapter = "sqlx"
database_url_env = "FRAISIER_CHECKPOINT_DSN"
migrations_path = "$WORK/migrations"

[service]
adapter = "systemd"
unit = "$UNIT"

[health]
adapter = "http"
url = "http://127.0.0.1:$HEALTH_PORT/health"
expected_status = 200
EOF

cat > "$WORK/migrations/0001_create_widgets.up.sql" <<'SQL'
CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
SQL
cat > "$WORK/migrations/0001_create_widgets.down.sql" <<'SQL'
DROP TABLE widgets;
SQL

DSN="sqlite://$WORK/app.db?mode=rwc"
deploy() { # <app-version>
  PATH="$SQLX_DIR:$PATH" \
  FRAISIER_CHECKPOINT_DSN="$DSN" \
  FRAISIER_SYSTEMCTL_BIN="$SYSTEMCTL_BIN" \
  OTEL_EXPORTER_OTLP_ENDPOINT="http://localhost:$OTLP_HTTP_PORT" \
    "$FRAISIER" --json deploy \
      --config "$WORK/fraisier.toml" \
      --state-dir "$WORK/state" \
      --app-version "$1"
}
active_release() { basename "$(readlink "$WORK/current")"; }
health_code() { curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$HEALTH_PORT/health"; }
# Poll for the expected health code: a just-restarted unit needs a moment to
# re-bind its socket (the deploy's own probe retries; this mirrors that).
wait_health() { # <expected-code>
  for _ in $(seq 1 20); do
    [ "$(health_code)" = "$1" ] && return 0
    sleep 0.25
  done
  return 1
}

# --------------------------------------------------------------------------
# Deploy #1 — v1, healthy → commits
# --------------------------------------------------------------------------
say "deploy #1 (v1, healthy)"
out="$(deploy v1)"; echo "$out"
echo "$out" | grep -q '"outcome": *"committed"' || die "deploy #1 did not commit"
[ "$(active_release)" = "v1" ] || die "v1 is not the active release"
wait_health 200 || die "the v1 app is not serving healthy"
ok "v1 committed, live, and healthy"

# --------------------------------------------------------------------------
# Deploy #2 — v2 adds a migration and a release whose app is unhealthy → rollback
# --------------------------------------------------------------------------
cat > "$WORK/migrations/0002_add_color.up.sql" <<'SQL'
ALTER TABLE widgets ADD COLUMN color TEXT;
SQL
cat > "$WORK/migrations/0002_add_color.down.sql" <<'SQL'
ALTER TABLE widgets DROP COLUMN color;
SQL

say "deploy #2 (v2, unhealthy → must roll back)"
set +e; out="$(deploy v2)"; code=$?; set -e; echo "$out"
[ "$code" -ne 0 ] || die "deploy #2 should have failed"
echo "$out" | grep -q '"outcome": *"rolled_back"' || die "deploy #2 did not roll back"
[ "$(active_release)" = "v1" ] || die "rollback did not re-activate v1 (got $(active_release))"
wait_health 200 || die "v1 is not healthy again after rollback"
ok "v2 rolled back: v1 re-activated and healthy"

# DB-level check (best effort; down_to over IPC is also covered by the
# e2e_ipc_sqlx integration test). After rollback only migration 1 is applied.
if command -v sqlite3 >/dev/null; then
  applied="$(sqlite3 "$WORK/app.db" 'SELECT COUNT(*) FROM _sqlx_migrations WHERE success=1')"
  [ "$applied" = "1" ] || die "expected 1 applied migration after rollback, got $applied"
  ok "migration 0002 reverted over IPC (1 migration applied)"
fi

# --------------------------------------------------------------------------
# OTel: the deploys' saga spans reached Jaeger
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
curl -fsS "http://localhost:$JAEGER_UI_PORT/api/traces?service=fraisier&limit=50" \
  | grep -q '"spans"' || die "no traces for service 'fraisier'"
ok "saga spans exported to Jaeger (service=fraisier)"

say "CHECKPOINT (a) PASSED [$MODE systemd] — real systemd, real symlink activation,"
say "sqlx-over-IPC, forced-failure rollback, and OTLP→Jaeger export, no spend."
[ "$KEEP_JAEGER" = "1" ] && say "inspect the trace at http://localhost:$JAEGER_UI_PORT (search service 'fraisier')"
exit 0
