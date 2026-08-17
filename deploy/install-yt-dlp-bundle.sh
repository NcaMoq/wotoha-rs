#!/usr/bin/env bash
# Bootstrap pinned yt-dlp and Deno releases directly from their official
# release repositories. The Wotoha release archive contains neither runtime.
set -Eeuo pipefail

package="${1:?package root is required}"
root=/opt/wotoha/yt-dlp
versions="$root/versions"
current="$root/current"
previous="$root/previous"
state=/var/lib/wotoha-updater/installed-yt-dlp
expected_fingerprint=AC0CBBE6848D6A873464AF4E57CF65933B5A7581
readonly MAX_YTDLP_BYTES=$((128 * 1024 * 1024))
readonly MAX_DENO_ZIP_BYTES=$((128 * 1024 * 1024))
readonly MAX_SUM_BYTES=$((256 * 1024))
readonly MAX_SIGNATURE_BYTES=$((64 * 1024))
readonly VERSION_TIMEOUT=20s
readonly CANARY_TIMEOUT=60s
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
  echo 'runtime promotion failed; restoring the previous yt-dlp/Deno state' >&2
  restore_snapshot /opt/wotoha/bin/deno deno
  restore_snapshot "$current" current
  restore_snapshot "$previous" previous
  restore_snapshot /opt/wotoha/bin/yt-dlp yt-dlp
  restore_snapshot "$state" state
}

cleanup() {
  local status=$?
  [[ "$promotion_started" != true ]] || rollback_promotion
  rm -f "$root/.current.new" "$root/.previous.new" \
    /opt/wotoha/bin/.yt-dlp.new /opt/wotoha/bin/deno.new "$state.new"
  [[ -z "${candidate_new:-}" ]] || rm -rf "$candidate_new"
  rm -rf "$tmp"
  return "$status"
}
trap cleanup EXIT

for command in awk chmod cp curl dirname flock gpg head install ln mkdir mktemp mv readlink rm \
  sha256sum stat systemctl timeout unzip; do
  command -v "$command" >/dev/null \
    || { echo "required command is missing: $command" >&2; exit 1; }
done
exec 8>/run/lock/wotoha-ytdlp-update.lock
flock 8
for path in \
  SHA256SUMS.txt \
  deploy/yt-dlp-public.key \
  deploy/third-party-versions.env \
  deploy/yt-dlp-update.service \
  deploy/yt-dlp-update.timer \
  yt-dlp-update.sh; do
  [[ -r "$package/$path" ]] || { echo "bootstrap package is missing $path" >&2; exit 1; }
done
(cd "$package" && sha256sum --check --status SHA256SUMS.txt)

