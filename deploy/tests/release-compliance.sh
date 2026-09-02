#!/usr/bin/env bash
# Static release-policy checks. The release workflow performs the corresponding
# archive-content checks after it builds the actual artifacts.
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VERSIONS="$ROOT/deploy/third-party-versions.env"
NOTICES="$ROOT/THIRD_PARTY_NOTICES.md"
WORKFLOW="$ROOT/.github/workflows/release.yml"
PACKAGER="$ROOT/deploy/package-release-assets.sh"
VERIFY="$ROOT/deploy/verify-release-archives.sh"
BOOTSTRAP="$ROOT/deploy/install-yt-dlp-bundle.sh"
APP_UPDATER="$ROOT/deploy/wotoha-update.sh"
WINDOWS_PACKAGER="$ROOT/deploy/build-ubuntu-musl.ps1"
ABOUT_CONFIG="$ROOT/deploy/release-about.toml"
ABOUT_TEMPLATE="$ROOT/deploy/third-party-licenses.hbs"
ATTRIBUTION_GENERATOR="$ROOT/deploy/generate-cargo-attributions.sh"
MODEL_COMPLIANCE="$ROOT/deploy/tests/neural-model-compliance.sh"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

[[ -s "$ROOT/LICENSE" ]] || fail 'root MIT LICENSE is required for release packaging'
[[ -s "$NOTICES" ]] || fail 'third-party notices are required for release packaging'
[[ -s "$VERSIONS" ]] || fail 'third-party version pins are required'
[[ -s "$PACKAGER" ]] || fail 'release packager is required'
[[ -s "$VERIFY" ]] || fail 'release archive verifier is required'
[[ -s "$BOOTSTRAP" ]] || fail 'third-party runtime bootstrap is required'
[[ -s "$WINDOWS_PACKAGER" ]] || fail 'PowerShell release packager is required'
[[ -s "$ABOUT_CONFIG" && -s "$ABOUT_TEMPLATE" ]] \
  || fail 'cargo-about release policy and template are required'
[[ -s "$ATTRIBUTION_GENERATOR" ]] || fail 'standalone Cargo attribution generator is required'
[[ -s "$MODEL_COMPLIANCE" ]] || fail 'embedded neural model compliance test is required'
bash -n "$PACKAGER" "$VERIFY" "$BOOTSTRAP" "$ATTRIBUTION_GENERATOR" "$MODEL_COMPLIANCE" "$0"
bash "$MODEL_COMPLIANCE"

# shellcheck source=/dev/null
source "$VERSIONS"
for variable in YTDLP_REPOSITORY YTDLP_VERSION DENO_VERSION DENO_X86_64_LINUX_GNU_SHA256; do
  [[ -n "${!variable:-}" ]] || fail "missing $variable"
done

case "$YTDLP_REPOSITORY" in
  yt-dlp/yt-dlp|yt-dlp/yt-dlp-nightly-builds) ;;
  *) fail 'YTDLP_REPOSITORY is not an official yt-dlp release repository' ;;
esac
[[ "$YTDLP_VERSION" =~ ^[0-9]{4}[.][0-9]{2}[.][0-9]{2}([.][0-9]{6})?$ ]] \
  || fail 'YTDLP_VERSION is not a release tag'
[[ "$DENO_VERSION" =~ ^[0-9]+[.][0-9]+[.][0-9]+$ ]] \
  || fail 'DENO_VERSION is not a release version'
[[ "$DENO_X86_64_LINUX_GNU_SHA256" =~ ^[0-9a-f]{64}$ ]] \
  || fail 'Deno digest is not lowercase SHA-256'

grep -Fq 'wotoha-linux-x86_64-musl.tar.gz' "$NOTICES" \
  || fail 'notices do not identify the app-only release artifact'
grep -Fq 'yt-dlp and Deno directly from their official' "$NOTICES" \
  || fail 'notices do not explain direct upstream runtime acquisition'
grep -Fq 'license-inventory.json' "$NOTICES" \
  || fail 'notices do not describe the Rust dependency inventory'
grep -Fq 'THIRD_PARTY_LICENSES.html' "$NOTICES" \
  || fail 'notices do not describe the generated Rust license texts'
grep -Fq 'THIRD_PARTY_ATTRIBUTIONS.txt' "$NOTICES" \
  || fail 'notices do not describe standalone Rust copyright/notice material'
