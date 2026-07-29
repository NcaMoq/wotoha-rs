#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPOSITORY_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TEST_ROOT="$(mktemp -d)"
trap 'rm -rf "$TEST_ROOT"' EXIT

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

assert_equal() {
  local expected="$1"
  local actual="$2"
  local message="$3"
  [[ "$expected" == "$actual" ]] \
    || fail "$message (expected=$expected actual=$actual)"
}

hash_text() {
  printf '%s' "$1" | sha256sum | awk '{print $1}'
}

file_snapshot() {
  sha256sum "$@" | sha256sum | awk '{print $1}'
}

write_executable() {
  local destination="$1"
  local label="$2"
  mkdir -p "$(dirname "$destination")"
  printf '#!/usr/bin/env sh\nexit 0\n# %s\n' "$label" > "$destination"
  chmod 0755 "$destination"
}

install_mocks() {
  local root="$1"
  local mock_bin="$root/mock-bin"
  mkdir -p "$mock_bin"
  install -m 0755 "$SCRIPT_DIR/mock-command.sh" "$mock_bin/mock-command"
  local command_name
  for command_name in curl gh systemctl getent id chown groupadd useradd; do
    ln -s "$mock_bin/mock-command" "$mock_bin/$command_name"
  done
}

rewrite_deploy_script() {
  local source="$1"
  local destination="$2"
  local root="$3"
  sed \
    -e 's#/var/lib/wotoha-updater#__TEST_VAR_LIB_UPDATER__#g' \
    -e 's#/var/lib/wotoha#__TEST_VAR_LIB_WOTOHA__#g' \
    -e 's#/var/log/wotoha#__TEST_VAR_LOG_WOTOHA__#g' \
    -e 's#/etc/systemd/system#__TEST_ETC_SYSTEMD__#g' \
    -e 's#/etc/wotoha#__TEST_ETC_WOTOHA__#g' \
    -e 's#/opt/wotoha#__TEST_OPT_WOTOHA__#g' \
    -e 's#/run/lock#__TEST_RUN_LOCK__#g' \
    -e "s#__TEST_VAR_LIB_UPDATER__#$root/var/lib/wotoha-updater#g" \
    -e "s#__TEST_VAR_LIB_WOTOHA__#$root/var/lib/wotoha#g" \
    -e "s#__TEST_VAR_LOG_WOTOHA__#$root/var/log/wotoha#g" \
    -e "s#__TEST_ETC_SYSTEMD__#$root/etc/systemd/system#g" \
    -e "s#__TEST_ETC_WOTOHA__#$root/etc/wotoha#g" \
    -e "s#__TEST_OPT_WOTOHA__#$root/opt/wotoha#g" \
    -e "s#__TEST_RUN_LOCK__#$root/run/lock#g" \
    -e 's/-o root -g root //g' \
    -e 's/-o wotoha -g wotoha //g' \
    "$source" > "$destination"
  chmod 0755 "$destination"
}

write_worker_state() {
  local destination="$1"
  local sequence="$2"
  local digest="$3"
  local tag="$4"
  mkdir -p "$(dirname "$destination")"
  jq --null-input \
    --argjson sequence "$sequence" \
    --arg sha256 "$digest" \
    --arg tag "$tag" \
    '{sequence: $sequence, sha256: $sha256, tag: $tag}' > "$destination"
}

prepare_update_root() {
  local root="$1"
  local sequence="$2"
  mkdir -p \
    "$root/etc/wotoha" \
    "$root/opt/wotoha/bin" \
    "$root/opt/wotoha/workers/versions" \
    "$root/run/lock" \
    "$root/var/lib/wotoha" \
    "$root/var/lib/wotoha-updater"

  local current_worker="$root/current-worker"
  write_executable "$current_worker" "current-$sequence"
  CURRENT_DIGEST="$(sha256sum "$current_worker" | awk '{print $1}')"
  local installed_worker="$root/opt/wotoha/workers/versions/$CURRENT_DIGEST/wotoha-youtube-js-worker"
  mkdir -p "$(dirname "$installed_worker")"
  cp "$current_worker" "$installed_worker"
  chmod 0755 "$installed_worker"

  printf '%s\n' "$CURRENT_DIGEST" > "$root/opt/wotoha/workers/current"
  printf '%s\n' "$sequence" > "$root/opt/wotoha/YOUTUBE_WORKER_SEQUENCE"
  write_worker_state \
    "$root/var/lib/wotoha-updater/installed-youtube-worker" \
    "$sequence" "$CURRENT_DIGEST" "youtube-worker-v${sequence}.0"
  printf '%s\n' 'v-test' > "$root/var/lib/wotoha-updater/installed-release"
  printf '%s\n' \
    'WOTOHA_UPDATE_REPOSITORY=test/repository' \
    'WOTOHA_UPDATE_GITHUB_TOKEN=' \
    'WOTOHA_UPDATE_YOUTUBE_CLIENTS=false' \
    'WOTOHA_UPDATE_YOUTUBE_WORKER=true' \
    > "$root/etc/wotoha/wotoha-update.env"
  printf '%s\n' \
    'WOTOHA_YOUTUBE_JS_WORKER_DIR=/unused-in-test' \
    'WOTOHA_YOUTUBE_JS_WORKER_ACK=/unused-in-test' \
    > "$root/etc/wotoha/wotoha.env"

  rewrite_deploy_script \
    "$REPOSITORY_ROOT/deploy/wotoha-update.sh" \
    "$root/wotoha-update.sh" \
    "$root"
  install_mocks "$root"
}

