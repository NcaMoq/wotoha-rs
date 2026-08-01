#!/usr/bin/env bash
set -Eeuo pipefail

readonly CONFIG=/etc/wotoha/wotoha-update.env
readonly INSTALL_DIR=/opt/wotoha/bin
readonly STATE_FILE=/var/lib/wotoha-updater/installed-release
readonly APP_DIGEST_STATE=/var/lib/wotoha-updater/installed-app-sha256
readonly BASELINE_MARKER=/var/lib/wotoha-updater/unmanaged-release-baseline
readonly YTDLP_ROOT=/opt/wotoha/yt-dlp
readonly YTDLP_STATE=/var/lib/wotoha-updater/installed-yt-dlp
readonly ASSET=wotoha-ubuntu-x86_64-musl.tar.gz
readonly RELEASE_MANIFEST="$ASSET.manifest.json"
readonly RELEASE_ATTESTATION="$ASSET.attestation.jsonl"
readonly MAX_RELEASE_ARCHIVE_BYTES=$((512 * 1024 * 1024))
readonly MAX_MANIFEST_BYTES=$((64 * 1024))
readonly MAX_ATTESTATION_BYTES=$((8 * 1024 * 1024))
readonly MAX_CHECKSUM_BYTES=$((64 * 1024))
readonly MAX_API_JSON_BYTES=$((4 * 1024 * 1024))

[[ -r "$CONFIG" ]] && source "$CONFIG"

repository="${WOTOHA_UPDATE_REPOSITORY:-NcaMoq/wotoha-rs}"
force=false
if [[ "${1:-}" == --force ]]; then
  force=true
elif (( $# > 0 )); then
  echo "usage: wotoha-update [--force]" >&2
  exit 2
fi
[[ "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || {
  echo "invalid WOTOHA_UPDATE_REPOSITORY: $repository" >&2
  exit 2
}
token="${WOTOHA_UPDATE_GITHUB_TOKEN:-}"
[[ -z "$token" || "$token" =~ ^[A-Za-z0-9_]+$ ]] || {
  echo "invalid WOTOHA_UPDATE_GITHUB_TOKEN" >&2
  exit 2
}

for command in awk chmod cmp cp curl flock gh grep install jq mktemp mv readlink rm sha256sum sleep sort stat systemctl tar timeout; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "required command is missing: $command" >&2
    exit 1
  }
done

exec 9>/run/lock/wotoha-update.lock
flock -n 9 || { echo "another update is already running"; exit 0; }
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

curl_auth=()
if [[ -n "$token" ]]; then
  curl_auth+=(--header "Authorization: Bearer $token")
fi
curl_retry=(--retry 4 --retry-all-errors --retry-delay 2 --retry-max-time 45 --connect-timeout 10)

is_sha256() {
  [[ "$1" =~ ^[0-9a-f]{64}$ ]]
}

atomic_write() {
  local value="$1" destination="$2" mode="${3:-0644}"
  local temporary="$destination.new.$$"
  printf '%s\n' "$value" > "$temporary"
  chmod "$mode" "$temporary"
  mv -f "$temporary" "$destination"
}

download_release_asset() {
  local release_file="$1" asset_name="$2" destination="$3" maximum_bytes="$4"
  local asset_count asset_size asset_url
  asset_count="$(jq --arg name "$asset_name" '[.assets[] | select(.name == $name)] | length' "$release_file")"
  [[ "$asset_count" == 1 ]] || return 1
  asset_size="$(jq --exit-status --raw-output --arg name "$asset_name" '.assets[] | select(.name == $name) | .size' "$release_file")"
  [[ "$asset_size" =~ ^[0-9]+$ ]] \
    && (( asset_size > 0 && asset_size <= maximum_bytes )) || return 1
  asset_url="$(jq --exit-status --raw-output --arg name "$asset_name" '.assets[] | select(.name == $name) | .url' "$release_file")"
  curl "${curl_auth[@]}" --fail --silent --show-error --location "${curl_retry[@]}" \
    --max-filesize "$maximum_bytes" --remove-on-error \
    --header 'Accept: application/octet-stream' "$asset_url" --output "$destination"
  [[ "$(stat --format='%s' "$destination")" == "$asset_size" ]]
}

managed_ytdlp_is_valid() {
  [[ -r "$YTDLP_STATE" && -L "$YTDLP_ROOT/current" ]] || return 1
  [[ -x "$INSTALL_DIR/yt-dlp" && -x "$INSTALL_DIR/deno" ]] || return 1
  local managed_repository managed_tag digest extra target
  read -r managed_repository managed_tag digest extra < "$YTDLP_STATE" || return 1
  [[ -z "${extra:-}" ]] || return 1
  case "$managed_repository" in yt-dlp/yt-dlp|yt-dlp/yt-dlp-nightly-builds) ;; *) return 1 ;; esac
  [[ "$managed_tag" =~ ^[0-9]{4}[.][0-9]{2}[.][0-9]{2}([.][0-9]{6})?$ ]] || return 1
  is_sha256 "$digest" || return 1
  target="$(readlink "$YTDLP_ROOT/current")"
  [[ "$target" == "versions/$digest/yt-dlp" ]] || return 1
  [[ -x "$YTDLP_ROOT/current" ]] || return 1
  [[ "$(sha256sum "$YTDLP_ROOT/current" | awk '{print $1}')" == "$digest" ]] || return 1
  [[ "$(sha256sum "$INSTALL_DIR/yt-dlp" | awk '{print $1}')" == "$digest" ]] || return 1
  timeout --signal=TERM --kill-after=5s 20s "$INSTALL_DIR/deno" --version >/dev/null 2>&1
}

