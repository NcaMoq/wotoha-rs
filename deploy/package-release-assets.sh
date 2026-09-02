#!/usr/bin/env bash
# Build the updater-compatible and app-only Linux release archives.
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

[[ $# -eq 5 ]] \
  || fail 'usage: package-release-assets.sh APP_BINARY CARGO_METADATA_JSON THIRD_PARTY_LICENSES_HTML THIRD_PARTY_ATTRIBUTIONS_TXT DIST_DIR'
binary="$1"
metadata="$2"
licenses_html="$3"
attributions_txt="$4"
mkdir -p "$5"
dist="$(cd "$5" && pwd)"
tag="${GITHUB_REF_NAME:?GITHUB_REF_NAME is required}"
commit="${GITHUB_SHA:?GITHUB_SHA is required}"
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || fail 'GITHUB_SHA must be a 40-character commit ID'

for command in awk bash cp find grep install jq mkdir sha256sum sort stat tar touch xargs; do
  command -v "$command" >/dev/null 2>&1 || fail "missing required command: $command"
done
for input in "$binary" "$metadata" "$licenses_html" "$attributions_txt" \
  "$ROOT/LICENSE" "$ROOT/THIRD_PARTY_NOTICES.md" \
  "$ROOT/Cargo.lock" \
  "$ROOT/crates/wotoha-runtime/models/NOTICE.txt" \
  "$ROOT/crates/wotoha-runtime/models/LICENSE.beat-this-rs.txt" \
  "$ROOT/crates/wotoha-runtime/models/LICENSE.beat-this-original.txt"; do
  [[ -s "$input" ]] || fail "required release input is missing: $input"
done

legacy_name='wotoha-ubuntu-x86_64-musl'
app_name='wotoha-linux-x86_64-musl'
legacy="$dist/$legacy_name"
app="$dist/$app_name"
for output in "$legacy" "$app" "$dist/$legacy_name.tar.gz" "$dist/$app_name.tar.gz"; do
  [[ ! -e "$output" ]] || fail "refusing to overwrite release output: $output"
done

mkdir -p "$app/bin" "$app/third-party/rust"
install -m 0755 "$binary" "$app/bin/wotoha-app"
install -m 0644 "$ROOT/LICENSE" "$ROOT/THIRD_PARTY_NOTICES.md" "$app/"
install -m 0644 "$ROOT/Cargo.lock" "$app/third-party/rust/Cargo.lock"
mkdir -p "$app/third-party/neural-models"
install -m 0644 \
  "$ROOT/crates/wotoha-runtime/models/NOTICE.txt" \
  "$ROOT/crates/wotoha-runtime/models/LICENSE.beat-this-rs.txt" \
  "$ROOT/crates/wotoha-runtime/models/LICENSE.beat-this-original.txt" \
  "$app/third-party/neural-models/"
grep -Fq 'Wotoha third-party Rust licenses' "$licenses_html" \
  || fail 'generated third-party license bundle has an unexpected format'
grep -Fq 'Used by:' "$licenses_html" \
  || fail 'generated third-party license bundle has no dependency attribution'
install -m 0644 "$licenses_html" "$app/third-party/rust/THIRD_PARTY_LICENSES.html"
grep -Fq 'Wotoha third-party Rust attributions' "$attributions_txt" \
  || fail 'generated third-party attribution bundle has an unexpected format'
install -m 0644 "$attributions_txt" "$app/third-party/rust/THIRD_PARTY_ATTRIBUTIONS.txt"
jq --exit-status '
  [.packages[]
    | select((((.license // "") | length) == 0)
      and (((.license_file // "") | length) == 0))]
  | length == 0
' "$metadata" >/dev/null || fail 'Cargo metadata contains a package without license information'
jq '{
  schema_version: 1,
  generated_from: "Cargo.lock",
  target: "x86_64-unknown-linux-musl",
  packages: ([.packages[] | {
    name,
    version,
    source,
    license,
    license_file: (if .license_file then (.license_file | split("/") | last) else null end),
    repository
  }] | sort_by(.name, .version, (.source // "")))
}' "$metadata" > "$app/third-party/rust/license-inventory.json"
printf '%s\n' "$tag" > "$app/RELEASE_VERSION"

# Hard links keep shared application material identical without doubling the
# packaging workspace. The two tar archives remain independent release files.
mkdir -p "$legacy"
cp -al "$app/." "$legacy/"
mkdir -p "$legacy/deploy" "$legacy/docs"

# shellcheck source=/dev/null
source "$ROOT/deploy/third-party-versions.env"
for variable in \
  YTDLP_REPOSITORY YTDLP_VERSION DENO_VERSION DENO_X86_64_LINUX_GNU_SHA256; do
  [[ -n "${!variable:-}" ]] || fail "third-party pin is missing: $variable"
done
case "$YTDLP_REPOSITORY" in
  yt-dlp/yt-dlp|yt-dlp/yt-dlp-nightly-builds) ;;
  *) fail 'YTDLP_REPOSITORY is not an official release repository' ;;
esac
[[ "$YTDLP_VERSION" =~ ^[0-9]{4}[.][0-9]{2}[.][0-9]{2}([.][0-9]{6})?$ ]] \
  || fail 'YTDLP_VERSION is not a release tag'
[[ "$DENO_VERSION" =~ ^[0-9]+[.][0-9]+[.][0-9]+$ ]] \
  || fail 'DENO_VERSION is not a release version'
[[ "$DENO_X86_64_LINUX_GNU_SHA256" =~ ^[0-9a-f]{64}$ ]] \
  || fail 'Deno digest is not lowercase SHA-256'

install -m 0755 "$ROOT/deploy/install-ubuntu.sh" "$ROOT/deploy/install-yt-dlp-bundle.sh" \
  "$ROOT/deploy/wotoha-update.sh" "$ROOT/deploy/yt-dlp-update.sh" "$legacy/"
install -m 0644 \
  "$ROOT/deploy/wotoha.service" \
  "$ROOT/deploy/wotoha-update.service" \
  "$ROOT/deploy/wotoha-update.timer" \
  "$ROOT/deploy/yt-dlp-update.service" \
  "$ROOT/deploy/yt-dlp-update.timer" \
  "$ROOT/deploy/wotoha.env.example" \
  "$ROOT/deploy/wotoha-update.env.example" \
  "$ROOT/deploy/yt-dlp-public.key" \
  "$ROOT/deploy/third-party-versions.env" \
  "$legacy/deploy/"
install -m 0644 "$ROOT/docs/ubuntu-deploy.md" "$ROOT/docs/youtube-extraction.md" "$legacy/docs/"

write_internal_checksums() {
  local package="$1"
  (cd "$package" && find . -type f ! -name SHA256SUMS.txt -print0 \
    | sort -z | xargs -0 sha256sum > SHA256SUMS.txt)
}
write_internal_checksums "$app"
write_internal_checksums "$legacy"
for package in "$app" "$legacy"; do
  (cd "$package" && sha256sum --check --strict SHA256SUMS.txt >/dev/null)
  find "$package" -exec touch -h -d '@0' {} +
done

tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner \
  -czf "$dist/$legacy_name.tar.gz" -C "$dist" "$legacy_name"
tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner \
  -czf "$dist/$app_name.tar.gz" -C "$dist" "$app_name"
bash "$ROOT/deploy/verify-release-archives.sh" \
  "$dist/$legacy_name.tar.gz" "$dist/$app_name.tar.gz"

write_sidecars() {
  local name archive manifest digest size
  name="$1"
  archive="$dist/$name.tar.gz"
  manifest="$dist/$name.manifest.json"
  (cd "$dist" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
  digest="$(sha256sum "$archive" | awk '{print $1}')"
  size="$(stat --format=%s "$archive")"
  jq --null-input \
    --arg tag "$tag" \
    --arg commit "$commit" \
    --arg asset "$name.tar.gz" \
    --arg digest "$digest" \
    --argjson size "$size" \
    '{schema_version: 1, tag: $tag, commit: $commit, asset: $asset, sha256: $digest, size: $size}' \
    > "$manifest"
  # Existing installations request the archive-suffixed name. Keep an exact
  # alias while the clean name is used by new manual verification guidance.
  cp "$manifest" "$archive.manifest.json"
}
write_sidecars "$legacy_name"
write_sidecars "$app_name"

printf 'ok - packaged %s and %s\n' "$legacy_name.tar.gz" "$app_name.tar.gz"
