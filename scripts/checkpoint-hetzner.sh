#!/usr/bin/env bash
#
# §10.3 checkpoint, part (b) — the genuinely-remote, system-level gate.
#
# Provisions a throwaway Hetzner Cloud host, installs the toolchain, and runs the
# *same* proven checkpoint scenario as scripts/checkpoint-local.sh but in
# `system` systemd mode (a real pid-1 manager, as root) on a real remote host
# reached over the network — the part the local run cannot cover. The server is
# ALWAYS deleted on exit (success, failure, or Ctrl-C), so a run costs a few cents.
#
# This is one command with auto-teardown so it is cheap and fast to pull the
# trigger on. It is NOT run by default and never from CI: it provisions paid
# infrastructure and so requires an explicit confirmation.
#
# Prerequisites:
#   * hcloud CLI, authenticated for a project (`hcloud context active`) or HCLOUD_TOKEN
#   * an SSH key registered in that project (`hcloud ssh-key list`) whose private
#     half is either loaded in your agent / at ~/.ssh/id_ed25519, or passed
#     explicitly via --ssh-identity (recommended if you keep a dedicated key)
#   * rsync, ssh
#
# Usage:
#   scripts/checkpoint-hetzner.sh --ssh-key <name> [--ssh-identity <file>]
#                                 [--yes] [--keep] [--matrix | --training]
#                                 [--type cpx22] [--location nbg1]
#
# Flags:
#   --ssh-key <name>      the hcloud ssh-key to inject (or env FRAISIER_HETZNER_SSH_KEY)
#   --ssh-identity <file> use ONLY this private key for ssh/rsync (adds
#                         -i <file> -o IdentitiesOnly=yes, so the agent's other
#                         keys are never offered — keeps a dedicated key isolated).
#                         Or env FRAISIER_SSH_IDENTITY.
#   --yes                 skip the interactive confirmation
#   --matrix              run the full §10.3 production matrix (checkpoint-matrix.sh,
#                         Part A) instead of the two-deploy checkpoint-local.sh
#                         scenario: per-phase forced-failure rollback (migrate /
#                         release / health) + three consecutive deploys, on real
#                         pid-1 systemd over the network. Exercises the release
#                         (activate/restart) split and the reset-failed-before-
#                         restart fix under a real rate-limited pid-1 manager —
#                         the part the older local scenario does not. The migration
#                         store is the reference sqlx adapter (SQLite). Criterion 1
#                         (fraiseql v2 vs real Postgres, via Confiture) is Part B:
#                         --keep, then run checkpoint-matrix.sh --real-config with
#                         your real deploy config — this host has no real artifact.
#   --training            run the training-field checkpoint (checkpoint-training.sh)
#                         instead: the in-process Confiture migration adapter
#                         against a real (throwaway, containerised) Postgres, inside
#                         the real deploy saga, on pid-1 systemd — three consecutive
#                         deploys + forced migrate/restart/health rollbacks. This is
#                         the genuinely-remote Confiture-on-Postgres proof. Installs
#                         Confiture (>=0.22) on the host.
#   --keep                do NOT delete the host afterwards (prints the ssh command);
#                         use this to run the operator's full §10.3 production matrix
#   --type <t>            server type (default cpx22, 4 GB — ≈ €0.008/hour). Note
#                         the older cpx11/cpx21 are US-only (ash/hil); the cpx2x
#                         line is the x86 option in the EU locations.
#   --location <loc>      hcloud location (default nbg1)
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SQLX_REPO="${FRAISIER_SQLX_REPO:-$(cd "$REPO_ROOT/.." && pwd)/fraisier-adapter-sqlx}"

SSH_KEY="${FRAISIER_HETZNER_SSH_KEY:-}"
SSH_IDENTITY="${FRAISIER_SSH_IDENTITY:-}"
ASSUME_YES=0
KEEP=0
MATRIX=0
TRAINING=0
TYPE="cpx22"
LOCATION="nbg1"
IMAGE="debian-12"