grep -Fq 'wotoha-linux-x86_64-musl.tar.gz' "$WORKFLOW" \
  || fail 'release workflow does not publish the app-only artifact'
grep -Fq 'package-release-assets.sh' "$WORKFLOW" \
  || fail 'release workflow does not use the compliance packager'
grep -Fq 'verify-release-archives.sh' "$PACKAGER" \
  || fail 'release packager does not enforce the archive contracts'
grep -Fq 'cargo-about --version 0.9.1 --locked --features cli' "$WORKFLOW" \
  || fail 'release workflow does not install the pinned license generator'
grep -Fq 'about generate --frozen --fail' "$WORKFLOW" \
  || fail 'release workflow does not generate licenses fail-closed from the lockfile'
! grep -Fq 'LGPL-2.1-or-later' "$ABOUT_CONFIG" \
  || fail 'release license policy must not broadly accept LGPL-only dependencies'
grep -Fq 'MPL-2.0' "$ABOUT_CONFIG" \
  || fail 'release license policy does not explicitly review MPL-2.0'
grep -Fqx 'ignore-build-dependencies = false' "$ABOUT_CONFIG" \
  || fail 'release license bundle must cover locked build dependencies'
grep -Fqx 'ignore-dev-dependencies = false' "$ABOUT_CONFIG" \
  || fail 'release license bundle must cover the complete locked workspace graph'
! grep -Eq '(releases/download|yt-dlp_linux|deno-x86_64-unknown-linux-gnu[.]zip)' "$PACKAGER" \
  || fail 'release packager still downloads a third-party runtime'
! grep -Eq '(releases/download|yt-dlp_linux|deno-x86_64-unknown-linux-gnu[.]zip|Expand-Archive)' \
  "$WINDOWS_PACKAGER" || fail 'PowerShell packager still downloads a third-party runtime'
grep -Fq '"$ROOT/LICENSE" "$ROOT/THIRD_PARTY_NOTICES.md"' "$PACKAGER" \
  || fail 'release packager does not require the project license and notices'
for bootstrap_contract in \
  'github.com/$repository/releases/download/$yt_dlp_version' \
  'github.com/denoland/deno/releases/download/v$deno_version' \
  '--retry-all-errors' '--retry-max-time' '--max-filesize' \
  'DENO_X86_64_LINUX_GNU_SHA256' 'SHA2-256SUMS.sig' \
  '--status-fd 1' 'VALIDSIG' 'valid_primary_fingerprints' \
  'unexpected yt-dlp signing key fingerprint' 'actual_yt_dlp_version' 'actual_deno_version'; do
  grep -Fq -- "$bootstrap_contract" "$BOOTSTRAP" \
    || fail "runtime bootstrap is missing contract: $bootstrap_contract"
done
grep -Fq 'tampered release manifest passed attestation verification' "$WORKFLOW" \
  || fail 'release workflow does not test tampered manifest attestations'
grep -Fq 'download_bundle "$name.tar.gz.manifest.json"' "$WORKFLOW" \
  || fail 'release attestation bundle does not explicitly include the manifest alias subject'
grep -Fq "release_size=\"\$(jq --raw-output '.size'" "$APP_UPDATER" \
  || fail 'application updater does not read the attested archive size'
grep -Fq "stat --format='%s' \"\$archive\"" "$APP_UPDATER" \
  || fail 'application updater does not compare the archive byte size'
grep -Fq 'https://www.mozilla.org/MPL/2.0/' "$NOTICES" \
  || fail 'notices do not link the MPL-2.0 terms'
for model_contract in \
  'third-party/neural-models/' \
  'a5f8d39d989f31859454ba27afe61c5317ca95e4d9373e6853e5361b8937172f' \
  'fdd59e65c515331308e4c8841edf99972deca646bdf6197744c2a5b7755e3de9' \
  'include_bytes!' \
  'do not duplicate the approximately 10 MiB' \
  'copyright attribution and complete pinned MIT license texts are included' \
  'No training files or datasets are included.' \
  'may have separate terms'; do
  grep -Fq -- "$model_contract" "$NOTICES" \
    || fail "notices are missing neural-model contract: $model_contract"
