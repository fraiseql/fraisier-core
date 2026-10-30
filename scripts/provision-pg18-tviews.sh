#!/usr/bin/env bash
# Get a Debian host ready for a deployable that declares
#
#   [database.requires]
#   postgres_major = 18
#   pg_tviews = true
#
# It installs PostgreSQL 18 from the PGDG repository, builds pg_tviews at a pinned
# commit, adds it to shared_preload_libraries, restarts PostgreSQL only when that
# setting changed, creates the database if it is absent, and creates the extension
# in it. fraisier itself never does any of this (docs/database-requirements.md):
# it needs root on the host and superuser in the database.
#
# The script is idempotent. Each step checks before it acts, and the last line
# counts what changed: a second run prints "0 change(s)" and does not restart
# PostgreSQL.
#
# Usage (as root, on Debian 12 bookworm or 13 trixie):
#   scripts/provision-pg18-tviews.sh --database <name>
set -euo pipefail

PG_MAJOR=18
# The commit of pg_tviews' signed v0.1.0-beta.32 tag. fraisier speaks its read
# contract 2, which 0.1.0-beta.30 and later offer.
PG_TVIEWS_REF=778346fb5df06a38f470901974d609d2b9dc26a6
PG_TVIEWS_REPO=https://github.com/fraiseql/pg_tviews
# pg_tviews pins pgrx = "=0.17.0"; the toolchain is the one its CI image uses.
PGRX_VERSION=0.17.0
RUST_TOOLCHAIN=1.91.1
# The PGDG archive signing key (https://www.postgresql.org/download/linux/debian/).
PGDG_FINGERPRINT=B97B0AFCAA1A47F044F244A07FCC7D46ACCC4CF8

STATE=/var/lib/fraisier-provision
export RUSTUP_HOME="$STATE/rustup" CARGO_HOME="$STATE/cargo"
PG_CONFIG="/usr/lib/postgresql/$PG_MAJOR/bin/pg_config"
CLUSTER_UNIT="postgresql@$PG_MAJOR-main.service"

CHANGES=0
say(){ printf '\033[1;36m==>\033[0m %s\n' "$*"; }
same(){ printf '  ok      %s\n' "$*"; }
done_(){ printf '  changed %s\n' "$*"; CHANGES=$((CHANGES + 1)); }
die(){ printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

DATABASE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --database) DATABASE="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,19p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done
[ -n "$DATABASE" ] || die "--database <name> is required"
case "$DATABASE" in
  [a-z_]*) ;; *) die "--database must start with a-z or _ (got $DATABASE)" ;;
esac
case "$DATABASE" in
  *[!a-z0-9_]*) die "--database may hold only a-z, 0-9 and _ (got $DATABASE)" ;;
esac

[ "$(id -u)" = 0 ] || die "run as root"
# shellcheck source=/dev/null
. /etc/os-release
case "${VERSION_CODENAME:-}" in
  bookworm|trixie) ;;
  *) die "this script supports Debian 12 (bookworm) and 13 (trixie), not '${PRETTY_NAME:-unknown}'" ;;
esac
export DEBIAN_FRONTEND=noninteractive

# psql as the postgres superuser, over the local socket (peer authentication).
psql_su(){ runuser -u postgres -- psql -X -q -v ON_ERROR_STOP=1 "$@"; }
scalar(){ psql_su -tA "$@"; }

say "PGDG repository"
KEYRING=/usr/share/keyrings/pgdg.gpg
LIST=/etc/apt/sources.list.d/pgdg.list
LINE="deb [signed-by=$KEYRING] https://apt.postgresql.org/pub/repos/apt $VERSION_CODENAME-pgdg main"
if [ -f "$KEYRING" ] && [ "$(cat "$LIST" 2>/dev/null)" = "$LINE" ]; then
  same "$LIST"
else
  apt-get update -qq
  apt-get install -y -qq curl gnupg ca-certificates >/dev/null
  key="$(mktemp)"
  curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc -o "$key"
  fingerprint="$(gpg --show-keys --with-colons "$key" | awk -F: '$1 == "fpr" { print $10; exit }')"
  [ "$fingerprint" = "$PGDG_FINGERPRINT" ] \
    || die "the downloaded PGDG key has fingerprint '$fingerprint', not $PGDG_FINGERPRINT"
  gpg --dearmor < "$key" > "$KEYRING"
  rm -f "$key"
  printf '%s\n' "$LINE" > "$LIST"
  apt-get update -qq
  done_ "$LIST"
fi

say "packages"
PACKAGES=("postgresql-$PG_MAJOR" "postgresql-server-dev-$PG_MAJOR" build-essential pkg-config
  clang libclang-dev git curl ca-certificates)
missing=()
for package in "${PACKAGES[@]}"; do
  dpkg-query -W -f='${Status}' "$package" 2>/dev/null | grep -q '^install ok installed$' \
    || missing+=("$package")
