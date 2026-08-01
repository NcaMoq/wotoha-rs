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
install -d -m 0755 /etc/wotoha
install -d -o wotoha -g wotoha -m 0755 /var/lib/wotoha
install -d -o wotoha -g wotoha -m 0755 /var/log/wotoha
install -d -o root -g root -m 0755 /var/lib/wotoha-updater

# Bootstrap and canary yt-dlp/Deno before installing or starting any app that may require them.
bash "$PACKAGE_DIR/install-yt-dlp-bundle.sh" "$PACKAGE_DIR"

install -m 0755 "$PACKAGE_DIR/bin/wotoha-app" /opt/wotoha/bin/wotoha-app
install -m 0755 "$PACKAGE_DIR/wotoha-update.sh" /opt/wotoha/bin/wotoha-update
install -m 0644 "$PACKAGE_DIR/deploy/wotoha.service" /etc/systemd/system/wotoha.service
install -m 0644 "$PACKAGE_DIR/deploy/wotoha-update.service" /etc/systemd/system/wotoha-update.service
install -m 0644 "$PACKAGE_DIR/deploy/wotoha-update.timer" /etc/systemd/system/wotoha-update.timer

if [ ! -f /etc/wotoha/wotoha.env ]; then
  install -m 0600 "$PACKAGE_DIR/deploy/wotoha.env.example" /etc/wotoha/wotoha.env
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
