#!/usr/bin/env bash
# Prepare the host-owned persistent directory used by the Docker deployment.
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DATA_DIR="${1:-${WOTOHA_DATA_DIR:-$ROOT/data}}"
RUNTIME_ENV="${2:-$ROOT/runtime.env}"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

if (( EUID != 0 )); then
  fail "run as root, for example: sudo bash deploy/prepare-docker-data.sh"
fi

DATA_DIR="$(realpath -m -- "$DATA_DIR")"
RUNTIME_ENV="$(realpath -m -- "$RUNTIME_ENV")"
[[ "$DATA_DIR" != / ]] || fail 'refusing to prepare / as the persistent directory'
[[ "$DATA_DIR" != "$ROOT" ]] || fail 'refusing to prepare the repository root as persistent data'
[[ ! -L "$DATA_DIR" ]] || fail 'persistent data path must not be a symlink'

[[ -f "$RUNTIME_ENV" ]] || fail "runtime environment file does not exist: $RUNTIME_ENV"
[[ ! -L "$RUNTIME_ENV" ]] || fail 'runtime environment file must not be a symlink'

runtime_mode="$(stat -c '%a' "$RUNTIME_ENV")"
(( (8#$runtime_mode & 077) == 0 )) || fail 'runtime.env must not be readable by group or other users'
if ! awk -F= '$1 == "DISCORD_TOKEN" && length($2) > 0 { found = 1 } END { exit(found ? 0 : 1) }' "$RUNTIME_ENV"; then
  fail 'runtime.env must contain a non-empty DISCORD_TOKEN'
fi
chmod 0600 "$RUNTIME_ENV"

install -d -o 10001 -g 10001 -m 0700 \
  "$DATA_DIR" \
  "$DATA_DIR/cache" \
  "$DATA_DIR/cache/analysis" \
  "$DATA_DIR/logs" \
  "$DATA_DIR/tools"
chown -R 10001:10001 "$DATA_DIR"
find "$DATA_DIR" -type d -exec chmod 0700 {} +
find "$DATA_DIR" -type f -exec chmod 0600 {} +
if [[ -f "$DATA_DIR/tools/yt-dlp" ]]; then
  chmod 0700 "$DATA_DIR/tools/yt-dlp"
fi

if command -v setpriv >/dev/null 2>&1; then
  setpriv --reuid=10001 --regid=10001 --init-groups -- \
    sh -c 'probe="$1/.wotoha-write-probe"; : > "$probe"; rm -f -- "$probe"' sh "$DATA_DIR" \
    || fail 'UID 10001 cannot write the prepared persistent directory'
else
  printf 'WARNING: setpriv is unavailable; verify UID 10001 with the container self-check before starting Compose.\n' >&2
fi

printf 'Prepared Docker data directory: %s\n' "$DATA_DIR"
printf 'Persistent owner: 10001:10001\n'
printf 'runtime.env: private mode 0600 with a non-empty token\n'
