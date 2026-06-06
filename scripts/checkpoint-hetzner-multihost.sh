#!/usr/bin/env bash
#
# §6.4 GA gate — multi-host rollout (IPC artifact) on REAL Hetzner hosts.
#
# The podman fixture (checkpoint-multihost.sh) proved the 2a/2b/2c logic across 3
# distinct-loopback-IP hosts. This runs the *same* rollout across **real Hetzner
# VMs over the real network** with the **IPC-over-SSH artifact** — the owed
# environmental delta (real network / firewall / latency for the IPC path + a real
# LB). The VMs are ALWAYS deleted on exit, so a run costs a few cents.
#
# Architecture (lean — 2 VMs):
#   * orchestrator: LOCAL (this machine runs `fraisier`, reaching the hosts by SSH);
#   * app hosts:    N (default 2) Hetzner **rocky-9** VMs — Rocky 9's glibc (2.34)
#                   matches the ubi9 builder, so the locally-built IPC adapter
#                   binary runs unchanged (no Rust toolchain on the VMs);
#   * load balancer: a real nginx in a LOCAL podman container (host network)
#                   routing real HTTP over the internet to the hosts' public IPs.
#
# The deploy migrates ONCE on the orchestrator (the `command` adapter); each host
# runs the `fraisier-adapter-release` IPC adapter launched over SSH, fetching its
# release from the host's own loopback origin.
#
# Prerequisites: hcloud (authenticated) + a registered ssh-key whose private half
# is local; podman, ssh, scp, curl, python3.
#
# Usage:
#   scripts/checkpoint-hetzner-multihost.sh --ssh-key <name> [--ssh-identity <file>]
#       [--yes] [--keep] [--hosts N] [--type cpx22] [--location nbg1]
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURE="$REPO/scripts/multihost-fixture"
PODMAN="${PODMAN:-podman}"

SSH_KEY="${FRAISIER_HETZNER_SSH_KEY:-}"
SSH_IDENTITY="${FRAISIER_SSH_IDENTITY:-}"
ASSUME_YES=0
KEEP=0
HOSTS=2
TYPE="cpx22"
LOCATION="nbg1"
IMAGE="rocky-9"
HTTP_PORT=8000
LB_PORT=8090
NGINX_IMG="${NGINX_IMG:-fraisier-mh-nginx}"
LB_CTR="fraisier-hmh-nginx"

while [ $# -gt 0 ]; do
  case "$1" in
    --ssh-key)      SSH_KEY="$2"; shift 2;;
    --ssh-identity) SSH_IDENTITY="$2"; shift 2;;
    --yes)          ASSUME_YES=1; shift;;
    --keep)         KEEP=1; shift;;
    --hosts)        HOSTS="$2"; shift 2;;
    --type)         TYPE="$2"; shift 2;;
    --location)     LOCATION="$2"; shift 2;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done
[ "$HOSTS" -ge 2 ] || { echo "--hosts must be >= 2 (the GA gate needs a multi-host fleet)" >&2; exit 2; }

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

command -v hcloud >/dev/null || die "hcloud CLI not found"
command -v ssh >/dev/null || die "ssh not found"
command -v scp >/dev/null || die "scp not found"
command -v "$PODMAN" >/dev/null || die "podman not found (for the IPC builder + LB)"
command -v python3 >/dev/null || die "python3 not found"
hcloud server list >/dev/null 2>&1 || die "hcloud is not authenticated"
[ -n "$SSH_KEY" ] || die "no SSH key. Pass --ssh-key <name> (see 'hcloud ssh-key list')."
hcloud ssh-key describe "$SSH_KEY" >/dev/null 2>&1 || die "ssh-key '$SSH_KEY' not in this project."
[ -z "$SSH_IDENTITY" ] || [ -f "$SSH_IDENTITY" ] || die "--ssh-identity '$SSH_IDENTITY' unreadable."

STAMP="${FRAISIER_RUN_STAMP:-$(date +%s)}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/fraisier-hmh.XXXXXX")"
NAMES=(); IPS=()
for i in $(seq 1 "$HOSTS"); do NAMES+=("fraisier-hmh-$STAMP-$i"); done

