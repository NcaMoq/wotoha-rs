#!/usr/bin/env bash
set -Eeuo pipefail

readonly CONFIG=/etc/wotoha/wotoha-update.env
readonly INSTALL_DIR=/opt/wotoha/bin
readonly STATE_FILE=/var/lib/wotoha-updater/installed-release
readonly ASSET=wotoha-ubuntu-x86_64-musl.tar.gz
readonly RELEASE_MANIFEST="$ASSET.manifest.json"
readonly RELEASE_ATTESTATION="$ASSET.attestation.jsonl"
readonly WORKER_ROOT=/opt/wotoha/workers
readonly WORKER_SEQUENCE_FILE=/opt/wotoha/YOUTUBE_WORKER_SEQUENCE
readonly WORKER_STATE_FILE=/var/lib/wotoha-updater/installed-youtube-worker
readonly WORKER_CANDIDATE_TAG_FILE=/var/lib/wotoha-updater/youtube-worker-candidate-tag
readonly WORKER_ACK=/var/lib/wotoha/youtube-worker-ack
readonly WORKER_ASSET=wotoha-youtube-js-worker-x86_64-musl
readonly WORKER_MANIFEST="$WORKER_ASSET.manifest.json"
readonly WORKER_ATTESTATION="$WORKER_ASSET.attestation.jsonl"
readonly MAX_WORKER_BYTES=$((128 * 1024 * 1024))
readonly MAX_WORKER_MANIFEST_BYTES=$((64 * 1024))
readonly MAX_ATTESTATION_BYTES=$((8 * 1024 * 1024))
readonly MAX_RELEASE_ARCHIVE_BYTES=$((512 * 1024 * 1024))
readonly MAX_CHECKSUM_BYTES=$((64 * 1024))
readonly MAX_API_JSON_BYTES=$((4 * 1024 * 1024))
readonly MAX_CLIENT_PROFILE_BYTES=$((256 * 1024))

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

for command in curl flock jq sha256sum sort stat tar systemctl; do
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

worker_bootstrap_changed=false

is_sha256() {
  [[ "$1" =~ ^[0-9a-f]{64}$ ]]
}

worker_state_sequence() {
  local state_file="$1"
  jq --exit-status --raw-output '
    select(
      .sequence | type == "number"
      and . >= 1
      and floor == .
    )
    | select(.sha256 | type == "string" and test("^[0-9a-f]{64}$"))
    | .sequence
  ' "$state_file"
}

write_worker_state() {
  local destination="$1"
  local sequence="$2"
  local digest="$3"
  local tag="$4"
  [[ "$sequence" =~ ^[1-9][0-9]*$ ]] || return 1
  is_sha256 "$digest" || return 1
  local temporary="$destination.new.$$"
  jq --null-input --argjson sequence "$sequence" --arg sha256 "$digest" \
    --arg tag "$tag" \
    '{sequence: $sequence, sha256: $sha256, tag: $tag}' \
    > "$temporary" || return 1
  chmod 0644 "$temporary" || return 1
  mv -f "$temporary" "$destination" || return 1
}

record_bundled_worker_state() {
  local sequence="$1"
  local digest="$2"
  local tag="$3"
  atomic_write_pointer "$sequence" "$WORKER_SEQUENCE_FILE" || return 1
  write_worker_state "$WORKER_STATE_FILE" "$sequence" "$digest" "$tag" || return 1
}

download_release_asset() {
  local release_file="$1"
  local asset_name="$2"
  local destination="$3"
  local maximum_bytes="$4"
  local asset_count asset_size asset_url
  asset_count="$(jq --arg name "$asset_name" \
    '[.assets[] | select(.name == $name)] | length' "$release_file")" || return 1
  [[ "$asset_count" == 1 ]] || return 1
  asset_size="$(jq --exit-status --raw-output --arg name "$asset_name" \
    '.assets[] | select(.name == $name) | .size' "$release_file")" || return 1
  [[ "$asset_size" =~ ^[0-9]+$ ]] \
    && (( asset_size > 0 && asset_size <= maximum_bytes )) || return 1
  asset_url="$(jq --exit-status --raw-output --arg name "$asset_name" \
    '.assets[] | select(.name == $name) | .url' "$release_file")" || return 1
  curl "${curl_auth[@]}" --fail --silent --show-error --location --retry 3 \
    --max-filesize "$maximum_bytes" --remove-on-error \
    --header "Accept: application/octet-stream" \
    "$asset_url" --output "$destination" || return 1
  [[ "$(stat --format='%s' "$destination")" == "$asset_size" ]] || return 1
}