prepare_release_fixture() {
  local fixture="$1"
  local tag="$2"
  local sequence="$3"
  local advertised_worker_size="${4:-}"
  mkdir -p "$fixture"
  write_executable "$fixture/worker" "candidate-$sequence"
  FIXTURE_WORKER_DIGEST="$(sha256sum "$fixture/worker" | awk '{print $1}')"
  printf '%s\n' '{}' > "$fixture/worker.attestation.jsonl"
  jq --null-input \
    --argjson sequence "$sequence" \
    --arg tag "$tag" \
    --arg sha256 "$FIXTURE_WORKER_DIGEST" \
    '{
      schema_version: 1,
      sequence: $sequence,
      tag: $tag,
      protocol_version: 1,
      target: "x86_64-unknown-linux-musl",
      asset: "wotoha-youtube-js-worker-x86_64-musl",
      sha256: $sha256,
      commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }' > "$fixture/worker.manifest.json"

  local worker_size manifest_size attestation_size
  worker_size="${advertised_worker_size:-$(stat --format='%s' "$fixture/worker")}"
  manifest_size="$(stat --format='%s' "$fixture/worker.manifest.json")"
  attestation_size="$(stat --format='%s' "$fixture/worker.attestation.jsonl")"
  jq --null-input \
    --arg tag "$tag" \
    --argjson worker_size "$worker_size" \
    --argjson manifest_size "$manifest_size" \
    --argjson attestation_size "$attestation_size" \
    '[{
      draft: false,
      prerelease: true,
      tag_name: $tag,
      assets: [
        {
          name: "wotoha-youtube-js-worker-x86_64-musl",
          size: $worker_size,
          url: "fixture://worker"
        },
        {
          name: "wotoha-youtube-js-worker-x86_64-musl.manifest.json",
          size: $manifest_size,
          url: "fixture://worker-manifest"
        },
        {
          name: "wotoha-youtube-js-worker-x86_64-musl.attestation.jsonl",
          size: $attestation_size,
          url: "fixture://worker-attestation"
        }
      ]
    }]' > "$fixture/worker-releases.json"
  printf '%s\n' '{"tag_name":"v-test","assets":[]}' > "$fixture/full-release.json"
}

run_update() {
  local root="$1"
  local fixture="$2"
  local gh_status="$3"
  PATH="$root/mock-bin:$PATH" \
    FIXTURE_DIR="$fixture" \
    MOCK_LOG="$root/mock.log" \
    FAKE_GH_VERIFY_STATUS="$gh_status" \
    bash "$root/wotoha-update.sh"
}

test_bash_syntax() {
  bash -n \
    "$REPOSITORY_ROOT/deploy/wotoha-update.sh" \
    "$REPOSITORY_ROOT/deploy/install-ubuntu.sh" \
    "$SCRIPT_DIR/mock-command.sh" \
    "$SCRIPT_DIR/run.sh"
  printf '%s\n' 'ok - bash syntax'
}