cat <<INFO
About to provision $HOSTS throwaway Hetzner hosts for the §6.4 multi-host IPC gate:
  names:    ${NAMES[*]}
  type:     $TYPE   (billed by the hour; a run is ~10–20 min ⇒ a few cents each)
  image:    $IMAGE   (Rocky 9 — glibc matches the ubi9-built IPC adapter)
  location: $LOCATION
  ssh-key:  $SSH_KEY
  LB:       local podman nginx routing over the internet to the hosts' public IPs
All hosts are deleted automatically on exit$( [ "$KEEP" = 1 ] && echo " — DISABLED by --keep" ).
INFO
if [ "$ASSUME_YES" != 1 ]; then
  printf 'Proceed? [y/N] '; read -r reply
  case "$reply" in y|Y|yes|YES) ;; *) die "aborted";; esac
fi

CREATED=0
teardown() {
  set +e
  $PODMAN rm -f "$LB_CTR" >/dev/null 2>&1 || true
  if [ "$CREATED" = 1 ]; then
    if [ "$KEEP" = 1 ]; then
      say "leaving hosts running (--keep): ${NAMES[*]}"
    else
      for n in "${NAMES[@]}"; do
        hcloud server delete "$n" >/dev/null 2>&1 && ok "deleted $n" \
          || printf '\033[1;31mWARNING:\033[0m could not delete %s — delete it manually!\n' "$n" >&2
      done
    fi
  fi
  [ "$KEEP" = 1 ] || rm -rf "$WORK"
}
trap teardown EXIT

SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
  -o ConnectTimeout=10 -o BatchMode=yes)
[ -n "$SSH_IDENTITY" ] && SSH_OPTS+=(-i "$SSH_IDENTITY" -o IdentitiesOnly=yes)
# shellcheck disable=SC2029  # remote command is intentionally expanded client-side
rssh() { ssh "${SSH_OPTS[@]}" "root@$1" "${@:2}"; }

# --------------------------------------------------------------------------
say "creating $HOSTS servers"
for n in "${NAMES[@]}"; do
  hcloud server create --name "$n" --type "$TYPE" --image "$IMAGE" \
    --location "$LOCATION" --ssh-key "$SSH_KEY" >/dev/null
done
CREATED=1
for n in "${NAMES[@]}"; do
  ip="$(hcloud server ip "$n")"; [ -n "$ip" ] || die "could not resolve IP for $n"
  IPS+=("$ip")
done
ok "servers: ${IPS[*]}"

say "waiting for SSH on all hosts"
for ip in "${IPS[@]}"; do
  for _ in $(seq 1 60); do rssh "$ip" true >/dev/null 2>&1 && break; sleep 5; done
  rssh "$ip" true >/dev/null 2>&1 || die "SSH never came up on $ip"
done
ok "SSH ready on all hosts"

# --------------------------------------------------------------------------
# Build the IPC adapter on a ubi9 builder (glibc-compatible with Rocky 9), so it
# runs on the hosts unchanged — no Rust toolchain on the VMs.
say "building fraisier (orchestrator) + the IPC adapter (ubi9 builder)"
( cd "$REPO" && cargo build -q -p fraisier-cli )
FRAISIER="$REPO/target/debug/fraisier"
[ -x "$FRAISIER" ] || die "fraisier binary missing"
# shellcheck disable=SC2016
"$PODMAN" run --rm -v "$REPO":/src:ro -v "$WORK":/out \
  registry.access.redhat.com/ubi9/ubi sh -c '
    set -e
    dnf -y install gcc gcc-c++ make cmake perl >/dev/null 2>&1
    curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain stable >/dev/null 2>&1
    . "$HOME/.cargo/env"
    cd /src
    CARGO_HOME=/out/cargo cargo build --release --locked \
      -p fraisier-adapter-release --target-dir /out/target >/dev/null 2>&1
    cp /out/target/release/fraisier-adapter-release /out/' \
  || die "could not build fraisier-adapter-release on the ubi9 builder"
[ -x "$WORK/fraisier-adapter-release" ] || die "adapter binary missing after build"
ok "IPC adapter built"

