#!/usr/bin/env bash
# Verify the redistribution and updater contracts of completed Linux archives.
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

[[ $# -eq 2 ]] || fail 'usage: verify-release-archives.sh LEGACY_ARCHIVE APP_ONLY_ARCHIVE'
legacy_archive="$1"
app_archive="$2"
legacy_name='wotoha-ubuntu-x86_64-musl'
app_name='wotoha-linux-x86_64-musl'

for command in awk cmp find grep jq mkdir mktemp rm sha256sum tar; do
  command -v "$command" >/dev/null 2>&1 || fail "missing required command: $command"
done
for archive in "$legacy_archive" "$app_archive"; do
  [[ -s "$archive" ]] || fail "release archive is missing: $archive"
done

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

extract_checked() {
  local archive="$1" expected_root="$2" destination="$3" member
  tar -tzf "$archive" >/dev/null
  if tar -tvzf "$archive" | awk 'substr($1, 1, 1) == "l" || substr($1, 1, 1) == "h" {found = 1} END {exit !found}'; then
    fail "$archive contains a symbolic or hard link"
  fi
  while IFS= read -r member; do
    [[ "$member" != *'\\'* ]] || fail "$archive contains a backslash path"
    [[ "$member" != /* ]] || fail "$archive contains an absolute path"
    [[ "/$member" != *'/../'* && "/$member" != *'/..' ]] \
      || fail "$archive contains a parent traversal"
    [[ "$member" == "$expected_root" || "$member" == "$expected_root/"* ]] \
      || fail "$archive contains an unexpected top-level path: $member"
  done < <(tar -tzf "$archive")
  mkdir -p "$destination"
  tar -xzf "$archive" -C "$destination" --no-same-owner --no-same-permissions
  [[ -d "$destination/$expected_root" ]] \
    || fail "$archive is missing its expected top-level directory"
}

extract_checked "$legacy_archive" "$legacy_name" "$work/legacy"
extract_checked "$app_archive" "$app_name" "$work/app"
legacy="$work/legacy/$legacy_name"
app="$work/app/$app_name"

verify_checksums() {
  local package="$1" line digest marker_and_path path relative
  declare -A checksummed=()
  [[ -s "$package/SHA256SUMS.txt" ]] || fail "$package has no internal checksums"
  while IFS= read -r line; do
    [[ "$line" =~ ^[0-9a-f]{64}[[:space:]][\ *][^[:space:]]+$ ]] \
      || fail "$package has a malformed internal checksum: $line"
    digest="${line%% *}"
    marker_and_path="${line#"$digest"}"
    path="${marker_and_path:2}"
    [[ "$path" != /* && "/$path" != *'/../'* && "/$path" != *'/..' ]] \
      || fail "$package checksum contains an unsafe path"
    relative="${path#./}"
    [[ "$relative" != SHA256SUMS.txt ]] \
      || fail "$package checksum list includes itself"
    [[ -z "${checksummed[$relative]+present}" ]] \
      || fail "$package checksum list contains a duplicate path: $relative"
    checksummed["$relative"]=1
  done < "$package/SHA256SUMS.txt"
  (cd "$package" && sha256sum --check --strict SHA256SUMS.txt >/dev/null)
  while IFS= read -r -d '' path; do
    relative="${path#"$package/"}"
    [[ -n "${checksummed[$relative]+present}" ]] \
      || fail "$package has an unchecked regular file: $relative"
  done < <(find "$package" -type f ! -name SHA256SUMS.txt -print0)
  if find "$package" -type l -print -quit | grep -q .; then
    fail "$package contains a symbolic link"
  fi
}

verify_common() {
  local package="$1"
  [[ -x "$package/bin/wotoha-app" ]] || fail "$package is missing executable wotoha-app"
  cmp --silent "$ROOT/LICENSE" "$package/LICENSE" \
    || fail "$package does not contain the repository MIT License"
  cmp --silent "$ROOT/THIRD_PARTY_NOTICES.md" "$package/THIRD_PARTY_NOTICES.md" \
    || fail "$package does not contain the repository third-party notices"
  cmp --silent "$ROOT/Cargo.lock" "$package/third-party/rust/Cargo.lock" \
    || fail "$package does not contain the locked dependency set"
  [[ -s "$package/third-party/rust/THIRD_PARTY_LICENSES.html" ]] \
    || fail "$package does not contain the generated Rust license texts"
  grep -Fq 'Wotoha third-party Rust licenses' \
    "$package/third-party/rust/THIRD_PARTY_LICENSES.html" \
    || fail "$package has an invalid Rust license-text bundle"
  grep -Fq 'https://crates.io/crates/' \
    "$package/third-party/rust/THIRD_PARTY_LICENSES.html" \
    || fail "$package license-text bundle has no dependency source links"
  [[ -s "$package/third-party/rust/THIRD_PARTY_ATTRIBUTIONS.txt" ]] \
    || fail "$package does not contain standalone Rust copyright/notice files"
  grep -Fq 'Wotoha third-party Rust attributions' \
    "$package/third-party/rust/THIRD_PARTY_ATTRIBUTIONS.txt" \
    || fail "$package has an invalid Rust attribution bundle"
  jq --exit-status '
    .schema_version == 1
    and .generated_from == "Cargo.lock"
    and (.packages | length > 0)
    and all(.packages[];
      (.name | type == "string" and length > 0)
      and (.version | type == "string" and length > 0)
      and (((.license // "") | length > 0)
        or ((.license_file // "") | length > 0)))
  ' "$package/third-party/rust/license-inventory.json" >/dev/null \
    || fail "$package has an invalid Rust license inventory"
  verify_checksums "$package"
}

verify_common "$app"
verify_common "$legacy"

for forbidden in deploy install-ubuntu.sh \
  install-yt-dlp-bundle.sh wotoha-update.sh yt-dlp-update.sh; do
  [[ ! -e "$app/$forbidden" ]] || fail "app-only archive contains $forbidden"
done

for required in \
  deploy/wotoha.service \
  deploy/wotoha-update.service \
  deploy/wotoha-update.timer \
  deploy/third-party-versions.env \
  deploy/yt-dlp-public.key \
  deploy/yt-dlp-update.service \
  deploy/yt-dlp-update.timer \
  install-ubuntu.sh \
  install-yt-dlp-bundle.sh \
  wotoha-update.sh \
  yt-dlp-update.sh; do
  [[ -s "$legacy/$required" ]] || fail "legacy archive is missing $required"
done
[[ -x "$legacy/install-ubuntu.sh" \
    && -x "$legacy/install-yt-dlp-bundle.sh" \
    && -x "$legacy/wotoha-update.sh" \
    && -x "$legacy/yt-dlp-update.sh" ]] \
  || fail 'legacy updater entry points are not executable'

for package in "$app" "$legacy"; do
  while IFS= read -r payload; do
    fail "release archive contains a third-party runtime payload: ${payload#"$package/"}"
  done < <(find "$package" -type f \
    \( -name 'yt-dlp' -o -name 'yt-dlp_linux' -o -name 'deno' \
       -o -name 'deno*.zip' -o -name 'SHA2-256SUMS' -o -name 'SHA2-256SUMS.sig' \))
done
for shared in bin/wotoha-app LICENSE THIRD_PARTY_NOTICES.md \
  third-party/rust/Cargo.lock third-party/rust/license-inventory.json \
  third-party/rust/THIRD_PARTY_LICENSES.html \
  third-party/rust/THIRD_PARTY_ATTRIBUTIONS.txt; do
  cmp --silent "$app/$shared" "$legacy/$shared" \
    || fail "archives disagree on shared file: $shared"
done

printf 'ok - release archives satisfy app-only and updater-compatible contracts\n'