read_pin() {
  local name="$1" file="$package/deploy/third-party-versions.env" values
  mapfile -t values < <(awk -F= -v name="$name" '$1 == name {print substr($0, index($0, "=") + 1)}' "$file")
  [[ ${#values[@]} -eq 1 && -n "${values[0]}" ]] \
    || { echo "missing or duplicate bootstrap pin: $name" >&2; return 1; }
  printf '%s\n' "${values[0]}"
}

repository="$(read_pin YTDLP_REPOSITORY)"
yt_dlp_version="$(read_pin YTDLP_VERSION)"
deno_version="$(read_pin DENO_VERSION)"
deno_digest="$(read_pin DENO_X86_64_LINUX_GNU_SHA256)"
case "$repository" in
  yt-dlp/yt-dlp|yt-dlp/yt-dlp-nightly-builds) ;;
  *) echo "invalid pinned yt-dlp repository" >&2; exit 1 ;;
esac
[[ "$yt_dlp_version" =~ ^[0-9]{4}[.][0-9]{2}[.][0-9]{2}([.][0-9]{6})?$ ]] \
  || { echo "invalid pinned yt-dlp version" >&2; exit 1; }
[[ "$deno_version" =~ ^[0-9]+[.][0-9]+[.][0-9]+$ ]] \
  || { echo "invalid pinned Deno version" >&2; exit 1; }
[[ "$deno_digest" =~ ^[0-9a-f]{64}$ ]] \
  || { echo "invalid pinned Deno digest" >&2; exit 1; }

curl_retry=(--retry 4 --retry-all-errors --retry-delay 2 --retry-max-time 45 \
  --connect-timeout 10 --max-time 180)
download() {
  local maximum="$1" url="$2" destination="$3" size
  curl --fail --silent --show-error --location "${curl_retry[@]}" \
    --max-filesize "$maximum" --remove-on-error "$url" --output "$destination"
  size="$(stat --format=%s "$destination")"
  (( size > 0 && size <= maximum )) \
    || { echo "downloaded asset has an invalid size: $url" >&2; return 1; }
}

yt_base="https://github.com/$repository/releases/download/$yt_dlp_version"
download "$MAX_YTDLP_BYTES" "$yt_base/yt-dlp_linux" "$tmp/yt-dlp"
download "$MAX_SUM_BYTES" "$yt_base/SHA2-256SUMS" "$tmp/SHA2-256SUMS"
download "$MAX_SIGNATURE_BYTES" "$yt_base/SHA2-256SUMS.sig" "$tmp/SHA2-256SUMS.sig"
download "$MAX_DENO_ZIP_BYTES" \
  "https://github.com/denoland/deno/releases/download/v$deno_version/deno-x86_64-unknown-linux-gnu.zip" \
  "$tmp/deno.zip"

install -d -m 0700 "$tmp/gnupg"
gpg --batch --homedir "$tmp/gnupg" --import "$package/deploy/yt-dlp-public.key" >/dev/null 2>&1
mapfile -t imported_fingerprints < <(gpg --batch --homedir "$tmp/gnupg" --with-colons --fingerprint \
  | awk -F: '$1 == "fpr" {print $10}')
expected_key_present=false
for fingerprint in "${imported_fingerprints[@]}"; do
  [[ "$fingerprint" == "$expected_fingerprint" ]] && expected_key_present=true
done
[[ "$expected_key_present" == true ]] \
  || { echo "unexpected yt-dlp signing key fingerprint" >&2; exit 1; }
if ! verify_status="$(gpg --batch --homedir "$tmp/gnupg" --status-fd 1 --verify \
  "$tmp/SHA2-256SUMS.sig" "$tmp/SHA2-256SUMS" 2>/dev/null)"; then
  echo "yt-dlp checksum signature verification failed" >&2
  exit 1
fi
mapfile -t valid_primary_fingerprints < <(awk '
  $1 == "[GNUPG:]" && $2 == "VALIDSIG" {print (NF >= 12 ? $12 : $3)}
' <<<"$verify_status")
[[ ${#valid_primary_fingerprints[@]} -eq 1 \
    && "${valid_primary_fingerprints[0]}" == "$expected_fingerprint" ]] \
  || { echo "yt-dlp checksum signature used an unexpected primary key" >&2; exit 1; }
yt_dlp_digest="$(sha256sum "$tmp/yt-dlp" | awk '{print $1}')"
mapfile -t expected_yt_dlp_digests < <(
  awk '$2 == "yt-dlp_linux" && $1 ~ /^[0-9a-f]{64}$/ {print $1}' "$tmp/SHA2-256SUMS"
)
[[ ${#expected_yt_dlp_digests[@]} -eq 1 \
    && "$yt_dlp_digest" == "${expected_yt_dlp_digests[0]}" ]] \
  || { echo "downloaded yt-dlp digest did not match the signed checksum" >&2; exit 1; }
[[ "$(sha256sum "$tmp/deno.zip" | awk '{print $1}')" == "$deno_digest" ]] \
  || { echo "downloaded Deno archive did not match its pinned digest" >&2; exit 1; }

chmod 0755 "$tmp/yt-dlp"
mkdir -p "$tmp/deno-unpacked"
unzip -q "$tmp/deno.zip" -d "$tmp/deno-unpacked"
[[ -f "$tmp/deno-unpacked/deno" ]] || { echo "Deno archive has an unexpected layout" >&2; exit 1; }
install -m 0755 "$tmp/deno-unpacked/deno" "$tmp/deno"

actual_deno_version="$(timeout --signal=TERM --kill-after=5s "$VERSION_TIMEOUT" \
  "$tmp/deno" --version | awk '$1 == "deno" {print $2; exit}')"
[[ "$actual_deno_version" == "$deno_version" ]] \
  || { echo "downloaded Deno reported an unexpected version" >&2; exit 1; }
actual_yt_dlp_version="$(timeout --signal=TERM --kill-after=5s "$VERSION_TIMEOUT" \
  "$tmp/yt-dlp" --ignore-config --version | head -n 1)"
[[ "$actual_yt_dlp_version" == "$yt_dlp_version" ]] \
  || { echo "downloaded yt-dlp reported an unexpected version" >&2; exit 1; }

yt_dlp_network_options=(--socket-timeout 10 --retries 2 --extractor-retries 2)
canary_ok=false
read -r -a canary_urls \
  <<<"${WOTOHA_YTDLP_CANARY_URLS:-https://www.youtube.com/watch?v=H7HmzwI67ec https://www.youtube.com/watch?v=jNQXAC9IVRw}"
for canary_url in "${canary_urls[@]}"; do
  direct_url="$(timeout --signal=TERM --kill-after=5s "$CANARY_TIMEOUT" \
    "$tmp/yt-dlp" --ignore-config --no-playlist --no-warnings --no-progress \
    "${yt_dlp_network_options[@]}" --js-runtimes "deno:$tmp/deno" \
    --format 'bestaudio[protocol^=http]/bestaudio/best' --skip-download \
    --print '%(url)s' "$canary_url" 2>/dev/null | head -n 1)" || true
  [[ "$direct_url" =~ ^https:// ]] || continue
  rm -f "$tmp/canary.bytes"
  if curl --fail --silent --show-error --location "${curl_retry[@]}" --remove-on-error \
    --range 0-1023 --max-time 20 --max-filesize 4096 \
    --output "$tmp/canary.bytes" "$direct_url" \
    && [[ -s "$tmp/canary.bytes" ]] \
    && (( $(stat --format=%s "$tmp/canary.bytes") <= 4096 )); then
    canary_ok=true
    break
  fi
done
[[ "$canary_ok" == true ]] \
  || { echo "pinned yt-dlp canaries failed; preserving current installation" >&2; exit 1; }

candidate="$versions/$yt_dlp_digest"
install -d -m 0755 "$versions" /opt/wotoha/bin "$(dirname "$state")"
if [[ -x "$candidate/yt-dlp" ]]; then
  [[ "$(sha256sum "$candidate/yt-dlp" | awk '{print $1}')" == "$yt_dlp_digest" ]] \
    || { echo "installed yt-dlp version has an unexpected digest" >&2; exit 1; }
else
  candidate_new="$versions/.${yt_dlp_digest}.new.$$"
  rm -rf "$candidate_new"
  install -d -m 0755 "$candidate_new"
  install -m 0755 "$tmp/yt-dlp" "$candidate_new/yt-dlp"
  mv "$candidate_new" "$candidate"
fi

install -m 0644 "$package/deploy/yt-dlp-public.key" /etc/wotoha/yt-dlp-public.key
install -m 0755 "$package/yt-dlp-update.sh" /opt/wotoha/bin/yt-dlp-update
install -d -m 0755 /etc/wotoha/systemd
install -m 0644 "$package/deploy/yt-dlp-update.service" /etc/wotoha/systemd/yt-dlp-update.service
install -m 0644 "$package/deploy/yt-dlp-update.timer" /etc/wotoha/systemd/yt-dlp-update.timer
systemctl link --force /etc/wotoha/systemd/yt-dlp-update.service /etc/wotoha/systemd/yt-dlp-update.timer

for active_path in "$current" "$previous" /opt/wotoha/bin/deno /opt/wotoha/bin/yt-dlp "$state"; do
  [[ ! -d "$active_path" || -L "$active_path" ]] \
    || { echo "refusing to replace unexpected directory: $active_path" >&2; exit 1; }
done
install -d -m 0700 "$tmp/rollback"
for snapshot_spec in \
  "$current:current" \
  "$previous:previous" \
  /opt/wotoha/bin/deno:deno \
  /opt/wotoha/bin/yt-dlp:yt-dlp \
  "$state:state"; do
  active_path="${snapshot_spec%:*}"
  snapshot="${snapshot_spec##*:}"
  if [[ -e "$active_path" || -L "$active_path" ]]; then
    cp -a -- "$active_path" "$tmp/rollback/$snapshot"
  fi
done

install -m 0755 "$tmp/deno" /opt/wotoha/bin/deno.new
rm -f "$root/.current.new" "$root/.previous.new" /opt/wotoha/bin/.yt-dlp.new "$state.new"
if [[ -L "$current" && "$(readlink "$current")" != "versions/$yt_dlp_digest/yt-dlp" ]]; then
  ln -s "$(readlink "$current")" "$root/.previous.new"
fi
ln -s "versions/$yt_dlp_digest/yt-dlp" "$root/.current.new"
ln -s ../yt-dlp/current /opt/wotoha/bin/.yt-dlp.new
printf '%s %s %s\n' "$repository" "$yt_dlp_version" "$yt_dlp_digest" > "$state.new"
chmod 0644 "$state.new"

promotion_started=true
mv -f /opt/wotoha/bin/deno.new /opt/wotoha/bin/deno
if [[ -L "$root/.previous.new" ]]; then
  mv -Tf "$root/.previous.new" "$previous"
fi
mv -Tf "$root/.current.new" "$current"
mv -Tf /opt/wotoha/bin/.yt-dlp.new /opt/wotoha/bin/yt-dlp
mv -f "$state.new" "$state"
promotion_started=false
printf 'installed verified upstream yt-dlp %s and Deno %s\n' "$yt_dlp_version" "$deno_version"
