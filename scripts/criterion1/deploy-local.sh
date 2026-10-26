#!/usr/bin/env bash
# Criterion-1 (Part B) LOCAL proof, zero spend (--systemd user):
# fraisier's own deploy saga deploys the REAL fraiseql-server, at the version pinned in
# tools/fraiseql-version, against the confiture-migrated ecommerce_api schema, 3x
# consecutively, each committed + /health 200.
# Modeled on scripts/checkpoint-training.sh. fraiseql-server is fixed infra (the real
# binary); the artifact is opaque per-version (activation = symlink swap), exactly as the
# matrix/training field do. The criterion-1 realness: the real pinned binary + real ecom
# schema.
#
# Inputs (environment):
#   FRAISEQL_SERVER  the fraiseql-server binary (default: ~/code/fraiseql's release build).
#                    Its --version must match the pin.
#   FRAISIER_BIN     the fraisier binary (default: this repo's debug build).
#   C1_PGHOST, C1_PGPORT, C1_PGUSER, C1_PGPASSWORD
#                    the Postgres server (default: localhost:5432 as $USER, no password).
#   C1_DB            the scratch database, recreated on every run. It must start with
#                    fraisier_criterion1_ (default: fraisier_criterion1_ecom).
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
FRAISIER_REPO="$(cd "$HERE/../.." && pwd)"
FRAISEQL_VERSION="$(tr -d '[:space:]' < "$FRAISIER_REPO/tools/fraiseql-version")"
FRAISIER="${FRAISIER_BIN:-$FRAISIER_REPO/target/debug/fraisier}"
SERVER="${FRAISEQL_SERVER:-$HOME/code/fraiseql/target/release/fraiseql-server}"
SCHEMA="$HERE/schema.compiled.json"
SRVCONF="$HERE/server.config.toml"
CONF_MIGRATIONS="$HERE/ecom-confiture"

HEALTH_PORT=8815
ARTIFACT_PORT=8761
UNIT="fraiseql-ecom.service"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
C1_PGHOST="${C1_PGHOST:-localhost}"
C1_PGPORT="${C1_PGPORT:-5432}"
C1_PGUSER="${C1_PGUSER:-$USER}"
C1_PGPASSWORD="${C1_PGPASSWORD:-}"
C1_DB="${C1_DB:-fraisier_criterion1_ecom}"

say(){ printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok(){ printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
die(){ printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# The database is dropped on every run, so only a name that says it is scratch is accepted.
case "$C1_DB" in fraisier_criterion1_*) ;; *) die "C1_DB must start with fraisier_criterion1_ (got $C1_DB)" ;; esac
case "$C1_DB" in *[!a-z0-9_]*) die "C1_DB may hold only a-z, 0-9 and _ (got $C1_DB)" ;; esac

# psql against the maintenance database. The connection travels in PG* variables, never argv.
admin_psql(){ PGHOST="$C1_PGHOST" PGPORT="$C1_PGPORT" PGUSER="$C1_PGUSER" PGPASSWORD="$C1_PGPASSWORD" \
  PGDATABASE=postgres psql -X -q -v ON_ERROR_STOP=1 "$@"; }
if [ -n "$C1_PGPASSWORD" ]; then
  DSN="postgresql://$C1_PGUSER:$C1_PGPASSWORD@$C1_PGHOST:$C1_PGPORT/$C1_DB?sslmode=disable"
else
  DSN="postgresql://$C1_PGUSER@$C1_PGHOST:$C1_PGPORT/$C1_DB?sslmode=disable"
fi

SC=(systemctl --user)
"${SC[@]}" show-environment >/dev/null 2>&1 || die "no user systemd manager"
for f in "$FRAISIER" "$SERVER" "$SCHEMA" "$SRVCONF"; do [ -e "$f" ] || die "missing $f"; done
[ -d "$CONF_MIGRATIONS" ] || die "missing $CONF_MIGRATIONS"
served="$("$SERVER" --version 2>&1 | head -1)"
[ "$served" = "fraiseql-server ${FRAISEQL_VERSION#v}" ] \
  || die "$SERVER reports '$served'; tools/fraiseql-version pins $FRAISEQL_VERSION"
ok "fraiseql-server $FRAISEQL_VERSION (the pin)"

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

mkdir -p "$WORK/www" "$WORK/migrations" "$WORK/staging" "$WORK/state" "$UNIT_DIR"
cp "$CONF_MIGRATIONS"/*.sql "$WORK/migrations/"

# Fresh scratch database (separate DROP/CREATE: CREATE DATABASE cannot share a transaction)
admin_psql -c "DROP DATABASE IF EXISTS $C1_DB WITH (FORCE);" || die "could not drop $C1_DB"
admin_psql -c "CREATE DATABASE $C1_DB;" || die "could not create $C1_DB"
ok "fresh $C1_DB database"

# systemd unit: real fraiseql-server (fixed infra). DATABASE_URL via env (avoids DSN
# quoting in ExecStart, and keeps it off argv). FRAISEQL_ENV is explicit, so the
# production boot checks run. ExecStartPost blocks until /health answers so fraisier's
# restart step returns only once the server is actually serving (the http health adapter
# defaults to only 3x500ms, so we make readiness synchronous here).
cat > "$UNIT_DIR/$UNIT" <<EOF
[Unit]
Description=fraiseql-server $FRAISEQL_VERSION (criterion-1 local)

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
ok "authored fraisier.toml (confiture + systemd + http health)"

deploy(){ ECOM_DATABASE_URL="$DSN" "$FRAISIER" --json deploy \
  --config "$WORK/fraisier.toml" --state-dir "$WORK/state" --app-version "$1"; }
health_code(){ curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$HEALTH_PORT/health"; }
assert_committed(){ echo "$2" | grep -q '"outcome": *"committed"' || die "deploy $1 did not commit:
$2"; }

say "CRITERION 1 (local) — 3 consecutive production deploys of fraiseql-server $FRAISEQL_VERSION"
mint_release "$FRAISEQL_VERSION"
for i in 1 2 3; do
  out="$(deploy "$FRAISEQL_VERSION")"; rc=$?
  [ $rc -eq 0 ] || { echo "$out"; die "deploy $i exited $rc"; }
  assert_committed "$i" "$out"
  [ "$(health_code)" = "200" ] || die "deploy $i: /health not 200 (got $(health_code))"
  rev="$(CONFITURE_DATABASE_URL="$DSN" confiture migrate current --no-config 2>/dev/null | tail -1)"
  ok "deploy $i/3 committed; /health 200; confiture revision $rev"
done

say "CRITERION 1 LOCAL PASSED — fraisier deployed real fraiseql-server $FRAISEQL_VERSION + ecommerce schema 3x, each committed + healthy."
