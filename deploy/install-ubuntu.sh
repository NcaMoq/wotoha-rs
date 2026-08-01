#!/usr/bin/env bash
set -euo pipefail

PACKAGE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if ! getent group wotoha >/dev/null; then
  groupadd --system wotoha
fi

if ! id -u wotoha >/dev/null 2>&1; then
  useradd --system --gid wotoha --home /var/lib/wotoha --shell /usr/sbin/nologin wotoha
fi

install -d -m 0755 /opt/wotoha/bin
install -d -m 0755 /opt/wotoha/workers
install -d -m 0755 /opt/wotoha/workers/versions
install -d -m 0755 /etc/wotoha
install -d -o wotoha -g wotoha -m 0755 /var/lib/wotoha
install -d -o wotoha -g wotoha -m 0755 /var/log/wotoha
install -d -o root -g root -m 0755 /var/lib/wotoha-updater

# Bootstrap and canary yt-dlp/Deno before installing or starting any app that may require them.
bash "$PACKAGE_DIR/install-yt-dlp-bundle.sh" "$PACKAGE_DIR"

install -m 0755 "$PACKAGE_DIR/bin/wotoha-app" /opt/wotoha/bin/wotoha-app
install -m 0755 "$PACKAGE_DIR/bin/wotoha-youtube-js-worker" /opt/wotoha/bin/wotoha-youtube-js-worker
worker_digest="$(sha256sum "$PACKAGE_DIR/bin/wotoha-youtube-js-worker" | awk '{print $1}')"
worker_sequence="$(tr -d '\r\n' < "$PACKAGE_DIR/deploy/YOUTUBE_WORKER_SEQUENCE")"
case "$worker_sequence" in
  ''|*[!0-9]*|0) echo "invalid YouTube worker sequence" >&2; exit 1 ;;
esac
worker_version_dir="/opt/wotoha/workers/versions/$worker_digest"
install -d -m 0755 "$worker_version_dir"
install -m 0755 "$PACKAGE_DIR/bin/wotoha-youtube-js-worker" \
  "$worker_version_dir/wotoha-youtube-js-worker"
if [ ! -r /opt/wotoha/workers/current ]; then
  printf '%s\n' "$worker_digest" > /opt/wotoha/workers/current.new
  chmod 0644 /opt/wotoha/workers/current.new
  mv -f /opt/wotoha/workers/current.new /opt/wotoha/workers/current
fi
installed_worker_digest="$(tr -d '\r\n' < /opt/wotoha/workers/current)"
case "$installed_worker_digest" in
  *[!0-9a-f]*|'') echo "invalid installed YouTube worker pointer" >&2; exit 1 ;;
esac
if [ "${#installed_worker_digest}" -ne 64 ] \
  || [ ! -x "/opt/wotoha/workers/versions/$installed_worker_digest/wotoha-youtube-js-worker" ] \
  || [ "$(sha256sum "/opt/wotoha/workers/versions/$installed_worker_digest/wotoha-youtube-js-worker" | awk '{print $1}')" != "$installed_worker_digest" ]; then
  echo "installed YouTube worker does not match its pointer" >&2
  exit 1
fi
if [ "$installed_worker_digest" = "$worker_digest" ]; then
  printf '%s\n' "$worker_sequence" > /opt/wotoha/YOUTUBE_WORKER_SEQUENCE.new
  chmod 0644 /opt/wotoha/YOUTUBE_WORKER_SEQUENCE.new
  mv -f /opt/wotoha/YOUTUBE_WORKER_SEQUENCE.new \
    /opt/wotoha/YOUTUBE_WORKER_SEQUENCE
  printf '{"sequence":%s,"sha256":"%s","tag":"bundled"}\n' \
    "$worker_sequence" "$worker_digest" \
    > /var/lib/wotoha-updater/installed-youtube-worker.new
  chmod 0644 /var/lib/wotoha-updater/installed-youtube-worker.new
  mv -f /var/lib/wotoha-updater/installed-youtube-worker.new \
    /var/lib/wotoha-updater/installed-youtube-worker
fi
install -m 0755 "$PACKAGE_DIR/wotoha-update.sh" /opt/wotoha/bin/wotoha-update
install -m 0644 "$PACKAGE_DIR/deploy/wotoha.service" /etc/systemd/system/wotoha.service
install -m 0644 "$PACKAGE_DIR/deploy/wotoha-update.service" /etc/systemd/system/wotoha-update.service
install -m 0644 "$PACKAGE_DIR/deploy/wotoha-update.timer" /etc/systemd/system/wotoha-update.timer
install -m 0644 "$PACKAGE_DIR/deploy/youtube-clients.json" /etc/wotoha/youtube-clients.json

if [ ! -f /etc/wotoha/wotoha.env ]; then
  install -m 0600 "$PACKAGE_DIR/deploy/wotoha.env.example" /etc/wotoha/wotoha.env
fi
if ! grep -q '^WOTOHA_YOUTUBE_JS_WORKER_DIR=' /etc/wotoha/wotoha.env; then
  printf '%s\n' 'WOTOHA_YOUTUBE_JS_WORKER_DIR=/opt/wotoha/workers' \
    >> /etc/wotoha/wotoha.env
fi
if ! grep -q '^WOTOHA_YOUTUBE_JS_WORKER_ACK=' /etc/wotoha/wotoha.env; then
  printf '%s\n' 'WOTOHA_YOUTUBE_JS_WORKER_ACK=/var/lib/wotoha/youtube-worker-ack' \
    >> /etc/wotoha/wotoha.env
fi
if ! grep -q '^WOTOHA_YTDLP_PATH=' /etc/wotoha/wotoha.env; then
  printf '%s\n' 'WOTOHA_YTDLP_PATH=/opt/wotoha/bin/yt-dlp' >> /etc/wotoha/wotoha.env
fi
if ! grep -q '^WOTOHA_DENO_PATH=' /etc/wotoha/wotoha.env; then
  printf '%s\n' 'WOTOHA_DENO_PATH=/opt/wotoha/bin/deno' >> /etc/wotoha/wotoha.env
fi

if [ ! -f /etc/wotoha/wotoha-update.env ]; then
  install -m 0600 "$PACKAGE_DIR/deploy/wotoha-update.env.example" /etc/wotoha/wotoha-update.env
fi

if [ -r "$PACKAGE_DIR/RELEASE_VERSION" ]; then
  release_version="$(tr -d '\r\n' < "$PACKAGE_DIR/RELEASE_VERSION")"
  case "$release_version" in
    v*) printf '%s\n' "$release_version" > /var/lib/wotoha-updater/installed-release ;;
  esac
fi

chown -R wotoha:wotoha /var/lib/wotoha /var/log/wotoha

systemctl daemon-reload
systemctl enable wotoha.service
systemctl enable --now wotoha-update.timer
systemctl enable --now yt-dlp-update.timer

if ! command -v gh >/dev/null 2>&1 \
  || ! gh attestation verify --help 2>&1 \
    | grep -q -- '--deny-self-hosted-runners'; then
  printf '%s\n' \
    'WARNING: install current GitHub CLI from the official repository; signed automatic updates remain disabled until then.' >&2
fi

printf '%s\n' 'Edit /etc/wotoha/wotoha.env and set DISCORD_TOKEN.'
printf '%s\n' 'After that, run: sudo systemctl restart wotoha.service'
