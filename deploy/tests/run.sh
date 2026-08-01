#!/usr/bin/env bash
# Executable regression tests for the independently-updated yt-dlp channel.
# Everything below runs against a rewritten copy of the updater: never /opt,
# /etc, /var, or /run on the host running the tests.
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
UPDATE="$ROOT/deploy/yt-dlp-update.sh"
UPDATE_SERVICE="$ROOT/deploy/yt-dlp-update.service"
INSTALL_BUNDLE="$ROOT/deploy/install-yt-dlp-bundle.sh"
APP_UPDATE="$ROOT/deploy/wotoha-update.sh"
INSTALL_UBUNTU="$ROOT/deploy/install-ubuntu.sh"
MOCK="$ROOT/deploy/tests/mock-command.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok - %s\n' "$*"; }
expect_fail() { "$@" && fail "expected failure: $*" || return 0; }

require() {
  command -v "$1" >/dev/null 2>&1 || fail "test prerequisite is missing: $1"
}
for cmd in bash sha256sum stat sed awk mktemp; do require "$cmd"; done
bash -n "$UPDATE" "$INSTALL_BUNDLE" "$APP_UPDATE" "$INSTALL_UBUNTU" "$MOCK" "$0"

# The extraction process and its network operations need independent bounds;
# the service deadline is the final backstop for the whole transaction.
grep -Eq '(^|[[:space:]])timeout([[:space:]]|$)' "$UPDATE" \
  || fail 'yt-dlp canary extraction is not wrapped in a process timeout'
grep -Fq -- '--socket-timeout' "$UPDATE" || fail 'yt-dlp canary socket timeout is missing'
grep -Fq -- '--retries' "$UPDATE" || fail 'yt-dlp canary retry bound is missing'
grep -Fq -- '--extractor-retries' "$UPDATE" || fail 'yt-dlp extractor retry bound is missing'
timeout_start="$(awk -F= '$1 == "TimeoutStartSec" { print $2; exit }' "$UPDATE_SERVICE")"
[[ "$timeout_start" =~ ^[1-9][0-9]*(s|min|h)?$ ]] \
  || fail 'yt-dlp update service needs a finite positive TimeoutStartSec'

# Git for Windows can report success for `ln -s` while creating a regular file
# when developer-mode symlinks are unavailable. The production updater is
# Linux-only, so run the semantic suite in Linux CI and keep this local pass to
# syntax/diff validation in that constrained environment.
link_probe="$work/symlink-probe"
touch "$work/symlink-target"
ln -s symlink-target "$link_probe" 2>/dev/null || true
if [[ ! -L "$link_probe" ]]; then
  printf 'ok - syntax checks (symlink semantics unavailable on this host; executable cases require Linux)\n'
  git -C "$ROOT" diff --check -- deploy/tests/run.sh deploy/tests/mock-command.sh
  exit 0
fi

make_fixture() {
  fixture="$1"
  mkdir -p "$fixture"
  cat >"$fixture/yt-dlp" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
for arg in "$@"; do
  [[ "$arg" == --version ]] && { printf '2026.07.23.234303\n'; exit 0; }
done
if [[ -n "${FAKE_YTDLP_LOG:-}" ]]; then
  printf '%q ' "$@" >>"$FAKE_YTDLP_LOG"
  printf '\n' >>"$FAKE_YTDLP_LOG"
fi
[[ "${FAKE_YTDLP_EXTRACT_FAIL:-false}" != true ]] || exit 1
if [[ "${FAKE_YTDLP_EXTRACT_HANG:-false}" == true ]]; then
  [[ -z "${FAKE_YTDLP_HANG_STARTED:-}" ]] || printf 'started\n' >"$FAKE_YTDLP_HANG_STARTED"
  while :; do sleep 1; done
fi
printf '%s\n' 'https://media.example/audio'
EOF
  chmod 0755 "$fixture/yt-dlp"
  digest="$(sha256sum "$fixture/yt-dlp" | awk '{print $1}')"
  printf '%s  yt-dlp_linux\n' "$digest" >"$fixture/SHA2-256SUMS"
  : >"$fixture/SHA2-256SUMS.sig"
  cat >"$fixture/yt-release.json" <<EOF
{"tag_name":"2026.07.23.234303","assets":[
 {"name":"yt-dlp_linux","browser_download_url":"fixture://yt-dlp"},
 {"name":"SHA2-256SUMS","browser_download_url":"fixture://yt-sums"},
 {"name":"SHA2-256SUMS.sig","browser_download_url":"fixture://yt-signature"}
]}
EOF
}

rewrite_updater() {
  local sandbox="$1" destination="$2"
  # Replacing prefixes, rather than binding mounts, keeps this safe on CI and
  # developer machines alike. The production source is never executed here.
  sed \
    -e "s|/etc/wotoha|$sandbox/etc/wotoha|g" \
    -e "s|/opt/wotoha|$sandbox/opt/wotoha|g" \
    -e "s|/var/lib/wotoha-updater|$sandbox/var/lib/wotoha-updater|g" \
    -e "s|/run/lock|$sandbox/run/lock|g" \
    "$UPDATE" >"$destination"
  chmod 0755 "$destination"
}