test_attestation_failure_preserves_state() {
  local root="$TEST_ROOT/attestation-failure"
  local fixture="$root/fixtures"
  prepare_update_root "$root" 1
  prepare_release_fixture "$fixture" "youtube-worker-v3.0" 3

  local existing_candidate
  existing_candidate="$(hash_text existing-candidate)"
  printf '%s\n' "$existing_candidate" > "$root/opt/wotoha/workers/candidate"
  write_worker_state \
    "$root/var/lib/wotoha-updater/youtube-worker-candidate-tag" \
    2 "$existing_candidate" "youtube-worker-v2.0"
  printf '%s\n' "$(hash_text mismatched-ack)" > "$root/var/lib/wotoha/youtube-worker-ack"

  local before after
  before="$(file_snapshot \
    "$root/opt/wotoha/workers/current" \
    "$root/opt/wotoha/workers/candidate" \
    "$root/opt/wotoha/YOUTUBE_WORKER_SEQUENCE" \
    "$root/var/lib/wotoha-updater/installed-youtube-worker" \
    "$root/var/lib/wotoha-updater/youtube-worker-candidate-tag" \
    "$root/var/lib/wotoha/youtube-worker-ack")"

  if run_update "$root" "$fixture" 42 > "$root/output.log" 2>&1; then
    fail "attestation failure unexpectedly succeeded"
  fi
  after="$(file_snapshot \
    "$root/opt/wotoha/workers/current" \
    "$root/opt/wotoha/workers/candidate" \
    "$root/opt/wotoha/YOUTUBE_WORKER_SEQUENCE" \
    "$root/var/lib/wotoha-updater/installed-youtube-worker" \
    "$root/var/lib/wotoha-updater/youtube-worker-candidate-tag" \
    "$root/var/lib/wotoha/youtube-worker-ack")"
  assert_equal "$before" "$after" "attestation failure changed worker state"
  [[ ! -e "$root/opt/wotoha/workers/versions/$FIXTURE_WORKER_DIGEST" ]] \
    || fail "attestation failure installed the candidate"
  printf '%s\n' 'ok - attestation failure is fail-closed'
}

test_sequence_downgrade_is_rejected() {
  local root="$TEST_ROOT/sequence-downgrade"
  local fixture="$root/fixtures"
  prepare_update_root "$root" 5
  prepare_release_fixture "$fixture" "youtube-worker-v4.0" 4

  local before after
  before="$(file_snapshot \
    "$root/opt/wotoha/workers/current" \
    "$root/opt/wotoha/YOUTUBE_WORKER_SEQUENCE" \
    "$root/var/lib/wotoha-updater/installed-youtube-worker")"
  run_update "$root" "$fixture" 0 > "$root/output.log" 2>&1 \
    || fail "downgrade check failed instead of safely skipping"
  after="$(file_snapshot \
    "$root/opt/wotoha/workers/current" \
    "$root/opt/wotoha/YOUTUBE_WORKER_SEQUENCE" \
    "$root/var/lib/wotoha-updater/installed-youtube-worker")"
  assert_equal "$before" "$after" "downgrade changed installed state"
  [[ ! -e "$root/opt/wotoha/workers/candidate" ]] \
    || fail "downgrade staged a candidate"
  [[ ! -e "$root/opt/wotoha/workers/versions/$FIXTURE_WORKER_DIGEST" ]] \
    || fail "downgrade installed a candidate version"
  printf '%s\n' 'ok - sequence downgrade is rejected'
}

test_oversize_worker_is_rejected() {
  local root="$TEST_ROOT/oversize"
  local fixture="$root/fixtures"
  prepare_update_root "$root" 1
  prepare_release_fixture "$fixture" "youtube-worker-v2.0" 2 "$((128 * 1024 * 1024 + 1))"

  local before after
  before="$(file_snapshot \
    "$root/opt/wotoha/workers/current" \
    "$root/opt/wotoha/YOUTUBE_WORKER_SEQUENCE" \
    "$root/var/lib/wotoha-updater/installed-youtube-worker")"
  if run_update "$root" "$fixture" 0 > "$root/output.log" 2>&1; then
    fail "oversize worker unexpectedly succeeded"
  fi
  after="$(file_snapshot \
    "$root/opt/wotoha/workers/current" \
    "$root/opt/wotoha/YOUTUBE_WORKER_SEQUENCE" \
    "$root/var/lib/wotoha-updater/installed-youtube-worker")"
  assert_equal "$before" "$after" "oversize worker changed installed state"
  [[ ! -e "$root/opt/wotoha/workers/candidate" ]] \
    || fail "oversize worker staged a candidate"
  if grep -Fq 'curl fixture://worker' "$root/mock.log"; then
    fail "oversize worker was downloaded before rejection"
  fi
  printf '%s\n' 'ok - oversize worker is rejected before download'
}

