#!/usr/bin/env bash
# Updates only yt-dlp.  The bot opens this path for each extraction, so changing
# the current symlink never restarts or mutates the running service.
set -Eeuo pipefail

readonly CONFIG=/etc/wotoha/wotoha-update.env
readonly ROOT=/opt/wotoha/yt-dlp
readonly CURRENT="$ROOT/current"
readonly PREVIOUS="$ROOT/previous"
readonly VERSIONS="$ROOT/versions"
readonly KEY=/etc/wotoha/yt-dlp-public.key
readonly STATE=/var/lib/wotoha-updater/installed-yt-dlp
readonly MAX_BINARY_BYTES=$((128 * 1024 * 1024))
readonly MAX_SUM_BYTES=$((256 * 1024))
readonly MAX_SIGNATURE_BYTES=$((64 * 1024))
readonly EXPECTED_KEY_FINGERPRINT=AC0CBBE6848D6A873464AF4E57CF65933B5A7581
readonly YTDLP_VERSION_TIMEOUT=20s
readonly YTDLP_CANARY_TIMEOUT=60s

[[ -r "$CONFIG" ]] && source "$CONFIG"
[[ "${WOTOHA_UPDATE_YTDLP:-true}" == true ]] || exit 0
repository="${WOTOHA_YTDLP_UPDATE_REPOSITORY:-yt-dlp/yt-dlp-nightly-builds}"
case "$repository" in
  yt-dlp/yt-dlp|yt-dlp/yt-dlp-nightly-builds) ;;
  *) echo "WOTOHA_YTDLP_UPDATE_REPOSITORY must name an official yt-dlp release repository" >&2; exit 2 ;;
esac
for command in awk chmod cp curl dirname flock gpg head install jq ln mkdir mktemp mv readlink rm sha256sum stat timeout; do command -v "$command" >/dev/null || { echo "missing $command" >&2; exit 1; }; done
[[ -r "$KEY" ]] || { echo "yt-dlp signing key is missing" >&2; exit 1; }
exec 9>/run/lock/wotoha-ytdlp-update.lock
flock -n 9 || exit 0
tmp="$(mktemp -d)"
promotion_started=false
restore_snapshot() {
  local path="$1" snapshot="$2"
  rm -f -- "$path"
  if [[ -e "$tmp/rollback/$snapshot" || -L "$tmp/rollback/$snapshot" ]]; then
    cp -a -- "$tmp/rollback/$snapshot" "$path"
  fi
}
rollback_promotion() {
  set +e
  echo 'yt-dlp promotion failed; restoring previous pointers and state' >&2
  restore_snapshot "$CURRENT" current
  restore_snapshot "$PREVIOUS" previous
  restore_snapshot "$STATE" state
}
cleanup() {
  local status=$?
  [[ "$promotion_started" != true ]] || rollback_promotion
  rm -f "$ROOT/.current.new" "$ROOT/.previous.new" "$STATE.new"
  [[ -z "${candidate_new:-}" ]] || rm -rf "$candidate_new"
  rm -rf "$tmp"
  return "$status"
}
trap cleanup EXIT
curl_retry=(--retry 4 --retry-all-errors --retry-delay 2 --retry-max-time 45 \
  --connect-timeout 10 --max-time 180)

api="https://api.github.com/repos/$repository/releases/latest"
curl --fail --silent --show-error --location "${curl_retry[@]}" --max-filesize $((4 * 1024 * 1024)) --remove-on-error "$api" -o "$tmp/release.json"
tag="$(jq -er '.tag_name | select(type == "string" and test("^[0-9]{4}[.][0-9]{2}[.][0-9]{2}([.][0-9]{6})?$"))' "$tmp/release.json")"
installed_repository=""
installed_tag=""
installed_digest=""
if [[ -r "$STATE" ]]; then
  read -r installed_repository installed_tag installed_digest < "$STATE"
  case "$installed_repository" in yt-dlp/yt-dlp|yt-dlp/yt-dlp-nightly-builds) ;; *) installed_repository="" ;; esac
  [[ -n "$installed_repository" && "$installed_tag" =~ ^[0-9]{4}[.][0-9]{2}[.][0-9]{2}([.][0-9]{6})?$ && "$installed_digest" =~ ^[0-9a-f]{64}$ ]] || {
    echo "installed yt-dlp state is invalid" >&2
    exit 1
  }
  [[ "$tag" < "$installed_tag" ]] && { echo "refusing yt-dlp downgrade from $installed_tag to $tag" >&2; exit 1; }
