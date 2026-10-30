#!/usr/bin/env bash
# The `fraisier db refresh` acceptance checks. Runs as root on a throwaway
# Debian host that has PostgreSQL 18 + pg_tviews (provision-pg18-tviews.sh) and
# `fraisier` on PATH; scripts/checkpoint-refresh.sh drives it. Every database,
# role, user and unit here is created by this script on that throwaway host.
#
#   checks.sh setup    # roles, databases, units, configs
#   checks.sh before   # everything up to and including a refresh that sets the flag
#   checks.sh after    # after a reboot: the flag still holds, then --clear-flag
#
# Each check prints PASS or FAIL; the last line counts them.
set -uo pipefail

W=/root/refresh-e2e
STATE=/var/lib/fraisier/state
CONFIG=/etc/fraisier/checkout.toml
FLAG=/etc/checkout/staging.refreshed.env
RESULTS=$W/results
APP_PW=app-pw
ADMIN_PW=admin-pw

pass() { printf 'PASS %s\n' "$*" | tee -a "$RESULTS"; }
fail() { printf 'FAIL %s\n' "$*" | tee -a "$RESULTS"; }
check() { # <description> <command...>: PASS when the command succeeds
  local what=$1; shift
  if "$@"; then pass "$what"; else fail "$what"; fi
}
say() { printf '\n=== %s\n' "$*"; }
P() { runuser -u postgres -- psql -X -q -v ON_ERROR_STOP=1 "$@"; }
Q() { runuser -u postgres -- psql -X -Atq -d "$1" -c "$2"; }
A() { PGPASSWORD=$APP_PW psql -X -q -v ON_ERROR_STOP=1 -h 127.0.0.1 -U app -d checkout_staging "$@"; }

# Run `$@` the way an operator runs fraisier: the shared and the ops env loaded.
as_operator() {
  (
    set -a
    # shellcheck source=/dev/null
    . /etc/checkout/staging.env
    # shellcheck source=/dev/null
    . /etc/checkout/staging.ops.env
    set +a
    cd / && "$@"
  )
}
F() { as_operator fraisier "$@"; }
refresh() { F db refresh --config "$CONFIG" --state-dir "$STATE" "$@" > "$W/out" 2>&1; local rc=$?; cat "$W/out"; return "$rc"; }
out_has() { grep -q -- "$1" "$W/out"; }
rows() { Q checkout_staging "SELECT string_agg(email, ',' ORDER BY pk_user) FROM tb_user"; }
active() { systemctl is-active -q "$1"; }
all_running() { active checkout-staging.service && active checkout-staging-worker.service && active checkout-staging-beat.timer; }
none_running() { ! active checkout-staging.service && ! active checkout-staging-worker.service && ! active checkout-staging-beat.timer; }
environ_has() { # <unit> <NAME=value>
  local pid
  pid=$(systemctl show -p MainPID --value "$1")
  [ "$pid" != 0 ] && tr '\0' '\n' < "/proc/$pid/environ" | grep -qx "$2"
}

# The [refresh] config, with `$1` as the sanitize lines and `$2` as extra
# [deploy] lines (the production key).
write_config() {
  local sanitize=$1 deploy=${2-production = false}
  cat > "$CONFIG" <<TOML
[deploy]
name = "checkout"
environment = "staging"
$deploy

[service]
adapter = "systemd"
unit = "checkout-staging.service"

[database]
admin_url_env = "CHECKOUT_ADMIN_URL"

[migration]
adapter = "command"
database_url_env = "CHECKOUT_DATABASE_URL"

[migration.settings.commands]
up = "python3 $W/migrate.py"
current_revision = "echo r1"

[notify]
command = "cat >> $W/notified.jsonl; echo >> $W/notified.jsonl"

[refresh]
source_dir = "/var/backups/checkout"
source_environment = "production"
units = ["checkout-staging-worker.service", "checkout-staging-beat.timer"]
min_tables = 3
schedule = "weekly Mon 05:00"
$sanitize
TOML
}
GOOD_SANITIZE="sanitize = \"$W/sanitize.sh\""

