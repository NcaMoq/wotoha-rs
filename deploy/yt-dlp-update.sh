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
for command in curl gpg jq sha256sum stat flock timeout; do command -v "$command" >/dev/null || { echo "missing $command" >&2; exit 1; }; done
[[ -r "$KEY" ]] || { echo "yt-dlp signing key is missing" >&2; exit 1; }
exec 9>/run/lock/wotoha-ytdlp-update.lock
flock -n 9 || exit 0
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT

api="https://api.github.com/repos/$repository/releases/latest"
curl --fail --silent --show-error --location --retry 3 --max-filesize $((4 * 1024 * 1024)) --remove-on-error "$api" -o "$tmp/release.json"
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
curl --fail --silent --show-error --location --retry 3 --max-filesize "$MAX_BINARY_BYTES" --remove-on-error "$(asset_url yt-dlp_linux)" -o "$tmp/yt-dlp"
curl --fail --silent --show-error --location --retry 3 --max-filesize "$MAX_SUM_BYTES" --remove-on-error "$(asset_url SHA2-256SUMS)" -o "$tmp/SHA2-256SUMS"
curl --fail --silent --show-error --location --retry 3 --max-filesize "$MAX_SIGNATURE_BYTES" --remove-on-error "$(asset_url SHA2-256SUMS.sig)" -o "$tmp/SHA2-256SUMS.sig"
(( $(stat --format=%s "$tmp/yt-dlp") > 0 && $(stat --format=%s "$tmp/yt-dlp") <= MAX_BINARY_BYTES ))
gpg_home="$tmp/gnupg"; mkdir -m 0700 "$gpg_home"
gpg --batch --homedir "$gpg_home" --import "$KEY" >/dev/null 2>&1
fingerprint="$(gpg --batch --homedir "$gpg_home" --with-colons --fingerprint | awk -F: '$1 == "fpr" { print $10; exit }')"
[[ "$fingerprint" == "$EXPECTED_KEY_FINGERPRINT" ]] || { echo "unexpected yt-dlp signing key fingerprint" >&2; exit 1; }
gpg --batch --homedir "$gpg_home" --verify "$tmp/SHA2-256SUMS.sig" "$tmp/SHA2-256SUMS" >/dev/null 2>&1
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
install -d -o root -g root -m 0755 "$VERSIONS"
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
  set +o pipefail
  curl --fail --silent --show-error --location --range 0-1023 --max-time 20 "$direct_url" | head -c 4096 >"$tmp/canary.bytes"
  statuses=("${PIPESTATUS[@]}")
  set -o pipefail
  if [[ "${statuses[0]}" == 0 || "${statuses[0]}" == 23 ]] \
    && (( $(stat --format=%s "$tmp/canary.bytes") > 0 && $(stat --format=%s "$tmp/canary.bytes") <= 4096 )); then
    canary_ok=true
    break
  fi
  echo "yt-dlp direct-byte canary failed: $canary_url" >&2
done
[[ "$canary_ok" == true ]] || { echo "all pinned yt-dlp canaries failed; keeping current" >&2; exit 1; }
[[ -L "$CURRENT" && "$(readlink "$CURRENT")" == "versions/$digest/yt-dlp" ]] && exit 0
rm -f "$ROOT/.current.new" "$ROOT/.previous.new"
ln -s "versions/$digest/yt-dlp" "$ROOT/.current.new"
if [[ -L "$CURRENT" ]]; then ln -s "$(readlink "$CURRENT")" "$ROOT/.previous.new"; mv -Tf "$ROOT/.previous.new" "$PREVIOUS"; fi
mv -Tf "$ROOT/.current.new" "$CURRENT"
printf '%s %s %s\n' "$repository" "$tag" "$digest" > "$STATE.new"; chmod 0644 "$STATE.new"; mv -f "$STATE.new" "$STATE"
echo "promoted verified yt-dlp $tag ($digest) without restarting wotoha"