service_health_allows_cleanup() {
  if systemctl is-failed --quiet wotoha.service; then
    echo "wotoha.service is failed; refusing legacy cleanup" >&2
    return 1
  fi
  # A successful prior updater transaction guarantees that an active service
  # stayed active. Inactive is valid when the operator intentionally kept it off.
  return 0
}

cleanup_legacy_youtube() {
  local changed=false env_file=/etc/wotoha/wotoha.env filtered
  if [[ -r "$env_file" ]]; then
    filtered="$tmp_dir/wotoha.env.cleaned"
    awk '
      !/^(WOTOHA_YOUTUBE_CLIENTS_FILE|WOTOHA_YOUTUBE_PO_TOKEN_PROVIDER|WOTOHA_YOUTUBE_PO_TOKEN_TIMEOUT_SECONDS|WOTOHA_YOUTUBE_JS_WORKER|WOTOHA_YOUTUBE_JS_WORKER_DIR|WOTOHA_YOUTUBE_JS_WORKER_ACK)=/
    ' "$env_file" > "$filtered"
    if ! cmp --silent "$filtered" "$env_file"; then
      chmod --reference="$env_file" "$filtered"
      mv -f "$filtered" "$env_file"
      changed=true
    fi
  fi

  local legacy_path
  for legacy_path in \
    /opt/wotoha/bin/wotoha-youtube-js-worker \
    /opt/wotoha/bin/wotoha-youtube-js-worker.new \
    /opt/wotoha/bin/wotoha-youtube-js-worker.previous \
    /opt/wotoha/workers \
    /opt/wotoha/YOUTUBE_WORKER_SEQUENCE \
    /opt/wotoha/YOUTUBE_WORKER_SEQUENCE.new \
    /var/lib/wotoha/youtube-worker-ack \
    /var/lib/wotoha/.youtube-worker-ack-*.tmp \
    /var/lib/wotoha-updater/installed-youtube-worker \
    /var/lib/wotoha-updater/installed-youtube-worker.new.* \
    /var/lib/wotoha-updater/youtube-worker-candidate-tag \
    /var/lib/wotoha-updater/youtube-worker-candidate-tag.new.* \
    /etc/wotoha/youtube-clients.json; do
    if [[ -e "$legacy_path" || -L "$legacy_path" ]]; then
      rm -rf -- "$legacy_path"
      changed=true
    fi
  done
  if [[ "$changed" == true ]]; then
    echo "removed legacy native YouTube worker state after verified Phase B activation"
  else
    echo "legacy native YouTube worker state is already absent"
  fi
}

record_release_state() {
  local tag="$1" binary="$2" digest
  digest="$(sha256sum "$binary" | awk '{print $1}')"
  is_sha256 "$digest"
  atomic_write "$digest" "$APP_DIGEST_STATE"
  atomic_write "$tag" "$STATE_FILE"
  rm -f "$BASELINE_MARKER"
}

