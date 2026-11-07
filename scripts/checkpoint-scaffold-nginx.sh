#!/usr/bin/env bash
#
# Real-nginx checkpoint for the site `fraisier scaffold` renders: it loads
# (`nginx -t`) and compresses JSON the way the config says.
#
#   1. the default render loads, and a JSON response through it carries
#      `Content-Encoding: gzip` when the client accepts gzip;
#   2. `[scaffold.nginx].gzip = false` loads, and the response is not
#      compressed;
#   3. (with --brotli) `[scaffold.nginx].brotli = true` loads, and a client
#      accepting br gets `Content-Encoding: br`.
#
# The site proxies to `127.0.0.1:8080`, where a second server block answers
# with a 4 KB JSON body. nginx runs from a private config in /tmp/fraisier-nginx,
# never the target's own.
#
# Where it runs:
#   default     a ubi9 nginx container (podman), as root inside it so the
#               site's `listen 80` binds. ubi9's nginx has no brotli module.
#   --hetzner   a throwaway Debian 13 host with nginx and
#               libnginx-mod-http-brotli-filter, created with hcloud and deleted
#               on exit. Implies --brotli.
#
# Usage:
#   scripts/checkpoint-scaffold-nginx.sh [--fraisier <binary>] [--image <image>]
#   scripts/checkpoint-scaffold-nginx.sh --hetzner --ssh-key <name>
#                                        [--ssh-identity <file>] [--yes]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FRAISIER=""
IMAGE="registry.access.redhat.com/ubi9/nginx-124"
HETZNER=0
BROTLI=0
SSH_KEY="${FRAISIER_HETZNER_SSH_KEY:-}"
SSH_IDENTITY=""
ASSUME_YES=0
TYPE="cx23"
LOCATION="nbg1"

while [ $# -gt 0 ]; do
  case "$1" in
    --fraisier) FRAISIER="$2"; shift 2;;
    --image) IMAGE="$2"; shift 2;;
    --hetzner) HETZNER=1; BROTLI=1; shift;;
    --brotli) BROTLI=1; shift;;
    --ssh-key) SSH_KEY="$2"; shift 2;;
    --ssh-identity) SSH_IDENTITY="$2"; shift 2;;
    --type) TYPE="$2"; shift 2;;
    --location) LOCATION="$2"; shift 2;;
    --yes) ASSUME_YES=1; shift;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
pass() { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; PASSED=$((PASSED + 1)); }
fail() { printf '\033[1;31m  ✗\033[0m %s\n' "$*"; FAILED=$((FAILED + 1)); }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
PASSED=0
FAILED=0

if [ -z "$FRAISIER" ]; then
  say "building fraisier"
  (cd "$REPO_ROOT" && cargo build -q -p fraisier)
  FRAISIER="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/debug/fraisier"
fi

WORK="$(mktemp -d)"
CONTAINER=""
SERVER=""
# shellcheck disable=SC2329  # invoked indirectly via `trap cleanup EXIT`
cleanup() {
  set +e
  [ -n "$CONTAINER" ] && podman rm -f "$CONTAINER" >/dev/null 2>&1
  if [ -n "$SERVER" ]; then
    say "deleting $SERVER"
    hcloud server delete "$SERVER" >/dev/null 2>&1 && say "host deleted" \
      || printf '\033[1;31mWARNING:\033[0m could not delete %s — delete it by hand!\n' "$SERVER" >&2
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

# --------------------------------------------------------------------------
# The target: `target <command>` runs a shell command there, stdin attached.
# --------------------------------------------------------------------------
if [ "$HETZNER" = 1 ]; then
  command -v hcloud >/dev/null || die "hcloud CLI not found"
  [ -n "$SSH_KEY" ] || die "--hetzner needs --ssh-key <name> (see 'hcloud ssh-key list')"
  SERVER="fraisier-scaffold-nginx-$(date +%s)"
  if [ "$ASSUME_YES" != 1 ]; then
    printf 'Create the throwaway host %s (%s, debian-13, a few cents)? [y/N] ' "$SERVER" "$TYPE"
    read -r reply
    case "$reply" in y|Y|yes|YES) ;; *) SERVER=""; die "aborted";; esac
  fi
  say "creating $SERVER"
  hcloud server create --name "$SERVER" --type "$TYPE" --image debian-13 \
    --location "$LOCATION" --ssh-key "$SSH_KEY" >/dev/null
  IP="$(hcloud server ip "$SERVER")"
  # A fresh, often recycled IP for a host deleted within minutes: no pinning.
  SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
            -o ConnectTimeout=10 -o BatchMode=yes -o LogLevel=ERROR)
  [ -n "$SSH_IDENTITY" ] && SSH_OPTS+=(-i "$SSH_IDENTITY" -o IdentitiesOnly=yes)
  # The command is built here on purpose: it is the remote shell's script.
  # shellcheck disable=SC2029
  target() { ssh "${SSH_OPTS[@]}" "root@$IP" "$1"; }
  for _ in $(seq 1 60); do target true </dev/null >/dev/null 2>&1 && break; sleep 5; done
  target true </dev/null >/dev/null 2>&1 || die "ssh never came up on $IP"
  say "installing nginx and its brotli module on $IP"
  target 'export DEBIAN_FRONTEND=noninteractive; apt-get update -qq &&
    apt-get install -y -qq nginx libnginx-mod-http-brotli-filter curl >/dev/null &&
    systemctl stop nginx' </dev/null
  MODULES="include /etc/nginx/modules-enabled/*.conf;"
  WHERE="debian-13 host $IP ($(target 'nginx -v 2>&1' </dev/null))"
