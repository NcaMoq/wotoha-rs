#!/usr/bin/env bash
# Verify embedded Beat This! model provenance and redistribution metadata.
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MODELS="$ROOT/crates/wotoha-runtime/models"
NOTICE="$MODELS/NOTICE.txt"

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

for command in grep head sha256sum stat; do
  command -v "$command" >/dev/null 2>&1 || fail "missing required command: $command"
done

for required in \
  "$NOTICE" \
  "$MODELS/LICENSE.beat-this-rs.txt" \
  "$MODELS/LICENSE.beat-this-original.txt" \
  "$MODELS/beat_this_small.onnx" \
  "$MODELS/mel_spectrogram.onnx"; do
  [[ -s "$required" ]] || fail "neural-model release input is missing: $required"
done

small_hash='a5f8d39d989f31859454ba27afe61c5317ca95e4d9373e6853e5361b8937172f'
mel_hash='fdd59e65c515331308e4c8841edf99972deca646bdf6197744c2a5b7755e3de9'
small_size=10555592
mel_size=270742
actual_small="$(sha256sum "$MODELS/beat_this_small.onnx" | awk '{print $1}')"
actual_mel="$(sha256sum "$MODELS/mel_spectrogram.onnx" | awk '{print $1}')"
[[ "$actual_small" == "$small_hash" ]] || fail 'beat_this_small.onnx SHA-256 drifted; update provenance before release'
[[ "$actual_mel" == "$mel_hash" ]] || fail 'mel_spectrogram.onnx SHA-256 drifted; update provenance before release'
[[ "$(stat -c '%s' "$MODELS/beat_this_small.onnx")" == "$small_size" ]] || fail 'beat_this_small.onnx size drifted'
[[ "$(stat -c '%s' "$MODELS/mel_spectrogram.onnx")" == "$mel_size" ]] || fail 'mel_spectrogram.onnx size drifted'
grep -Fq "SHA-256: \`$small_hash\`" "$NOTICE" || fail 'small model hash is absent from NOTICE.txt'
grep -Fq "SHA-256: \`$mel_hash\`" "$NOTICE" || fail 'mel model hash is absent from NOTICE.txt'
grep -Fq 'Copyright (c) 2024 Institute of Computational Perception, JKU Linz, Austria' "$NOTICE" \
  || fail 'model copyright attribution is absent from NOTICE.txt'
grep -Fq 'Copyright (c) 2025 danigb (Rust port)' "$NOTICE" \
  || fail 'beat-this-rs copyright attribution is absent from NOTICE.txt'
grep -Fq 'https://github.com/danigb/beat-this-rs/blob/089b509247e6fdcec666511c0dcf0d5f39c21e73/' "$NOTICE" \
  || fail 'pinned beat-this-rs provenance URL is absent from NOTICE.txt'
grep -Fq 'https://github.com/CPJKU/beat_this/blob/b95c8ab0c58c2d9fcfd40508ae8dffbc05ac4f5c/README.md#license' "$NOTICE" \
  || fail 'original model license source URL is absent from NOTICE.txt'
grep -Fq 'https://github.com/CPJKU/beat_this/blob/72f586c02402bce53cb9bf30029bd4c2f620efa0/LICENSE' "$NOTICE" \
  || fail 'original MIT license source URL is absent from NOTICE.txt'
grep -Fq 'MIT License' "$MODELS/LICENSE.beat-this-rs.txt" \
  || fail 'pinned beat-this-rs license text is absent'
grep -Fq 'MIT License' "$MODELS/LICENSE.beat-this-original.txt" \
  || fail 'original Beat This! license text is absent'

# A Git LFS pointer is not a model and must never be embedded in a release.
! head -n 1 "$MODELS/beat_this_small.onnx" | grep -Fq 'https://git-lfs.github.com/spec/v1' \
  || fail 'beat_this_small.onnx is a Git LFS pointer'
! head -n 1 "$MODELS/mel_spectrogram.onnx" | grep -Fq 'https://git-lfs.github.com/spec/v1' \
  || fail 'mel_spectrogram.onnx is a Git LFS pointer'

printf 'ok - embedded neural model bytes, hashes, provenance, and license texts are present\n'
