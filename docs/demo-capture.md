# AutoMix demo capture brief

Wotoha does not currently ship a recorded demo. This document is a capture
plan, not a benchmark or a promise that a particular transition will be
selected.

## Goal

Create a short 15–30 second screen recording that lets a viewer understand the
handoff without requiring access to private logs or a private Discord server.

## Suggested capture

1. Show two public, license-safe tracks being queued in a test server.
2. Show the now-playing message and the AutoMix control.
3. Before the handoff, show only facts exposed by the running build: track
   names, the available tempo/beat evidence, and the selected transition kind
   if the UI or log exposes it.
4. Capture the handoff and a few seconds of the next track.
5. End with a link to the repository and the current release, not a claim of
   universal beat matching.

## Evidence rules

- Do not use private URLs, credentials, user IDs, guild IDs, or provider
  cookies in the recording.
- Do not draw a fake waveform, BPM label, benchmark, or quality score over the
  video.
- Label `BeatMatched`, `Crossfade`, and `Gapless` only from an actual runtime
  diagnostic or UI value.
- If a handoff falls back, keep it: conservative fallback is part of the
  product behavior.
- Record the exact Wotoha commit and planner mode used for the capture.