else
  command -v podman >/dev/null || die "podman not found"
  CONTAINER="fraisier-scaffold-nginx-$$"
  podman run -d --rm --user 0 --name "$CONTAINER" --entrypoint sleep "$IMAGE" infinity >/dev/null
  target() { podman exec -i "$CONTAINER" sh -c "$1"; }
  MODULES=""
  WHERE="$IMAGE ($(target 'nginx -v 2>&1' </dev/null))"
fi

body="$(head -c 4000 /dev/zero | tr '\0' x)"
cat >"$WORK/nginx.conf" <<EOF
pid /tmp/fraisier-nginx/nginx.pid;
error_log /tmp/fraisier-nginx/error.log;
$MODULES
events {}
http {
    include /etc/nginx/mime.types;
    access_log off;
    client_body_temp_path /tmp/fraisier-nginx;
    proxy_temp_path /tmp/fraisier-nginx;
    fastcgi_temp_path /tmp/fraisier-nginx;
    uwsgi_temp_path /tmp/fraisier-nginx;
    scgi_temp_path /tmp/fraisier-nginx;
    server {
        listen 127.0.0.1:8080;
        location / {
            default_type application/json;
            return 200 '{"data":"$body"}';
        }
    }
    include /tmp/fraisier-nginx/site.conf;
}
EOF
target 'mkdir -p /tmp/fraisier-nginx && cat > /tmp/fraisier-nginx/nginx.conf' <"$WORK/nginx.conf"

# Render the site for a config whose extra TOML is $2, into $WORK/$1.
render() {
  local name="$1" extra="$2" dir="$WORK/$1"
  mkdir -p "$dir"
  cat >"$dir/fraisier.toml" <<EOF
[deploy]
name = "shop"
environment = "production"

[service]
adapter = "systemd"
unit = "shop.service"
run_as = "shop"
$extra
EOF
  "$FRAISIER" scaffold --config "$dir/fraisier.toml" --out "$dir/out" >"$dir/log" 2>&1
}

# Render $1's site (extra TOML $3), load it, start nginx, and print the
# Content-Encoding a client accepting $2 gets: "identity" when none, or what
# went wrong instead.
encoding() {
  local name="$1" accept="$2" check headers
  if ! render "$name" "$3"; then
    echo "scaffold failed: $(head -c 300 "$WORK/$name/log")"
    return
  fi
  target 'cat > /tmp/fraisier-nginx/site.conf' <"$WORK/$name/out/nginx/shop.conf"
  if ! check="$(target 'nginx -t -c /tmp/fraisier-nginx/nginx.conf 2>&1' </dev/null)"; then
    echo "nginx -t failed: $check"
    return
  fi
  target 'nginx -c /tmp/fraisier-nginx/nginx.conf' </dev/null
  if headers="$(target "curl -sf -o /dev/null -D - -H 'Accept-Encoding: $accept' \
      http://127.0.0.1/" </dev/null)"; then
    printf '%s\n' "$headers" | tr -d '\r' \
      | awk -F': ' 'tolower($1) == "content-encoding" { e = $2 } END { print e ? e : "identity" }'
  else
    echo "no response"
  fi
  target 'nginx -c /tmp/fraisier-nginx/nginx.conf -s stop 2>/dev/null; sleep 1' </dev/null
}

say "fraisier ($FRAISIER) against $WHERE"

seen="$(encoding default gzip "")"
if [ "$seen" = gzip ]; then pass "the default site loads and gzips JSON"
else fail "default: expected gzip, got '$seen'"; fi

seen="$(encoding off gzip "$(printf '\n[scaffold.nginx]\ngzip = false\n')")"
if [ "$seen" = identity ]; then pass "gzip = false loads and leaves JSON uncompressed"
else fail "gzip = false: expected identity, got '$seen'"; fi

if [ "$BROTLI" = 1 ]; then
  seen="$(encoding brotli br "$(printf '\n[scaffold.nginx]\nbrotli = true\n')")"
  if [ "$seen" = br ]; then pass "brotli = true loads and brotli-compresses JSON"
  else fail "brotli: expected br, got '$seen'"; fi
fi

echo
echo "checkpoint-scaffold-nginx: $PASSED/$((PASSED + FAILED)) passed"
[ "$FAILED" -eq 0 ]
