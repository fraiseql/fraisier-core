#!/usr/bin/env bash
#
# §10.3 criterion 2 — multi-host fixture gate (local, zero spend).
#
# Stands up 3 real systemd+sshd app hosts as podman containers (on distinct
# loopback IPs 127.0.0.2/3/4) plus an nginx load balancer, then drives the real
# `fraisier` multi-host rollout against them:
#
#   * 3 consecutive deploys commit (migrate ONCE on the orchestrator, host-pull
#     artifact + systemctl + http health on each host over SSH, nginx drain/
#     reattach), every host ending on the new release;
#   * a forced health failure ("sick" build) and a forced restart failure
#     ("crash" build) each roll the whole fleet back to the prior release.
#
# Needs: podman (rootless ok), cargo, ssh, curl, sha256sum. No root, no spend.
# Usage: scripts/checkpoint-multihost.sh [--keep]   (--keep leaves the cluster up)
set -euo pipefail

PODMAN=${PODMAN:-podman}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/.." && pwd)
FIXTURE="$HERE/multihost-fixture"
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

APP_IMG=fraisier-mh-app
NGINX_IMG=fraisier-mh-nginx
LB_CTR=fraisier-mh-lb
CTRS=(fraisier-mh-1 fraisier-mh-2 fraisier-mh-3)
ADDRS=(127.0.0.2 127.0.0.3 127.0.0.4)
SSH_PORT=2222
LB_PORT=8090
WORK=$(mktemp -d "${TMPDIR:-/tmp}/fraisier-mh.XXXXXX")
STATE="$WORK/state"
REV="$WORK/db-revision"
MIGLOG="$WORK/migrate.log"

say() { printf '\n==> %s\n' "$*"; }
fail() { printf '\nFAIL: %s\n' "$*" >&2; exit 1; }

teardown() {
  if [ "$KEEP" = 1 ]; then
    printf '\n--keep: cluster + %s left running\n' "$WORK"
    return
  fi
  for c in "${CTRS[@]}" "$LB_CTR"; do "$PODMAN" rm -f "$c" >/dev/null 2>&1 || true; done
  rm -rf "$WORK"
}
trap teardown EXIT

SSH_OPTS=(-o IdentitiesOnly=yes -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o ConnectTimeout=4)
rssh() { ssh -i "$WORK/id" -p "$SSH_PORT" "${SSH_OPTS[@]}" "root@$1" "${@:2}"; }

# ---------------------------------------------------------------------------
say "building fraisier + container images"
( cd "$REPO" && cargo build -q -p fraisier-cli )
FRAISIER="$REPO/target/debug/fraisier"
"$PODMAN" build -q -t "$APP_IMG" -f "$FIXTURE/Containerfile.app" "$FIXTURE" >/dev/null
"$PODMAN" build -q -t "$NGINX_IMG" -f "$FIXTURE/Containerfile.nginx" "$FIXTURE" >/dev/null

ssh-keygen -t ed25519 -N "" -f "$WORK/id" -q

# Releases the host-pull adapter fetches (file:// from a read-only mount — the
# adapter runs the identical curl/sha256sum/ln; only the URL scheme is hermetic).
RELEASES="$WORK/releases"
mkdir -p "$RELEASES" "$STATE"
mint() {
  printf '%s' "$1" >"$RELEASES/app-$1.tar.gz"
  sha256sum "$RELEASES/app-$1.tar.gz" | awk '{print $1}' >"$RELEASES/app-$1.tar.gz.sha256"
  chmod 644 "$RELEASES/app-$1.tar.gz" "$RELEASES/app-$1.tar.gz.sha256"
}

# ---------------------------------------------------------------------------
say "starting 3 app hosts (systemd + sshd) + nginx LB"
for i in 0 1 2; do
  c=${CTRS[$i]}; a=${ADDRS[$i]}
  "$PODMAN" rm -f "$c" >/dev/null 2>&1 || true
  "$PODMAN" run -d --name "$c" --systemd=always \
    -p "$a:$SSH_PORT:22" -p "$a:8080:8080" \
    -v "$RELEASES:/releases:ro,Z" "$APP_IMG" >/dev/null
done

cp "$FIXTURE/nginx.conf" "$WORK/nginx.conf"
"$PODMAN" rm -f "$LB_CTR" >/dev/null 2>&1 || true
"$PODMAN" run -d --name "$LB_CTR" --network host \
  -v "$WORK/nginx.conf:/etc/nginx/nginx.conf:Z" "$NGINX_IMG" >/dev/null

cat >"$WORK/nginx-reload" <<EOF
#!/bin/sh
exec $PODMAN exec $LB_CTR nginx "\$@"
EOF
chmod +x "$WORK/nginx-reload"

