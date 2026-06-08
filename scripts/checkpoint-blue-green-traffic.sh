#!/usr/bin/env bash
#
# Blue-green TRAFFIC checkpoint — the phase-07 §7.5 traffic-tier gates, end-to-end
# against a **real nginx** routing real HTTP between a blue and a green fleet.
#
# Drives the real `fraisier deploy` with `[deploy].strategy = "blue-green"` through the full flow and
# asserts what traffic nginx actually serves:
#
#   * Happy swap: green comes up healthy -> traffic SWAPS blue->green (nginx now
#     serves "green"), the deploy commits, blue is reaped.
#   * Pre-swap health gate: green is sick (health 500) -> the pre-swap gate fails,
#     traffic NEVER moves (nginx still serves "blue"), green is decommissioned.
#   * Post-swap degradation: green passes the gate then degrades during the hold
#     window -> traffic SWAPS BACK to still-hot blue (nginx serves "blue" again).
#
# Topology (rootless podman + user systemd, zero spend):
#   * blue + green = two `python3` HTTP fleets as **user systemd units** on
#     127.0.0.1:8080 / :8081 (each answers any path with its own name, or 500);
#   * a real **nginx** (host network, in a container) whose active upstream is the
#     `include` + symlink fraisier's TrafficDirector repoints + reloads;
#   * a throwaway **Postgres** + **confiture** for the migrate / window-safety step.
#
# The include dir is bind-mounted at its *same absolute path* so fraisier's
# absolute swap symlink resolves identically inside the nginx container.
#
# Usage: scripts/checkpoint-blue-green-traffic.sh [--keep]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURE="$REPO_ROOT/scripts/multihost-fixture"
PODMAN="${PODMAN:-podman}"
PG_PORT="${PG_PORT:-55435}"
LB_PORT="${LB_PORT:-8092}"
BLUE_PORT="${BLUE_PORT:-8080}"
GREEN_PORT="${GREEN_PORT:-8081}"
PG_NAME="fraisier-bgt-pg"
LB_CTR="fraisier-bgt-nginx"
NGINX_IMG="${NGINX_IMG:-fraisier-mh-nginx}"
# Postgres image. Override to the org's ghcr mirror to skip Docker Hub (blocked
# on the dev box; 429-prone on shared CI/runner IPs), e.g.
#   PG_IMAGE=ghcr.io/fraiseql/postgres:16-alpine GHCR_TOKEN=$PAT ./scripts/checkpoint-blue-green-traffic.sh
PG_IMAGE="${PG_IMAGE:-postgres:16-alpine}"
BLUE_UNIT="fraisier-bgt-blue.service"
GREEN_UNIT="fraisier-bgt-green.service"
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()  { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# Log in to ghcr.io when $PG_IMAGE is a ghcr ref and a token is available, so the
# org's private mirror can be pulled (Docker-Hub-free, no 429). A no-op for
# Docker Hub images or when no token is set. The token is passed on stdin, never
# in argv. Usage: ghcr_login <runtime>
ghcr_login() {
  case "$PG_IMAGE" in ghcr.io/*) ;; *) return 0 ;; esac
  local token="${GHCR_TOKEN:-${GITHUB_TOKEN:-}}"
  [ -n "$token" ] || { say "ghcr: no GHCR_TOKEN/GITHUB_TOKEN; trying anonymous pull of $PG_IMAGE"; return 0; }
  if printf '%s' "$token" | "$1" login ghcr.io -u "${GHCR_USER:-${GITHUB_ACTOR:-x}}" --password-stdin >/dev/null; then
    say "ghcr: logged in"
  else
    die "ghcr.io login failed"
  fi
}

command -v "$PODMAN" >/dev/null || die "podman not on PATH"
command -v confiture >/dev/null || die "confiture not on PATH (>= 0.22)"
command -v python3 >/dev/null   || die "python3 not on PATH"
command -v curl >/dev/null      || die "curl not on PATH"
systemctl --user is-system-running >/dev/null 2>&1 \
  || die "no user systemd manager (need a logged-in session)"

UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
SC=(systemctl --user)
WORK="$(mktemp -d "${TMPDIR:-/tmp}/fraisier-bgt.XXXXXX")"

cleanup() {
  "${SC[@]}" stop "$BLUE_UNIT" "$GREEN_UNIT" >/dev/null 2>&1 || true
  rm -f "$UNIT_DIR/$BLUE_UNIT" "$UNIT_DIR/$GREEN_UNIT"
  "${SC[@]}" daemon-reload >/dev/null 2>&1 || true
  $PODMAN rm -f "$PG_NAME" "$LB_CTR" >/dev/null 2>&1 || true
  if [ "$KEEP" = 1 ]; then say "keeping $WORK (--keep)"; else rm -rf "$WORK"; fi
}
trap cleanup EXIT

mkdir -p "$WORK/nginx" "$WORK/build" "$WORK/staging" "$WORK/state" "$WORK/migrations" "$UNIT_DIR"
printf bg > "$WORK/build/app"

say "building fraisier"
( cd "$REPO_ROOT" && cargo build --bin fraisier >/dev/null 2>&1 ) || die "fraisier build failed"
FRAISIER="$REPO_ROOT/target/debug/fraisier"
[ -x "$FRAISIER" ] || die "fraisier binary missing"

# Blue-green requires confiture's first-class window_safe verdict (confiture#154
# Phase 3) — the happy swap needs window-safety to PASS, which needs window_safe.
# An older confiture returns no verdict and is refused, so the swap can't run.
probe="$WORK/probe"; mkdir -p "$probe"
printf 'CREATE TABLE _bg_probe (id int);\n' > "$probe/001_p.up.sql"
printf 'DROP TABLE _bg_probe;\n' > "$probe/001_p.down.sql"
CONFITURE_DATABASE_URL='postgresql://u@127.0.0.1:1/n?sslmode=disable' \
  confiture migrate preflight --no-config --format json --output "$probe/r.json" \
  --migrations-dir "$probe" >/dev/null 2>&1 || true
grep -q '"window_safe"' "$probe/r.json" 2>/dev/null \
  || die "this confiture does not emit window_safe — blue-green needs confiture#154 Phase 3 ($(confiture --version 2>&1 | head -1)). The traffic-tier gates are also proven hermetically in fraisier-core::blue_green."
ok "confiture emits window_safe (Phase 3)"

# --- the trivial blue/green app -------------------------------------------
cat > "$WORK/app.py" <<'PY'
import http.server, os, sys, time
PORT = int(sys.argv[1]); NAME = sys.argv[2]; START = time.time()
class H(http.server.BaseHTTPRequestHandler):
    def _sick(self):
        m = os.environ.get("MODE", "ok")
        return m == "sick" or (m == "flip" and time.time() - START > 6)
    def do_GET(self):
        code = 500 if self._sick() else 200
        self.send_response(code); self.end_headers(); self.wfile.write(NAME.encode())
    def log_message(self, *a): pass
# Threaded so concurrent probes (fraisier health polls + the checkpoint's curls)
# don't serialize and intermittently time out.
http.server.ThreadingHTTPServer(("0.0.0.0", PORT), H).serve_forever()
PY

install_unit() { # <unit> <port> <name> <mode>
  cat > "$UNIT_DIR/$1" <<EOF
[Unit]
Description=fraisier blue-green traffic fixture ($3)
[Service]
Environment=MODE=$4
ExecStart=$(command -v python3) $WORK/app.py $2 $3
Restart=no
EOF
  "${SC[@]}" daemon-reload
}

# --- nginx: active-upstream include the TrafficDirector repoints -----------
upstream_file() { # <name> <port>  -> writes $WORK/nginx/<name>.upstream.conf
  printf 'upstream checkout_upstream {\n    server 127.0.0.1:%s;\n}\n' "$2" \
    > "$WORK/nginx/$1.upstream.conf"
}
upstream_file blue "$BLUE_PORT"
ln -sfn "$WORK/nginx/blue.upstream.conf" "$WORK/nginx/active.upstream"

cat > "$WORK/nginx.conf" <<EOF
events {}
http {
    include $WORK/nginx/active.upstream;
    server {
        listen $LB_PORT;
        location / {
            proxy_pass http://checkout_upstream;
            proxy_connect_timeout 1s;
            proxy_next_upstream off;
        }
    }
}
EOF

# Reuse the multihost nginx image; build it if it is not cached.
if ! $PODMAN image exists "$NGINX_IMG" 2>/dev/null; then
  say "building nginx image $NGINX_IMG"
  $PODMAN build -q -t "$NGINX_IMG" -f "$FIXTURE/Containerfile.nginx" "$FIXTURE" >/dev/null
fi

# nginx on the host network; the include dir is mounted at its SAME absolute path
# so fraisier's absolute swap symlink resolves identically inside the container.
$PODMAN run -d --name "$LB_CTR" --network host \
  -v "$WORK/nginx.conf:/etc/nginx/nginx.conf:Z" \
  -v "$WORK/nginx:$WORK/nginx:Z" "$NGINX_IMG" >/dev/null \
  || die "could not start nginx container"

cat > "$WORK/nginx-reload" <<EOF
#!/bin/sh
exec $PODMAN exec $LB_CTR nginx "\$@"
EOF
chmod +x "$WORK/nginx-reload"
sleep 0.5
$PODMAN exec "$LB_CTR" nginx -t >/dev/null 2>&1 || die "nginx config invalid"
ok "nginx LB on :$LB_PORT (active upstream -> blue)"

# --- Postgres + confiture migration ---------------------------------------
say "starting throwaway Postgres ($PG_IMAGE)"
ghcr_login "$PODMAN"
$PODMAN run -d --name "$PG_NAME" -e POSTGRES_PASSWORD=bgpw \
  -p "$PG_PORT:5432" "$PG_IMAGE" >/dev/null || die "Postgres failed"
for _ in $(seq 1 40); do
  $PODMAN exec "$PG_NAME" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 0.5
done
$PODMAN exec "$PG_NAME" pg_isready -U postgres >/dev/null 2>&1 || die "Postgres did not come up"
DSN="postgresql://postgres:bgpw@127.0.0.1:$PG_PORT/postgres?sslmode=disable"
cat > "$WORK/migrations/001_create.up.sql"  <<'SQL'
CREATE TABLE notes (id bigserial PRIMARY KEY, body text NOT NULL);
SQL
cat > "$WORK/migrations/001_create.down.sql" <<'SQL'
DROP TABLE notes;
SQL
cat > "$WORK/migrations/002_add_label.up.sql"  <<'SQL'
ALTER TABLE notes ADD COLUMN label text;
SQL
cat > "$WORK/migrations/002_add_label.down.sql" <<'SQL'
ALTER TABLE notes DROP COLUMN label;
SQL
ok "Postgres ready + window-safe ADD COLUMN migration staged"

# --- fraisier.toml --------------------------------------------------------
write_config() { # <hold_secs>
  cat > "$WORK/fraisier.toml" <<EOF
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
unit = "$BLUE_UNIT"
user = true

[health]
adapter = "http"
url = "http://127.0.0.1:$BLUE_PORT/healthz"

[lb]
adapter = "nginx"
upstream = "checkout_upstream"
include_dir = "$WORK/nginx"

[blue_green]
green_unit = "$GREEN_UNIT"
green_health_url = "http://127.0.0.1:$GREEN_PORT/healthz"
green_servers = ["127.0.0.1:$GREEN_PORT"]
blue_servers = ["127.0.0.1:$BLUE_PORT"]
hold_secs = $1
EOF
}

served() { curl -fsS --max-time 4 "http://127.0.0.1:$LB_PORT/" 2>/dev/null || echo DOWN; }
# What a backend serves on its own port (decouples "is the fleet up" from routing).
backend() { curl -fsS --max-time 4 "http://127.0.0.1:$1/" 2>/dev/null || echo DOWN; }
# Poll until $1 is served (through the LB) or time out — absorbs startup/reload races.
wait_served() { # <name>
  for _ in $(seq 1 40); do [ "$(served)" = "$1" ] && return 0; sleep 0.25; done
  return 1
}
wait_backend() { # <port> <name>
  for _ in $(seq 1 40); do [ "$(backend "$1")" = "$2" ] && return 0; sleep 0.25; done
  return 1
}

DEPLOY_OUT=""
deploy() {
  set +e
  DEPLOY_OUT="$(BG_DATABASE_URL="$DSN" FRAISIER_NGINX_BIN="$WORK/nginx-reload" \
    "$FRAISIER" --json deploy --config "$WORK/fraisier.toml" \
    --state-dir "$WORK/state" --app-version 2.0.0)"
  set -e
}

# Reset to the steady state: blue live + serving, green stopped, nginx -> blue,
# a fresh saga state-dir.
reset_state() { # <green-mode> <hold-secs>
  write_config "$2"
  install_unit "$BLUE_UNIT" "$BLUE_PORT" blue ok
  install_unit "$GREEN_UNIT" "$GREEN_PORT" green "$1"
  "${SC[@]}" restart "$BLUE_UNIT"
  "${SC[@]}" stop "$GREEN_UNIT" >/dev/null 2>&1 || true
  # Confirm blue is actually serving on its own port BEFORE pointing nginx at it,
  # so a reload never races blue's restart (the cause of intermittent 502s).
  wait_backend "$BLUE_PORT" blue || die "blue did not come up on :$BLUE_PORT after restart"
  upstream_file blue "$BLUE_PORT"
  ln -sfn "$WORK/nginx/blue.upstream.conf" "$WORK/nginx/active.upstream"
  "$WORK/nginx-reload" -s reload >/dev/null 2>&1 || true
  rm -rf "$WORK/state"; mkdir -p "$WORK/state"
  wait_served blue || die "fixture not in the blue steady state (served: $(served))"
}

# ==========================================================================
say "BLUE-GREEN TRAFFIC — real nginx routing (user systemd, zero spend)"

# --- Gate: happy swap blue -> green -----------------------------------------
say "gate: healthy green -> traffic SWAPS blue->green"
reset_state ok 4
deploy
echo "$DEPLOY_OUT"
echo "$DEPLOY_OUT" | grep -q '"outcome": *"committed"' || die "happy deploy must commit:\n$DEPLOY_OUT"
wait_served green || die "nginx must serve green after the swap (served: $(served))"
"${SC[@]}" is-active "$BLUE_UNIT" >/dev/null 2>&1 && die "blue must be reaped after commit"
ok "traffic swapped to green; deploy committed; blue reaped"

# --- Gate: pre-swap health gate (green sick) -> traffic never moves ---------
say "gate: sick green -> pre-swap health gate, traffic NEVER moves"
reset_state sick 4
deploy
echo "$DEPLOY_OUT"
echo "$DEPLOY_OUT" | grep -q "step 'health-gate-green'" \
  || die "a sick green must fail at the pre-swap health gate:\n$DEPLOY_OUT"
wait_served blue || die "traffic must NOT move when green fails the gate (served: $(served))"
ok "pre-swap health gate held the line; nginx still serves blue"

# --- Gate: post-swap degradation -> swap back to still-hot blue ------------
say "gate: green degrades during hold -> swap BACK to still-hot blue"
reset_state flip 20
deploy
echo "$DEPLOY_OUT"
echo "$DEPLOY_OUT" | grep -q "step 'hold'" \
  || die "a green that degrades in the hold window must fail at 'hold':\n$DEPLOY_OUT"
wait_served blue || die "traffic must swap back to blue (served: $(served))"
"${SC[@]}" is-active "$BLUE_UNIT" >/dev/null 2>&1 || die "blue must still be hot for the swap-back"
ok "green degradation swapped traffic back to still-hot blue"

say "BLUE-GREEN TRAFFIC gates: PASS (real nginx, real swap)"