done
if [ ${#missing[@]} -eq 0 ]; then
  same "${PACKAGES[*]}"
else
  apt-get install -y -qq "${missing[@]}" >/dev/null
  done_ "installed ${missing[*]}"
fi
systemctl is-active --quiet "$CLUSTER_UNIT" || systemctl start "$CLUSTER_UNIT"

say "pg_tviews $PG_TVIEWS_REF"
LIB="$($PG_CONFIG --pkglibdir)/pg_tviews.so"
STAMP="$STATE/pg_tviews.ref"
REBUILT=0
if [ -f "$LIB" ] && [ "$(cat "$STAMP" 2>/dev/null)" = "$PG_TVIEWS_REF" ]; then
  same "$LIB is built from the pinned commit"
else
  mkdir -p "$STATE"
  if [ ! -x "$CARGO_HOME/bin/rustup" ]; then
    # rustup-init, checked against the digest published beside it.
    init="$(mktemp -d)"
    base=https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu
    curl -fsSL "$base/rustup-init" -o "$init/rustup-init"
    curl -fsSL "$base/rustup-init.sha256" -o "$init/rustup-init.sha256"
    expected="$(cut -d' ' -f1 "$init/rustup-init.sha256")"
    actual="$(sha256sum "$init/rustup-init" | cut -d' ' -f1)"
    [ "$actual" = "$expected" ] || die "rustup-init's digest is $actual, not the published $expected"
    chmod +x "$init/rustup-init"
    "$init/rustup-init" -y -q --no-modify-path --profile minimal --default-toolchain "$RUST_TOOLCHAIN"
    rm -rf "$init"
  fi
  PATH="$CARGO_HOME/bin:$PATH"
  if [ "$(cargo pgrx --version 2>/dev/null)" != "cargo-pgrx $PGRX_VERSION" ]; then
    cargo install --locked --quiet cargo-pgrx --version "$PGRX_VERSION"
  fi
  cargo pgrx init --pg$PG_MAJOR "$PG_CONFIG" >/dev/null
  src="$STATE/pg_tviews-src"
  rm -rf "$src"
  git init -q "$src"
  git -C "$src" fetch -q --depth 1 "$PG_TVIEWS_REPO" "$PG_TVIEWS_REF"
  git -C "$src" checkout -q FETCH_HEAD
  (cd "$src" && cargo pgrx install --release --no-default-features --features "pg$PG_MAJOR" \
    --pg-config "$PG_CONFIG" >/dev/null)
  printf '%s\n' "$PG_TVIEWS_REF" > "$STAMP"
  REBUILT=1
  done_ "built and installed $LIB"
fi

say "shared_preload_libraries"
RESTART=0
preload="$(scalar -c 'SHOW shared_preload_libraries')"
case "$preload" in
  *[!A-Za-z0-9_,.\ -]*) die "shared_preload_libraries holds unexpected characters: '$preload'" ;;
esac
if printf '%s' "$preload" | tr ',' '\n' | tr -d ' ' | grep -qx pg_tviews; then
  same "pg_tviews is preloaded ('$preload')"
  # A preloaded library that was just rebuilt is only picked up by a restart.
  [ "$REBUILT" = 1 ] && RESTART=1
else
  wanted="${preload:+$preload,}pg_tviews"
  psql_su -c "ALTER SYSTEM SET shared_preload_libraries = '$wanted'"
  RESTART=1
  done_ "shared_preload_libraries = '$wanted'"
fi
if [ "$RESTART" = 1 ]; then
  systemctl restart "$CLUSTER_UNIT"
  for _ in $(seq 1 30); do runuser -u postgres -- pg_isready -q && break; sleep 1; done
  runuser -u postgres -- pg_isready -q || die "PostgreSQL did not come back after the restart"
  done_ "restarted $CLUSTER_UNIT"
fi

say "database $DATABASE"
if [ "$(scalar -c "SELECT 1 FROM pg_database WHERE datname = '$DATABASE'")" = 1 ]; then
  same "$DATABASE exists"
else
  psql_su -c "CREATE DATABASE \"$DATABASE\""
  done_ "created $DATABASE"
fi

say "extension pg_tviews in $DATABASE"
if [ "$(scalar -d "$DATABASE" -c "SELECT 1 FROM pg_extension WHERE extname = 'pg_tviews'")" = 1 ]; then
  if [ "$REBUILT" = 1 ]; then
    psql_su -d "$DATABASE" -c 'ALTER EXTENSION pg_tviews UPDATE'
    done_ "updated the extension to the rebuilt library"
  else
    same "pg_tviews is installed"
  fi
else
  psql_su -d "$DATABASE" -c 'CREATE EXTENSION pg_tviews'
  done_ "created the extension"
fi

say "$CHANGES change(s)"