atomic_write_pointer() {
  local value="$1"
  local destination="$2"
  local temporary="$destination.new.$$"
  printf '%s\n' "$value" > "$temporary" || return 1
  chmod 0644 "$temporary" || return 1
  mv -f "$temporary" "$destination" || return 1
}

install_worker_version() {
  local source="$1"
  local digest="$2"
  local versions="$WORKER_ROOT/versions"
  local destination="$versions/$digest"
  local installed="$destination/wotoha-youtube-js-worker"
  is_sha256 "$digest" || return 1
  install -d -o root -g root -m 0755 "$WORKER_ROOT" "$versions" || return 1
  if [[ -f "$installed" ]]; then
    [[ "$(sha256sum "$installed" | awk '{print $1}')" == "$digest" ]]
    return
  fi
  local temporary="$versions/.${digest}.new.$$"
  rm -rf "$temporary" || return 1
  install -d -o root -g root -m 0755 "$temporary" || return 1
  install -o root -g root -m 0755 "$source" \
    "$temporary/wotoha-youtube-js-worker" || return 1
  if [[ "$(sha256sum "$temporary/wotoha-youtube-js-worker" | awk '{print $1}')" != "$digest" ]]; then
    rm -rf "$temporary"
    return 1
  fi
  mv "$temporary" "$destination" || return 1
}

promote_acknowledged_worker() {
  [[ -r "$WORKER_ROOT/current" && -r "$WORKER_ROOT/candidate" && -r "$WORKER_ACK" ]] \
    || return 0
  local current candidate ack
  current="$(tr -d '\r\n' < "$WORKER_ROOT/current")"
  candidate="$(tr -d '\r\n' < "$WORKER_ROOT/candidate")"
  ack="$(tr -d '\r\n' < "$WORKER_ACK")"
  if ! is_sha256 "$current" || ! is_sha256 "$candidate" || [[ "$ack" != "$candidate" ]]; then
    return 0
  fi
  [[ -r "$WORKER_CANDIDATE_TAG_FILE" ]] || return 1
  local candidate_state_digest candidate_sequence installed_sequence=0
  candidate_state_digest="$(jq --exit-status --raw-output '.sha256' \
    "$WORKER_CANDIDATE_TAG_FILE")" || return 1
  candidate_sequence="$(worker_state_sequence "$WORKER_CANDIDATE_TAG_FILE")" \
    || return 1
  if [[ -r "$WORKER_STATE_FILE" ]]; then
    installed_sequence="$(worker_state_sequence "$WORKER_STATE_FILE")" || return 1
  fi
  [[ "$candidate_state_digest" == "$candidate" ]] \
    && (( candidate_sequence > installed_sequence )) || return 1
  local worker="$WORKER_ROOT/versions/$candidate/wotoha-youtube-js-worker"
  if [[ ! -x "$worker" ]] \
    || [[ "$(sha256sum "$worker" | awk '{print $1}')" != "$candidate" ]]; then
    echo "acknowledged YouTube worker was invalid; keeping current" >&2
    return 0
  fi
  atomic_write_pointer "$current" "$WORKER_ROOT/previous" || return 1
  atomic_write_pointer "$candidate" "$WORKER_ROOT/current" || return 1
  atomic_write_pointer "$candidate_sequence" "$WORKER_SEQUENCE_FILE" || return 1
  rm -f "$WORKER_ROOT/candidate" "$WORKER_ACK" || return 1
  mv -f "$WORKER_CANDIDATE_TAG_FILE" "$WORKER_STATE_FILE" || return 1
  echo "promoted acknowledged YouTube worker $candidate"
}