fi
asset_url() { jq -er --arg n "$1" '[.assets[] | select(.name == $n)] | if length == 1 then .[0].browser_download_url else error("missing or duplicate asset") end' "$tmp/release.json"; }
curl --fail --silent --show-error --location "${curl_retry[@]}" --max-filesize "$MAX_BINARY_BYTES" --remove-on-error "$(asset_url yt-dlp_linux)" -o "$tmp/yt-dlp"
curl --fail --silent --show-error --location "${curl_retry[@]}" --max-filesize "$MAX_SUM_BYTES" --remove-on-error "$(asset_url SHA2-256SUMS)" -o "$tmp/SHA2-256SUMS"
curl --fail --silent --show-error --location "${curl_retry[@]}" --max-filesize "$MAX_SIGNATURE_BYTES" --remove-on-error "$(asset_url SHA2-256SUMS.sig)" -o "$tmp/SHA2-256SUMS.sig"
(( $(stat --format=%s "$tmp/yt-dlp") > 0 && $(stat --format=%s "$tmp/yt-dlp") <= MAX_BINARY_BYTES ))
gpg_home="$tmp/gnupg"; mkdir -m 0700 "$gpg_home"
gpg --batch --homedir "$gpg_home" --import "$KEY" >/dev/null 2>&1
mapfile -t imported_fingerprints < <(gpg --batch --homedir "$gpg_home" --with-colons --fingerprint \
  | awk -F: '$1 == "fpr" {print $10}')
expected_key_present=false
for fingerprint in "${imported_fingerprints[@]}"; do
  [[ "$fingerprint" == "$EXPECTED_KEY_FINGERPRINT" ]] && expected_key_present=true
done
[[ "$expected_key_present" == true ]] || { echo "unexpected yt-dlp signing key fingerprint" >&2; exit 1; }
if ! verify_status="$(gpg --batch --homedir "$gpg_home" --status-fd 1 --verify \
  "$tmp/SHA2-256SUMS.sig" "$tmp/SHA2-256SUMS" 2>/dev/null)"; then
  echo "yt-dlp checksum signature verification failed" >&2
  exit 1
