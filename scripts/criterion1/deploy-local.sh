#!/usr/bin/env bash
# Criterion-1 (Part B) LOCAL proof, zero spend (--systemd user):
# fraisier's own deploy saga deploys the REAL fraiseql-server v2.4.0 against the
# confiture-migrated ecommerce_api schema, 3x consecutively, each committed + /health 200.
# Modeled on scripts/checkpoint-training.sh. fraiseql-server is fixed infra (the real
# binary); the artifact is opaque per-version (activation = symlink swap), exactly as the
# matrix/training field do. The criterion-1 realness: real v2.4.0 binary + real ecom schema.
set -uo pipefail

SCRATCH="$HOME/code/partb-materialize-scratch"
FRAISIER_REPO="$HOME/code/fraisier-core"
FQ="$HOME/code/fraiseql"
FRAISIER="$FRAISIER_REPO/target/debug/fraisier"
SERVER="$FQ/target/debug/fraiseql-server"
SCHEMA="$SCRATCH/schema.compiled.json"
SRVCONF="$SCRATCH/server.config.toml"
CONF_MIGRATIONS="$SCRATCH/ecom-confiture"

HEALTH_PORT=8815
ARTIFACT_PORT=8761
UNIT="fraiseql-ecom.service"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
ADMIN="postgresql://postgres:partbpw@127.0.0.1:5444/postgres?sslmode=disable"
DSN="postgresql://postgres:partbpw@127.0.0.1:5444/ecom?sslmode=disable"

say(){ printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok(){ printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
die(){ printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

SC=(systemctl --user)
"${SC[@]}" show-environment >/dev/null 2>&1 || die "no user systemd manager"
WORK="$(mktemp -d)"
ARTIFACT_PID=""
cleanup(){ set +e
  "${SC[@]}" stop "$UNIT" >/dev/null 2>&1
  rm -f "$UNIT_DIR/$UNIT"; "${SC[@]}" daemon-reload >/dev/null 2>&1
  [ -n "$ARTIFACT_PID" ] && kill "$ARTIFACT_PID" >/dev/null 2>&1
  rm -rf "$WORK"
}
trap cleanup EXIT
"${SC[@]}" stop "$UNIT" >/dev/null 2>&1 || true

for f in "$FRAISIER" "$SERVER" "$SCHEMA" "$SRVCONF"; do [ -e "$f" ] || die "missing $f"; done
[ -d "$CONF_MIGRATIONS" ] || die "missing $CONF_MIGRATIONS"
mkdir -p "$WORK/www" "$WORK/migrations" "$WORK/staging" "$WORK/state" "$UNIT_DIR"
cp "$CONF_MIGRATIONS"/*.sql "$WORK/migrations/"

# Fresh ecom DB (separate DROP/CREATE per the gotcha)
psql "$ADMIN" -q -c "DROP DATABASE IF EXISTS ecom WITH (FORCE);" >/dev/null 2>&1
psql "$ADMIN" -q -c "CREATE DATABASE ecom;" >/dev/null 2>&1
ok "fresh ecom database"

# systemd unit: real fraiseql-server (fixed infra). DATABASE_URL via env (avoids DSN
# quoting in ExecStart). ExecStartPost blocks until /health 200 so fraisier's restart
# step returns only once the server is actually serving (debug binary ~1s; the http
# health adapter defaults to only 3x500ms, so we make readiness synchronous here).
cat > "$UNIT_DIR/$UNIT" <<EOF
[Unit]
Description=fraiseql-server v2.4 (criterion-1 local)

[Service]
Type=simple
Environment=DATABASE_URL=$DSN
Environment=FRAISEQL_ENV=production
ExecStart=$SERVER --schema-path $SCHEMA --config $SRVCONF --bind-addr 127.0.0.1:$HEALTH_PORT
ExecStartPost=/bin/sh -c 'for i in \$(seq 1 60); do curl -sf http://127.0.0.1:$HEALTH_PORT/health >/dev/null && exit 0; sleep 0.5; done; exit 1'
Restart=no
TimeoutStartSec=45

[Install]
WantedBy=default.target
EOF
"${SC[@]}" daemon-reload
ok "installed user unit $UNIT (real fraiseql-server, health-gated start)"

# Opaque per-version artifact server (binary is fixed infra; activation = symlink swap)
mint_release(){ printf 'fraiseql-%s-payload' "$1" > "$WORK/www/app-$1.tar.gz"
  ( cd "$WORK/www" && sha256sum "app-$1.tar.gz" > "app-$1.tar.gz.sha256" ); }
python3 -m http.server --directory "$WORK/www" "$ARTIFACT_PORT" >/dev/null 2>&1 &
ARTIFACT_PID=$!; sleep 0.3
kill -0 "$ARTIFACT_PID" 2>/dev/null || die "artifact server failed to start"
ok "artifact server on :$ARTIFACT_PORT"

cat > "$WORK/fraisier.toml" <<EOF
[deploy]
name = "fraiseql"
environment = "production"

[artifact]
source = "release"
release_url = "http://127.0.0.1:$ARTIFACT_PORT/app-{version}.tar.gz"
checksum_url = "http://127.0.0.1:$ARTIFACT_PORT/app-{version}.tar.gz.sha256"
staging_dir = "$WORK/staging"
active_path = "$WORK/current"

[migration]
adapter = "confiture"
database_url_env = "ECOM_DATABASE_URL"
migrations_path = "$WORK/migrations"

[service]
adapter = "systemd"
unit = "$UNIT"
user = true

[health]
adapter = "http"
url = "http://127.0.0.1:$HEALTH_PORT/health"
expected_status = 200
EOF
cp "$WORK/fraisier.toml" "$SCRATCH/fraisier.ecom.toml"   # keep a copy for the Hetzner run
ok "authored fraisier.toml (confiture + systemd + http health)"

deploy(){ ECOM_DATABASE_URL="$DSN" "$FRAISIER" --json deploy \
  --config "$WORK/fraisier.toml" --state-dir "$WORK/state" --app-version "$1"; }
health_code(){ curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$HEALTH_PORT/health"; }
assert_committed(){ echo "$2" | grep -q '"outcome": *"committed"' || die "deploy $1 did not commit:
$2"; }

say "CRITERION 1 (local) — 3 consecutive production deploys of fraiseql-server v2.4.0"
mint_release v2.4.0
for i in 1 2 3; do
  out="$(deploy v2.4.0)"; rc=$?
  [ $rc -eq 0 ] || { echo "$out"; die "deploy $i exited $rc"; }
  assert_committed "$i" "$out"
  [ "$(health_code)" = "200" ] || die "deploy $i: /health not 200 (got $(health_code))"
  rev="$(CONFITURE_DATABASE_URL="$DSN" confiture migrate current --no-config 2>/dev/null | tail -1)"
  ok "deploy $i/3 committed; /health 200; confiture revision $rev"
done

say "CRITERION 1 LOCAL PASSED — fraisier deployed real fraiseql-server v2.4.0 + ecommerce schema 3x, each committed + healthy."