bootstrap_worker_store() {
  local source="$1"
  [[ -x "$source" ]] || return 0
  local digest
  digest="$(sha256sum "$source" | awk '{print $1}')"
  install_worker_version "$source" "$digest" || return 1
  if [[ ! -r "$WORKER_ROOT/current" ]]; then
    atomic_write_pointer "$digest" "$WORKER_ROOT/current" || return 1
    worker_bootstrap_changed=true
  fi
}

restart_for_worker_bootstrap() {
  [[ "$worker_bootstrap_changed" == true ]] || return 0
  if systemctl is-active --quiet wotoha.service; then
    systemctl restart wotoha.service
    sleep 5
    systemctl is-active --quiet wotoha.service
  fi
  worker_bootstrap_changed=false
  echo "activated the content-addressed YouTube worker channel"
}

check_youtube_worker_update() {
  promote_acknowledged_worker || return 1
  if [[ "${WOTOHA_UPDATE_YOUTUBE_WORKER:-true}" != true ]]; then
    return 0
  fi
  if ! command -v gh >/dev/null 2>&1 \
    || ! gh attestation verify --help 2>&1 \
      | grep -q -- '--deny-self-hosted-runners'; then
    echo "gh 2.49+ with attestation support is unavailable; skipping YouTube worker update" >&2
    return 0
  fi

  local releases_json="$tmp_dir/youtube-worker-releases.json"
  curl "${curl_auth[@]}" --fail --silent --show-error --location --retry 3 \
    --max-filesize "$MAX_API_JSON_BYTES" --remove-on-error \
    --header "Accept: application/vnd.github+json" \
    --header "X-GitHub-Api-Version: 2022-11-28" \
    "https://api.github.com/repos/$repository/releases?per_page=30" \
    --output "$releases_json" || return 1
  local release tag
  tag="$(jq --raw-output '
    .[] | select(
      .draft == false
      and .prerelease == true
      and (.tag_name | test("^youtube-worker-v[0-9]+([.][0-9]+){1,3}$"))
    ) | .tag_name
  ' "$releases_json" | sort -V | tail -n 1)" || return 1
  [[ -n "$tag" ]] || return 0
  release="$(jq --compact-output --arg tag "$tag" \
    '.[] | select(.tag_name == $tag)' "$releases_json")" || return 1
  [[ -n "$release" ]] || return 1
  [[ "$tag" =~ ^youtube-worker-v[0-9]+([.][0-9]+){1,3}$ ]] || return 1
  local release_file="$tmp_dir/youtube-worker-release.json"
  printf '%s\n' "$release" > "$release_file" || return 1

  local worker="$tmp_dir/$WORKER_ASSET"
  local manifest="$tmp_dir/$WORKER_MANIFEST"
  local attestation="$tmp_dir/$WORKER_ATTESTATION"
  download_release_asset "$release_file" "$WORKER_ASSET" "$worker" \
    "$MAX_WORKER_BYTES" || return 1
  download_release_asset "$release_file" "$WORKER_MANIFEST" "$manifest" \
    "$MAX_WORKER_MANIFEST_BYTES" || return 1
  download_release_asset "$release_file" "$WORKER_ATTESTATION" "$attestation" \
    "$MAX_ATTESTATION_BYTES" || return 1

  (
    export GH_CONFIG_DIR="$tmp_dir/gh-config-manifest"
    export XDG_CACHE_HOME="$tmp_dir/gh-cache-manifest"
    unset GITHUB_TOKEN
    if [[ -n "$token" ]]; then
      export GH_TOKEN="$token"
    else
      unset GH_TOKEN
    fi
    gh attestation verify "$manifest" \
      --bundle "$attestation" \
      --repo "$repository" \
      --signer-workflow "$repository/.github/workflows/youtube-worker-release.yml" \
      --source-ref "refs/tags/$tag" \
      --deny-self-hosted-runners \
      --format json >/dev/null
  ) || return 1

  if ! jq --exit-status --arg tag "$tag" --arg asset "$WORKER_ASSET" '
    .schema_version == 1
    and (.sequence | type == "number" and . >= 1 and floor == .)
    and .tag == $tag
    and .protocol_version == 1
    and .target == "x86_64-unknown-linux-musl"
    and .asset == $asset
    and (.sha256 | type == "string" and test("^[0-9a-f]{64}$"))
    and (.commit | type == "string" and test("^[0-9a-f]{40}$"))
  ' "$manifest" >/dev/null; then
    echo "YouTube worker manifest was invalid" >&2
    return 1
  fi
  local digest commit sequence
  digest="$(jq --raw-output '.sha256' "$manifest")" || return 1
  commit="$(jq --raw-output '.commit' "$manifest")" || return 1
  sequence="$(jq --raw-output '.sequence' "$manifest")" || return 1
  if [[ "$(sha256sum "$worker" | awk '{print $1}')" != "$digest" ]]; then
    echo "YouTube worker digest did not match its manifest" >&2
    return 1
  fi

  (
    export GH_CONFIG_DIR="$tmp_dir/gh-config"
    export XDG_CACHE_HOME="$tmp_dir/gh-cache"
    unset GITHUB_TOKEN
    if [[ -n "$token" ]]; then
      export GH_TOKEN="$token"
    else
      unset GH_TOKEN
    fi
    gh attestation verify "$worker" \
      --bundle "$attestation" \
      --repo "$repository" \
      --signer-workflow "$repository/.github/workflows/youtube-worker-release.yml" \
      --source-ref "refs/tags/$tag" \
      --source-digest "$commit" \
      --deny-self-hosted-runners \
      --format json >/dev/null
    gh attestation verify "$manifest" \
      --bundle "$attestation" \
      --repo "$repository" \
      --signer-workflow "$repository/.github/workflows/youtube-worker-release.yml" \
      --source-ref "refs/tags/$tag" \
      --source-digest "$commit" \
      --deny-self-hosted-runners \
      --format json >/dev/null
  ) || return 1

  local installed_sequence
  if [[ ! -r "$WORKER_STATE_FILE" ]]; then
    echo "YouTube worker sequence is not initialized; waiting for a full release" >&2
    return 0
  fi
  installed_sequence="$(worker_state_sequence "$WORKER_STATE_FILE")" || return 1
  local staged_sequence=0
  if [[ -r "$WORKER_CANDIDATE_TAG_FILE" ]]; then
    staged_sequence="$(worker_state_sequence "$WORKER_CANDIDATE_TAG_FILE")" || return 1
  fi
  if (( sequence <= installed_sequence || sequence <= staged_sequence )); then
    return 0
  fi

  install_worker_version "$worker" "$digest" || return 1
  local current=""
  if [[ -r "$WORKER_ROOT/current" ]]; then
    current="$(tr -d '\r\n' < "$WORKER_ROOT/current")"
  fi
  if [[ "$current" == "$digest" ]]; then
    write_worker_state "$WORKER_STATE_FILE" "$sequence" "$digest" "$tag" || return 1
    rm -f "$WORKER_ROOT/candidate" "$WORKER_ACK" "$WORKER_CANDIDATE_TAG_FILE" \
      || return 1
    return 0
  fi
  local candidate=""
  if [[ -r "$WORKER_ROOT/candidate" ]]; then
    candidate="$(tr -d '\r\n' < "$WORKER_ROOT/candidate")"
  fi
  if [[ "$candidate" != "$digest" ]]; then
    atomic_write_pointer "$digest" "$WORKER_ROOT/candidate" || return 1
    rm -f "$WORKER_ACK" || return 1
  fi
  write_worker_state "$WORKER_CANDIDATE_TAG_FILE" "$sequence" "$digest" "$tag" \
    || return 1
  echo "staged attested YouTube worker $tag for in-process validation"
}