release_json="$tmp_dir/release.json"
curl "${curl_auth[@]}" --fail --silent --show-error --location "${curl_retry[@]}" \
  --max-filesize "$MAX_API_JSON_BYTES" --remove-on-error \
  --header 'Accept: application/vnd.github+json' \
  --header 'X-GitHub-Api-Version: 2022-11-28' \
  "https://api.github.com/repos/$repository/releases/latest" --output "$release_json"
tag="$(jq --exit-status --raw-output '.tag_name' "$release_json")"
[[ "$tag" =~ ^v[0-9]+([.][0-9]+){1,3}([_-][A-Za-z0-9.-]+)?$ ]] || {
  echo "latest release tag is invalid: $tag" >&2
  exit 1
}

same_tag_at_start=false
if [[ -r "$STATE_FILE" && "$(<"$STATE_FILE")" == "$tag" ]]; then
  same_tag_at_start=true
fi

if [[ "$same_tag_at_start" == true && "$force" == false ]]; then
  if [[ -r "$BASELINE_MARKER" ]]; then
    echo "latest release is only an unmanaged baseline; use --force before legacy cleanup"
    exit 0
  fi
  if managed_ytdlp_is_valid \
    && [[ -r "$APP_DIGEST_STATE" ]] \
    && is_sha256 "$(<"$APP_DIGEST_STATE")" \
    && [[ -x "$INSTALL_DIR/wotoha-app" ]] \
    && [[ "$(sha256sum "$INSTALL_DIR/wotoha-app" | awk '{print $1}')" == "$(<"$APP_DIGEST_STATE")" ]]; then
    service_health_allows_cleanup || exit 1
    cleanup_legacy_youtube
    echo "wotoha is already at $tag"
    exit 0
  fi
  echo "same-tag installation lacks complete Phase B proof; re-verifying the signed package"
fi

if ! gh attestation verify --help 2>&1 | grep -q -- '--deny-self-hosted-runners'; then
  echo "gh 2.49+ is required to verify the Wotoha release" >&2
  exit 1
fi

archive="$tmp_dir/$ASSET"
download_release_asset "$release_json" "$ASSET" "$archive" "$MAX_RELEASE_ARCHIVE_BYTES"
download_release_asset "$release_json" "$ASSET.sha256" "$archive.sha256" "$MAX_CHECKSUM_BYTES"
release_manifest="$tmp_dir/$RELEASE_MANIFEST"
release_attestation="$tmp_dir/$RELEASE_ATTESTATION"
download_release_asset "$release_json" "$RELEASE_MANIFEST" "$release_manifest" "$MAX_MANIFEST_BYTES"
download_release_asset "$release_json" "$RELEASE_ATTESTATION" "$release_attestation" "$MAX_ATTESTATION_BYTES"

verify_attestation() {
  local subject="$1"
  shift
  (
    export GH_CONFIG_DIR="$tmp_dir/gh-config"
    export XDG_CACHE_HOME="$tmp_dir/gh-cache"
    unset GITHUB_TOKEN
    if [[ -n "$token" ]]; then export GH_TOKEN="$token"; else unset GH_TOKEN; fi
    gh attestation verify "$subject" --bundle "$release_attestation" \
      --repo "$repository" \
      --signer-workflow "$repository/.github/workflows/release.yml" \
      --source-ref "refs/tags/$tag" --deny-self-hosted-runners "$@" \
      --format json >/dev/null
  )
}

verify_attestation "$release_manifest"
if ! jq --exit-status --arg tag "$tag" --arg asset "$ASSET" '
  .schema_version == 1
  and .tag == $tag
  and .asset == $asset
  and (.sha256 | type == "string" and test("^[0-9a-f]{64}$"))
  and (.commit | type == "string" and test("^[0-9a-f]{40}$"))
' "$release_manifest" >/dev/null; then
  echo "release archive manifest was invalid" >&2
  exit 1
fi
release_digest="$(jq --raw-output '.sha256' "$release_manifest")"
release_commit="$(jq --raw-output '.commit' "$release_manifest")"
[[ "$(sha256sum "$archive" | awk '{print $1}')" == "$release_digest" ]] || {
  echo "release archive digest did not match its manifest" >&2
  exit 1
}
verify_attestation "$archive" --source-digest "$release_commit"
verify_attestation "$release_manifest" --source-digest "$release_commit"
(cd "$tmp_dir" && sha256sum --check "$ASSET.sha256")
tar -xzf "$archive" -C "$tmp_dir"
package="$tmp_dir/wotoha-ubuntu-x86_64-musl"
[[ -d "$package" ]] || { echo "release archive has an unexpected layout" >&2; exit 1; }
(cd "$package" && sha256sum --check SHA256SUMS.txt)
[[ -x "$package/install-yt-dlp-bundle.sh" ]] || {
  echo "release archive is missing the managed yt-dlp bootstrap" >&2
  exit 1
}
bash "$package/install-yt-dlp-bundle.sh" "$package"
managed_ytdlp_is_valid || { echo "managed yt-dlp validation failed after bootstrap" >&2; exit 1; }

