#!/usr/bin/env bash
# Runs ON the Hetzner host (root, pid-1 system systemd). Self-contained:
# starts a throwaway Postgres container, sets up the systemd unit + artifact server,
# and drives fraisier's deploy saga to deploy the real fraiseql-server v2.4.0 against
# the confiture-migrated ecommerce schema 3x consecutively (each committed + /health 200).
# Confiture (>=0.22), docker, python3 are pre-installed by the orchestrator.
set -uo pipefail
BASE=/root/criterion1
FRAISIER=$BASE/fraisier
SERVER=$BASE/fraiseql-server
SCHEMA=$BASE/schema.compiled.json
SRVCONF=$BASE/server.config.toml
SRC_MIGR=$BASE/migrations
HEALTH_PORT=8815
ARTIFACT_PORT=8761
UNIT=fraiseql-ecom.service
UNIT_DIR=/etc/systemd/system
PG_NAME=criterion1-pg
PG_PORT=5432

say(){ printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok(){ printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
die(){ printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

[ -d /run/systemd/system ] || die "pid 1 is not systemd"
[ "$(id -u)" = 0 ] || die "must run as root"
for f in "$FRAISIER" "$SERVER" "$SCHEMA" "$SRVCONF"; do [ -e "$f" ] || die "missing $f"; done
chmod +x "$FRAISIER" "$SERVER"

WORK="$(mktemp -d)"
ARTIFACT_PID=""
cleanup(){ set +e
  systemctl stop "$UNIT" >/dev/null 2>&1; rm -f "$UNIT_DIR/$UNIT"; systemctl daemon-reload >/dev/null 2>&1
  [ -n "$ARTIFACT_PID" ] && kill "$ARTIFACT_PID" >/dev/null 2>&1
  docker rm -f "$PG_NAME" >/dev/null 2>&1
  rm -rf "$WORK"
}
trap cleanup EXIT
systemctl stop "$UNIT" >/dev/null 2>&1 || true

mkdir -p "$WORK/www" "$WORK/migrations" "$WORK/staging" "$WORK/state"
cp "$SRC_MIGR"/*.sql "$WORK/migrations/"

# Throwaway Postgres (container), published on loopback only
say "starting Postgres container"
docker rm -f "$PG_NAME" >/dev/null 2>&1 || true
docker run -d --name "$PG_NAME" -e POSTGRES_PASSWORD=ecompw -e POSTGRES_DB=postgres \
  -p 127.0.0.1:$PG_PORT:5432 postgres:16 >/dev/null
for _ in $(seq 1 30); do docker exec "$PG_NAME" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
docker exec "$PG_NAME" pg_isready -U postgres >/dev/null 2>&1 || die "Postgres did not come up"
docker exec "$PG_NAME" psql -U postgres -q -c "DROP DATABASE IF EXISTS ecom;" >/dev/null
docker exec "$PG_NAME" psql -U postgres -q -c "CREATE DATABASE ecom;" >/dev/null
DSN="postgresql://postgres:ecompw@127.0.0.1:$PG_PORT/ecom?sslmode=disable"
ok "Postgres ready (database ecom)"

# system systemd unit: the real fraiseql-server v2.4 (fixed infra). ExecStartPost blocks
# until /health 200 so fraisier's restart step returns only once the server is serving.
cat > "$UNIT_DIR/$UNIT" <<EOF
[Unit]
Description=fraiseql-server v2.4 (criterion-1 Hetzner)

[Service]
Type=simple
Environment=DATABASE_URL=$DSN
Environment=FRAISEQL_ENV=production
ExecStart=$SERVER --schema-path $SCHEMA --config $SRVCONF --bind-addr 127.0.0.1:$HEALTH_PORT
ExecStartPost=/bin/sh -c 'for i in \$(seq 1 60); do curl -sf http://127.0.0.1:$HEALTH_PORT/health >/dev/null && exit 0; sleep 0.5; done; exit 1'
Restart=no
TimeoutStartSec=60

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
ok "installed system unit $UNIT"

mint_release(){ printf 'fraiseql-%s-payload' "$1" > "$WORK/www/app-$1.tar.gz"
  ( cd "$WORK/www" && sha256sum "app-$1.tar.gz" > "app-$1.tar.gz.sha256" ); }
python3 -m http.server --directory "$WORK/www" "$ARTIFACT_PORT" >/dev/null 2>&1 &
ARTIFACT_PID=$!; sleep 0.3
kill -0 "$ARTIFACT_PID" 2>/dev/null || die "artifact server failed"
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

[health]
adapter = "http"
url = "http://127.0.0.1:$HEALTH_PORT/health"
expected_status = 200
EOF
ok "authored fraisier.toml"

deploy(){ ECOM_DATABASE_URL="$DSN" "$FRAISIER" --json deploy \
  --config "$WORK/fraisier.toml" --state-dir "$WORK/state" --app-version "$1"; }
health_code(){ curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$HEALTH_PORT/health"; }
assert_committed(){ echo "$2" | grep -q '"outcome": *"committed"' || die "deploy $1 did not commit:
$2"; }

say "CRITERION 1 (Hetzner pid-1) — 3 consecutive production deploys of fraiseql-server v2.4.0"
mint_release v2.4.0
for i in 1 2 3; do
  out="$(deploy v2.4.0)"; rc=$?
  [ $rc -eq 0 ] || { echo "$out"; die "deploy $i exited $rc"; }
  assert_committed "$i" "$out"
  [ "$(health_code)" = "200" ] || die "deploy $i: /health not 200 (got $(health_code))"
  rev="$(CONFITURE_DATABASE_URL="$DSN" confiture migrate current --no-config 2>/dev/null | tail -1)"
  ok "deploy $i/3 committed; /health 200; confiture revision $rev"
done
echo "CRITERION1_HETZNER_PASSED"