curl_auth=()
if [[ -n "$token" ]]; then
  auth_config="$tmp_dir/curl-auth.conf"
  printf 'header = "Authorization: Bearer %s"\n' "$token" > "$auth_config"
  chmod 0600 "$auth_config"
  curl_auth=(--config "$auth_config")
fi

bootstrap_worker_store "$INSTALL_DIR/wotoha-youtube-js-worker"
if [[ -r "$WORKER_ROOT/current" && -r /etc/wotoha/wotoha.env ]]; then
  if ! grep -q '^WOTOHA_YOUTUBE_JS_WORKER_DIR=' /etc/wotoha/wotoha.env; then
    printf '%s\n' 'WOTOHA_YOUTUBE_JS_WORKER_DIR=/opt/wotoha/workers' \
      >> /etc/wotoha/wotoha.env
    worker_bootstrap_changed=true
  fi
  if ! grep -q '^WOTOHA_YOUTUBE_JS_WORKER_ACK=' /etc/wotoha/wotoha.env; then
    printf '%s\n' 'WOTOHA_YOUTUBE_JS_WORKER_ACK=/var/lib/wotoha/youtube-worker-ack' \
      >> /etc/wotoha/wotoha.env
    worker_bootstrap_changed=true
  fi
fi
check_youtube_worker_update

