#!/usr/bin/env bash
set -Eeuo pipefail

package="${1:?package root is required}"
root=/opt/wotoha/yt-dlp
versions="$root/versions"
current="$root/current"
previous="$root/previous"
expected_fingerprint=AC0CBBE6848D6A873464AF4E57CF65933B5A7581
state=/var/lib/wotoha-updater/installed-yt-dlp
readonly YTDLP_VERSION_TIMEOUT=20s
readonly YTDLP_CANARY_TIMEOUT=60s
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT

for command in awk curl flock gpg install sha256sum stat systemctl timeout; do
  command -v "$command" >/dev/null || { echo "required command is missing: $command" >&2; exit 1; }
done
exec 8>/run/lock/wotoha-ytdlp-update.lock
flock 8
for path in third-party/yt-dlp third-party/deno third-party/SHA2-256SUMS third-party/SHA2-256SUMS.sig deploy/yt-dlp-public.key deploy/third-party-versions.env deploy/yt-dlp-update.service deploy/yt-dlp-update.timer yt-dlp-update.sh; do
  [[ -r "$package/$path" ]] || { echo "bundle is missing $path" >&2; exit 1; }
done
repository="$(awk -F= '$1 == "YTDLP_REPOSITORY" {print $2}' "$package/deploy/third-party-versions.env")"
case "$repository" in yt-dlp/yt-dlp|yt-dlp/yt-dlp-nightly-builds) ;; *) echo "invalid bundled yt-dlp repository" >&2; exit 1 ;; esac
version="$(awk -F= '$1 == "YTDLP_VERSION" {print $2}' "$package/deploy/third-party-versions.env")"
[[ "$version" =~ ^[0-9]{4}[.][0-9]{2}[.][0-9]{2}([.][0-9]{6})?$ ]] || { echo "invalid bundled yt-dlp version" >&2; exit 1; }
(cd "$package" && sha256sum --check --status SHA256SUMS.txt)
install -d -m 0700 "$tmp/gnupg"
gpg --batch --homedir "$tmp/gnupg" --import "$package/deploy/yt-dlp-public.key" >/dev/null 2>&1
fingerprint="$(gpg --batch --homedir "$tmp/gnupg" --with-colons --fingerprint | awk -F: '$1 == "fpr" {print $10; exit}')"
[[ "$fingerprint" == "$expected_fingerprint" ]] || { echo "unexpected yt-dlp signing key fingerprint" >&2; exit 1; }
gpg --batch --homedir "$tmp/gnupg" --verify "$package/third-party/SHA2-256SUMS.sig" "$package/third-party/SHA2-256SUMS" >/dev/null 2>&1
digest="$(sha256sum "$package/third-party/yt-dlp" | awk '{print $1}')"
expected_digest="$(awk '$2 == "yt-dlp_linux" && $1 ~ /^[0-9a-f]{64}$/ {print $1}' "$package/third-party/SHA2-256SUMS")"
[[ "$expected_digest" =~ ^[0-9a-f]{64}$ && "$digest" == "$expected_digest" ]] || {
  echo "bundled yt-dlp digest did not match the signed checksum" >&2
  exit 1
}
candidate="$versions/$digest"
install -d -m 0755 "$versions"
if [[ -x "$candidate/yt-dlp" ]]; then
  [[ "$(sha256sum "$candidate/yt-dlp" | awk '{print $1}')" == "$digest" ]] || {
    echo "installed yt-dlp version has an unexpected digest" >&2
    exit 1
  }
else
  candidate_new="$versions/.${digest}.new.$$"
  rm -rf "$candidate_new"
  install -d -m 0755 "$candidate_new"
  install -m 0755 "$package/third-party/yt-dlp" "$candidate_new/yt-dlp"
  [[ "$(sha256sum "$candidate_new/yt-dlp" | awk '{print $1}')" == "$digest" ]]
  mv "$candidate_new" "$candidate"
fi
install -m 0755 "$package/third-party/deno" "$tmp/deno"
timeout --signal=TERM --kill-after=5s "$YTDLP_VERSION_TIMEOUT" "$tmp/deno" --version >/dev/null
yt_dlp_network_options=(--socket-timeout 10 --retries 2 --extractor-retries 2)
timeout --signal=TERM --kill-after=5s "$YTDLP_VERSION_TIMEOUT" \
  "$candidate/yt-dlp" --ignore-config --no-playlist \
  "${yt_dlp_network_options[@]}" --js-runtimes "deno:$tmp/deno" --version >/dev/null

canary_ok=false
read -r -a canary_urls <<<"${WOTOHA_YTDLP_CANARY_URLS:-https://www.youtube.com/watch?v=H7HmzwI67ec https://www.youtube.com/watch?v=jNQXAC9IVRw}"
for canary_url in "${canary_urls[@]}"; do
  direct_url="$(timeout --signal=TERM --kill-after=5s "$YTDLP_CANARY_TIMEOUT" \
    "$candidate/yt-dlp" --ignore-config --no-playlist --no-warnings --no-progress \
    "${yt_dlp_network_options[@]}" --js-runtimes "deno:$tmp/deno" \
    --format 'bestaudio[protocol^=http]/bestaudio/best' --skip-download \
    --print '%(url)s' "$canary_url" 2>/dev/null | head -n1)" || true
  [[ "$direct_url" =~ ^https:// ]] || continue
  set +o pipefail
  curl --fail --silent --show-error --location --range 0-1023 --max-time 20 "$direct_url" | head -c 4096 >"$tmp/canary.bytes"
  statuses=("${PIPESTATUS[@]}"); set -o pipefail
  if [[ "${statuses[0]}" == 0 || "${statuses[0]}" == 23 ]] && [[ -s "$tmp/canary.bytes" ]] && (( $(stat --format=%s "$tmp/canary.bytes") <= 4096 )); then canary_ok=true; break; fi
done
[[ "$canary_ok" == true ]] || { echo "bundled yt-dlp canaries failed; preserving current" >&2; exit 1; }

install -m 0755 "$package/third-party/deno" /opt/wotoha/bin/deno.new
mv -f /opt/wotoha/bin/deno.new /opt/wotoha/bin/deno
rm -f "$root/.current.new" "$root/.previous.new" /opt/wotoha/bin/.yt-dlp.new
if [[ -L "$current" ]] && [[ "$(readlink "$current")" != "versions/$digest/yt-dlp" ]]; then ln -s "$(readlink "$current")" "$root/.previous.new"; mv -Tf "$root/.previous.new" "$previous"; fi
ln -s "versions/$digest/yt-dlp" "$root/.current.new"; mv -Tf "$root/.current.new" "$current"
ln -s ../yt-dlp/current /opt/wotoha/bin/.yt-dlp.new; mv -Tf /opt/wotoha/bin/.yt-dlp.new /opt/wotoha/bin/yt-dlp
install -m 0644 "$package/deploy/yt-dlp-public.key" /etc/wotoha/yt-dlp-public.key
install -m 0755 "$package/yt-dlp-update.sh" /opt/wotoha/bin/yt-dlp-update
install -d -m 0755 /etc/wotoha/systemd
install -m 0644 "$package/deploy/yt-dlp-update.service" /etc/wotoha/systemd/yt-dlp-update.service
install -m 0644 "$package/deploy/yt-dlp-update.timer" /etc/wotoha/systemd/yt-dlp-update.timer
systemctl link --force /etc/wotoha/systemd/yt-dlp-update.service /etc/wotoha/systemd/yt-dlp-update.timer
printf '%s %s %s\n' "$repository" "$version" "$digest" > "$state.new"
chmod 0644 "$state.new"
mv -f "$state.new" "$state"
