# Wotoha RS v0.6.0 — AutoMix V2

> Draft only. This file is not a published GitHub Release. The current
> published release is [v0.5.36](https://github.com/NcaMoq/wotoha-rs/releases/tag/v0.5.36).
> This is the versioned candidate draft. Rolling changes belong in
> [`release-notes-main-draft.md`](release-notes-main-draft.md); neither file
> is publishable until the release preflight is complete.

## Highlights

Wotoha RS v0.6.0 is a candidate release focused on safer, more inspectable
automatic transitions and a more reproducible production container. It keeps
the familiar Discord controls and provider workflow while making the handoff
planner's evidence and fallbacks easier to operate.

## AutoMix V2

- Adds a timeline-first transition-planning surface with bounded tempo-pair and
  cue-candidate evaluation.
- Uses beat phase, structure, cue suitability, energy, vocal risk, tempo
  adjustment, and peak safety as explicit planning evidence where available.
- Keeps BeatMatched, Crossfade, and Gapless as separate outcomes so an
  uncertain beat match can fall back safely.
- Preserves the compatibility planner as the default production authority.
  V2 is available only through explicit planner configuration or shadow
  observation; research reports do not silently change playback behavior.

## Audio analysis

- Extends the shared analysis surface for rhythm, tempo hypotheses, beat
  timelines, structure, vocal activity, energy, tonal information, and
  loudness/peak measurements.
- Keeps analysis-lab experiments, fixed-tempo consensus work, and controlled
  reference packets outside the production decision path. These are research
  and validation artifacts, not release features or production authority.
- Documents the planner boundary and data flow in
  [`docs/automix.md`](automix.md).

## Production deployment

- Supports the digest-pinned Docker deployment documented in
  [`docs/docker-deploy.md`](docker-deploy.md).
- The production image uses a non-root runtime, a read-only root filesystem,
  dropped capabilities, no-new-privileges, and an offline self-check.
- Host data and runtime configuration remain explicit so upgrades and rollback
  can be performed by changing the image digest.

## Safety and reliability

- Loudness normalization remains enabled by default at `-16 LUFS`, with
  attenuation-first behavior and a `-2 dBTP` normalization-stage true-peak
  ceiling.
- BeatMatched remains bounded by timing, overlap, phase, gain, structure, and
  renderer-compatibility checks.
- Crossfade or Gapless remains available when the analysis cannot justify a
  beat-aligned handoff.

## Upgrade notes

1. Read the [Docker deployment guide](docker-deploy.md) before replacing an
   existing image.
2. Use a recorded image digest for production and keep the host data directory
   across upgrades.
3. Review `runtime.env.example` for new or changed settings.
4. Leave `WOTOHA_AUTOMIX_PLANNER_MODE` unset to retain the default compatibility
   planner. Enable V2 only after reviewing the controlled-evaluation notes.

## Known limitations

- Provider extraction depends on public provider behavior and can change
  independently of Wotoha.
- Audio analysis is evidence-driven; not every track pair is suitable for
  BeatMatched, and safe fallback is expected.
- V2 shadow output and analysis-lab research are not a claim of production
  superiority or real-world benchmark performance.
- This document remains a draft until a maintainer verifies the final release
  contents, upgrade path, and CI/container artifacts.

## Release preflight

Before publishing a tag or GitHub Release, a maintainer must complete this
checklist against the deliberate release commit:

- [ ] Confirm the version, tag, release assets, and release notes agree.
- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo check --workspace --locked`.
- [ ] Run `cargo test --workspace --locked` and the isolated
      `cargo test --locked -p wotoha-media` check.
- [ ] Run `cargo clippy --workspace --all-targets --locked --no-deps -- -D warnings`.
- [ ] Run the deployment safety tests and verify the container workflow and
      offline smoke test for the release commit.
- [ ] Review image digest, checksums, provenance/attestation, and rollback
      instructions before publishing.
- [ ] Confirm that research-only reports and unverified benchmark claims are
      not presented as production release features.
