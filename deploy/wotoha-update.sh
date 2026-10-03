#!/usr/bin/env bash
# Deliberately fail closed. Application releases are deployed by immutable
# Digest-pinned Docker images; the independent deploy/yt-dlp-update.sh channel is retained
# for legacy migration hosts.
set -euo pipefail
printf '%s\n' \
  'wotoha-update is disabled: deploy the Linux Docker image with a recorded digest reference' \
  >&2
exit 1
