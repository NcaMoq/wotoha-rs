#!/usr/bin/env bash
# Executable regression tests for signed application updates, Phase-B cleanup,
# and the independently updated yt-dlp channel.
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
for cmd in bash sha256sum stat sed awk mktemp tar grep cmp readlink; do require "$cmd"; done
bash -n "$UPDATE" "$INSTALL_BUNDLE" "$APP_UPDATE" "$INSTALL_UBUNTU" "$MOCK" "$0"

# The extraction process and its network operations need independent bounds;
# the service deadline is the final backstop for the whole transaction.
grep -Eq '(^|[[:space:]])timeout([[:space:]]|$)' "$UPDATE" \
  || fail 'yt-dlp canary extraction is not wrapped in a process timeout'
grep -Fq -- '--socket-timeout' "$UPDATE" || fail 'yt-dlp canary socket timeout is missing'
grep -Fq -- '--retries' "$UPDATE" || fail 'yt-dlp canary retry bound is missing'
grep -Fq -- '--extractor-retries' "$UPDATE" || fail 'yt-dlp extractor retry bound is missing'
for remote_script in "$APP_UPDATE" "$UPDATE" "$INSTALL_BUNDLE"; do
  grep -Fq -- '--retry-all-errors' "$remote_script" || fail "retry-all-errors is missing: $remote_script"
  grep -Fq -- '--retry-max-time' "$remote_script" || fail "bounded retry window is missing: $remote_script"
  grep -Fq -- '--connect-timeout' "$remote_script" || fail "connect timeout is missing: $remote_script"
  grep -Fq -- '--remove-on-error' "$remote_script" || fail "partial-download cleanup is missing: $remote_script"
done
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
    -e "s|/etc/wotoha|$sandbox/mock-etc|g" \
    -e "s|/opt/wotoha|$sandbox/mock-opt|g" \
    -e "s|/var/lib/wotoha-updater|$sandbox/mock-updater-state|g" \
    -e "s|/run/lock|$sandbox/mock-locks|g" \
    "$UPDATE" >"$destination"
  chmod 0755 "$destination"
  for production_path in /opt/wotoha /etc/wotoha /var/lib/wotoha-updater /run/lock; do
    ! grep -Fq "$production_path" "$destination" \
      || fail "rewritten updater retained production path: $production_path"
  done
}

prepare_case() {
  case_root="$work/$1"
  sandbox="$case_root/root"
  fixture="$case_root/fixture"
  bin="$case_root/bin"
  fake_etc="$sandbox/mock-etc"
  fake_opt="$sandbox/mock-opt"
  fake_state="$sandbox/mock-updater-state"
  fake_locks="$sandbox/mock-locks"
  mkdir -p "$fake_etc" "$fake_opt/bin" "$fake_opt/yt-dlp" \
    "$fake_state" "$fake_locks" "$bin"
  make_fixture "$fixture"
  cp "$ROOT/deploy/yt-dlp-public.key" "$fake_etc/yt-dlp-public.key"
  cat >"$fake_opt/bin/deno" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  chmod 0755 "$fake_opt/bin/deno"
  cp "$MOCK" "$bin/mock-command"
  chmod 0755 "$bin/mock-command"
  for command in curl gpg install jq flock mkdir timeout; do
    ln -s mock-command "$bin/$command"
  done
  [[ "$(PATH="$bin:$PATH" command -v install)" == "$bin/install" ]] \
    || fail 'mock install does not take precedence in PATH'
  [[ -x "$bin/install" ]] || fail 'mock install is not executable'
  rewrite_updater "$sandbox" "$case_root/yt-dlp-update"
}

run_case() {
  PATH="$bin:$PATH" FIXTURE_DIR="$fixture" MOCK_CURL_MODE=ytdlp \
    FAKE_YTDLP_LOG="$case_root/yt-dlp.log" "$case_root/yt-dlp-update"
}