setup() {
  say "setup: roles, databases, units"
  rm -rf "$W" && mkdir -p "$W" /etc/fraisier /etc/checkout /var/backups/checkout "$STATE"
  chmod 755 /etc/checkout
  : > "$RESULTS"
  id checkout >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin checkout
  P -c "ALTER ROLE postgres PASSWORD '$ADMIN_PW'"
  P -c "DROP ROLE IF EXISTS app" -c "DROP ROLE IF EXISTS prod_app" 2>/dev/null
  P -c "CREATE ROLE prod_app LOGIN PASSWORD 'prod-pw'" -c "CREATE ROLE app LOGIN PASSWORD '$APP_PW'"
  P -c "CREATE DATABASE checkout_production OWNER prod_app" -c "CREATE DATABASE checkout_staging OWNER app"
  P -d checkout_production -c 'CREATE EXTENSION pg_tviews' -c 'GRANT USAGE ON SCHEMA tviews TO prod_app'
  PGPASSWORD=prod-pw psql -X -q -v ON_ERROR_STOP=1 -h 127.0.0.1 -U prod_app -d checkout_production <<'SQL'
CREATE TABLE tb_user (pk_user int PRIMARY KEY, email text NOT NULL);
INSERT INTO tb_user VALUES (1, 'ada@customer.example'), (2, 'grace@customer.example');
CREATE TABLE tb_setting (key text PRIMARY KEY, value text NOT NULL);
INSERT INTO tb_setting VALUES ('webhook_url', 'https://partner.example/hook');
CREATE TABLE tb_item (pk_item int PRIMARY KEY, id uuid NOT NULL DEFAULT gen_random_uuid(), name text NOT NULL);
INSERT INTO tb_item (pk_item, name) VALUES (1, 'one'), (2, 'two');
CREATE TABLE tv_item AS SELECT pk_item, id, jsonb_build_object('name', name) AS data FROM tb_item;
SQL
  A -c "CREATE TABLE tb_user (pk_user int PRIMARY KEY, email text); INSERT INTO tb_user VALUES (9, 'staging-only')"
  P -c "GRANT SET ON PARAMETER fraisier.refresh_target TO app"

  printf 'CHECKOUT_DATABASE_URL=postgres://app:%s@127.0.0.1/checkout_staging\nPGHOST=127.0.0.1\nPGUSER=app\nPGPASSWORD=%s\nPGDATABASE=checkout_staging\n' "$APP_PW" "$APP_PW" > /etc/checkout/staging.env
  printf 'CHECKOUT_ADMIN_URL=postgres://postgres:%s@127.0.0.1/postgres\n' "$ADMIN_PW" > /etc/checkout/staging.ops.env
  chmod 0640 /etc/checkout/staging.env /etc/checkout/staging.ops.env
  chgrp checkout /etc/checkout/staging.env
  printf 'CHECKOUT_DATABASE_URL=postgres://prod_app:prod-pw@127.0.0.1/checkout_production\n' > "$W/production.env"
  cat > "$W/production.toml" <<TOML
[deploy]
name = "checkout"
environment = "production"
production = true
[migration]
adapter = "command"
database_url_env = "CHECKOUT_DATABASE_URL"
[backup]
dir = "/var/backups/checkout"
TOML

  # The app: a web server. The worker: an idle session held open on the
  # database, which a refresh that forgot it would trip over (an idle backend
  # exits as soon as its client does). The beat: a timer.
  cat > /etc/systemd/system/checkout-staging.service <<UNIT
[Service]
User=checkout
Group=checkout
ExecStart=/usr/bin/python3 -m http.server 18081 --bind 127.0.0.1 --directory /tmp
EnvironmentFile=-/etc/checkout/staging.env
EnvironmentFile=-$FLAG
[Install]
WantedBy=multi-user.target
UNIT
  cat > /etc/systemd/system/checkout-staging-worker.service <<'UNIT'
[Service]
User=checkout
Group=checkout
ExecStart=/bin/sh -c 'sleep infinity | exec /usr/bin/psql -X -q'
Restart=on-failure
EnvironmentFile=-/etc/checkout/staging.env
[Install]
WantedBy=multi-user.target
UNIT
  cat > /etc/systemd/system/checkout-staging-beat.service <<'UNIT'
[Service]
Type=oneshot
User=checkout
ExecStart=/bin/true
EnvironmentFile=-/etc/checkout/staging.env
UNIT
  cat > /etc/systemd/system/checkout-staging-beat.timer <<'UNIT'
[Timer]
OnCalendar=hourly
[Install]
WantedBy=timers.target
UNIT

  # The migration maps the DSN to PG* variables (never argv) and dumps its env.
  cat > "$W/migrate.py" <<'PY'
import os, subprocess, urllib.parse
with open("/root/refresh-e2e/migrate.env", "w") as f:
    f.write("\n".join(f"{k}={v}" for k, v in os.environ.items()))
u = urllib.parse.urlparse(os.environ["DATABASE_URL"])
env = dict(os.environ, PGHOST=u.hostname, PGUSER=u.username, PGPASSWORD=u.password or "", PGDATABASE=u.path.lstrip("/"))
subprocess.run(["psql", "-X", "-q", "-v", "ON_ERROR_STOP=1", "-c", "CREATE TABLE IF NOT EXISTS tb_migrated (id int)"], env=env, check=True)
PY
  # The sanitize step: dumps its env, then scrubs in one transaction.
  cat > "$W/sanitize.sql" <<'SQL'
UPDATE tb_user SET email = 'user' || pk_user || '@example.invalid';
UPDATE tb_setting SET value = 'http://127.0.0.1:9/disabled' WHERE key = 'webhook_url';
SQL
  cat > "$W/sanitize.sh" <<SH
#!/bin/sh
env > $W/sanitize.env
exec psql -X -q -1 -v ON_ERROR_STOP=1 -f $W/sanitize.sql
SH
  printf "UPDATE tb_user SET email = 'scrubbed';\nSELECT no_such_column FROM tb_user;\n" > "$W/bad.sql"
  chmod 0700 "$W/sanitize.sh"; chmod 0600 "$W/sanitize.sql" "$W/bad.sql"

  write_config "$GOOD_SANITIZE"
  # The drop-ins for the worker and the beat come from `fraisier scaffold`.
  (cd "$W" && F scaffold --config "$CONFIG" --out "$W/scaffold") > /dev/null
  cp -r "$W"/scaffold/systemd/checkout-staging-worker.service.d "$W"/scaffold/systemd/checkout-staging-beat.service.d /etc/systemd/system/
  systemctl daemon-reload
  systemctl enable -q --now checkout-staging.service checkout-staging-worker.service checkout-staging-beat.timer
  F scheduled install --config "$CONFIG" --root / --yes > /dev/null
  systemctl daemon-reload
  (
    set -a
    # shellcheck source=/dev/null
    . "$W/production.env"
    set +a
    cd / && fraisier backup --config "$W/production.toml"
  )
  ls /var/backups/checkout
}

