#!/usr/bin/env bash
set -Eeuo pipefail

readonly CONFIG=/etc/wotoha/wotoha-update.env
readonly INSTALL_DIR=/opt/wotoha/bin
readonly STATE_FILE=/var/lib/wotoha-updater/installed-release
readonly ASSET=wotoha-ubuntu-x86_64-musl.tar.gz

if [[ -r "$CONFIG" ]]; then
  # shellcheck disable=SC1090
  source "$CONFIG"
fi

repository="${WOTOHA_UPDATE_REPOSITORY:-NcaMoq/wotoha-rs}"
force=false
if [[ "${1:-}" == "--force" ]]; then
  force=true
elif [[ $# -gt 0 ]]; then
  echo "usage: wotoha-update [--force]" >&2
  exit 2
fi
if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "invalid WOTOHA_UPDATE_REPOSITORY: $repository" >&2
  exit 2
fi

token="${WOTOHA_UPDATE_GITHUB_TOKEN:-}"
if [[ -n "$token" && ! "$token" =~ ^[A-Za-z0-9_]+$ ]]; then
  echo "invalid WOTOHA_UPDATE_GITHUB_TOKEN" >&2
  exit 2
fi

for command in curl flock jq sha256sum tar systemctl; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "required command is missing: $command" >&2
    exit 1
  fi
done

exec 9>/run/lock/wotoha-update.lock
if ! flock -n 9; then
  echo "another update is already running"
  exit 0
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

curl_auth=()
if [[ -n "$token" ]]; then
  auth_config="$tmp_dir/curl-auth.conf"
  printf 'header = "Authorization: Bearer %s"\n' "$token" > "$auth_config"
  chmod 0600 "$auth_config"
  curl_auth=(--config "$auth_config")
fi

release_json="$tmp_dir/release.json"
curl "${curl_auth[@]}" --fail --silent --show-error --location --retry 3 \
  --header "Accept: application/vnd.github+json" \
  --header "X-GitHub-Api-Version: 2022-11-28" \
  "https://api.github.com/repos/$repository/releases/latest" \
  --output "$release_json"
tag="$(jq --exit-status --raw-output '.tag_name' "$release_json")"

if [[ -r "$STATE_FILE" && "$(<"$STATE_FILE")" == "$tag" ]]; then
  echo "wotoha is already at $tag"
  exit 0
fi

archive="$tmp_dir/$ASSET"
archive_api_url="$(jq --exit-status --raw-output --arg name "$ASSET" '.assets[] | select(.name == $name) | .url' "$release_json")"
checksum_api_url="$(jq --exit-status --raw-output --arg name "$ASSET.sha256" '.assets[] | select(.name == $name) | .url' "$release_json")"
curl "${curl_auth[@]}" --fail --silent --show-error --location --retry 3 \
  --header "Accept: application/octet-stream" "$archive_api_url" --output "$archive"
curl "${curl_auth[@]}" --fail --silent --show-error --location --retry 3 \
  --header "Accept: application/octet-stream" "$checksum_api_url" --output "$archive.sha256"
(cd "$tmp_dir" && sha256sum --check "$ASSET.sha256")
tar -xzf "$archive" -C "$tmp_dir"
package="$tmp_dir/wotoha-ubuntu-x86_64-musl"
if [[ ! -d "$package" ]]; then
  echo "release archive has an unexpected layout" >&2
  exit 1
fi
(cd "$package" && sha256sum --check SHA256SUMS.txt)

new_binary="$package/bin/wotoha-app"
if [[ -x "$INSTALL_DIR/wotoha-app" ]] && cmp --silent "$new_binary" "$INSTALL_DIR/wotoha-app"; then
  printf '%s\n' "$tag" > "$STATE_FILE"
  echo "binary is already current; recorded $tag"
  exit 0
fi
if [[ ! -e "$STATE_FILE" && "$force" == false ]]; then
  printf '%s\n' "$tag" > "$STATE_FILE"
  echo "recorded $tag as the update baseline; kept the untracked installed binary"
  exit 0
fi

install -m 0755 "$new_binary" "$INSTALL_DIR/wotoha-app.new"
was_active=false
if systemctl is-active --quiet wotoha.service; then
  was_active=true
fi
if [[ -x "$INSTALL_DIR/wotoha-app" ]]; then
  cp -a "$INSTALL_DIR/wotoha-app" "$INSTALL_DIR/wotoha-app.previous"
fi
mv -f "$INSTALL_DIR/wotoha-app.new" "$INSTALL_DIR/wotoha-app"

restart_ok=true
if [[ "$was_active" == true ]]; then
  if ! systemctl restart wotoha.service; then
    restart_ok=false
  fi
  sleep 5
fi
if [[ "$was_active" == true ]] && { [[ "$restart_ok" == false ]] || ! systemctl is-active --quiet wotoha.service; }; then
  echo "updated service failed; rolling back" >&2
  if [[ -x "$INSTALL_DIR/wotoha-app.previous" ]]; then
    mv -f "$INSTALL_DIR/wotoha-app.previous" "$INSTALL_DIR/wotoha-app"
    systemctl restart wotoha.service
  fi
  exit 1
fi

printf '%s\n' "$tag" > "$STATE_FILE"
echo "updated wotoha to $tag"