if [[ -r /etc/wotoha/wotoha.env ]]; then
  grep -q '^WOTOHA_YTDLP_PATH=' /etc/wotoha/wotoha.env \
    || printf '%s\n' 'WOTOHA_YTDLP_PATH=/opt/wotoha/bin/yt-dlp' >> /etc/wotoha/wotoha.env
  grep -q '^WOTOHA_DENO_PATH=' /etc/wotoha/wotoha.env \
    || printf '%s\n' 'WOTOHA_DENO_PATH=/opt/wotoha/bin/deno' >> /etc/wotoha/wotoha.env
fi
systemctl daemon-reload
systemctl enable --now yt-dlp-update.timer

if [[ -x "$package/wotoha-update.sh" ]] \
  && ! cmp --silent "$package/wotoha-update.sh" "$INSTALL_DIR/wotoha-update"; then
  install -m 0755 "$package/wotoha-update.sh" "$INSTALL_DIR/wotoha-update.new"
  mv -f "$INSTALL_DIR/wotoha-update.new" "$INSTALL_DIR/wotoha-update"
  echo "updated wotoha updater"
fi

new_binary="$package/bin/wotoha-app"
[[ -x "$new_binary" ]] || { echo "release archive is missing wotoha-app" >&2; exit 1; }
app_is_current=false
if [[ -x "$INSTALL_DIR/wotoha-app" ]] && cmp --silent "$new_binary" "$INSTALL_DIR/wotoha-app"; then
  app_is_current=true
fi

if [[ "$app_is_current" == true ]]; then
  service_health_allows_cleanup || exit 1
  if [[ -r "$BASELINE_MARKER" ]] && systemctl is-active --quiet wotoha.service; then
    systemctl restart wotoha.service
    sleep 5
    systemctl is-active --quiet wotoha.service || {
      echo "unable to activate the signed application; preserving legacy state" >&2
      exit 1
    }
  fi
  record_release_state "$tag" "$new_binary"
  if [[ "$same_tag_at_start" == true ]]; then
    cleanup_legacy_youtube
  fi
  echo "application is already current; recorded $tag"
  exit 0
fi

if [[ ! -e "$STATE_FILE" && "$force" == false && -x "$INSTALL_DIR/wotoha-app" ]]; then
  atomic_write "$tag" "$STATE_FILE"
  atomic_write "$(sha256sum "$INSTALL_DIR/wotoha-app" | awk '{print $1}')" "$BASELINE_MARKER"
  echo "recorded $tag as an unmanaged baseline; kept the installed application and legacy state"
  exit 0
fi

install -m 0755 "$new_binary" "$INSTALL_DIR/wotoha-app.new"
was_active=false
if systemctl is-active --quiet wotoha.service; then was_active=true; fi
if [[ -x "$INSTALL_DIR/wotoha-app" ]]; then
  cp -a "$INSTALL_DIR/wotoha-app" "$INSTALL_DIR/wotoha-app.previous"
fi
mv -f "$INSTALL_DIR/wotoha-app.new" "$INSTALL_DIR/wotoha-app"

restart_ok=true
if [[ "$was_active" == true ]]; then
  systemctl restart wotoha.service || restart_ok=false
  sleep 5
fi
if [[ "$was_active" == true ]] \
  && { [[ "$restart_ok" == false ]] || ! systemctl is-active --quiet wotoha.service; }; then
  echo "updated service failed; rolling back" >&2
  if [[ -x "$INSTALL_DIR/wotoha-app.previous" ]]; then
    mv -f "$INSTALL_DIR/wotoha-app.previous" "$INSTALL_DIR/wotoha-app"
  fi
  systemctl restart wotoha.service
  exit 1
fi

record_release_state "$tag" "$INSTALL_DIR/wotoha-app"
echo "updated wotoha to $tag"