done
! grep -Fq "This is sufficient for MIT's redistribution terms" "$NOTICES" \
  || fail 'notices make an unsupported blanket MIT sufficiency claim'
! grep -Fq 'MIT imposes no source-availability requirement' "$NOTICES" \
  || fail 'notices make an unsupported source-availability conclusion'
for model_notice_name in NOTICE.txt LICENSE.beat-this-rs.txt LICENSE.beat-this-original.txt; do
  grep -Fq -- "$model_notice_name" "$PACKAGER" \
    || fail "Linux packager does not copy neural-model attribution: $model_notice_name"
  grep -Fq -- "$model_notice_name" "$WINDOWS_PACKAGER" \
    || fail "PowerShell packager does not copy neural-model attribution: $model_notice_name"
done
grep -Fq 'third-party/neural-models/NOTICE.txt' "$VERIFY" \
  || fail 'archive verifier does not require neural-model attribution'
grep -Fq -- "-name '*.onnx'" "$VERIFY" \
  || fail 'archive verifier does not reject duplicate ONNX payloads'

# On CI, require an exact crates.io source link and upstream repository for
# every MPL-2.0 package in the locked release dependency graph. This keeps the
# executable-form source-location notice in sync with Cargo metadata.
if command -v cargo >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
  found_mpl=false
  while IFS=$'\t' read -r name version repository; do
    repository="${repository%$'\r'}"
    found_mpl=true
    [[ -n "$repository" ]] || fail "MPL-2.0 package has no upstream repository: $name $version"
    source_url="https://crates.io/api/v1/crates/$name/$version/download"
    grep -Fq "$source_url" "$NOTICES" \
      || fail "notices lack the exact crates.io source link for MPL-2.0 package: $name $version"
    grep -Fq "$repository" "$NOTICES" \
      || fail "notices lack the upstream repository for MPL-2.0 package: $name $version"
  done < <(cargo metadata --locked --format-version 1 | jq -r '
    .packages[]
    | select((.license // "") | contains("MPL-2.0"))
    | [.name, .version, (.repository // "")] | @tsv
  ')
  [[ "$found_mpl" == true ]] || fail 'locked release dependency graph has no MPL-2.0 packages to audit'
fi

# Exercise the real archive verifier with small, offline fixtures on CI. A
# developer host without jq still gets all static policy and syntax checks.
if command -v jq >/dev/null 2>&1; then
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  legacy_name='wotoha-ubuntu-x86_64-musl'
  app_name='wotoha-linux-x86_64-musl'
  fixture_binary="$work/wotoha-app"
  fixture_metadata="$work/cargo-metadata.json"
  fixture_licenses="$work/THIRD_PARTY_LICENSES.html"
  fixture_attributions="$work/THIRD_PARTY_ATTRIBUTIONS.txt"
  fixture_dist="$work/dist"
  attribution_source="$work/attribution-source"
  mkdir -p "$attribution_source"
  printf '[package]\nname = "fixture"\nversion = "1.0.0"\n' \
    > "$attribution_source/Cargo.toml"
  printf 'Copyright 2026 Release Fixture\n' > "$attribution_source/COPYRIGHT"
  jq --null-input --arg manifest "$attribution_source/Cargo.toml" \
    '{packages: [{name: "fixture", version: "1.0.0", source: "registry+fixture", manifest_path: $manifest}]}' \
    > "$work/attribution-metadata.json"
  bash "$ATTRIBUTION_GENERATOR" "$work/attribution-metadata.json" "$fixture_attributions"
  grep -Fq '===== fixture 1.0.0' "$fixture_attributions" \
    && grep -Fq 'Copyright 2026 Release Fixture' "$fixture_attributions" \
    || fail 'standalone Cargo attribution generator omitted a package copyright file'
  printf '#!/usr/bin/env bash\nexit 0\n' > "$fixture_binary"
  chmod 0755 "$fixture_binary"
  printf '%s\n' '{"packages":[{"name":"fixture","version":"1.0.0","source":"registry+fixture","license":"MIT","license_file":null,"repository":"https://crates.io/crates/fixture"}]}' \
    > "$fixture_metadata"
  printf '%s\n' '<h1>Wotoha third-party Rust licenses</h1><p>Used by:</p><a href="https://crates.io/crates/fixture/1.0.0">fixture</a>' \
    > "$fixture_licenses"
  GITHUB_REF_NAME=v0.0.0 \
    GITHUB_SHA=0000000000000000000000000000000000000000 \
    bash "$PACKAGER" "$fixture_binary" "$fixture_metadata" "$fixture_licenses" \
    "$fixture_attributions" "$fixture_dist" \
    >/dev/null

  app="$fixture_dist/$app_name"
  legacy="$fixture_dist/$legacy_name"
  cmp --silent "$fixture_dist/$legacy_name.manifest.json" \
    "$fixture_dist/$legacy_name.tar.gz.manifest.json" \
    || fail 'updater-compatible manifest alias differs from the canonical manifest'
  jq --exit-status --argjson size "$(stat --format=%s "$fixture_dist/$legacy_name.tar.gz")" \
    '.size == $size' "$fixture_dist/$legacy_name.manifest.json" >/dev/null \
    || fail 'release manifest does not bind the archive size'
  grep -Fq 'third-party/rust/THIRD_PARTY_LICENSES.html' "$app/SHA256SUMS.txt" \
    || fail 'app-only checksum list omits the generated license texts'
  grep -Fq 'third-party/rust/THIRD_PARTY_ATTRIBUTIONS.txt' "$app/SHA256SUMS.txt" \
    || fail 'app-only checksum list omits standalone copyright/notice material'
  for model_file in \
    third-party/neural-models/NOTICE.txt \
    third-party/neural-models/LICENSE.beat-this-rs.txt \
    third-party/neural-models/LICENSE.beat-this-original.txt; do
    grep -Fq "$model_file" "$app/SHA256SUMS.txt" \
      || fail "app-only checksum list omits neural-model attribution: $model_file"
  done

  mkdir -p "$work/missing-model"
  cp -a "$app" "$work/missing-model/$app_name"
  rm -f "$work/missing-model/$app_name/third-party/neural-models/NOTICE.txt"
  tar -czf "$work/missing-model.tar.gz" -C "$work/missing-model" "$app_name"
  if bash "$VERIFY" "$fixture_dist/$legacy_name.tar.gz" "$work/missing-model.tar.gz" \
    >/dev/null 2>&1; then
    fail 'archive verifier accepted an app-only archive missing neural-model attribution'
  fi

  mkdir -p "$work/duplicate-model"
  cp -a "$app" "$work/duplicate-model/$app_name"
  printf 'duplicate model fixture\n' \
    > "$work/duplicate-model/$app_name/third-party/neural-models/duplicate.onnx"
  (
    cd "$work/duplicate-model/$app_name"
    find . -type f ! -name SHA256SUMS.txt -print0 \
      | sort -z | xargs -0 sha256sum > SHA256SUMS.txt
  )
  tar -czf "$work/duplicate-model.tar.gz" -C "$work/duplicate-model" "$app_name"
  if bash "$VERIFY" "$fixture_dist/$legacy_name.tar.gz" "$work/duplicate-model.tar.gz" \
    >/dev/null 2>&1; then
    fail 'archive verifier accepted an app-only archive with a duplicate ONNX payload'
  fi

  grep -Fq 'yt-dlp-update.sh' "$legacy/SHA256SUMS.txt" \
    || fail 'updater-compatible checksum list omits an installed executable'

  printf '#!/usr/bin/env bash\nexit 0\n' > "$app/third-party/deno"
  tar -czf "$work/contaminated-app.tar.gz" -C "$fixture_dist" "$app_name"
  if bash "$VERIFY" "$fixture_dist/$legacy_name.tar.gz" "$work/contaminated-app.tar.gz" \
    >/dev/null 2>&1; then
    fail 'archive verifier accepted third-party tooling in app-only artifact'
  fi
  rm -f "$app/third-party/deno"
  printf '#!/usr/bin/env bash\nexit 0\n' > "$legacy/third-party/yt-dlp"
  tar -czf "$work/contaminated-legacy.tar.gz" -C "$fixture_dist" "$legacy_name"
  if bash "$VERIFY" "$work/contaminated-legacy.tar.gz" "$fixture_dist/$app_name.tar.gz" \
    >/dev/null 2>&1; then
    fail 'archive verifier accepted third-party tooling in updater-compatible artifact'
  fi
fi

printf 'ok - release redistribution policy inputs are present and pinned\n'