prepare_installer_package() {
  local package="$1"
  local root="$2"
  mkdir -p "$package/bin" "$package/deploy"
  rewrite_deploy_script \
    "$REPOSITORY_ROOT/deploy/install-ubuntu.sh" \
    "$package/install-ubuntu.sh" \
    "$root"
  write_executable "$package/bin/wotoha-app" package-app
  write_executable "$package/bin/wotoha-youtube-js-worker" package-worker
  cp "$REPOSITORY_ROOT/deploy/wotoha-update.sh" "$package/wotoha-update.sh"
  cp \
    "$REPOSITORY_ROOT/deploy/wotoha.service" \
    "$REPOSITORY_ROOT/deploy/wotoha-update.service" \
    "$REPOSITORY_ROOT/deploy/wotoha-update.timer" \
    "$REPOSITORY_ROOT/deploy/youtube-clients.json" \
    "$REPOSITORY_ROOT/deploy/wotoha.env.example" \
    "$REPOSITORY_ROOT/deploy/wotoha-update.env.example" \
    "$REPOSITORY_ROOT/deploy/YOUTUBE_WORKER_SEQUENCE" \
    "$package/deploy/"
  printf '%s\n' 'v-test' > "$package/RELEASE_VERSION"
}

test_installer_rerun_preserves_worker_state() {
  local root="$TEST_ROOT/installer-rerun"
  local package="$root/package"
  mkdir -p \
    "$root/etc/systemd/system" \
    "$root/opt/wotoha/workers/versions" \
    "$root/var/lib/wotoha" \
    "$root/var/lib/wotoha-updater"
  install_mocks "$root"
  prepare_installer_package "$package" "$root"

  local current_source="$root/current-worker"
  write_executable "$current_source" installed-current
  local current_digest
  current_digest="$(sha256sum "$current_source" | awk '{print $1}')"
  local current_installed="$root/opt/wotoha/workers/versions/$current_digest/wotoha-youtube-js-worker"
  mkdir -p "$(dirname "$current_installed")"
  cp "$current_source" "$current_installed"
  chmod 0755 "$current_installed"

  local candidate_digest
  candidate_digest="$(hash_text pending-candidate)"
  printf '%s\n' "$current_digest" > "$root/opt/wotoha/workers/current"
  printf '%s\n' "$candidate_digest" > "$root/opt/wotoha/workers/candidate"
  printf '%s\n' '7' > "$root/opt/wotoha/YOUTUBE_WORKER_SEQUENCE"
  printf '%s\n' "$candidate_digest" > "$root/var/lib/wotoha/youtube-worker-ack"
  write_worker_state \
    "$root/var/lib/wotoha-updater/installed-youtube-worker" \
    7 "$current_digest" "youtube-worker-v7.0"
  write_worker_state \
    "$root/var/lib/wotoha-updater/youtube-worker-candidate-tag" \
    8 "$candidate_digest" "youtube-worker-v8.0"

  local before after
  before="$(file_snapshot \
    "$root/opt/wotoha/workers/current" \
    "$root/opt/wotoha/workers/candidate" \
    "$root/opt/wotoha/YOUTUBE_WORKER_SEQUENCE" \
    "$root/var/lib/wotoha/youtube-worker-ack" \
    "$root/var/lib/wotoha-updater/installed-youtube-worker" \
    "$root/var/lib/wotoha-updater/youtube-worker-candidate-tag")"

  PATH="$root/mock-bin:$PATH" bash "$package/install-ubuntu.sh" > "$root/first.log" 2>&1
  PATH="$root/mock-bin:$PATH" bash "$package/install-ubuntu.sh" > "$root/second.log" 2>&1

  after="$(file_snapshot \
    "$root/opt/wotoha/workers/current" \
    "$root/opt/wotoha/workers/candidate" \
    "$root/opt/wotoha/YOUTUBE_WORKER_SEQUENCE" \
    "$root/var/lib/wotoha/youtube-worker-ack" \
    "$root/var/lib/wotoha-updater/installed-youtube-worker" \
    "$root/var/lib/wotoha-updater/youtube-worker-candidate-tag")"
  assert_equal "$before" "$after" "installer rerun changed active worker state"
  printf '%s\n' 'ok - installer rerun preserves current, candidate, and ACK'
}

test_bash_syntax
test_attestation_failure_preserves_state
test_sequence_downgrade_is_rejected
test_oversize_worker_is_rejected
test_installer_rerun_preserves_worker_state
printf '%s\n' 'all deploy updater tests passed'