while [ $# -gt 0 ]; do
  case "$1" in
    --ssh-key)      SSH_KEY="$2"; shift 2;;
    --ssh-identity) SSH_IDENTITY="$2"; shift 2;;
    --yes)          ASSUME_YES=1; shift;;
    --matrix)       MATRIX=1; shift;;
    --training)     TRAINING=1; shift;;
    --keep)         KEEP=1; shift;;
    --type)         TYPE="$2"; shift 2;;
    --location)     LOCATION="$2"; shift 2;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done
[ "$MATRIX" = 1 ] && [ "$TRAINING" = 1 ] && { echo "choose one of --matrix / --training" >&2; exit 2; }

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# --------------------------------------------------------------------------
# Preconditions
# --------------------------------------------------------------------------
command -v hcloud >/dev/null || die "hcloud CLI not found (https://github.com/hetznercloud/cli)"
command -v rsync  >/dev/null || die "rsync not found"
command -v ssh    >/dev/null || die "ssh not found"
hcloud server list >/dev/null 2>&1 \
  || die "hcloud is not authenticated. Run 'hcloud context create <name>' or export HCLOUD_TOKEN."
[ -d "$SQLX_REPO" ] || die "sqlx adapter repo not found at $SQLX_REPO (set FRAISIER_SQLX_REPO)"
[ -n "$SSH_KEY" ] || die "no SSH key. Pass --ssh-key <name> (see 'hcloud ssh-key list')."
hcloud ssh-key describe "$SSH_KEY" >/dev/null 2>&1 \
  || die "ssh-key '$SSH_KEY' not found in this project (see 'hcloud ssh-key list')."
[ -z "$SSH_IDENTITY" ] || [ -f "$SSH_IDENTITY" ] \
  || die "--ssh-identity '$SSH_IDENTITY' is not a readable file."

# A unique-per-run name. Pass a stamp in so the script stays deterministic.
STAMP="${FRAISIER_RUN_STAMP:-$(date +%s)}"
NAME="fraisier-checkpoint-$STAMP"

# --------------------------------------------------------------------------
# Confirm — this provisions paid infrastructure
# --------------------------------------------------------------------------
cat <<INFO
About to provision a throwaway Hetzner host for the §10.3 checkpoint:
  name:     $NAME
  type:     $TYPE   (billed by the hour; a run is ~15–25 min ⇒ a few cents)
  image:    $IMAGE
  location: $LOCATION
  ssh-key:  $SSH_KEY
  scenario: $( if [ "$MATRIX" = 1 ]; then echo "production matrix Part A (per-phase rollback + 3 consecutive)"; elif [ "$TRAINING" = 1 ]; then echo "training field (Confiture + real Postgres, in-saga)"; else echo "two-deploy checkpoint-local.sh (committed + rolled_back)"; fi )
The host is deleted automatically on exit$( [ "$KEEP" = 1 ] && echo " — DISABLED by --keep" ).
INFO
if [ "$ASSUME_YES" != 1 ]; then
  printf 'Proceed? [y/N] '
  read -r reply
  case "$reply" in y|Y|yes|YES) ;; *) die "aborted";; esac
fi

# --------------------------------------------------------------------------
# Provision + guaranteed teardown
# --------------------------------------------------------------------------
SERVER_CREATED=0
# shellcheck disable=SC2329  # invoked indirectly via `trap teardown EXIT`
teardown() {
  set +e
  if [ "$SERVER_CREATED" = 1 ]; then
    if [ "$KEEP" = 1 ]; then
      say "leaving $NAME running (--keep). Delete it with: hcloud server delete $NAME"
    else
      say "deleting $NAME"
      hcloud server delete "$NAME" >/dev/null 2>&1 && ok "host deleted" || \
        printf '\033[1;31mWARNING:\033[0m could not delete %s — delete it manually to stop charges!\n' "$NAME" >&2
    fi
  fi
}
trap teardown EXIT

say "creating server $NAME"
hcloud server create --name "$NAME" --type "$TYPE" --image "$IMAGE" \
  --location "$LOCATION" --ssh-key "$SSH_KEY" >/dev/null
