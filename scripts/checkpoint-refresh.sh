#!/usr/bin/env bash
# The `fraisier db refresh` acceptance gate on a throwaway Hetzner host.
#
# It creates a Debian 12 host, provisions PostgreSQL 18 + pg_tviews with
# scripts/provision-pg18-tviews.sh, builds fraisier from this checkout's HEAD,
# and runs scripts/refresh-e2e/checks.sh as root: setup, the checks up to a
# refresh that sets the outbound flag, a reboot, then the checks after it.
# The host is ALWAYS deleted on exit (success, failure, or Ctrl-C).
#
# Usage: scripts/checkpoint-refresh.sh
#   HCLOUD_SSH_KEY       the hcloud key name   (default: fraisier-checkpoint)
#   HCLOUD_SSH_IDENTITY  its private key file  (default: ~/.ssh/hetzner_fraisier)
#   HCLOUD_SERVER_TYPE   the server type       (default: cpx42)
set -euo pipefail

REPO=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
NAME=fraisier-refresh-gate-$$
KEY=${HCLOUD_SSH_KEY:-fraisier-checkpoint}
IDENTITY=${HCLOUD_SSH_IDENTITY:-$HOME/.ssh/hetzner_fraisier}
TYPE=${HCLOUD_SERVER_TYPE:-cpx42}
SSH=(ssh -i "$IDENTITY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=10)

cleanup() {
  if hcloud server delete "$NAME" >/dev/null 2>&1; then
    echo "host $NAME deleted"
  else
    echo "WARNING: could not delete $NAME; delete it by hand" >&2
  fi
}
trap cleanup EXIT

hcloud server create --name "$NAME" --type "$TYPE" --image debian-12 \
  --location nbg1 --ssh-key "$KEY" >/dev/null
IP=$(hcloud server ip "$NAME")
echo "host $NAME at $IP"
remote() { "${SSH[@]}" "root@$IP" "$@"; }
wait_for_ssh() {
  for _ in $(seq 1 90); do
    remote true >/dev/null 2>&1 && return 0
    sleep 5
  done
  echo "the host never answered ssh" >&2
  return 1
}
wait_for_ssh

git -C "$REPO" archive --format=tar HEAD | remote 'rm -rf /root/fc && mkdir /root/fc && tar -x -C /root/fc'
echo "provisioning PostgreSQL 18 + pg_tviews"
remote 'bash /root/fc/scripts/provision-pg18-tviews.sh --database scratch > /root/provision.log 2>&1 || { tail -20 /root/provision.log; exit 1; }; tail -1 /root/provision.log'
echo "building fraisier"
remote 'apt-get install -y -qq libssl-dev pkg-config >/dev/null 2>&1
  curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain none >/dev/null
  cd /root/fc && /root/.cargo/bin/rustup toolchain install >/dev/null 2>&1 \
    && /root/.cargo/bin/cargo build -q --release --locked -p fraisier \
    && install -m 0755 target/release/fraisier /usr/local/bin/fraisier && fraisier --version'

remote 'bash /root/fc/scripts/refresh-e2e/checks.sh setup'
remote 'bash /root/fc/scripts/refresh-e2e/checks.sh before'
echo "rebooting with the outbound flag set"
remote 'systemctl reboot' || true
sleep 20
wait_for_ssh
sleep 15
remote 'bash /root/fc/scripts/refresh-e2e/checks.sh after'

passed=$(remote 'grep -c ^PASS /root/refresh-e2e/results' || true)
failed=$(remote 'grep -c ^FAIL /root/refresh-e2e/results' || true)
echo
echo "RESULT: ${passed:-0}/$((${passed:-0} + ${failed:-0})) checks passed"
remote 'grep ^FAIL /root/refresh-e2e/results' || true
[ "${failed:-0}" = 0 ]