youtube_clients_changed=false
if [[ "${WOTOHA_UPDATE_YOUTUBE_CLIENTS:-true}" == true ]]; then
  youtube_clients="$tmp_dir/youtube-clients.json"
  if curl "${curl_auth[@]}" --fail --silent --show-error --location --retry 3 \
    --max-filesize "$MAX_CLIENT_PROFILE_BYTES" --remove-on-error \
    --header "Accept: application/vnd.github.raw+json" \
    --header "X-GitHub-Api-Version: 2022-11-28" \
    "https://api.github.com/repos/$repository/contents/deploy/youtube-clients.json?ref=main" \
    --output "$youtube_clients"; then
    if ! jq --exit-status '
      type == "array"
      and length > 0
      and length <= 8
      and all(.[];
        (.id | type == "string" and test("^[A-Za-z0-9_]+$"))
        and (.client_name | type == "string" and length > 0 and length <= 64)
        and (.client_version | type == "string" and length > 0 and length <= 64)
        and (.client_number | type == "string" and test("^[0-9]+$"))
        and (.user_agent | type == "string" and length > 0 and length <= 512)
        and (.os_name | type == "string" and length > 0 and length <= 64)
        and (.os_version | type == "string" and length > 0 and length <= 64)
      )
    ' "$youtube_clients" >/dev/null; then
      echo "downloaded YouTube client profile is invalid; keeping installed copy" >&2
    elif [[ ! -r /etc/wotoha/youtube-clients.json ]] \
      || ! cmp --silent "$youtube_clients" /etc/wotoha/youtube-clients.json; then
      install -m 0644 "$youtube_clients" /etc/wotoha/youtube-clients.json
      youtube_clients_changed=true
      echo "updated YouTube client profiles"
    fi
  else
    echo "unable to check YouTube client profile updates; continuing" >&2
  fi
fi

release_json="$tmp_dir/release.json"
curl "${curl_auth[@]}" --fail --silent --show-error --location --retry 3 \
  --max-filesize "$MAX_API_JSON_BYTES" --remove-on-error \
  --header "Accept: application/vnd.github+json" \
  --header "X-GitHub-Api-Version: 2022-11-28" \
  "https://api.github.com/repos/$repository/releases/latest" \
  --output "$release_json"
tag="$(jq --exit-status --raw-output '.tag_name' "$release_json")"

worker_sequence_initialized=false
if [[ -r "$WORKER_STATE_FILE" && -r "$WORKER_SEQUENCE_FILE" ]]; then
  installed_sequence_check="$(worker_state_sequence "$WORKER_STATE_FILE")"
  local_sequence_check="$(tr -d '\r\n' < "$WORKER_SEQUENCE_FILE")"
  if [[ "$installed_sequence_check" == "$local_sequence_check" ]]; then
    worker_sequence_initialized=true
  fi