old_digest='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
seed_old() {
  mkdir -p "$fake_opt/yt-dlp/versions/$old_digest"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$fake_opt/yt-dlp/versions/$old_digest/yt-dlp"
  chmod 0755 "$fake_opt/yt-dlp/versions/$old_digest/yt-dlp"
  ln -s "versions/$old_digest/yt-dlp" "$fake_opt/yt-dlp/current"
  ln -s "versions/$old_digest/yt-dlp" "$fake_opt/yt-dlp/previous"
  printf '%s %s %s\n' yt-dlp/yt-dlp-nightly-builds 2026.01.01.000001 "$old_digest" >"$fake_state/installed-yt-dlp"
}
assert_old_preserved() {
  [[ "$(readlink "$fake_opt/yt-dlp/current")" == "versions/$old_digest/yt-dlp" ]] || fail 'current changed on rejected candidate'
  [[ "$(readlink "$fake_opt/yt-dlp/previous")" == "versions/$old_digest/yt-dlp" ]] || fail 'previous changed on rejected candidate'
  grep -Fqx "yt-dlp/yt-dlp-nightly-builds 2026.01.01.000001 $old_digest" "$fake_state/installed-yt-dlp" || fail 'state changed on rejected candidate'
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
touch "$fake_opt/yt-dlp/.current.new" "$fake_opt/yt-dlp/.previous.new"
run_case
new_digest="$(sha256sum "$fixture/yt-dlp" | awk '{print $1}')"
grep -F -- '--format' "$case_root/yt-dlp.log" >/dev/null \
  && grep -F -- 'bestaudio' "$case_root/yt-dlp.log" >/dev/null \
  || fail 'yt-dlp canary must explicitly select an audio format'
grep -F -- '--socket-timeout' "$case_root/yt-dlp.log" >/dev/null \
  && grep -F -- '--retries' "$case_root/yt-dlp.log" >/dev/null \
  && grep -F -- '--extractor-retries' "$case_root/yt-dlp.log" >/dev/null \
  || fail 'yt-dlp canary must bound sockets and retries'
[[ "$(readlink "$fake_opt/yt-dlp/current")" == "versions/$new_digest/yt-dlp" ]] || fail 'new current pointer was not promoted'
[[ "$(readlink "$fake_opt/yt-dlp/previous")" == "versions/$old_digest/yt-dlp" ]] || fail 'previous pointer was not retained'
[[ -x "$fake_opt/yt-dlp/versions/$new_digest/yt-dlp" ]] || fail 'candidate was not installed'
grep -Fqx "yt-dlp/yt-dlp-nightly-builds 2026.07.23.234303 $new_digest" "$fake_state/installed-yt-dlp" || fail 'promotion state was not written'
[[ ! -e "$fake_opt/yt-dlp/.current.new" && ! -e "$fake_opt/yt-dlp/.previous.new" ]] || fail 'stale promotion files remain'
run_case
[[ "$(readlink "$fake_opt/yt-dlp/current")" == "versions/$new_digest/yt-dlp" ]] || fail 'rerun changed promoted pointer'
pass 'atomic promotion, state recording, and interrupted-rerun recovery'

# The final general updater is exercised against a complete workerless release
# archive. Like the yt-dlp cases above, all absolute paths are rewritten into a
# disposable root before the script is run.
rewrite_general_updater() {
  local sandbox="$1" destination="$2"
  sed \
    -e "s|/etc/wotoha|$sandbox/mock-etc|g" \
    -e "s|/opt/wotoha|$sandbox/mock-opt|g" \
    -e "s|/var/lib/wotoha-updater|$sandbox/mock-updater-state|g" \
    -e "s|/var/lib/wotoha|$sandbox/mock-app-state|g" \
    -e "s|/run/lock|$sandbox/mock-locks|g" \
    "$APP_UPDATE" >"$destination"
  chmod 0755 "$destination"
  for production_path in /opt/wotoha /etc/wotoha /var/lib/wotoha-updater /var/lib/wotoha /run/lock; do
    ! grep -Fq "$production_path" "$destination" \
      || fail "rewritten general updater retained production path: $production_path"
  done
}

make_general_fixture() {
  local tag="$1" package="$fixture/wotoha-ubuntu-x86_64-musl" archive_digest
  mkdir -p "$package/bin" "$package/third-party"
  cat >"$package/bin/wotoha-app" <<'EOF'
#!/usr/bin/env bash
printf 'phase-b-final\n'
EOF
  cat >"$package/third-party/yt-dlp" <<'EOF'
#!/usr/bin/env bash
printf '2026.07.23.234303\n'
EOF
  cat >"$package/third-party/deno" <<'EOF'
#!/usr/bin/env bash
printf 'deno 2.4.5\n'
EOF
  chmod 0755 "$package/bin/wotoha-app" "$package/third-party/yt-dlp" "$package/third-party/deno"
  cp "$APP_UPDATE" "$package/wotoha-update.sh"
  chmod 0755 "$package/wotoha-update.sh"
  cat >"$package/install-yt-dlp-bundle.sh" <<EOF
#!/usr/bin/env bash
set -euo pipefail
[[ "\${FAKE_BUNDLE_STATUS:-0}" == 0 ]] || exit "\$FAKE_BUNDLE_STATUS"
package="\${1:?missing package}"
install_dir="$sandbox/mock-opt/bin"
ytdlp_root="$sandbox/mock-opt/yt-dlp"
state_dir="$sandbox/mock-updater-state"
digest="\$(sha256sum "\$package/third-party/yt-dlp" | awk '{print \$1}')"
mkdir -p "\$install_dir" "\$ytdlp_root/versions/\$digest" "\$state_dir"
cp "\$package/third-party/yt-dlp" "\$ytdlp_root/versions/\$digest/yt-dlp"
chmod 0755 "\$ytdlp_root/versions/\$digest/yt-dlp"
ln -sfn "versions/\$digest/yt-dlp" "\$ytdlp_root/current"
ln -sfn ../yt-dlp/current "\$install_dir/yt-dlp"
cp "\$package/third-party/deno" "\$install_dir/deno"
chmod 0755 "\$install_dir/deno"
printf '%s %s %s\n' yt-dlp/yt-dlp-nightly-builds 2026.07.23.234303 "\$digest" >"\$state_dir/installed-yt-dlp"
EOF
  chmod 0755 "$package/install-yt-dlp-bundle.sh"
  (cd "$package" && sha256sum bin/wotoha-app third-party/yt-dlp third-party/deno > SHA256SUMS.txt)
  tar -czf "$fixture/release.tar.gz" -C "$fixture" wotoha-ubuntu-x86_64-musl
  archive_digest="$(sha256sum "$fixture/release.tar.gz" | awk '{print $1}')"
  printf '%s  %s\n' "$archive_digest" wotoha-ubuntu-x86_64-musl.tar.gz >"$fixture/release.tar.gz.sha256"
  printf '{"schema_version":1,"tag":"%s","commit":"%040d","asset":"wotoha-ubuntu-x86_64-musl.tar.gz","sha256":"%s"}\n' \
    "$tag" 1 "$archive_digest" >"$fixture/release.tar.gz.manifest.json"
  printf '{"attestation":"fixture"}\n' >"$fixture/release.tar.gz.attestation.jsonl"
  printf '{"tag_name":"%s","assets":[]}\n' "$tag" >"$fixture/full-release.json"
}

prepare_general_case() {
  case_root="$work/general-$1"
  sandbox="$case_root/root"
  fixture="$case_root/fixture"
  bin="$case_root/bin"
  fake_etc="$sandbox/mock-etc"
  fake_opt="$sandbox/mock-opt"
  fake_state="$sandbox/mock-updater-state"
  fake_app_state="$sandbox/mock-app-state"
  fake_locks="$sandbox/mock-locks"
  service_state="$case_root/service-state"
  mock_log="$case_root/mock.log"
  mkdir -p "$fake_etc" "$fake_opt/bin" "$fake_state" "$fake_app_state" "$fake_locks" "$fixture" "$bin"
  rewrite_general_updater "$sandbox" "$case_root/wotoha-update"
  make_general_fixture v0.5.31
  cp "$MOCK" "$bin/mock-command"
  chmod 0755 "$bin/mock-command"
  for command in curl gh jq systemctl flock timeout install sleep; do
    ln -s mock-command "$bin/$command"
  done
  printf 'active\n' >"$service_state"
}

seed_legacy_state() {
  mkdir -p "$fake_opt/workers/native" "$fake_app_state/youtube-worker-ack" "$fake_state"
  printf 'legacy-worker\n' >"$fake_opt/bin/wotoha-youtube-js-worker"
  printf 'legacy-worker-new\n' >"$fake_opt/bin/wotoha-youtube-js-worker.new"
  printf 'legacy-worker-previous\n' >"$fake_opt/bin/wotoha-youtube-js-worker.previous"
  printf 'sequence\n' >"$fake_opt/YOUTUBE_WORKER_SEQUENCE"
  printf 'sequence-new\n' >"$fake_opt/YOUTUBE_WORKER_SEQUENCE.new"
  printf 'ack\n' >"$fake_app_state/.youtube-worker-ack-1.tmp"
  printf 'worker-state\n' >"$fake_state/installed-youtube-worker"
  printf 'worker-state-new\n' >"$fake_state/installed-youtube-worker.new.1"
  printf 'candidate\n' >"$fake_state/youtube-worker-candidate-tag"
  printf 'candidate-new\n' >"$fake_state/youtube-worker-candidate-tag.new.1"
  printf '{}\n' >"$fake_etc/youtube-clients.json"
  cat >"$fake_etc/wotoha.env" <<EOF
WOTOHA_YOUTUBE_JS_WORKER=$fake_opt/bin/wotoha-youtube-js-worker
WOTOHA_YOUTUBE_JS_WORKER_DIR=$fake_opt/workers
WOTOHA_YOUTUBE_JS_WORKER_ACK=$fake_app_state/youtube-worker-ack
WOTOHA_YOUTUBE_CLIENTS_FILE=$fake_etc/youtube-clients.json
WOTOHA_YOUTUBE_PO_TOKEN_PROVIDER=legacy
WOTOHA_YOUTUBE_PO_TOKEN_TIMEOUT_SECONDS=10
WOTOHA_YOUTUBE_OPERATOR_NOTE=keep
WOTOHA_YTDLP_PATH=/custom/yt-dlp
WOTOHA_DENO_PATH=/custom/deno
WOTOHA_YTDLP_COOKIES_FILE=$fake_etc/cookies.txt
EOF
  printf 'cookies\n' >"$fake_etc/cookies.txt"
  printf -- '--cookies %s\n' "$fake_etc/cookies.txt" >"$fake_etc/yt-dlp.conf"
}

seed_old_application() {
  cat >"$fake_opt/bin/wotoha-app" <<'EOF'
#!/usr/bin/env bash
printf 'phase-a-old\n'
EOF
  chmod 0755 "$fake_opt/bin/wotoha-app"
  printf 'v0.5.30\n' >"$fake_state/installed-release"
}

seed_final_application() {
  cp "$fixture/wotoha-ubuntu-x86_64-musl/bin/wotoha-app" "$fake_opt/bin/wotoha-app"
  chmod 0755 "$fake_opt/bin/wotoha-app"
  printf 'v0.5.31\n' >"$fake_state/installed-release"
}

seed_valid_ytdlp() {
  local digest
  digest="$(sha256sum "$fixture/wotoha-ubuntu-x86_64-musl/third-party/yt-dlp" | awk '{print $1}')"
  mkdir -p "$fake_opt/yt-dlp/versions/$digest"
  cp "$fixture/wotoha-ubuntu-x86_64-musl/third-party/yt-dlp" "$fake_opt/yt-dlp/versions/$digest/yt-dlp"
  chmod 0755 "$fake_opt/yt-dlp/versions/$digest/yt-dlp"
  ln -s "versions/$digest/yt-dlp" "$fake_opt/yt-dlp/current"
  ln -s ../yt-dlp/current "$fake_opt/bin/yt-dlp"
  cp "$fixture/wotoha-ubuntu-x86_64-musl/third-party/deno" "$fake_opt/bin/deno"
  chmod 0755 "$fake_opt/bin/deno"
  printf '%s %s %s\n' yt-dlp/yt-dlp-nightly-builds 2026.07.23.234303 "$digest" >"$fake_state/installed-yt-dlp"
}

run_general_case() {
  PATH="$bin:$PATH" FIXTURE_DIR="$fixture" MOCK_CURL_MODE=general \
    MOCK_LOG="$mock_log" FAKE_SERVICE_STATE_FILE="$service_state" \
    "$case_root/wotoha-update"
}

assert_legacy_present() {
  [[ -e "$fake_opt/bin/wotoha-youtube-js-worker" ]] || fail 'legacy worker was removed before the safe gate'
  [[ -e "$fake_state/installed-youtube-worker" ]] || fail 'legacy worker state was removed before the safe gate'
  [[ -e "$fake_etc/youtube-clients.json" ]] || fail 'legacy client profile was removed before the safe gate'
}

assert_legacy_cleaned_and_custom_preserved() {
  local legacy
  for legacy in \
    "$fake_opt/bin/wotoha-youtube-js-worker" \
    "$fake_opt/bin/wotoha-youtube-js-worker.new" \
    "$fake_opt/bin/wotoha-youtube-js-worker.previous" \
    "$fake_opt/workers" \
    "$fake_opt/YOUTUBE_WORKER_SEQUENCE" \
    "$fake_opt/YOUTUBE_WORKER_SEQUENCE.new" \
    "$fake_app_state/youtube-worker-ack" \
    "$fake_app_state/.youtube-worker-ack-1.tmp" \
    "$fake_state/installed-youtube-worker" \
    "$fake_state/installed-youtube-worker.new.1" \
    "$fake_state/youtube-worker-candidate-tag" \
    "$fake_state/youtube-worker-candidate-tag.new.1" \
    "$fake_etc/youtube-clients.json"; do
    [[ ! -e "$legacy" && ! -L "$legacy" ]] || fail "legacy path was not cleaned: $legacy"
  done
  ! grep -Eq '^WOTOHA_YOUTUBE_(JS_WORKER|JS_WORKER_DIR|JS_WORKER_ACK|CLIENTS_FILE|PO_TOKEN_PROVIDER|PO_TOKEN_TIMEOUT_SECONDS)=' "$fake_etc/wotoha.env" \
    || fail 'legacy environment keys were not cleaned'
  grep -Fqx 'WOTOHA_YOUTUBE_OPERATOR_NOTE=keep' "$fake_etc/wotoha.env" || fail 'unknown operator setting was changed'
  grep -Fqx 'WOTOHA_YTDLP_PATH=/custom/yt-dlp' "$fake_etc/wotoha.env" || fail 'custom yt-dlp path was changed'
  grep -Fqx 'WOTOHA_DENO_PATH=/custom/deno' "$fake_etc/wotoha.env" || fail 'custom Deno path was changed'
  grep -Fqx "WOTOHA_YTDLP_COOKIES_FILE=$fake_etc/cookies.txt" "$fake_etc/wotoha.env" || fail 'cookie setting was changed'
  [[ -f "$fake_etc/cookies.txt" && -f "$fake_etc/yt-dlp.conf" ]] || fail 'cookie or yt-dlp config file was removed'
}

prepare_general_case normal; seed_old_application; seed_legacy_state
run_general_case
[[ "$("$fake_opt/bin/wotoha-app")" == phase-b-final ]] || fail 'final app was not installed'
grep -Fqx v0.5.31 "$fake_state/installed-release" || fail 'final release state was not recorded'
[[ -s "$fake_state/installed-app-sha256" ]] || fail 'final app digest was not recorded'
assert_legacy_present
[[ "$(grep -c '^systemctl restart wotoha.service$' "$mock_log")" == 1 ]] || fail 'normal app update did not restart exactly once'
run_general_case
assert_legacy_cleaned_and_custom_preserved
[[ "$(grep -c '^systemctl restart wotoha.service$' "$mock_log")" == 1 ]] || fail 'same-tag cleanup restarted the live application'
run_general_case
assert_legacy_cleaned_and_custom_preserved
[[ "$(grep -c '^systemctl restart wotoha.service$' "$mock_log")" == 1 ]] || fail 'idempotent rerun restarted the live application'
pass 'normal update preserves legacy state until same-tag proof, then cleanup is idempotent without restart'

prepare_general_case ytdlp-failure; seed_old_application; seed_legacy_state
expect_fail env PATH="$bin:$PATH" FIXTURE_DIR="$fixture" MOCK_CURL_MODE=general MOCK_LOG="$mock_log" \
  FAKE_SERVICE_STATE_FILE="$service_state" FAKE_BUNDLE_STATUS=42 "$case_root/wotoha-update"
assert_legacy_present
grep -Fqx v0.5.30 "$fake_state/installed-release" || fail 'yt-dlp failure changed release state'
[[ "$("$fake_opt/bin/wotoha-app")" == phase-a-old ]] || fail 'yt-dlp failure changed the application'
pass 'yt-dlp bootstrap failure preserves the app and all legacy state'

prepare_general_case app-failure; seed_old_application; seed_legacy_state
expect_fail env PATH="$bin:$PATH" FIXTURE_DIR="$fixture" MOCK_CURL_MODE=general MOCK_LOG="$mock_log" \
  FAKE_SERVICE_STATE_FILE="$service_state" FAKE_RESTART_FAIL=true "$case_root/wotoha-update"
assert_legacy_present
grep -Fqx v0.5.30 "$fake_state/installed-release" || fail 'failed app activation changed release state'
[[ "$("$fake_opt/bin/wotoha-app")" == phase-a-old ]] || fail 'failed app activation did not roll back'
pass 'failed app activation rolls back and preserves all legacy state'

prepare_general_case bridge-v0.5.31; seed_final_application; seed_legacy_state
run_general_case
assert_legacy_cleaned_and_custom_preserved
[[ -s "$fake_state/installed-app-sha256" ]] || fail 'bridge-to-final run did not record app proof'
[[ ! -e "$mock_log" ]] || ! grep -q '^systemctl restart wotoha.service$' "$mock_log" \
  || fail 'bridge-to-final same-tag cleanup restarted the application'
pass 'v0.5.31 Phase-A bridge reaches workerless final state on the next signed same-tag run'

prepare_general_case failed-service; seed_final_application; seed_valid_ytdlp; seed_legacy_state
sha256sum "$fake_opt/bin/wotoha-app" | awk '{print $1}' >"$fake_state/installed-app-sha256"
printf 'failed\n' >"$service_state"
expect_fail run_general_case
assert_legacy_present
pass 'failed service health blocks same-tag legacy cleanup'

prepare_general_case inactive-service; seed_final_application; seed_valid_ytdlp; seed_legacy_state
sha256sum "$fake_opt/bin/wotoha-app" | awk '{print $1}' >"$fake_state/installed-app-sha256"
printf 'inactive\n' >"$service_state"
run_general_case
assert_legacy_cleaned_and_custom_preserved
[[ ! -e "$mock_log" ]] || ! grep -q '^systemctl restart wotoha.service$' "$mock_log" \
  || fail 'intentional inactivity was changed during cleanup'
run_general_case
assert_legacy_cleaned_and_custom_preserved
pass 'signed same-tag proof allows idempotent cleanup while preserving intentional inactivity'

archive_listing="$work/workerless-archive.txt"
tar -tzf "$fixture/release.tar.gz" >"$archive_listing"
! grep -Eq '(wotoha-youtube-js-worker|YOUTUBE_WORKER_SEQUENCE|youtube-clients[.]json|workers/)' "$archive_listing" \
  || fail 'Phase-B archive still contains native worker assets'
(cd "$fixture/wotoha-ubuntu-x86_64-musl" && sha256sum --check SHA256SUMS.txt)
pass 'Phase-B archive is workerless and its application/yt-dlp/Deno hashes verify'

! grep -Fq 'releases?per_page=30' "$APP_UPDATE" || fail 'general updater still polls worker releases'
[[ "$(grep -c 'youtube-worker-candidate-tag' "$APP_UPDATE")" == 2 ]] \
  || fail 'worker candidate state is used outside the two bounded cleanup paths'
! grep -Eq '(worker_release|worker_candidate|installed_worker|new_worker=|new_worker_digest)' "$APP_UPDATE" \
  || fail 'general updater retains worker polling or candidate logic'
grep -Fq 'install-yt-dlp-bundle.sh' "$APP_UPDATE" || fail 'general updater does not bootstrap managed yt-dlp'
! grep -Fq 'wotoha-youtube-js-worker' "$INSTALL_UBUNTU" || fail 'fresh installer still installs the native worker'
grep -Fq 'yt-dlp/yt-dlp-nightly-builds' "$UPDATE" || fail 'nightly repository is not the default'
grep -Fq 'yt-dlp/yt-dlp|yt-dlp/yt-dlp-nightly-builds' "$UPDATE" || fail 'yt-dlp repository allowlist is missing'
grep -Fq '^[0-9]{4}[.][0-9]{2}[.][0-9]{2}([.][0-9]{6})?$' "$UPDATE" || fail 'yt-dlp version tag validation is missing'
grep -Fq 'OnUnitActiveSec=6h' "$ROOT/deploy/yt-dlp-update.timer" || fail 'yt-dlp update timer is not six-hourly'
grep -Fqx 'TimeoutStartSec=5min' "$ROOT/deploy/yt-dlp-update.service" || fail 'yt-dlp update service runtime is not bounded'
grep -Fqx 'TimeoutStartSec=15min' "$ROOT/deploy/wotoha-update.service" || fail 'general update service runtime is not bounded'
pass 'final deploy contract keeps only the independent managed yt-dlp channel'

git -C "$ROOT" diff --check -- deploy/tests/run.sh deploy/tests/mock-command.sh
printf 'ok - yt-dlp deploy regression suite\n'
