#!/usr/bin/env bash
# Deployment contract checks. Native application self-update is intentionally
# absent; the independent verified yt-dlp updater remains covered here.
set -Eeuo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
UPDATE="$ROOT/deploy/yt-dlp-update.sh"
BOOTSTRAP="$ROOT/deploy/install-yt-dlp-bundle.sh"
APP_UPDATE="$ROOT/deploy/wotoha-update.sh"
PACKAGER="$ROOT/deploy/package-release-assets.sh"
INSTALLER="$ROOT/deploy/install-ubuntu.sh"
VERIFY="$ROOT/deploy/verify-release-archives.sh"
COMPOSE="$ROOT/compose.yaml"

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

pass() {
  printf 'ok - %s\n' "$*"
}

for command in bash grep sed awk; do
  command -v "$command" >/dev/null 2>&1 || fail "test prerequisite is missing: $command"
done

bash -n "$UPDATE" "$BOOTSTRAP" "$APP_UPDATE" "$PACKAGER" "$INSTALLER" "$VERIFY" "$0"
bash "$ROOT/deploy/tests/release-compliance.sh"

grep -Fq -- '--retry-all-errors' "$UPDATE" \
  || fail 'yt-dlp updater is missing bounded retry handling'
grep -Fq -- '--socket-timeout' "$UPDATE" \
  || fail 'yt-dlp updater is missing extractor socket bounds'
grep -Fq -- '--extractor-retries' "$UPDATE" \
  || fail 'yt-dlp updater is missing extractor retry bounds'
grep -Fq -- 'SHA2-256SUMS.sig' "$UPDATE" \
  || fail 'yt-dlp updater is missing signed checksum verification'
grep -Fq -- 'DENO_X86_64_LINUX_GNU_SHA256' "$BOOTSTRAP" \
  || fail 'yt-dlp bootstrap is missing the pinned Deno digest'
grep -Fq -- 'actual_yt_dlp_version' "$BOOTSTRAP" \
  || fail 'yt-dlp bootstrap is missing version verification'

! grep -Fq 'wotoha-update.sh' "$PACKAGER" \
  || fail 'application updater is still copied into native release archives'
! grep -Fq 'wotoha-update.service' "$INSTALLER" \
  || fail 'application updater service is still installed'
if "$APP_UPDATE" >/dev/null 2>&1; then
  fail 'disabled application updater unexpectedly succeeded'
fi
pass 'native application updater is disabled while yt-dlp updater remains bounded'

if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  WOTOHA_IMAGE_TAG=sha-test DISCORD_TOKEN= docker compose -f "$COMPOSE" config >/dev/null \
    || fail 'Compose configuration is invalid'
  grep -Fq 'read_only: true' "$COMPOSE" \
    || fail 'Compose root filesystem is not read-only'
  grep -Fq 'cap_drop:' "$COMPOSE" \
    || fail 'Compose does not drop capabilities'
  grep -Fq 'stop_grace_period: 30s' "$COMPOSE" \
    || fail 'Compose stop grace period is not explicit'
  pass 'Compose production contract parses locally'
else
  pass 'Compose runtime validation skipped because Docker is unavailable'
fi

git -C "$ROOT" diff --check -- deploy compose.yaml Dockerfile
printf 'ok - deployment contract suite\n'