fi
ytdlp_ready=false
if [[ -x /opt/wotoha/bin/yt-dlp ]] \
  && [[ -x /opt/wotoha/bin/deno ]] \
  && [[ -x /opt/wotoha/bin/yt-dlp-update ]] \
  && [[ -L /opt/wotoha/yt-dlp/current ]] \
  && [[ -r /etc/wotoha/yt-dlp-public.key ]] \
  && [[ -r /etc/systemd/system/yt-dlp-update.service ]] \
  && [[ -r /etc/systemd/system/yt-dlp-update.timer ]] \
  && [[ -r /var/lib/wotoha-updater/installed-yt-dlp ]]; then
  ytdlp_ready=true
fi
if [[ -r "$STATE_FILE" && "$(<"$STATE_FILE")" == "$tag" ]] \
  && [[ "$worker_sequence_initialized" == true ]] \
  && [[ "$ytdlp_ready" == true ]]; then
  restart_for_worker_bootstrap
  if [[ "$youtube_clients_changed" == true ]]; then
    echo "wotoha is already at $tag; new YouTube profiles load on the next request"
    exit 0
  fi
  echo "wotoha is already at $tag"
  exit 0
fi

archive="$tmp_dir/$ASSET"
if ! command -v gh >/dev/null 2>&1 \
  || ! gh attestation verify --help 2>&1 \
    | grep -q -- '--deny-self-hosted-runners'; then
  echo "gh 2.49+ is required to verify the Wotoha release" >&2
  exit 1
fi
download_release_asset "$release_json" "$ASSET" "$archive" \
  "$MAX_RELEASE_ARCHIVE_BYTES"
download_release_asset "$release_json" "$ASSET.sha256" "$archive.sha256" \
  "$MAX_CHECKSUM_BYTES"
release_manifest="$tmp_dir/$RELEASE_MANIFEST"
release_attestation="$tmp_dir/$RELEASE_ATTESTATION"
download_release_asset "$release_json" "$RELEASE_MANIFEST" "$release_manifest" \
  "$MAX_WORKER_MANIFEST_BYTES"
download_release_asset "$release_json" "$RELEASE_ATTESTATION" "$release_attestation" \
  "$MAX_ATTESTATION_BYTES"
(
  export GH_CONFIG_DIR="$tmp_dir/gh-config-release-manifest"
  export XDG_CACHE_HOME="$tmp_dir/gh-cache-release-manifest"
  unset GITHUB_TOKEN
  if [[ -n "$token" ]]; then
    export GH_TOKEN="$token"
  else
    unset GH_TOKEN
  fi
  gh attestation verify "$release_manifest" \
    --bundle "$release_attestation" \
    --repo "$repository" \
    --signer-workflow "$repository/.github/workflows/release.yml" \
    --source-ref "refs/tags/$tag" \
    --deny-self-hosted-runners \
    --format json >/dev/null
)
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
if [[ "$(sha256sum "$archive" | awk '{print $1}')" != "$release_digest" ]]; then
  echo "release archive digest did not match its manifest" >&2
  exit 1
fi
(
  export GH_CONFIG_DIR="$tmp_dir/gh-config-release"
  export XDG_CACHE_HOME="$tmp_dir/gh-cache-release"
  unset GITHUB_TOKEN
  if [[ -n "$token" ]]; then
    export GH_TOKEN="$token"
  else
    unset GH_TOKEN
  fi
  gh attestation verify "$archive" \
    --bundle "$release_attestation" \
    --repo "$repository" \
    --signer-workflow "$repository/.github/workflows/release.yml" \
    --source-ref "refs/tags/$tag" \
    --source-digest "$release_commit" \
    --deny-self-hosted-runners \
    --format json >/dev/null
  gh attestation verify "$release_manifest" \
    --bundle "$release_attestation" \
    --repo "$repository" \
    --signer-workflow "$repository/.github/workflows/release.yml" \
    --source-ref "refs/tags/$tag" \
    --source-digest "$release_commit" \
    --deny-self-hosted-runners \
    --format json >/dev/null
)
(cd "$tmp_dir" && sha256sum --check "$ASSET.sha256")
tar -xzf "$archive" -C "$tmp_dir"
package="$tmp_dir/wotoha-ubuntu-x86_64-musl"
if [[ ! -d "$package" ]]; then
  echo "release archive has an unexpected layout" >&2
  exit 1