SERVER_CREATED=1
IP="$(hcloud server ip "$NAME")"
[ -n "$IP" ] || die "could not resolve the server IP"
ok "server $NAME at $IP"

# Throwaway, single-use host reached by a fresh — and often RECYCLED — Hetzner IP.
# Host-key pinning is meaningless here (we just created the host via the
# authenticated hcloud API and delete it in minutes) and actively harmful:
# `accept-new` REFUSES a recycled IP whose key differs from a stale known_hosts
# entry, which surfaces as "SSH never came up" after the wait loop times out. So
# disable verification and never touch the user's known_hosts (no pinning, no
# pollution, no recycled-IP conflicts on the next run).
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
         -o ConnectTimeout=10 -o BatchMode=yes)
# A dedicated key: use ONLY it, so the agent's other identities are never offered
# to the throwaway host (honours strict per-service key separation).
[ -n "$SSH_IDENTITY" ] && SSH_OPTS+=(-i "$SSH_IDENTITY" -o IdentitiesOnly=yes)
# Commands are literal/single-quoted or piped via stdin heredocs, so client-side
# expansion of "$@" is exactly what we want here.
# shellcheck disable=SC2029
remote() { ssh "${SSH_OPTS[@]}" "root@$IP" "$@"; }

say "waiting for SSH"
for _ in $(seq 1 60); do
  remote true >/dev/null 2>&1 && break
  sleep 5
done
remote true >/dev/null 2>&1 || die "SSH never came up"
ok "SSH ready"

# --------------------------------------------------------------------------
# Install the toolchain (Debian's rustc is far too old; rustup honours the
# repo's rust-toolchain.toml) + docker + python + sqlite
# --------------------------------------------------------------------------
say "installing dependencies (this is the slow part — toolchain + deps)"
remote 'bash -s' <<'REMOTE'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq curl git rsync build-essential pkg-config libssl-dev \
  python3 sqlite3 ca-certificates >/dev/null
# Docker (for Jaeger) via the convenience script.
if ! command -v docker >/dev/null; then
  curl -fsSL https://get.docker.com | sh >/dev/null
fi
systemctl enable --now docker >/dev/null 2>&1 || true
# Rust via rustup (non-interactive); the repo's rust-toolchain.toml pins the rest.
if ! command -v cargo >/dev/null 2>&1 && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y >/dev/null
fi
REMOTE
ok "dependencies installed"

# --------------------------------------------------------------------------
# Training mode also needs Confiture (>=0.22) on PATH for the in-process adapter.
# --------------------------------------------------------------------------
if [ "$TRAINING" = 1 ]; then
  say "installing Confiture (>=0.22) on the host"
  remote 'bash -s' <<'REMOTE'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get install -y -qq python3-pip >/dev/null
# Debian's Python is externally-managed (PEP 668); this is a throwaway host.
pip3 install --break-system-packages --quiet 'fraiseql-confiture>=0.22' >/dev/null
confiture --version
REMOTE
  ok "Confiture installed"
fi

# --------------------------------------------------------------------------
# Ship the two repos (sibling layout the checkpoint expects) and run it
# --------------------------------------------------------------------------
say "syncing repositories"
RSYNC_OPTS=(-az --delete --exclude target --exclude .git -e "ssh ${SSH_OPTS[*]}")
rsync "${RSYNC_OPTS[@]}" "$REPO_ROOT/"  "root@$IP:/root/fraisier-core/"
rsync "${RSYNC_OPTS[@]}" "$SQLX_REPO/"  "root@$IP:/root/fraisier-adapter-sqlx/"
ok "repositories synced"

if [ "$MATRIX" = 1 ]; then
  say "running the production matrix on the host (system systemd, real network, OTLP→Jaeger)"
  # checkpoint-matrix.sh Part A in system mode (SQLite migration store via the
  # reference sqlx adapter). Piped to a non-login `bash -s`, so PATH to cargo is
  # set explicitly (rustup edits the login profile, which `-s` does not source).
  remote 'bash -s' <<'REMOTE'
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
cd /root/fraisier-core
FRAISIER_SQLX_REPO=/root/fraisier-adapter-sqlx \
  ./scripts/checkpoint-matrix.sh --systemd system
