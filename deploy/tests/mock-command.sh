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
        --header|--retry|--config|--range|--max-time)
          shift 2
          ;;
        --fail|--silent|--show-error|--location|--remove-on-error)
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
    if [[ -z "$output" ]]; then
      [[ "$url" == https://media.example/* ]] || {
        printf 'unexpected streaming fixture URL: %s\n' "$url" >&2
        exit 64
      }
      [[ "${FAKE_DIRECT_FAIL:-false}" != true ]] || exit 22
      head -c "${FAKE_DIRECT_BYTES:-8192}" /dev/zero
      exit "${FAKE_DIRECT_STATUS:-0}"
    fi
    case "$url" in
      *"/releases?per_page=30")
        source_file="$FIXTURE_DIR/worker-releases.json"
        ;;
      *"/releases/latest")
        if [[ "${MOCK_CURL_MODE:-general}" == ytdlp ]]; then
          source_file="$FIXTURE_DIR/yt-release.json"
        else
          source_file="$FIXTURE_DIR/full-release.json"
        fi
        ;;
      fixture://worker)
        source_file="$FIXTURE_DIR/worker"
        ;;
      fixture://worker-manifest)
        source_file="$FIXTURE_DIR/worker.manifest.json"
        ;;
      fixture://worker-attestation)
        source_file="$FIXTURE_DIR/worker.attestation.jsonl"
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
    # The deploy tests deliberately need only the release metadata queries
    # issued by yt-dlp-update.sh. Keeping this tiny avoids a host jq dependency.
    args=" $* "
    if [[ "$args" == *".tag_name"* ]]; then
      sed -n 's/.*"tag_name":"\([^"]*\)".*/\1/p' "${!#}" | head -n 1
      exit 0
    fi
    if [[ "$args" == *"--arg n"* ]]; then
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
    if [[ "${1:-}" == "is-active" ]]; then
      exit 1
    fi
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