before() {
  say "1. the scaffold gives the app unit the flag line and each unit a drop-in"
  check "app unit line" grep -qx "EnvironmentFile=-$FLAG" "$W/scaffold/systemd/checkout-staging.service"
  check "worker drop-in" grep -qx "EnvironmentFile=-$FLAG" /etc/systemd/system/checkout-staging-worker.service.d/fraisier-refreshed.conf
  check "units running" all_running

  say "2. D17: a config without [deploy] production is refused, naming the key"
  write_config "$GOOD_SANITIZE" ""
  refresh --yes; rc=$?
  check "refused (exit 1) naming production = false" test "$rc" = 1 -a -n "$(grep -l 'production = false' "$W/out")"
  F db mark-refresh-target --config "$CONFIG" > "$W/out" 2>&1; rc=$?
  check "mark-refresh-target refused too" test "$rc" = 1
  write_config "$GOOD_SANITIZE" "production = true"
  F validate-config --config "$CONFIG" > "$W/out" 2>&1
  check "production = true with [refresh] is a validate error" out_has "deploy.production"
  refresh --yes; rc=$?
  check "production = true refused" test "$rc" = 1
  write_config "" "production = false"
  refresh --yes; rc=$?
  check "a missing sanitize is refused" test "$rc" = 1 -a -n "$(grep -l 'sanitize' "$W/out")"
  write_config "$GOOD_SANITIZE"
  check "staging untouched" test "$(rows)" = staging-only
  check "units running" all_running

  say "3. an unmarked target is refused before the drop; the units start again"
  refresh --yes; rc=$?
  check "unmarked refused naming mark-refresh-target" test "$rc" = 1 -a -n "$(grep -l mark-refresh-target "$W/out")"
  check "staging untouched" test "$(rows)" = staging-only
  check "units running again" all_running
  F db mark-refresh-target --config "$CONFIG" > "$W/out" 2>&1
  check "marked as the app role" out_has "marked checkout_staging"

  say "4. D18 check 1: a unit that does not load the flag is refused before anything stops"
  mv /etc/systemd/system/checkout-staging-worker.service.d "$W/worker.d.off"; systemctl daemon-reload
  refresh --yes; rc=$?
  check "refused naming the worker" test "$rc" = 1 -a -n "$(grep -l 'checkout-staging-worker.service' "$W/out")"
  check "nothing stopped" all_running
  check "staging untouched" test "$(rows)" = staging-only
  mv "$W/worker.d.off" /etc/systemd/system/checkout-staging-worker.service.d; systemctl daemon-reload

  say "5. R2: a session no unit owns refuses the drop, by name"
  PGPASSWORD=$APP_PW psql -X -q -h 127.0.0.1 -U app -d checkout_staging -c "SET application_name = 'stray-console'; SELECT pg_sleep(120)" > /dev/null 2>&1 &
  stray=$!; sleep 2
  refresh --yes; rc=$?
  # A backend in pg_sleep outlives its client, so end it on the server.
  Q postgres "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name = 'stray-console'" > /dev/null
  wait "$stray" 2>/dev/null
  check "refused naming the session" test "$rc" = 1 -a -n "$(grep -l 'stray-console' "$W/out")"
  check "staging untouched" test "$(rows)" = staging-only
  check "units running again" all_running

  say "6. a failing sanitize, through the real timer unit: stopped, no flag, one alert"
  write_config "sanitize = \"$W/bad.sql\""
  rm -f "$W/notified.jsonl" "$FLAG"
  systemctl start fraisier-checkout-staging-refresh.service; sleep 5
  journalctl -u fraisier-checkout-staging-refresh.service -n 3 --no-pager | tail -2
  n=$(grep -c scheduled-refresh-failed "$W/notified.jsonl" 2>/dev/null || true)
  check "exactly one alert" test "${n:-0}" = 1
  check "the alert names the sanitize step" grep -q 'sanitize' "$W/notified.jsonl"
  check "every unit stopped" none_running
  check "no flag written" test ! -e "$FLAG"
  check "the failed sanitize committed nothing" test "$(rows)" = "ada@customer.example,grace@customer.example"
  check "the receipt says sanitize failed" python3 -c "import json,sys;r=json.load(open('$STATE/refreshes/checkout-staging.json'));sys.exit(r['sanitize']['outcome']!='failed')"
  write_config "$GOOD_SANITIZE"

  say "7. R4: the refresh cannot read the app's environ, so it fails, never skips"
  systemctl start checkout-staging.service checkout-staging-worker.service checkout-staging-beat.timer
  as_operator setpriv --bounding-set -sys_ptrace fraisier db refresh --config "$CONFIG" --state-dir "$STATE" --yes > "$W/out" 2>&1; rc=$?; cat "$W/out"
  check "fails at the outbound flag" test "$rc" = 1 -a -n "$(grep -l 'outbound flag' "$W/out")"
  check "names the unreadable environ" out_has "environ"
  check "every unit stopped again" none_running

  say "8. D18 check 2: a worker that starts without the flag stops everything"
  mkdir -p /etc/systemd/system/checkout-staging-worker.service.d
  printf '[Service]\nUnsetEnvironment=FRAISIER_REFRESHED\n' > /etc/systemd/system/checkout-staging-worker.service.d/zz-unset.conf
  systemctl daemon-reload; systemctl start checkout-staging.service checkout-staging-worker.service checkout-staging-beat.timer
  refresh --yes; rc=$?
  check "fails naming the worker without the flag" test "$rc" = 1 -a -n "$(grep -l 'checkout-staging-worker.service (pid' "$W/out")"
  check "every unit stopped again" none_running
  rm /etc/systemd/system/checkout-staging-worker.service.d/zz-unset.conf; systemctl daemon-reload

  say "9. skip-if-locked"
  systemctl start checkout-staging.service checkout-staging-worker.service checkout-staging-beat.timer
  lock=$(find "$STATE" -name '*.lock' | head -1)
  if [ -n "$lock" ]; then
    as_operator flock "$lock" fraisier db refresh --config "$CONFIG" --state-dir "$STATE" --yes --skip-if-locked > "$W/out" 2>&1; rc=$?
    check "a held lock is a quiet skip" test "$rc" = 0 -a -n "$(grep -l skipped "$W/out")"
  else
    fail "no lock file under $STATE"
  fi

  say "10. the refresh: pg_tviews archive, non-superuser app, every unit flagged"
  refresh --yes; rc=$?
  check "refresh exits 0" test "$rc" = 0
  check "sanitized production rows" test "$(rows)" = "user1@example.invalid,user2@example.invalid"
  check "the webhook was disabled" test "$(Q checkout_staging "SELECT value FROM tb_setting")" = "http://127.0.0.1:9/disabled"
  check "migrated after the restore" test "$(Q checkout_staging "SELECT count(*) FROM pg_tables WHERE tablename = 'tb_migrated'")" = 1
  check "every table owned by app" test "$(Q checkout_staging "SELECT string_agg(DISTINCT tableowner, ',') FROM pg_tables WHERE schemaname = 'public'")" = app
  check "the database owned by app" test "$(Q postgres "SELECT pg_get_userbyid(datdba) FROM pg_database WHERE datname = 'checkout_staging'")" = app
  check "pg_tviews registry restored" test "$(Q checkout_staging "SELECT count(*) FROM tviews.pg_tview_meta")" -ge 1
  check "the TVIEW holds production's rows" test "$(Q checkout_staging "SELECT count(*) FROM tv_item")" = 2
  A -c "INSERT INTO tb_item (pk_item, name) VALUES (3, 'three')" > /dev/null 2>&1
  check "the cascade runs for the app role" test "$(Q checkout_staging "SELECT count(*) FROM tv_item")" = 3
  check "the marker survived" test "$(Q postgres "SELECT count(*) FROM pg_db_role_setting s JOIN pg_database d ON d.oid = s.setdatabase WHERE d.datname = 'checkout_staging' AND 'fraisier.refresh_target=checkout/staging' = ANY(s.setconfig)")" = 1
  check "production carries no marker" test "$(Q postgres "SELECT count(*) FROM pg_db_role_setting s JOIN pg_database d ON d.oid = s.setdatabase WHERE d.datname = 'checkout_production'")" = 0
  check "every unit running" all_running
  check "the app runs with the flag" environ_has checkout-staging.service FRAISIER_REFRESHED=1
  check "the worker runs with the flag" environ_has checkout-staging-worker.service FRAISIER_REFRESHED=1
  check "R3: sanitize saw no admin identity" sh -c "! grep -q -e CHECKOUT_ADMIN_URL -e $ADMIN_PW $W/sanitize.env"
  check "R3: sanitize ran as app" grep -qx PGUSER=app "$W/sanitize.env"
  check "R3: the migration saw no admin identity" sh -c "! grep -q -e CHECKOUT_ADMIN_URL -e $ADMIN_PW $W/migrate.env"
  sum=$(sha256sum "$W/sanitize.sh" | cut -d' ' -f1)
  check "the receipt holds the sanitize hash and both proofs" python3 -c "
import json,sys
r=json.load(open('$STATE/refreshes/checkout-staging.json'))
units={p['unit'] for p in r['outbound_flag']['proof']}
sys.exit(not (r['ok'] and r['sanitize']['sha256']=='$sum' and {'checkout-staging.service','checkout-staging-worker.service'}<=units))"
  check "no password printed" sh -c "! grep -q -e $APP_PW -e $ADMIN_PW $W/out"

  say "11. S3: the flag file is root's; the app can read it, not change or delete it"
  check "root:checkout 0640" test "$(stat -c '%U:%G %a' "$FLAG")" = "root:checkout 640"
  check "the app user can read it" runuser -u checkout -- cat "$FLAG"
  check "the app user cannot write it" sh -c "! runuser -u checkout -- sh -c 'echo X=1 >> $FLAG' 2>/dev/null"
  check "the app user cannot delete it" sh -c "! runuser -u checkout -- rm -f $FLAG 2>/dev/null && test -e $FLAG"
}