fi
mapfile -t valid_primary_fingerprints < <(awk '
  $1 == "[GNUPG:]" && $2 == "VALIDSIG" {print (NF >= 12 ? $12 : $3)}
' <<<"$verify_status")
[[ ${#valid_primary_fingerprints[@]} -eq 1 \
    && "${valid_primary_fingerprints[0]}" == "$EXPECTED_KEY_FINGERPRINT" ]] \
  || { echo "yt-dlp checksum signature used an unexpected primary key" >&2; exit 1; }
digest="$(sha256sum "$tmp/yt-dlp" | awk '{print $1}')"
[[ "$digest" =~ ^[0-9a-f]{64}$ ]]
expected_digest="$(awk '$2 == "yt-dlp_linux" && $1 ~ /^[0-9a-f]{64}$/ {print $1}' "$tmp/SHA2-256SUMS")"
[[ "$expected_digest" =~ ^[0-9a-f]{64}$ && "$digest" == "$expected_digest" ]] || {
  echo "yt-dlp digest did not match the signed checksum" >&2
  exit 1
}
if [[ -n "$installed_tag" && "$tag" == "$installed_tag" && "$digest" != "$installed_digest" ]]; then
  echo "refusing changed yt-dlp assets for already-installed tag $tag" >&2
  exit 1
fi
candidate="$VERSIONS/$digest"
install -d -o root -g root -m 0755 "$VERSIONS" "$(dirname "$STATE")"
if [[ -x "$candidate/yt-dlp" ]]; then
  [[ "$(sha256sum "$candidate/yt-dlp" | awk '{print $1}')" == "$digest" ]] || {
    echo "installed yt-dlp version has an unexpected digest" >&2
    exit 1
  }
else
  candidate_new="$VERSIONS/.${digest}.new.$$"
  rm -rf "$candidate_new"
  install -d -o root -g root -m 0755 "$candidate_new"
  install -o root -g root -m 0755 "$tmp/yt-dlp" "$candidate_new/yt-dlp"
  [[ "$(sha256sum "$candidate_new/yt-dlp" | awk '{print $1}')" == "$digest" ]]
  mv "$candidate_new" "$candidate"
fi
deno="${WOTOHA_DENO_PATH:-/opt/wotoha/bin/deno}"
yt_dlp_network_options=(--socket-timeout 10 --retries 2 --extractor-retries 2)
timeout --signal=TERM --kill-after=5s "$YTDLP_VERSION_TIMEOUT" \
  "$candidate/yt-dlp" --ignore-config --no-playlist \
  "${yt_dlp_network_options[@]}" --js-runtimes "deno:$deno" --version >/dev/null
canary_ok=false
read -r -a canary_urls <<<"${WOTOHA_YTDLP_CANARY_URLS:-https://www.youtube.com/watch?v=H7HmzwI67ec https://www.youtube.com/watch?v=jNQXAC9IVRw}"
for canary_url in "${canary_urls[@]}"; do
  direct_url="$(timeout --signal=TERM --kill-after=5s "$YTDLP_CANARY_TIMEOUT" \
    "$candidate/yt-dlp" --ignore-config --no-playlist --no-warnings --no-progress \
    "${yt_dlp_network_options[@]}" --js-runtimes "deno:$deno" \
    --format 'bestaudio[protocol^=http]/bestaudio/best' --skip-download \
    --print '%(url)s' "$canary_url" 2>/dev/null | head -n 1)" || true
  [[ "$direct_url" =~ ^https:// ]] || { echo "yt-dlp canary extraction failed: $canary_url" >&2; continue; }
  if curl --fail --silent --show-error --location "${curl_retry[@]}" --remove-on-error \
    --range 0-1023 --max-time 20 --max-filesize 4096 \
    --output "$tmp/canary.bytes" "$direct_url" \
    && (( $(stat --format=%s "$tmp/canary.bytes") > 0 && $(stat --format=%s "$tmp/canary.bytes") <= 4096 )); then
    canary_ok=true
    break
  fi
  echo "yt-dlp direct-byte canary failed: $canary_url" >&2
done
[[ "$canary_ok" == true ]] || { echo "all pinned yt-dlp canaries failed; keeping current" >&2; exit 1; }
if [[ -L "$CURRENT" && "$(readlink "$CURRENT")" == "versions/$digest/yt-dlp" ]]; then
  printf '%s %s %s\n' "$repository" "$tag" "$digest" > "$STATE.new"
  chmod 0644 "$STATE.new"
  mv -f "$STATE.new" "$STATE"
  exit 0
fi
for active_path in "$CURRENT" "$PREVIOUS" "$STATE"; do
  [[ ! -d "$active_path" || -L "$active_path" ]] \
    || { echo "refusing to replace unexpected directory: $active_path" >&2; exit 1; }
done
mkdir -m 0700 "$tmp/rollback"
for snapshot_spec in "$CURRENT:current" "$PREVIOUS:previous" "$STATE:state"; do
  active_path="${snapshot_spec%:*}"
  snapshot="${snapshot_spec##*:}"
  if [[ -e "$active_path" || -L "$active_path" ]]; then
    cp -a -- "$active_path" "$tmp/rollback/$snapshot"
  fi
done
rm -f "$ROOT/.current.new" "$ROOT/.previous.new" "$STATE.new"
ln -s "versions/$digest/yt-dlp" "$ROOT/.current.new"
if [[ -L "$CURRENT" ]]; then ln -s "$(readlink "$CURRENT")" "$ROOT/.previous.new"; fi
printf '%s %s %s\n' "$repository" "$tag" "$digest" > "$STATE.new"
chmod 0644 "$STATE.new"
promotion_started=true
if [[ -L "$ROOT/.previous.new" ]]; then mv -Tf "$ROOT/.previous.new" "$PREVIOUS"; fi
mv -Tf "$ROOT/.current.new" "$CURRENT"
mv -f "$STATE.new" "$STATE"
promotion_started=false
echo "promoted verified yt-dlp $tag ($digest) without restarting wotoha"
