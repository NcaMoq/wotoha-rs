#!/usr/bin/env bash
# Classify a production-path media canary without conflating shared-runner
# challenges with extractor or playback regressions.
set -Eeuo pipefail

LOG_FILE="${1:?usage: classify-youtube-probe.sh LOG_FILE}"
[[ -f "$LOG_FILE" ]] || {
  printf 'EXTRACTOR_FAILURE\n'
  exit 0
}

if grep -Eiq 'sign in to confirm|not a bot|cookies-from-browser|cookies?\.txt|captcha|challenge' "$LOG_FILE"; then
  printf 'AUTH_OR_BOT_CHALLENGE\n'
elif grep -Eiq 'media byte|direct media|range request|failed to read|unexpected eof|content-length|http error 4(0[03]|2[0-9])' "$LOG_FILE"; then
  printf 'MEDIA_BYTE_FAILURE\n'
elif grep -Eiq 'did not return a playable url|missingurl|playable.*error|no playable format|unable to extract' "$LOG_FILE"; then
  printf 'PLAYBACK_URL_FAILURE\n'
else
  printf 'EXTRACTOR_FAILURE\n'
fi