after() {
  say "12. S1: after a reboot with the flag set, every unit still runs with it"
  check "units running after the reboot" all_running
  check "the app still has the flag" environ_has checkout-staging.service FRAISIER_REFRESHED=1
  check "the worker still has the flag" environ_has checkout-staging-worker.service FRAISIER_REFRESHED=1

  say "13. --clear-flag: logged, never restarts; the next start runs without it"
  pid=$(systemctl show -p MainPID --value checkout-staging.service)
  as_operator env SUDO_USER=gate-operator fraisier db refresh --config "$CONFIG" --state-dir "$STATE" --clear-flag > "$W/out" 2> "$W/err"; rc=$?
  cat "$W/out" "$W/err"
  check "clear exits 0" test "$rc" = 0
  check "the file is gone" test ! -e "$FLAG"
  check "stderr names who" grep -q gate-operator "$W/err"
  check "the receipt records who and when" python3 -c "import json,sys;r=json.load(open('$STATE/refreshes/checkout-staging.json'));c=r['flag_cleared'];sys.exit(not (c['by']=='gate-operator' and c['at'] and c['removed']))"
  check "the app was not restarted" test "$(systemctl show -p MainPID --value checkout-staging.service)" = "$pid"
  systemctl restart checkout-staging.service
  check "restarted, the app runs without the flag" sh -c "! tr '\\0' '\\n' < /proc/\$(systemctl show -p MainPID --value checkout-staging.service)/environ | grep -q FRAISIER_REFRESHED"
}

case "${1:-}" in
  setup) setup ;;
  before) before ;;
  after) after ;;
  *) echo "usage: $0 setup|before|after" >&2; exit 2 ;;
esac
