#!/usr/bin/env bash
set -euo pipefail

command_name="${0##*/}"

case "$command_name" in
  curl)
    output=""
    maximum_bytes=""
    url=""
    while (( $# > 0 )); do
      case "$1" in
        --output|-o)
          output="${2:?missing curl output path}"
          shift 2
          ;;
        --max-filesize)
          maximum_bytes="${2:?missing curl size limit}"
          shift 2
          ;;
        --header|--retry|--retry-delay|--retry-max-time|--connect-timeout|--config|--range|--max-time)
          shift 2
          ;;
        --fail|--silent|--show-error|--location|--remove-on-error|--retry-all-errors)
          shift
          ;;
        http://*|https://*|fixture://*)
          url="$1"
          shift
          ;;
        *)
          shift
          ;;
      esac
    done

    [[ -n "$url" && -n "${FIXTURE_DIR:-}" ]]
    if [[ "$url" == https://media.example/* ]]; then
      [[ "${FAKE_DIRECT_FAIL:-false}" != true ]] || exit 22
      [[ -n "$output" ]] || {
        head -c "${FAKE_DIRECT_BYTES:-1024}" /dev/zero
        exit "${FAKE_DIRECT_STATUS:-0}"
      }
      mkdir -p "$(dirname "$output")"
      head -c "${FAKE_DIRECT_BYTES:-1024}" /dev/zero >"$output"
      exit "${FAKE_DIRECT_STATUS:-0}"
    fi
    if [[ -z "$output" ]]; then
      [[ "$url" == https://media.example/* ]] || {
        printf 'unexpected streaming fixture URL: %s\n' "$url" >&2
        exit 64
      }
    fi
    case "$url" in
      *"/releases/latest")
        if [[ "${MOCK_CURL_MODE:-general}" == ytdlp ]]; then
          source_file="$FIXTURE_DIR/yt-release.json"
        else
          source_file="$FIXTURE_DIR/full-release.json"
        fi
        ;;
      fixture://release-archive)
        source_file="$FIXTURE_DIR/release.tar.gz"
        ;;
      fixture://release-checksum)
        source_file="$FIXTURE_DIR/release.tar.gz.sha256"
        ;;
      fixture://release-manifest)
        source_file="$FIXTURE_DIR/release.tar.gz.manifest.json"
        ;;
      fixture://release-attestation)
        source_file="$FIXTURE_DIR/release.tar.gz.attestation.jsonl"
        ;;
      fixture://yt-dlp)
        [[ "${FAKE_YTDLP_OVERSIZE:-false}" != true ]] || exit 63
        source_file="$FIXTURE_DIR/yt-dlp"
        ;;
      fixture://yt-sums)
        source_file="$FIXTURE_DIR/SHA2-256SUMS"
        ;;
      fixture://yt-signature)
        source_file="$FIXTURE_DIR/SHA2-256SUMS.sig"
        ;;
      *)
        printf 'unexpected fixture URL: %s\n' "$url" >&2
        exit 64
        ;;
    esac

    if [[ -n "${MOCK_LOG:-}" ]]; then
      printf 'curl %s\n' "$url" >> "$MOCK_LOG"
    fi
    [[ -f "$source_file" ]]
    if [[ -n "$maximum_bytes" ]] \
      && (( $(stat --format='%s' "$source_file") > maximum_bytes )); then
      exit 63
    fi
    mkdir -p "$(dirname "$output")"
    cp "$source_file" "$output"
    ;;

  gpg)
    if [[ " $* " == *" --with-colons "* ]]; then
      printf 'pub:-:2048:1:57CF65933B5A7581:0:0:::::::\n'
      printf 'fpr:::::::::%s:\n' "${FAKE_GPG_FINGERPRINT:-AC0CBBE6848D6A873464AF4E57CF65933B5A7581}"
      exit 0
    fi
    if [[ " $* " == *" --verify "* ]]; then
      exit "${FAKE_GPG_VERIFY_STATUS:-0}"
    fi
    exit 0
    ;;

  jq)
    # The deployment tests use compact, generated JSON fixtures. Supporting
    # only the updater's fixed queries avoids a host jq dependency.
    args=" $* "
    if [[ "$args" == *".tag_name"* ]]; then
      release_tag="$(sed -n 's/.*"tag_name":"\([^"]*\)".*/\1/p' "${!#}" | head -n 1)"
      [[ -n "$release_tag" ]] || exit 1
      if [[ "$args" == *".tag_name | select("* ]]; then
        [[ "$release_tag" =~ ^[0-9]{4}[.][0-9]{2}[.][0-9]{2}([.][0-9]{6})?$ ]] || exit 1
      fi
      printf '%s\n' "$release_tag"
      exit 0
    fi
    if [[ "$args" == *"--arg n "* ]]; then
      name=""
      while (( $# > 0 )); do
        if [[ "$1" == --arg && "${2:-}" == n ]]; then name="$3"; break; fi
        shift
      done
      case "$name" in
        yt-dlp_linux) printf '%s\n' fixture://yt-dlp ;;
        SHA2-256SUMS) printf '%s\n' fixture://yt-sums ;;
        SHA2-256SUMS.sig) printf '%s\n' fixture://yt-signature ;;
        *) exit 1 ;;
      esac
      exit 0
    fi
    if [[ "$args" == *"--arg name"* ]]; then
      name=""
      while (( $# > 0 )); do
        if [[ "$1" == --arg && "${2:-}" == name ]]; then name="$3"; break; fi
        shift
      done
      case "$name" in
        wotoha-ubuntu-x86_64-musl.tar.gz)
          source_file="$FIXTURE_DIR/release.tar.gz"
          asset_url=fixture://release-archive
          ;;
        wotoha-ubuntu-x86_64-musl.tar.gz.sha256)
          source_file="$FIXTURE_DIR/release.tar.gz.sha256"
          asset_url=fixture://release-checksum
          ;;
        wotoha-ubuntu-x86_64-musl.tar.gz.manifest.json)
          source_file="$FIXTURE_DIR/release.tar.gz.manifest.json"
          asset_url=fixture://release-manifest
          ;;
        wotoha-ubuntu-x86_64-musl.tar.gz.attestation.jsonl)
          source_file="$FIXTURE_DIR/release.tar.gz.attestation.jsonl"
          asset_url=fixture://release-attestation
          ;;
        *) exit 1 ;;
      esac
      if [[ "$args" == *"| length"* ]]; then
        printf '1\n'
      elif [[ "$args" == *"| .size"* ]]; then
        stat --format='%s' "$source_file"
      elif [[ "$args" == *"| .url"* ]]; then
        printf '%s\n' "$asset_url"
      else
        exit 64
      fi
      exit 0
    fi
    if [[ "$args" == *".schema_version == 1"* ]]; then
      [[ "${FAKE_MANIFEST_INVALID:-false}" != true ]] || exit 1
      grep -Fq '"schema_version":1' "${!#}" \
        && grep -Fq '"asset":"wotoha-ubuntu-x86_64-musl.tar.gz"' "${!#}"
      exit
    fi
    if [[ "$args" == *".sha256"* ]]; then
      sed -n 's/.*"sha256":"\([0-9a-f]*\)".*/\1/p' "${!#}"
      exit 0
    fi
    if [[ "$args" == *".commit"* ]]; then
      sed -n 's/.*"commit":"\([0-9a-f]*\)".*/\1/p' "${!#}"
      exit 0
    fi
    printf 'unsupported jq fixture query: %s\n' "$args" >&2
    exit 64
    ;;

  gh)
    if [[ " $* " == *" --help "* ]]; then
      printf '%s\n' \
        '--signer-workflow string' \
        '--source-digest string' \
        '--deny-self-hosted-runners'
      exit 0
    fi
    if [[ -n "${MOCK_LOG:-}" ]]; then
      printf 'gh %s\n' "$*" >> "$MOCK_LOG"
    fi
    exit "${FAKE_GH_VERIFY_STATUS:-0}"
    ;;

  systemctl)
    action="${1:-}"
    state_file="${FAKE_SERVICE_STATE_FILE:-}"
    case "$action" in
      is-active)
        [[ -n "$state_file" && -r "$state_file" && "$(<"$state_file")" == active ]]
        ;;
      is-failed)
        [[ -n "$state_file" && -r "$state_file" && "$(<"$state_file")" == failed ]]
        ;;
      restart)
        [[ -z "${MOCK_LOG:-}" ]] || printf 'systemctl restart %s\n' "${2:-}" >>"$MOCK_LOG"
        if [[ "${FAKE_RESTART_FAIL:-false}" == true ]]; then
          [[ -z "$state_file" ]] || printf 'failed\n' >"$state_file"
          exit 1
        fi
        [[ -z "$state_file" ]] || printf 'active\n' >"$state_file"
        ;;
      daemon-reload|enable)
        exit 0
        ;;
      *) exit 0 ;;
    esac
    ;;

  sleep)
    exit 0
    ;;

  getent)
    printf '%s\n' 'wotoha:x:12345:'
    ;;

  id)
    printf '%s\n' '12345'
    ;;

  chown|groupadd|useradd)
    exit 0
    ;;

  install)
    directory=false
    args=()
    while (( $# > 0 )); do
      case "$1" in
        -d) directory=true; shift ;;
        -m|-o|-g) shift 2 ;;
        --) shift; args+=("$@"); break ;;
        -*) shift ;;
        *) args+=("$1"); shift ;;
      esac
    done
    if [[ "$directory" == true ]]; then
      mkdir -p "${args[@]}"
    else
      (( ${#args[@]} == 2 )) || exit 64
      mkdir -p "$(dirname "${args[1]}")"
      cp "${args[0]}" "${args[1]}"
      chmod 0755 "${args[1]}"
    fi
    ;;

  flock)
    exit 0
    ;;

  mkdir)
    paths=()
    while (( $# > 0 )); do
      case "$1" in
        -m) shift 2 ;;
        -p) shift ;;
        -*) shift ;;
        *) paths+=("$1"); shift ;;
      esac
    done
    /usr/bin/mkdir -p "${paths[@]}"
    ;;

  timeout)
    timeout_args=("$@")
    while (( $# > 0 )); do
      case "$1" in
        --signal=*|--kill-after=*|-s*) shift ;;
        --signal|--kill-after|-k) shift 2 ;;
        --) shift; break ;;
        -*) shift ;;
        *) shift; break ;;
      esac
    done
    [[ "${1:-}" == -- ]] && shift
    (( $# > 0 )) || exit 64
    if [[ -n "${MOCK_LOG:-}" ]]; then
      printf 'timeout' >>"$MOCK_LOG"
      printf ' %q' "${timeout_args[@]}" >>"$MOCK_LOG"
      printf '\n' >>"$MOCK_LOG"
    fi
    if [[ "${FAKE_YTDLP_EXTRACT_HANG:-false}" == true && " $* " == *" --print "* ]]; then
      "$@" & child=$!
      for _ in {1..100}; do
        [[ -n "${FAKE_YTDLP_HANG_STARTED:-}" && -e "$FAKE_YTDLP_HANG_STARTED" ]] && break
        sleep 0.01
      done
      kill "$child" 2>/dev/null || true
      wait "$child" 2>/dev/null || true
      exit 124
    fi
    exec "$@"
    ;;

  *)
    printf 'unexpected mocked command: %s\n' "$command_name" >&2
    exit 64
    ;;
esac