# Inject the ssh key + wait for sshd on every host.
for i in 0 1 2; do
  c=${CTRS[$i]}; a=${ADDRS[$i]}
  "$PODMAN" cp "$WORK/id.pub" "$c:/root/.ssh/authorized_keys"
  "$PODMAN" exec "$c" chmod 600 /root/.ssh/authorized_keys
  for n in $(seq 1 30); do
    rssh "$a" true 2>/dev/null && break
    [ "$n" = 30 ] && fail "ssh never came up on $a"
    sleep 1
  done
done

# ---------------------------------------------------------------------------
# The shared-DB migration: a revision file on the orchestrator, driven by the
# `command` adapter (runs once, on the orchestrator — not per host).
echo "rev-0" >"$REV"
: >"$MIGLOG"
cat >"$WORK/fraisier.toml" <<EOF
[deploy]
name = "fixture"
environment = "production"

[hosts]
strategy = "rolling"
rolling_batch_size = 1
inventory = [
  { name = "web-1", address = "127.0.0.2" },
  { name = "web-2", address = "127.0.0.3" },
  { name = "web-3", address = "127.0.0.4" },
]

[ssh]
user = "root"
port = $SSH_PORT
identity_path = "$WORK/id"
options = ["IdentitiesOnly=yes", "StrictHostKeyChecking=no", "UserKnownHostsFile=/dev/null", "ConnectTimeout=4"]

[artifact]
source = "pull"
release_url = "file:///releases/app-{version}.tar.gz"
checksum_url = "file:///releases/app-{version}.tar.gz.sha256"
staging_dir = "/var/lib/app/releases"
active_path = "/var/lib/app/current"

[migration]
adapter = "command"

[migration.settings.commands]
current_revision = "cat $REV"
up = "echo up >> $MIGLOG; echo rev-applied > $REV"
down_to = "echo down >> $MIGLOG"
verify = "echo verify >> $MIGLOG"

[service]
adapter = "systemd"
unit = "app.service"

[health]
adapter = "http"
url = "http://{host.address}:8080/health"
expected_status = 200

[lb]
adapter = "nginx"
config_path = "$WORK/nginx.conf"
upstream = "fixture_upstream"
EOF

deploy() { # deploy <version> -> prints the JSON outcome
  mint "$1"
  FRAISIER_NGINX_BIN="$WORK/nginx-reload" "$FRAISIER" deploy \
    --config "$WORK/fraisier.toml" --state-dir "$STATE" --app-version "$1" --json 2>/dev/null
}
outcome() { printf '%s' "$1" | python3 -c 'import json,sys;print(json.load(sys.stdin)["outcome"])'; }
host_version() { curl -fsS --max-time 4 "http://$1:8080/health" 2>/dev/null || echo "DOWN"; }
assert_all_on() { # assert_all_on <version>
  for a in "${ADDRS[@]}"; do
    v=$(host_version "$a")
    [ "$v" = "$1" ] || fail "host $a serves '$v', expected '$1'"
  done
}

# ---------------------------------------------------------------------------
say "criterion 2a — 3 consecutive multi-host deploys commit"
for v in v1 v2 v3; do
  : >"$MIGLOG"
  out=$(deploy "$v") || true; oc=$(outcome "$out")
  [ "$oc" = "committed" ] || fail "deploy $v: outcome '$oc' (expected committed): $out"
  ups=$(grep -c '^up$' "$MIGLOG" || true)
  [ "$ups" = "1" ] || fail "deploy $v: migrate ran $ups times, expected once (migrate-once)"
  assert_all_on "$v"
  say "deploy $v committed — migrate once, all 3 hosts on $v"
done

say "the load balancer routes to the fleet"
lbv=$(curl -fsS --max-time 4 "http://127.0.0.1:$LB_PORT/" 2>/dev/null || echo DOWN)
[ "$lbv" = "v3" ] || fail "LB served '$lbv', expected v3"

say "criterion 2b — a sick build (health fails) rolls the fleet back to v3"
: >"$MIGLOG"
out=$(deploy "v4-sick") || true; oc=$(outcome "$out")
[ "$oc" = "rolled_back" ] || fail "sick deploy: outcome '$oc' (expected rolled_back): $out"
[ "$(grep -c '^down$' "$MIGLOG" || true)" -ge 1 ] || fail "sick deploy: migration was not rolled back"
assert_all_on "v3"
say "sick build rolled back — all 3 hosts restored to v3, migration reverted"

say "criterion 2c — a crash build (restart fails) rolls the fleet back to v3"
: >"$MIGLOG"
out=$(deploy "v5-crash") || true; oc=$(outcome "$out")
[ "$oc" = "rolled_back" ] || fail "crash deploy: outcome '$oc' (expected rolled_back): $out"
assert_all_on "v3"
say "crash build rolled back — all 3 hosts restored to v3"

printf '\n=========================================================\n'
printf 'PASS — §10.3 criterion 2 (multi-host) proven on 3 real hosts\n'
printf '=========================================================\n'
