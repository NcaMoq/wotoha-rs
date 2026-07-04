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

install -m 0755 "$PACKAGE_DIR/bin/wotoha-app" /opt/wotoha/bin/wotoha-app
install -m 0755 "$PACKAGE_DIR/wotoha-update.sh" /opt/wotoha/bin/wotoha-update
install -m 0644 "$PACKAGE_DIR/deploy/wotoha.service" /etc/systemd/system/wotoha.service
install -m 0644 "$PACKAGE_DIR/deploy/wotoha-update.service" /etc/systemd/system/wotoha-update.service
install -m 0644 "$PACKAGE_DIR/deploy/wotoha-update.timer" /etc/systemd/system/wotoha-update.timer

if [ ! -f /etc/wotoha/wotoha.env ]; then
  install -m 0600 "$PACKAGE_DIR/deploy/wotoha.env.example" /etc/wotoha/wotoha.env
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

printf '%s\n' 'Edit /etc/wotoha/wotoha.env and set DISCORD_TOKEN.'
printf '%s\n' 'After that, run: sudo systemctl restart wotoha.service'
