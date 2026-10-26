#!/usr/bin/env bash
# Stack e2e, host setup: run as root on the throwaway host (see README.md).
# Every database object and file it touches is scratch on that host; a re-run
# starts over. It compiles the subgraph schemas and prints their SDLs; the
# supergraphs are composed from those (README.md), then e2e.sh runs.
set -euo pipefail
F="$(cd "$(dirname "$0")" && pwd)"
W="${WORK:-/var/tmp/fraisier-stack-e2e}"
E=/srv/erp
P(){ runuser -u postgres -- psql -X -q -v ON_ERROR_STOP=1 "$@"; }

systemctl stop erp-users erp-orders erp-gateway 2>/dev/null || true
P -d erp -c 'DROP VIEW IF EXISTS v_user, v_order; DROP TABLE IF EXISTS tb_user, tb_order'
rm -rf "$W/o" "$W/o.json"

P -c "DO \$\$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname='erp') THEN CREATE ROLE erp LOGIN PASSWORD 'erp-pw'; END IF; END \$\$"
P -d erp -c 'GRANT ALL ON SCHEMA public TO erp'
# Readiness reads shared_preload_libraries to see pg_tviews preloaded.
P -c 'GRANT pg_read_all_settings TO erp'
mkdir -p /etc/erp; umask 077
printf 'DATABASE_URL=postgresql://erp:erp-pw@127.0.0.1/erp\n' > /etc/erp/db.env
umask 022

rm -rf "$E"; mkdir -p "$E"/{src,conf,tools,state} "$E"/src/gateway/sdl
cp "$F/migrate.py" "$E/tools/"
mkdir -p "$W/out"
for v in users-v1 users-v2 orders-v1; do
  fraiseql-cli compile "$F/$v.schema.json" -o "$W/out/$v.compiled.json" >/dev/null
  fraiseql-cli federation sdl "$W/out/$v.compiled.json" -o "$W/out/$v.graphql"
done
release(){ # <dir> <variant>
  rm -rf "$1"; mkdir -p "$1"
  cp "$W/out/$2.compiled.json" "$1/schema.compiled.json"
  cp "$F/server.config.toml" "$1/"
  cp "$F/$2.up.sql" "$1/up.sql"
}
release "$E/src/users" users-v1
release "$E/src/orders" orders-v1
cp "$W"/out/users-v1.graphql "$E/src/gateway/sdl/users.graphql"
cp "$W"/out/orders-v1.graphql "$E/src/gateway/sdl/orders.graphql"
cp "$F/router.config.yaml" "$E/src/gateway/"

unit(){ # <name> <port>
cat > "/etc/systemd/system/erp-$1.service" <<UNIT
[Unit]
Description=erp $1 subgraph (fraiseql-server)
[Service]
EnvironmentFile=/etc/erp/db.env
Environment=FRAISEQL_ENV=production
ExecStart=/usr/local/bin/fraiseql-server --schema-path $E/$1/current/schema.compiled.json --config $E/$1/current/server.config.toml --bind-addr 127.0.0.1:$2
ExecStartPost=/bin/sh -c 'for i in \$(seq 1 60); do curl -sf http://127.0.0.1:$2/health >/dev/null && exit 0; sleep 0.5; done; exit 1'
UNIT
}
unit users 8101
unit orders 8102
cat > /etc/systemd/system/erp-gateway.service <<UNIT
[Unit]
Description=erp gateway (Hive Router)
[Service]
WorkingDirectory=$E/gateway/current
ExecStart=/usr/local/bin/hive_router
ExecStartPost=/bin/sh -c 'for i in \$(seq 1 60); do curl -sf http://127.0.0.1:4000/readiness >/dev/null && exit 0; sleep 0.5; done; exit 1'
UNIT
systemctl daemon-reload

member(){ # <name> <port>
cat > "$E/conf/$1.staging.toml" <<TOML
[deploy]
name = "$1"
environment = "staging"

[artifact]
source = "local"
path = "$E/src/$1"
active_path = "$E/$1/current"
staging_dir = "$E/$1/releases"

[migration]
adapter = "command"
database_url_env = "ERP_DSN"
preflight_mode = "off"

[migration.settings.commands]
up = "python3 $E/tools/migrate.py $E/src/$1/up.sql"
current_revision = "echo r1"
down_to = "true"

[database.requires]
postgres_major = 18
pg_tviews = true

[service]
adapter = "systemd"
unit = "erp-$1.service"

[health]
adapter = "http"
url = "http://127.0.0.1:$2/health"
TOML
}
member users 8101
member orders 8102
cat > "$E/conf/gateway.staging.toml" <<TOML
[deploy]
name = "gateway"
environment = "staging"

[artifact]
source = "local"
path = "$E/src/gateway"
active_path = "$E/gateway/current"
staging_dir = "$E/gateway/releases"

[migration]
adapter = "command"
preflight_mode = "off"

# A constant revision: with none ("true" prints nothing), every failed router
# deploy ends partial_rollback, since migrate cannot undo an "initial" migration.
[migration.settings.commands]
current_revision = "echo r0"
up = "true"
down_to = "true"

[service]
adapter = "systemd"
unit = "erp-gateway.service"

# /readiness is 503 until the router has a usable supergraph; /health is not.
[health]
adapter = "http"
url = "http://127.0.0.1:4000/readiness"
TOML
cat > "$E/conf/stack.staging.toml" <<TOML
[stack]
name = "erp"
environment = "staging"
router = "gateway"

[[stack.member]]
config = "users.staging.toml"
sdl = "sdl/users.graphql"
graphql_url = "http://127.0.0.1:8101/graphql"

[[stack.member]]
config = "orders.staging.toml"
sdl = "sdl/orders.graphql"
graphql_url = "http://127.0.0.1:8102/graphql"

[[stack.member]]
config = "gateway.staging.toml"
depends_on = ["users", "orders"]
TOML
echo "setup done; compose $W/supergraph-v1.graphql and -v2 from $W/out/*.graphql"