fi
(cd "$package" && sha256sum --check SHA256SUMS.txt)
if [[ -x "$package/install-yt-dlp-bundle.sh" ]]; then
  bash "$package/install-yt-dlp-bundle.sh" "$package"
  if [[ -r /etc/wotoha/wotoha.env ]]; then
    grep -q '^WOTOHA_YTDLP_PATH=' /etc/wotoha/wotoha.env \
      || printf '%s\n' 'WOTOHA_YTDLP_PATH=/opt/wotoha/bin/yt-dlp' >> /etc/wotoha/wotoha.env
    grep -q '^WOTOHA_DENO_PATH=' /etc/wotoha/wotoha.env \
      || printf '%s\n' 'WOTOHA_DENO_PATH=/opt/wotoha/bin/deno' >> /etc/wotoha/wotoha.env
  fi
  systemctl daemon-reload
  systemctl enable --now yt-dlp-update.timer
fi
if [[ -r "$package/deploy/YOUTUBE_WORKER_SEQUENCE" ]]; then
  new_worker_sequence="$(tr -d '\r\n' < "$package/deploy/YOUTUBE_WORKER_SEQUENCE")"
elif [[ -r "$WORKER_STATE_FILE" ]]; then
  new_worker_sequence="$(worker_state_sequence "$WORKER_STATE_FILE")"
else
  new_worker_sequence=1
fi
[[ "$new_worker_sequence" =~ ^[1-9][0-9]*$ ]] || {
  echo "release package has an invalid YouTube worker sequence" >&2
  exit 1
}

if [[ -x "$package/wotoha-update.sh" ]] \
  && ! cmp --silent "$package/wotoha-update.sh" "$INSTALL_DIR/wotoha-update"; then
  install -m 0755 "$package/wotoha-update.sh" "$INSTALL_DIR/wotoha-update.new"
  mv -f "$INSTALL_DIR/wotoha-update.new" "$INSTALL_DIR/wotoha-update"
  echo "updated wotoha updater"
fi
if [[ ! -r /etc/wotoha/youtube-clients.json ]] \
  && [[ -r "$package/deploy/youtube-clients.json" ]]; then
  install -m 0644 "$package/deploy/youtube-clients.json" /etc/wotoha/youtube-clients.json
fi

new_binary="$package/bin/wotoha-app"
new_worker="$package/bin/wotoha-youtube-js-worker"
if [[ ! -x "$new_worker" ]] && [[ -x "$INSTALL_DIR/wotoha-youtube-js-worker" ]]; then
  new_worker="$INSTALL_DIR/wotoha-youtube-js-worker"
fi
if [[ ! -x "$new_binary" || ! -x "$new_worker" ]]; then
  echo "release archive is missing a required executable" >&2
  exit 1
fi
new_worker_digest="$(sha256sum "$new_worker" | awk '{print $1}')"
installed_worker_sequence=0
installed_worker_digest=""
if [[ -r "$WORKER_STATE_FILE" ]]; then
  installed_worker_sequence="$(worker_state_sequence "$WORKER_STATE_FILE")"
  installed_worker_digest="$(jq --exit-status --raw-output '.sha256' \
    "$WORKER_STATE_FILE")"
fi
if (( new_worker_sequence < installed_worker_sequence )); then
  echo "full release would downgrade the YouTube worker sequence" >&2
  exit 1
fi
if (( new_worker_sequence == installed_worker_sequence )) \
  && [[ -n "$installed_worker_digest" ]] \
  && [[ "$new_worker_digest" != "$installed_worker_digest" ]]; then
  echo "full release changed the YouTube worker without increasing its sequence" >&2
  exit 1