# --------------------------------------------------------------------------
# Provision each app host: python3, the app + its (not-yet-enabled) unit, deploy
# dirs, the IPC adapter, and a loopback release origin (a transient systemd unit).
say "provisioning app hosts (python3 + app unit + IPC adapter + release origin)"
for ip in "${IPS[@]}"; do
  rssh "$ip" 'bash -s' <<'REMOTE'
set -euo pipefail
dnf -y install python3 >/dev/null 2>&1 || true
command -v python3 >/dev/null || { echo "python3 missing"; exit 1; }
mkdir -p /opt /var/lib/app /releases
REMOTE
  scp "${SSH_OPTS[@]}" "$FIXTURE/app.py" "root@$ip:/opt/app.py" >/dev/null
  scp "${SSH_OPTS[@]}" "$FIXTURE/app.service" "root@$ip:/etc/systemd/system/app.service" >/dev/null
  scp "${SSH_OPTS[@]}" "$WORK/fraisier-adapter-release" \
    "root@$ip:/usr/local/bin/fraisier-adapter-release" >/dev/null
  rssh "$ip" 'bash -s' <<REMOTE
set -euo pipefail
chmod 755 /opt/app.py /usr/local/bin/fraisier-adapter-release
# app.service ExecStart uses /usr/bin/python3 — ensure it resolves on Rocky 9.
command -v python3 | grep -qx /usr/bin/python3 || ln -sf "\$(command -v python3)" /usr/bin/python3
systemctl daemon-reload
# A loopback release origin the IPC adapter fetches from (transient unit so it
# survives the SSH session that starts it).
systemctl reset-failed release-origin.service 2>/dev/null || true
systemd-run --unit=release-origin --collect \
  /usr/bin/python3 -m http.server $HTTP_PORT --bind 127.0.0.1 --directory /releases >/dev/null
REMOTE
done
ok "app hosts provisioned"

# --------------------------------------------------------------------------
# Local nginx LB (host network) routing to the hosts' public IPs.
say "starting the local nginx LB (routes over the internet to the fleet)"
{
  echo "events {}"
  echo "http {"
  echo "    upstream fixture_upstream {"
  for ip in "${IPS[@]}"; do echo "        server $ip:8080;"; done
  echo "    }"
  echo "    server {"
  echo "        listen $LB_PORT;"
  echo "        location / {"
  echo "            proxy_pass http://fixture_upstream;"
  echo "            proxy_connect_timeout 2s;"
  echo "            proxy_next_upstream error timeout http_500;"
  echo "        }"
  echo "    }"
  echo "}"
} > "$WORK/nginx.conf"
if ! "$PODMAN" image exists "$NGINX_IMG" 2>/dev/null; then
  "$PODMAN" build -q -t "$NGINX_IMG" -f "$FIXTURE/Containerfile.nginx" "$FIXTURE" >/dev/null
fi
"$PODMAN" rm -f "$LB_CTR" >/dev/null 2>&1 || true
"$PODMAN" run -d --name "$LB_CTR" --network host \
  -v "$WORK/nginx.conf:/etc/nginx/nginx.conf:Z" "$NGINX_IMG" >/dev/null
cat >"$WORK/nginx-reload" <<EOF
#!/bin/sh
exec $PODMAN exec $LB_CTR nginx "\$@"
EOF
chmod +x "$WORK/nginx-reload"
ok "LB on :$LB_PORT"

# --------------------------------------------------------------------------
# The shared-DB migration (command adapter; runs once on the orchestrator).
REV="$WORK/db-revision"; MIGLOG="$WORK/migrate.log"
echo "rev-0" >"$REV"; : >"$MIGLOG"
RELEASES="$WORK/releases"; STATE="$WORK/state"; mkdir -p "$RELEASES" "$STATE"

# Build the [hosts] inventory from the public IPs.
inventory=""
n=0
for ip in "${IPS[@]}"; do
  n=$((n + 1))
  inventory+="  { name = \"web-$n\", address = \"$ip\" },
"
done

cat >"$WORK/fraisier.toml" <<EOF
[deploy]
name = "fixture"
environment = "production"

[hosts]
strategy = "rolling"
rolling_batch_size = 1
inventory = [
$inventory]

