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
        --output)
          output="${2:?missing curl output path}"
          shift 2
          ;;
        --max-filesize)
          maximum_bytes="${2:?missing curl size limit}"
          shift 2
          ;;
        --header|--retry|--config)
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

    [[ -n "$output" && -n "$url" && -n "${FIXTURE_DIR:-}" ]]
    case "$url" in
      *"/releases?per_page=30")
        source_file="$FIXTURE_DIR/worker-releases.json"
        ;;
      *"/releases/latest")
        source_file="$FIXTURE_DIR/full-release.json"
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

  *)
    printf 'unexpected mocked command: %s\n' "$command_name" >&2
    exit 64
    ;;
esac