prepare_case() {
  case_root="$work/$1"
  sandbox="$case_root/root"
  fixture="$case_root/fixture"
  bin="$case_root/bin"
  mkdir -p "$sandbox/etc/wotoha" "$sandbox/opt/wotoha/bin" \
    "$sandbox/opt/wotoha/yt-dlp" "$sandbox/var/lib/wotoha-updater" \
    "$sandbox/run/lock" "$bin"
  make_fixture "$fixture"
  cp "$ROOT/deploy/yt-dlp-public.key" "$sandbox/etc/wotoha/yt-dlp-public.key"
  cat >"$sandbox/opt/wotoha/bin/deno" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  chmod 0755 "$sandbox/opt/wotoha/bin/deno"
  for command in curl gpg install jq flock mkdir timeout; do ln -s "$MOCK" "$bin/$command"; done
  rewrite_updater "$sandbox" "$case_root/yt-dlp-update"
}

run_case() {
  PATH="$bin:$PATH" FIXTURE_DIR="$fixture" MOCK_CURL_MODE=ytdlp \
    FAKE_YTDLP_LOG="$case_root/yt-dlp.log" "$case_root/yt-dlp-update"
}

old_digest='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
seed_old() {
  mkdir -p "$sandbox/opt/wotoha/yt-dlp/versions/$old_digest"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$sandbox/opt/wotoha/yt-dlp/versions/$old_digest/yt-dlp"
  chmod 0755 "$sandbox/opt/wotoha/yt-dlp/versions/$old_digest/yt-dlp"
  ln -s "versions/$old_digest/yt-dlp" "$sandbox/opt/wotoha/yt-dlp/current"
  ln -s "versions/$old_digest/yt-dlp" "$sandbox/opt/wotoha/yt-dlp/previous"
  printf '%s %s %s\n' yt-dlp/yt-dlp-nightly-builds 2026.01.01.000001 "$old_digest" >"$sandbox/var/lib/wotoha-updater/installed-yt-dlp"
}
assert_old_preserved() {
  [[ "$(readlink "$sandbox/opt/wotoha/yt-dlp/current")" == "versions/$old_digest/yt-dlp" ]] || fail 'current changed on rejected candidate'
  [[ "$(readlink "$sandbox/opt/wotoha/yt-dlp/previous")" == "versions/$old_digest/yt-dlp" ]] || fail 'previous changed on rejected candidate'
  grep -Fqx "yt-dlp/yt-dlp-nightly-builds 2026.01.01.000001 $old_digest" "$sandbox/var/lib/wotoha-updater/installed-yt-dlp" || fail 'state changed on rejected candidate'
}

# A bad key, bad signature, bad checksum and an oversized asset must all fail
# before either pointer or state is touched.
prepare_case fingerprint; seed_old
expect_fail env FAKE_GPG_FINGERPRINT=BAD PATH="$bin:$PATH" FIXTURE_DIR="$fixture" MOCK_CURL_MODE=ytdlp "$case_root/yt-dlp-update"
assert_old_preserved; pass 'wrong GPG fingerprint fails closed'

prepare_case repository; seed_old
expect_fail env WOTOHA_YTDLP_UPDATE_REPOSITORY=untrusted/example PATH="$bin:$PATH" FIXTURE_DIR="$fixture" MOCK_CURL_MODE=ytdlp "$case_root/yt-dlp-update"
assert_old_preserved; pass 'untrusted yt-dlp repository is rejected'

prepare_case tag; seed_old
sed -i 's/2026.07.23.234303/not-a-release-tag/' "$fixture/yt-release.json"
expect_fail run_case
assert_old_preserved; pass 'invalid yt-dlp release tag is rejected'

prepare_case signature; seed_old
expect_fail env FAKE_GPG_VERIFY_STATUS=1 PATH="$bin:$PATH" FIXTURE_DIR="$fixture" MOCK_CURL_MODE=ytdlp "$case_root/yt-dlp-update"
assert_old_preserved; pass 'bad signed checksum fails closed'

prepare_case checksum; seed_old
printf '%064d  yt-dlp_linux\n' 0 >"$fixture/SHA2-256SUMS"
expect_fail run_case
assert_old_preserved; pass 'checksum mismatch fails closed'

prepare_case oversize; seed_old
expect_fail env FAKE_YTDLP_OVERSIZE=true PATH="$bin:$PATH" FIXTURE_DIR="$fixture" MOCK_CURL_MODE=ytdlp "$case_root/yt-dlp-update"
assert_old_preserved; pass 'oversize download is rejected'

prepare_case canary; seed_old
expect_fail env FAKE_DIRECT_FAIL=true PATH="$bin:$PATH" FIXTURE_DIR="$fixture" MOCK_CURL_MODE=ytdlp "$case_root/yt-dlp-update"
assert_old_preserved; pass 'failed extraction/byte canary preserves installed release'

prepare_case timeout; seed_old
expect_fail env FAKE_YTDLP_EXTRACT_HANG=true FAKE_YTDLP_HANG_STARTED="$case_root/hang.started" \
  PATH="$bin:$PATH" FIXTURE_DIR="$fixture" MOCK_CURL_MODE=ytdlp "$case_root/yt-dlp-update"
