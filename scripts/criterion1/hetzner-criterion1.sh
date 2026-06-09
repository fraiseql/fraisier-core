#!/usr/bin/env bash
# Orchestrate the §10.3 criterion-1 deploy on a throwaway Hetzner pid-1 host:
# provision debian-13 (glibc >= 2.39 for the prebuilt binaries), install docker +
# confiture, ship the stripped fraisier + fraiseql-server binaries + schema/config/
# migrations + criterion1-host.sh, and run it. Host deleted on exit unless --keep.
set -euo pipefail
SCRATCH="$HOME/code/partb-materialize-scratch"
SSH_KEY="${SSH_KEY:-fraisier-checkpoint}"
SSH_IDENTITY="${SSH_IDENTITY:-$HOME/.ssh/hetzner_fraisier}"
TYPE="${TYPE:-cpx22}"
IMAGE="${IMAGE:-debian-13}"
LOCATION="${LOCATION:-nbg1}"
NAME="${NAME:-fraisier-crit1}"
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

say(){ printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok(){ printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
die(){ printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

command -v hcloud >/dev/null || die "hcloud not found"
command -v rsync  >/dev/null || die "rsync not found"
hcloud server list >/dev/null 2>&1 || die "hcloud not authenticated"
hcloud ssh-key describe "$SSH_KEY" >/dev/null 2>&1 || die "ssh-key '$SSH_KEY' not in project"
[ -r "$SSH_IDENTITY" ] || die "ssh identity '$SSH_IDENTITY' not readable"
for f in fraisier.bin fraiseql-server.bin schema.compiled.json server.config.toml criterion1-host.sh; do
  [ -e "$SCRATCH/$f" ] || die "missing $SCRATCH/$f"
done
[ -d "$SCRATCH/ecom-confiture" ] || die "missing ecom-confiture migrations"

# Delete a stale host of the same name (idempotent re-runs)
hcloud server delete "$NAME" >/dev/null 2>&1 || true

teardown(){ set +e
  if [ "$KEEP" = 1 ]; then say "leaving $NAME up (--keep). Delete: hcloud server delete $NAME"
  else hcloud server delete "$NAME" >/dev/null 2>&1 && ok "host deleted" \
       || printf '\033[1;31mWARNING:\033[0m could not delete %s — delete manually!\n' "$NAME" >&2
  fi
}
trap teardown EXIT

say "creating $NAME ($TYPE / $IMAGE / $LOCATION)"
hcloud server create --name "$NAME" --type "$TYPE" --image "$IMAGE" \
  --location "$LOCATION" --ssh-key "$SSH_KEY" >/dev/null
IP="$(hcloud server ip "$NAME")"
ok "host $NAME at $IP"

SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  -o IdentitiesOnly=yes -i "$SSH_IDENTITY" -o ConnectTimeout=10)
remote(){ ssh "${SSH_OPTS[@]}" "root@$IP" "$@"; }

say "waiting for SSH"
for _ in $(seq 1 60); do remote true >/dev/null 2>&1 && break; sleep 3; done
remote true >/dev/null 2>&1 || die "SSH never came up"
ok "SSH ready"

say "installing docker + confiture (>=0.22) + python3 (the slow part)"
remote 'bash -s' <<'REMOTE'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq curl python3 python3-pip ca-certificates >/dev/null
command -v docker >/dev/null || { curl -fsSL https://get.docker.com | sh >/dev/null; }
systemctl enable --now docker >/dev/null 2>&1 || true
pip3 install --break-system-packages --quiet 'fraiseql-confiture>=0.22' >/dev/null
echo "confiture: $(confiture --version 2>&1 | head -1)"
REMOTE
ok "deps installed"

say "shipping binaries + assets (stripped; rsync -z)"
remote 'mkdir -p /root/criterion1/migrations'
RS=(-az -e "ssh ${SSH_OPTS[*]}")
rsync "${RS[@]}" "$SCRATCH/fraisier.bin"          "root@$IP:/root/criterion1/fraisier"
rsync "${RS[@]}" "$SCRATCH/fraiseql-server.bin"   "root@$IP:/root/criterion1/fraiseql-server"
rsync "${RS[@]}" "$SCRATCH/schema.compiled.json"  "root@$IP:/root/criterion1/schema.compiled.json"
rsync "${RS[@]}" "$SCRATCH/server.config.toml"    "root@$IP:/root/criterion1/server.config.toml"
rsync "${RS[@]}" "$SCRATCH/criterion1-host.sh"    "root@$IP:/root/criterion1/criterion1-host.sh"
rsync "${RS[@]}" "$SCRATCH/ecom-confiture/"       "root@$IP:/root/criterion1/migrations/"
ok "assets shipped"

say "running criterion-1 deploy on the host (pid-1 systemd, real network)"
remote 'bash /root/criterion1/criterion1-host.sh'
