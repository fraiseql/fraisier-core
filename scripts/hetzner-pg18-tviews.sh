#!/usr/bin/env bash
# Acceptance for scripts/provision-pg18-tviews.sh on a throwaway Hetzner host.
#
# On a fresh Debian 12 VPS it checks that:
#   1. the script gets the host ready, and fraisier's database_ready check passes;
#   2. a second run changes nothing and does not restart PostgreSQL;
#   3. dropping the extension fails the check with the right reason, and a re-run
#      of the script repairs it;
#   4. removing the preload and restarting fails the check with the right reason,
#      and a re-run of the script repairs it.
# fraisier is built on the host, because a binary built here needs a newer glibc
# than Debian 12 has. The host is deleted on exit unless --keep. A run takes about
# 20 minutes and costs a few cents.
#
# Usage: scripts/hetzner-pg18-tviews.sh [--keep]
# Environment: SSH_KEY (hcloud ssh-key name), SSH_IDENTITY (its private half),
# TYPE, IMAGE (default debian-12), LOCATION, NAME.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
SSH_KEY="${SSH_KEY:-fraisier-checkpoint}"
SSH_IDENTITY="${SSH_IDENTITY:-$HOME/.ssh/hetzner_fraisier}"
TYPE="${TYPE:-cpx22}"
IMAGE="${IMAGE:-debian-12}"
LOCATION="${LOCATION:-nbg1}"
NAME="${NAME:-fraisier-pg18-tviews}"
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

# A stale host of the same name is from an earlier run of this script.
hcloud server delete "$NAME" >/dev/null 2>&1 || true
teardown(){ set +e
  if [ "$KEEP" = 1 ]; then say "leaving $NAME up (--keep). Delete: hcloud server delete $NAME"
  else hcloud server delete "$NAME" >/dev/null 2>&1 && ok "host deleted" \
       || printf '\033[1;31mWARNING:\033[0m could not delete %s; delete it by hand!\n' "$NAME" >&2
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
# The arguments are the remote command, so expanding them here is the point.
# shellcheck disable=SC2029
remote(){ ssh "${SSH_OPTS[@]}" "root@$IP" "$@"; }
say "waiting for SSH"
for _ in $(seq 1 60); do remote true >/dev/null 2>&1 && break; sleep 3; done
remote true >/dev/null 2>&1 || die "SSH never came up"
ok "SSH ready"

say "shipping the repository"
remote 'apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq rsync >/dev/null'
rsync -az --exclude target --exclude .git -e "ssh ${SSH_OPTS[*]}" "$REPO/" "root@$IP:/root/fraisier-core/"
ok "shipped"

remote 'bash -s' <<'REMOTE'
set -euo pipefail
say(){ printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok(){ printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
die(){ printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
cd /root/fraisier-core
DB=fraisier_tviews_accept
provision(){ bash scripts/provision-pg18-tviews.sh --database "$DB" 2>&1 | tee /root/provision.log; }
# The script's last line ends "<n> change(s)" (after colour codes).
changes(){ sed -n 's/.* \([0-9][0-9]*\) change(s)$/\1/p' /root/provision.log | tail -1; }
started(){ runuser -u postgres -- psql -XAtc 'SELECT pg_postmaster_start_time()'; }

say "provision, run 1 (the slow part: PostgreSQL 18 and a pg_tviews build)"
provision >/dev/null
[ "$(changes)" -gt 0 ] || die "run 1 changed nothing on a fresh host"
ok "run 1 made $(changes) change(s)"

say "provision, run 2 (must change nothing)"
before="$(started)"
provision >/dev/null
[ "$(changes)" = 0 ] || { cat /root/provision.log; die "run 2 made $(changes) change(s)"; }
[ "$(started)" = "$before" ] || die "run 2 restarted PostgreSQL"
ok "run 2 made 0 changes and did not restart PostgreSQL"

say "building fraisier on the host"
apt-get install -y -qq libssl-dev >/dev/null
if [ ! -x /root/.cargo/bin/cargo ]; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain none >/dev/null
fi
# Install the toolchain rust-toolchain.toml pins.
/root/.cargo/bin/rustup toolchain install >/dev/null
/root/.cargo/bin/cargo build -q --release --locked -p fraisier
install -m 0755 target/release/fraisier /usr/local/bin/fraisier
ok "$(fraisier --version)"

cat > /root/accept.toml <<TOML
[deploy]
name = "accept"
environment = "vm"

[migration]
adapter = "command"
database_url_env = "ACCEPT_DSN"

[database.requires]
postgres_major = 18
pg_tviews = true
TOML
chmod 0644 /root/accept.toml
cp /root/accept.toml /tmp/accept.toml
check(){ runuser -u postgres -- env "ACCEPT_DSN=postgresql:///$DB?host=/var/run/postgresql" \
  fraisier doctor --config /tmp/accept.toml --check database_ready 2>&1; }
expect_pass(){ out="$(check)" || { echo "$out"; die "database_ready failed: $1"; }
  echo "$out" | grep -q '^\[ok\] database_ready' || { echo "$out"; die "no passing database_ready: $1"; }; }
expect_fail(){ # <words> <why>
  if out="$(check)"; then echo "$out"; die "database_ready passed, but $2"; fi
  echo "$out" | grep -q "$1" || { echo "$out"; die "database_ready failed without naming '$1'"; }; }

say "fraisier's probe on the provisioned host"
expect_pass "after provisioning"
ok "database_ready passes"

say "a TVIEW is a real TVIEW, not a plain table"
runuser -u postgres -- psql -X -q -v ON_ERROR_STOP=1 -d "$DB" <<'SQL'
CREATE TABLE tb_item (pk_item integer PRIMARY KEY, id uuid NOT NULL DEFAULT gen_random_uuid(), name text NOT NULL);
INSERT INTO tb_item (pk_item, name) VALUES (1, 'one');
CREATE TABLE tv_item AS SELECT pk_item, id, jsonb_build_object('name', name) AS data FROM tb_item;
SQL
schema="$(runuser -u postgres -- psql -XAt -d "$DB" -c \
  "SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace WHERE e.extname = 'pg_tviews'")"
registered="$(runuser -u postgres -- psql -XAt -d "$DB" -c "SELECT count(*) FROM $schema.registry")"
[ "$registered" -ge 1 ] || die "tv_item was not registered: the preload is not in effect"
ok "tv_item is registered ($registered in $schema.registry)"

say "dropping the extension fails the probe"
runuser -u postgres -- psql -X -q -d "$DB" -c 'DROP EXTENSION pg_tviews CASCADE'
expect_fail "not installed in this database" "the extension was dropped"
ok "database_ready names the missing extension"
provision >/dev/null
[ "$(changes)" -ge 1 ] || die "the re-run did not recreate the extension"
expect_pass "after recreating the extension"
ok "a re-run recreated it and the probe passes"

say "removing the preload fails the probe"
runuser -u postgres -- psql -X -q -c 'ALTER SYSTEM RESET shared_preload_libraries'
systemctl restart postgresql@18-main
for _ in $(seq 1 30); do runuser -u postgres -- pg_isready -q && break; sleep 1; done
expect_fail "not in shared_preload_libraries" "the preload was removed"
ok "database_ready names the missing preload"
provision >/dev/null
expect_pass "after restoring the preload"
ok "a re-run restored the preload and the probe passes"

say "ACCEPTANCE PASSED: provision-pg18-tviews.sh on $(. /etc/os-release && echo "$PRETTY_NAME")"
REMOTE
