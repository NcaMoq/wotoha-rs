# AutoMix architecture

Wotoha's AutoMix treats a track handoff as a bounded analysis and planning
problem. It compares the outgoing and incoming tracks, builds a small set of
transition candidates, applies quality checks, and chooses the safest
available playback strategy.

AutoMix is not a claim that every pair of tracks can be beat matched. A
successful planner is allowed to reject BeatMatched and use a simpler
handoff when the evidence or the render safety is insufficient.

## Analysis inputs

The production analysis path exposes a `TrackAnalysis` view and the richer
`TrackAnalysisV2` timeline. Depending on source quality and analyzer
availability, the planner can use:

- tempo hypotheses, beat confidence, beat markers, and beat phase;
- audible bounds and intro/outro regions;
- downbeat and phrase information when confidence is sufficient;
- heuristic DJ cues and cue roles for possible mix-in/mix-out positions;
- energy profiles and local handoff levels;
- vocal activity and confidence, used to avoid risky vocal overlap;
- optional musical-key information and harmonic compatibility;
- integrated loudness, sample peak, and oversampled true peak.

These are evidence inputs, not guarantees. Missing or low-confidence fields
reduce the set of eligible candidates rather than being silently replaced with
ground truth or a fixed synthetic timeline.

The legacy view keeps the observed beat markers as its timeline. A tempo
hypothesis is used for timing and compatibility calculations; it does not
rewrite the observed event timestamps.

## Transition planning

The planner follows a bounded path:

```text
outgoing + incoming analysis
            │
     bounded cue candidates
            │
   bounded tempo-pair combinations
            │
  timing, structure, vocal, energy,
     loudness, and peak checks
            │
 BeatMatched / Crossfade / Gapless
```

For a BeatMatched candidate, the implementation checks usable overlap and
source bounds, the tempo relation, beat-phase coverage, and the available
cue pair. Candidate scoring also accounts for tempo stretch, phase precision,
structure alignment, cue suitability, blend duration, and rhythm uncertainty.
The candidate set is deliberately bounded so a long queue cannot create an
uncontrolled search.

## Transition types

### BeatMatched

BeatMatched uses a compatible tempo pair and observed beat timelines to align
the handoff. The planner rejects a candidate when phase evidence, beat-pair
coverage, source bounds, overlap, gain, or other hard safety conditions are
insufficient. A valid tempo relationship can be primary, half-time,
double-time, or another explicitly supported relation. Relations are resolved
from Wotoha's own analysis evidence; external observations do not select a
production relation.

### Crossfade

Crossfade overlaps the tracks without claiming that their beat grids are
aligned. Its duration and gain behavior come from the AutoMix configuration
and the available audible regions.

### Gapless

Gapless hands off without an overlap when an overlap would be a poor or unsafe
choice. It is a deliberate fallback, not an analysis failure.

## Quality guards

The transition quality layer evaluates the selected plan and can reject it or
select a non-BeatMatched fallback. The checks include, where the analysis
provides the necessary evidence:

- beat phase and downbeat/phrase alignment;
- beat-pair count and timeline coverage;
- vocal collision risk;
- energy dips and structural-break behavior;
- intro/outro and audible-bound constraints;
- bounded tempo adjustment and finite gain values;
- low-handoff gain safety;
- sample-peak and true-peak headroom.

The runtime also verifies whether the active renderer can schedule the
BeatMatched frame plan. If it cannot, playback falls back to a supported
non-BeatMatched plan rather than assuming that a candidate is executable.

## Loudness and peak safety

Track loudness uses the integrated loudness field represented in LUFS, with the
EBU R128 / ITU-R BS.1770 convention used by the core loudness implementation.
The default target is `-16.0 LUFS`, the default maximum positive gain is `0.0
dB`, and the normalization-stage true-peak ceiling is `-2.0 dBTP`. Quieter
tracks are therefore not automatically boosted unless an operator explicitly
opts into positive gain.

Normalization is applied once per track. The true-peak ceiling at this stage
is not a substitute for validating a deployment's separately configured master
volume or output chain.

## Planner modes and authority

The runtime configuration has three explicit planner modes:

| Mode | Role | Playback authority |
| --- | --- | --- |
| `legacy` | Compatibility planner and default behavior | Yes |
| `shadow` | Runs the V2 observation path beside the legacy result and records bounded diagnostics | Legacy result remains authoritative |
| `v2` | Explicitly selects the V2 planner for a controlled deployment or evaluation | Yes, only when explicitly configured |

The default is `legacy`. `WOTOHA_AUTOMIX_V2_SHADOW_ENABLED=true` enables the
backwards-compatible shadow setting when no explicit planner mode is given;
it does not silently promote V2. The analysis lab and its tempo-consensus or
other research reports are not production planner modes.

## Research boundary

Research tooling is intentionally separate from playback authority. The
analysis lab may generate controlled fixtures, compare candidate analyzers,
or evaluate external reference packets, but those results do not change
production configuration by themselves. A production promotion requires an
explicit design decision, regression coverage, and a separate review.

Keep generated audio, private observations, screenshots, reports, and other
research artifacts outside the repository. See
[`docs/analysis-lab.md`](analysis-lab.md) for the lab commands and data
hygiene rules.

## Architecture in the workspace

```text
media provider
      │
      ▼
decode + loudness measurement
      │
      ▼
core analysis (rhythm, structure, energy, vocal, tonal)
      │
      ▼
runtime cache / TrackAnalysis view
      │
      ▼
transition planner + quality guard
      │
      ▼
tempo stretch / gain scheduling / playback
```

## Relevant source code

- [`crates/wotoha-core/src/automix.rs`](../crates/wotoha-core/src/automix.rs) — legacy transition model and V2 module boundary.
- [`crates/wotoha-core/src/automix/candidate.rs`](../crates/wotoha-core/src/automix/candidate.rs) — V2 candidate generation, tempo pairs, and bounded planning.
- [`crates/wotoha-core/src/automix/reliability.rs`](../crates/wotoha-core/src/automix/reliability.rs) — timeline and rhythm reliability calculations.
- [`crates/wotoha-core/src/analysis/`](../crates/wotoha-core/src/analysis/) — rhythm, structure, cue, energy, vocal, and tonal data types.
- [`crates/wotoha-core/src/loudness.rs`](../crates/wotoha-core/src/loudness.rs) — integrated loudness and peak measurements.
- [`crates/wotoha-runtime/src/beat_this_analysis.rs`](../crates/wotoha-runtime/src/beat_this_analysis.rs) — runtime beat-analysis adapter and timeline evidence.
- [`crates/wotoha-runtime/src/tempo_stretch.rs`](../crates/wotoha-runtime/src/tempo_stretch.rs) — bounded tempo-stretch scheduling.
- [`crates/wotoha-runtime/src/transition_dsp.rs`](../crates/wotoha-runtime/src/transition_dsp.rs) — rendered transition timing and DSP checks.
- [`crates/wotoha-voice/src/playback.rs`](../crates/wotoha-voice/src/playback.rs) — runtime planner-mode selection, fallback, and renderer integration.
- [`crates/wotoha-core/src/config.rs`](../crates/wotoha-core/src/config.rs) — planner mode, loudness, and BeatMatched configuration.