REMOTE
  ok "remote matrix passed"
  say "CHECKPOINT (b) PASSED — production matrix Part A on a real remote host,"
  say "pid-1 systemd, real network, OTLP→Jaeger."
elif [ "$TRAINING" = 1 ]; then
  say "running the training-field checkpoint on the host (Confiture + Postgres, system systemd)"
  # checkpoint-training.sh manages its own throwaway Postgres + Jaeger containers,
  # so it is self-contained in system mode. Confiture (installed above) is on PATH
  # via pip's /usr/local/bin console script.
  remote 'bash -s' <<'REMOTE'
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
cd /root/fraisier-core
./scripts/checkpoint-training.sh --systemd system
REMOTE
  ok "remote training-field checkpoint passed"
  say "CHECKPOINT (b) PASSED — Confiture-on-Postgres deploy matrix on a real remote"
  say "host, pid-1 systemd, real network, OTLP→Jaeger."
else
  say "running the checkpoint on the host (system systemd, real network, OTLP→Jaeger)"
  # Reuse the *same* tested scenario as checkpoint-local.sh, in system mode. Piped
  # to a non-login `bash -s`, so PATH to cargo is set explicitly (rustup edits the
  # login profile, which `-s` does not source).
  remote 'bash -s' <<'REMOTE'
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
cd /root/fraisier-core
CHECKPOINT_SYSTEMD=system \
FRAISIER_SQLX_REPO=/root/fraisier-adapter-sqlx \
  ./scripts/checkpoint-local.sh
REMOTE
  ok "remote checkpoint passed"
  say "CHECKPOINT (b) PASSED — real remote host, pid-1 systemd, real network, OTLP→Jaeger."
fi
if [ "$MATRIX" = 1 ]; then
  cat <<'NEXT'

Matrix Part A is now confirmed on real infra (per-phase forced-failure rollback +
three consecutive deploys, pid-1 systemd, real network). Still owner-bound for the
literal §10.3 criterion 1 — fraiseql v2 deploying 3× in production vs real Postgres:
  * re-run with --keep --matrix, then on the host point checkpoint-matrix.sh at
    YOUR real fraisier deploy config (real artifact + real Postgres via Confiture):
      scripts/checkpoint-matrix.sh --systemd system \
        --real-config <your fraisier.toml> --real-version v2 --real-version v2 --real-version v2
    (export the DSN env its [migration].database_url_env names; Confiture path
    needs Confiture >= 0.20.0). This host has no real fraiseql v2 artifact.
Only then tag v1.0.0-alpha.1-week2 / rename to fraisier v1.0.0-beta.1.
NEXT
elif [ "$TRAINING" = 1 ]; then
  cat <<'NEXT'

Training-field checkpoint confirmed on real infra: the in-process Confiture
adapter ran against a real Postgres inside the deploy saga (3 consecutive
deploys + forced migrate/restart/health rollbacks), pid-1 systemd. This proves
the Confiture-on-Postgres pipeline; it is NOT §10.3 criterion 1 (fraiseql v2).
Next: drive the real fraiseql v2 artifact via checkpoint-matrix.sh --real-config.
NEXT
else
  cat <<NEXT

Next, for the full PRD §10.3 production sign-off (operator judgement, infra-bound):
  * re-run with --keep --matrix for the per-phase forced-failure matrix + three
    consecutive deploys on real pid-1 systemd (Part A);
  * then point checkpoint-matrix.sh --real-config at the real fraiseql v2 artifact
    against real Postgres (Confiture) for criterion 1 (Part B);
  * confirm the fraiseql/production trace renders — headless, run
    scripts/show-trace.sh on the host (or tunnel: ssh -L 16686:localhost:16686 root@<ip>).
Only then tag v1.0.0-alpha.1-week2 / rename to fraisier v1.0.0-beta.1.
NEXT
fi
exit 0