fi
install_worker_version "$new_worker" "$new_worker_digest"
app_is_current=false
worker_is_current=false
if [[ -x "$INSTALL_DIR/wotoha-app" ]] && cmp --silent "$new_binary" "$INSTALL_DIR/wotoha-app"; then
  app_is_current=true
fi
if [[ -x "$INSTALL_DIR/wotoha-youtube-js-worker" ]] \
  && cmp --silent "$new_worker" "$INSTALL_DIR/wotoha-youtube-js-worker"; then
  worker_is_current=true
fi
if [[ "$app_is_current" == true && "$worker_is_current" == true ]]; then
  atomic_write_pointer "$new_worker_digest" "$WORKER_ROOT/current"
  rm -f "$WORKER_ROOT/candidate" "$WORKER_ACK" "$WORKER_CANDIDATE_TAG_FILE"
  record_bundled_worker_state "$new_worker_sequence" "$new_worker_digest" "$tag"
  printf '%s\n' "$tag" > "$STATE_FILE"
  restart_for_worker_bootstrap
  echo "executables are already current; recorded $tag"
  exit 0
fi
if [[ ! -e "$STATE_FILE" && "$force" == false ]]; then
  printf '%s\n' "$tag" > "$STATE_FILE"
  restart_for_worker_bootstrap
  echo "recorded $tag as the update baseline; kept the untracked installed binary"
  exit 0
fi

previous_worker_current=""
if [[ -r "$WORKER_ROOT/current" ]]; then
  previous_worker_current="$(tr -d '\r\n' < "$WORKER_ROOT/current")"
fi
install -m 0755 "$new_binary" "$INSTALL_DIR/wotoha-app.new"
install -m 0755 "$new_worker" "$INSTALL_DIR/wotoha-youtube-js-worker.new"
was_active=false
if systemctl is-active --quiet wotoha.service; then
  was_active=true
fi
if [[ -x "$INSTALL_DIR/wotoha-app" ]]; then
  cp -a "$INSTALL_DIR/wotoha-app" "$INSTALL_DIR/wotoha-app.previous"
fi
worker_had_previous=false
if [[ -x "$INSTALL_DIR/wotoha-youtube-js-worker" ]]; then
  worker_had_previous=true
  cp -a "$INSTALL_DIR/wotoha-youtube-js-worker" "$INSTALL_DIR/wotoha-youtube-js-worker.previous"
fi
mv -f "$INSTALL_DIR/wotoha-youtube-js-worker.new" "$INSTALL_DIR/wotoha-youtube-js-worker"
mv -f "$INSTALL_DIR/wotoha-app.new" "$INSTALL_DIR/wotoha-app"
if is_sha256 "$previous_worker_current" \
  && [[ "$previous_worker_current" != "$new_worker_digest" ]]; then
  atomic_write_pointer "$previous_worker_current" "$WORKER_ROOT/previous"
fi
atomic_write_pointer "$new_worker_digest" "$WORKER_ROOT/current"
rm -f "$WORKER_ROOT/candidate" "$WORKER_ACK" "$WORKER_CANDIDATE_TAG_FILE"

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
  fi
  if [[ -x "$INSTALL_DIR/wotoha-youtube-js-worker.previous" ]]; then
    mv -f "$INSTALL_DIR/wotoha-youtube-js-worker.previous" "$INSTALL_DIR/wotoha-youtube-js-worker"
  elif [[ "$worker_had_previous" == false ]]; then
    rm -f "$INSTALL_DIR/wotoha-youtube-js-worker"
  fi
  if is_sha256 "$previous_worker_current"; then
    atomic_write_pointer "$previous_worker_current" "$WORKER_ROOT/current"
  fi
  systemctl restart wotoha.service
  exit 1
fi

record_bundled_worker_state "$new_worker_sequence" "$new_worker_digest" "$tag"
printf '%s\n' "$tag" > "$STATE_FILE"
echo "updated wotoha to $tag"