[[ -s "$case_root/hang.started" ]] || fail 'timeout fixture never entered the extraction hang'
assert_old_preserved; pass 'hung yt-dlp extraction times out without changing installed state'

# Successful canary installs content-addressed payload first, then atomically
# rotates previous/current and records state. Interrupted staging files are
# explicitly discarded and a rerun is harmless.
prepare_case promote; seed_old
touch "$sandbox/opt/wotoha/yt-dlp/.current.new" "$sandbox/opt/wotoha/yt-dlp/.previous.new"
run_case
new_digest="$(sha256sum "$fixture/yt-dlp" | awk '{print $1}')"
grep -F -- '--format' "$case_root/yt-dlp.log" >/dev/null \
  && grep -F -- 'bestaudio' "$case_root/yt-dlp.log" >/dev/null \
  || fail 'yt-dlp canary must explicitly select an audio format'
grep -F -- '--socket-timeout' "$case_root/yt-dlp.log" >/dev/null \
  && grep -F -- '--retries' "$case_root/yt-dlp.log" >/dev/null \
  && grep -F -- '--extractor-retries' "$case_root/yt-dlp.log" >/dev/null \
  || fail 'yt-dlp canary must bound sockets and retries'
[[ "$(readlink "$sandbox/opt/wotoha/yt-dlp/current")" == "versions/$new_digest/yt-dlp" ]] || fail 'new current pointer was not promoted'
[[ "$(readlink "$sandbox/opt/wotoha/yt-dlp/previous")" == "versions/$old_digest/yt-dlp" ]] || fail 'previous pointer was not retained'
[[ -x "$sandbox/opt/wotoha/yt-dlp/versions/$new_digest/yt-dlp" ]] || fail 'candidate was not installed'
grep -Fqx "yt-dlp/yt-dlp-nightly-builds 2026.07.23.234303 $new_digest" "$sandbox/var/lib/wotoha-updater/installed-yt-dlp" || fail 'promotion state was not written'
[[ ! -e "$sandbox/opt/wotoha/yt-dlp/.current.new" && ! -e "$sandbox/opt/wotoha/yt-dlp/.previous.new" ]] || fail 'stale promotion files remain'
run_case
[[ "$(readlink "$sandbox/opt/wotoha/yt-dlp/current")" == "versions/$new_digest/yt-dlp" ]] || fail 'rerun changed promoted pointer'
pass 'atomic promotion, state recording, and interrupted-rerun recovery'

# These are package contracts exercised by the updater test above where it is
# possible. Keep the remaining bridge rules explicit until the Phase-B package
# removes the legacy worker completely.
grep -Fq 'rm -rf /opt/wotoha/workers' "$INSTALL_BUNDLE" && fail 'Phase-A yt-dlp bootstrap must not delete legacy worker'
grep -Fq 'wotoha-youtube-js-worker' "$INSTALL_UBUNTU" || fail 'Phase-A bridge must retain legacy worker support'
grep -Fq 'YOUTUBE_WORKER_SEQUENCE' "$INSTALL_UBUNTU" || fail 'Phase-A bridge must retain worker sequence'
grep -Fxq '2' "$ROOT/deploy/YOUTUBE_WORKER_SEQUENCE" || fail 'Phase-A bridge worker sequence must advance production sequence 1'
grep -Fq 'ytdlp_ready=true' "$APP_UPDATE" || fail 'same-tag updater must require a ready yt-dlp channel'
grep -Fq 'install-yt-dlp-bundle.sh' "$APP_UPDATE" || fail 'general updater must install missing yt-dlp on same app tag'
grep -Fq 'new_worker="$INSTALL_DIR/wotoha-youtube-js-worker"' "$APP_UPDATE" \
  || fail 'Phase-B package without worker must retain a verified installed worker'
grep -Fq 'yt-dlp/yt-dlp-nightly-builds' "$UPDATE" || fail 'nightly repository is not the default'
grep -Fq 'yt-dlp/yt-dlp|yt-dlp/yt-dlp-nightly-builds' "$UPDATE" || fail 'yt-dlp repository allowlist is missing'
grep -Fq '^[0-9]{4}[.][0-9]{2}[.][0-9]{2}([.][0-9]{6})?$' "$UPDATE" || fail 'yt-dlp version tag validation is missing'
grep -Fq 'OnUnitActiveSec=6h' "$ROOT/deploy/yt-dlp-update.timer" || fail 'yt-dlp update timer is not six-hourly'
grep -Fqx 'TimeoutStartSec=5min' "$ROOT/deploy/yt-dlp-update.service" || fail 'yt-dlp update service runtime is not bounded'
grep -Fqx 'TimeoutStartSec=15min' "$ROOT/deploy/wotoha-update.service" || fail 'general update service runtime is not bounded'
pass 'Phase-A bridge retains worker and same-tag update repairs yt-dlp'

git -C "$ROOT" diff --check -- deploy/tests/run.sh deploy/tests/mock-command.sh
printf 'ok - yt-dlp deploy regression suite\n'
