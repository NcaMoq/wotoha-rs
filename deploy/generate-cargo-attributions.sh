#!/usr/bin/env bash
# Preserve standalone COPYRIGHT/NOTICE files that are not necessarily folded
# into cargo-about's normalized license text.
set -Eeuo pipefail

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

[[ $# -eq 2 ]] \
  || fail 'usage: generate-cargo-attributions.sh CARGO_METADATA_JSON OUTPUT_FILE'
metadata="$1"
output="$2"
[[ -s "$metadata" ]] || fail "Cargo metadata is missing: $metadata"
for command in cat dirname find jq mkdir mv sort; do
  command -v "$command" >/dev/null 2>&1 || fail "missing required command: $command"
done

mkdir -p "$(dirname "$output")"
tmp="$output.new.$$"
packages="$tmp.packages"
attributions="$tmp.attributions"
trap 'rm -f "$tmp" "$packages" "$attributions"' EXIT
jq -r '.packages | sort_by(.name, .version, (.source // ""))[]
  | [.name, .version, .manifest_path] | @tsv' "$metadata" > "$packages"
{
  printf 'Wotoha third-party Rust attributions\n'
  printf 'Generated from standalone COPYRIGHT and NOTICE files in the locked dependency graph.\n'
  while IFS=$'\t' read -r name version manifest_path; do
    package_dir="$(dirname "$manifest_path")"
    [[ -d "$package_dir" ]] || fail "Cargo package source is unavailable: $name $version"
    find "$package_dir" -maxdepth 1 -type f \
      \( -iname 'COPYRIGHT' -o -iname 'COPYRIGHT.*' -o -iname 'COPYRIGHT-*' \
         -o -iname 'NOTICE' -o -iname 'NOTICE.*' -o -iname 'NOTICE-*' \) \
      -print0 | sort -z > "$attributions"
    while IFS= read -r -d '' attribution; do
      printf '\n===== %s %s — %s =====\n' "$name" "$version" "${attribution##*/}"
      cat "$attribution"
      printf '\n'
    done < "$attributions"
  done < "$packages"
} > "$tmp"
mv -f "$tmp" "$output"
rm -f "$packages" "$attributions"
trap - EXIT