[ssh]
user = "root"
port = 22
$( [ -n "$SSH_IDENTITY" ] && echo "identity_path = \"$SSH_IDENTITY\"" )
options = ["StrictHostKeyChecking=no", "UserKnownHostsFile=/dev/null", "ConnectTimeout=10"$( [ -n "$SSH_IDENTITY" ] && echo ", \"IdentitiesOnly=yes\"" )]

[artifact]
source = "release-ipc"
adapter_bin = "/usr/local/bin/fraisier-adapter-release"
release_url = "http://127.0.0.1:$HTTP_PORT/app-{version}.tar.gz"
checksum_url = "http://127.0.0.1:$HTTP_PORT/app-{version}.tar.gz.sha256"
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

mint() { # mint + distribute a release to every host's /releases
  printf '%s' "$1" >"$RELEASES/app-$1.tar.gz"
  sha256sum "$RELEASES/app-$1.tar.gz" | awk '{print $1}' >"$RELEASES/app-$1.tar.gz.sha256"
  for ip in "${IPS[@]}"; do
    scp "${SSH_OPTS[@]}" "$RELEASES/app-$1.tar.gz" "$RELEASES/app-$1.tar.gz.sha256" \
      "root@$ip:/releases/" >/dev/null
  done
}
deploy() { # deploy <version> -> prints the JSON outcome
  mint "$1"
  FRAISIER_NGINX_BIN="$WORK/nginx-reload" "$FRAISIER" deploy \
    --config "$WORK/fraisier.toml" --state-dir "$STATE" --app-version "$1" --json 2>/dev/null
}
outcome() { printf '%s' "$1" | python3 -c 'import json,sys;print(json.load(sys.stdin)["outcome"])'; }
host_version() { curl -fsS --max-time 8 "http://$1:8080/health" 2>/dev/null || echo DOWN; }
assert_all_on() {
  for ip in "${IPS[@]}"; do
    v=$(host_version "$ip"); [ "$v" = "$1" ] || die "host $ip serves '$v', expected '$1'"
  done
}

# ==========================================================================
say "2a — $HOSTS-host: 3 consecutive deploys commit (artifact: ipc, real network)"
for v in v1 v2 v3; do
  : >"$MIGLOG"
  out=$(deploy "$v") || true; oc=$(outcome "$out")
  [ "$oc" = committed ] || die "deploy $v: outcome '$oc' (expected committed): $out"
  ups=$(grep -c '^up$' "$MIGLOG" || true)
  [ "$ups" = 1 ] || die "deploy $v: migrate ran $ups times, expected once (migrate-once)"
  assert_all_on "$v"
  ok "deploy $v committed — migrate once, all hosts on $v"
done

lbv=$(curl -fsS --max-time 8 "http://127.0.0.1:$LB_PORT/" 2>/dev/null || echo DOWN)
[ "$lbv" = v3 ] || die "LB served '$lbv', expected v3"
ok "the load balancer routes to the fleet (v3)"

say "2b — a sick build (health 500) rolls the whole fleet back to v3 + reverts the migration"
: >"$MIGLOG"
out=$(deploy "v4-sick") || true; oc=$(outcome "$out")
[ "$oc" = rolled_back ] || die "sick deploy: outcome '$oc' (expected rolled_back): $out"
[ "$(grep -c '^down$' "$MIGLOG" || true)" -ge 1 ] || die "sick deploy: migration was not reverted"
assert_all_on v3
ok "sick build rolled back — all hosts restored to v3, migration reverted"

say "2c — a crash build (restart fails) rolls the whole fleet back to v3"
: >"$MIGLOG"
out=$(deploy "v5-crash") || true; oc=$(outcome "$out")
[ "$oc" = rolled_back ] || die "crash deploy: outcome '$oc' (expected rolled_back): $out"
assert_all_on v3
ok "crash build rolled back — all hosts restored to v3"

printf '\n=========================================================\n'
printf 'PASS — §6.4 multi-host IPC proven on %s real Hetzner hosts\n' "$HOSTS"
printf '       real network, real LB, --artifact ipc, auto-teardown\n'
printf '=========================================================\n'
