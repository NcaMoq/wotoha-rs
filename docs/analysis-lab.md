# Wotoha analysis lab

The analysis lab is an isolated research/evaluation layer for rhythm and
analysis accuracy. It is implemented in the `wotoha-analysis-lab` crate and
does not run in playback, AutoMix planning, transition rendering, or the
production analysis-cache hot path.

## Clean-room boundary

Team A may inspect external DJ software and provide versioned, neutral
observation packets. Team B imports only those packets, public product
behavior, synthetic known truth, human review, Wotoha's own code, and
independently selected public algorithms. Team B does not inspect, disassemble,
decompile, run `strings` against, or copy implementation details from an
external application. External DJ software is an observation reference, not
ground truth.

The observation schema is vendor-neutral. It records `sample_id`, canonical
audio SHA-256, observer product/version/platform, public analysis settings,
optional reported BPM, optional beatgrid/downbeat/key/meter results, completion
state, timing, and notes. Optional fields are `Option` values where absence
must remain distinguishable from an observed empty result. For example,
`beatgrid_times_micros: null` means unobserved, while `[]` means a completed
observation explicitly produced zero beats. The same rule applies to
downbeats. Incomplete observations are counted separately and incomplete empty
results are not scored as authoritative failures. A record or document with
an unsupported schema version is rejected. Duplicate sample/observer/version/
settings records are rejected.

The current clean-room target can be represented by an observer identity such
as `Traktor Pro 4`, version `4.1.1 (23)`, macOS. No binary is needed by this
crate. A future Team A packet can be passed to `--external-observations` after
its hashes and schema have been validated.

The lab does not contain a Traktor implementation or a vendor-specific
adapter. External records are generic observations, grouped by observer
product/version/settings and matched by cryptographic audio identity.

## Synthetic corpus

Synthetic fixtures generate canonical PCM audio and exact `AnalysisGroundTruth`
together. Truth retains beat times, downbeat indexes, meter, constant tempo
alternates, and variable-tempo segments; it is not reduced to one BPM for
drift fixtures. The default development corpus includes:

- constant tempos from 60 through 180 BPM, including 127.5 BPM;
- half/double-time and accent ambiguity, including 64↔128, 70↔140,
  75↔150, 80↔160, 85↔170, and 90↔180 BPM families;
- missing beats and extra off-grid transients;
- linear ramps of ±0.1%, ±0.25%, ±0.5%, ±1%, ±2%, and ±4%, plus a step/return;
- kick, snare, hats, attenuated/removed/syncopated kick evidence;
- downbeat ambiguity, breakdown/re-entry, pickup, and 2/4, 3/4, 4/4, 6/8;
- deterministic gain, compression, EQ, high-pass, low-pass, mono, stereo, and
  sample-rate variants.

Transform fixtures retain the same musical truth even when the PCM changes.
Sample-rate variants, for example 22,050 Hz and 16,000 Hz, are matched by
`base_id` and shared truth rather than equal PCM hashes; duration may differ by
at most sample-rounding tolerance. Lossy codec variants are intentionally not
claimed until a repository-native codec fixture path is available.

For portable clean-room work, export the exact corpus as canonical WAV files:

```bash
cargo run --release --locked -p wotoha-analysis-lab -- \
  export-blackbox --output /tmp/wotoha-blackbox-v1 --seed 246813579
```

The export contains `audio/*.wav`, `manifest.json`,
`external-observations-template.json`, `checksums.sha256`, and a README for
Team A. The manifest stores both the WAV file hash and decoded PCM hash, so a
receiver can reject a changed or mismatched file before analysis. Package the
directory with the platform's standard ZIP tool when transferring it; the
directory itself is the reproducible source artifact and generated packages
belong outside the repository.

## Commands

Generate a versioned fixture manifest:

```bash
cargo run --locked -p wotoha-analysis-lab -- generate \
  --output /tmp/wotoha-analysis-fixtures.json --seed 123
```

Evaluate the current Wotoha V2 adapter and optionally import external records:

```bash
cargo run --locked -p wotoha-analysis-lab -- evaluate \
  --manifest /tmp/wotoha-analysis-fixtures.json \
  --external-observations /tmp/observations.json \
  --report /tmp/wotoha-analysis-report.json
```

`--external-observations` is optional. `--mode hybrid` exercises the existing
CPU Beat This! V2 rhythm path with its established fallback; `--mode classical`
is useful for fast deterministic fixture iteration. Each fixture is generated,
hashed, analyzed, and released before the next one. The evaluator has a
cancellation-aware library entry point and bounded fixture/audio limits.

Run the lab-only tempo-family experiment directly when a standalone artifact
is useful:

```bash
WOTOHA_SOURCE_COMMIT=$(git rev-parse HEAD) \
  cargo run --release --locked -p wotoha-analysis-lab -- \
  research-tempo --mode hybrid --report /tmp/wotoha-tempo-experiment-v1.json
```

This report compares the current production hypothesis with independent
half/native/double candidate evidence. It may return `ambiguous`; it never
replaces the production tempo label or BeatEvent timeline.

Runs do not use `.wotoha-analysis/` or the production analysis cache. Generated
manifests, reports, and run directories belong in `/tmp`, `target`, or the
ignored lab paths.

## Metrics and reports

Reports are JSON with concise CLI summaries and the following sections:

`overall`, `by_fixture_family`, `by_tempo_range`, `by_meter`, `by_transform`,
`half_double_errors`, `downbeat_errors`, `variable_tempo`,
`confidence_calibration`, `failure_clusters`, `external`, and `per_track`.
Metadata also records the evaluator/report versions, split, analyzer mode,
optional source commit, and Hybrid backend counts (`native_neural` versus
`classical_fallback`). Meter evidence always contains explicit 2-, 3-, 4-,
and 6-beat candidate slots; an absent raw hypothesis is serialized as
unavailable rather than being confused with a zero score.

When enabled by the CLI, `backend_comparison` evaluates Hybrid and Classical
on the same generated audio and records per-fixture deltas and descriptive
outcomes. It is not a production selector and is not reduced to one aggregate
winner. `tempo_experiment` contains the lab-only independent candidate
resolver: candidate BPM/half/double relations are scored from generated
rhythm evidence with activation support, coverage, periodic consistency,
phase, and ambiguity. Its result is observational and does not alter the
production tempo hypotheses.

Beat metrics use a deterministic monotonic one-to-one matcher and report MAE,
p50, p95, and precision/recall at 10, 20, 40, and 70 ms. Per-track percentiles
remain per-track; overall p50/p95 are pooled over every matched beat error, and
overall MAE and precision/recall are observation-weighted micro metrics. Tempo
metrics retain primary correctness, canonical correct-hypothesis top-N credit,
relative/absolute error, and explicit canonical/half-time/double-time/
alternative/absent relation counts. Top-N denominators include only fixtures
with defined scalar tempo truth; variable-tempo fixtures are not failed
predictions. `valid_alternates_bpm` is reported separately from canonical
presence so a musically valid half/double interpretation receives appropriate
credit without becoming canonical correctness.

Meter scoring consumes Wotoha's canonical `RhythmAnalysis::resolved_meter()`
decision, not the highest raw hypothesis. The domain's confidence and margin
rules therefore preserve `Unknown`; raw meter hypotheses remain in the
normalized result. Truth meter plus a resolved prediction is scored as correct,
wrong, or unknown, while absent external meter is unobserved and not scored.
Grid phase is measured modulo the expected period only for constant-tempo truth;
variable-tempo truth returns an unscored global phase and is evaluated through
the local timeline and phase-drift metrics instead. For Wotoha↔external
comparisons, a global phase is allowed only for an explicit BPM+phase pair
without a grid, or for a beatgrid whose positive intervals have a robust median
and all remain within the generic three-percent stability band. The median
tolerates sample rounding and one modest outlier; the first external interval
is never treated as a universal period. An explicit external phase is still
used for constant truth even when no external beat grid is present.
Downbeat/bar phase and meter are evaluated separately; `Unknown` is not counted
as `wrong`. Wotoha downbeat phase is not scored as resolved bar evidence when
Wotoha's resolved meter is `None`; raw downbeat evidence remains preserved.

Variable-tempo metrics report local BPM error, phase drift, and change-tracking
delay where a step change exists. Transform metrics match transformed beats to
the base timeline instead of zipping indexes, and report matched, missing, and
extra beats, mean/p95 displacement, tempo interpretation, downbeat, meter, and
confidence changes. Confidence calibration bins timing confidence separately
from model score, onset support, low-frequency support, downbeat evidence, and
structure evidence; each timing bin reports matched and unmatched predicted
beats so false positives cannot disappear from calibration.

High-pass is classified under `evidence_ablation`, because it intentionally
removes low-frequency rhythm evidence and is not an ordinary invariance claim.
Gain, compression, EQ, low-pass, channel, and sample-rate transforms remain
under `transform_invariance` unless their fixture semantics change.

When external observations exist, the report separately records:

```text
Wotoha ↔ synthetic truth
External observer/version ↔ synthetic truth
Wotoha ↔ External observer/version
```

Observer versions are grouped separately rather than silently combined, and
materially different public analysis settings are separate report groups. The
report includes total, complete/incomplete, beat, tempo, meter, downbeat, and
grid-phase observation counts per group. Real
music without exact truth should be labeled `disagreement`, not `error`, and a
future human review may attach `WotohaCorrect`, `ExternalCorrect`,
`BothAcceptable`, `Ambiguous`, or `NeitherCorrect`. No Memory Cue training is
performed here; absence of a human cue is not a negative label.

## Baseline commands

Run both descriptive baselines after correctness changes:

```bash
cargo run --release --locked -p wotoha-analysis-lab -- \
  baseline --mode hybrid --report /tmp/wotoha-analysis-baseline-hybrid.json
cargo run --release --locked -p wotoha-analysis-lab -- \
  baseline --mode classical --report /tmp/wotoha-analysis-baseline-classical.json
```

Hybrid uses the existing CPU Beat This! adapter where available and reports
any classical fallback per backend count; classical uses the existing
classical adapter. The comparison is diagnostic across beat timing, tempo,
phase, downbeat, meter, variable tempo, and transforms; it is not collapsed to
one winner and it does not tune production thresholds.
Both modes also run the independent tempo experiment and same-audio backend
comparison when invoked by the CLI. Release reports should be kept outside the
repository; their metadata records analyzer mode and optional source revision
rather than relying on the filename.

## Baseline discipline

The first baseline is descriptive. It is not a release gate and it does not
tune Wotoha to agree with an external DJ application. Any future production
change must be justified by synthetic error, signal-processing rationale,
public literature, or human validation, and must preserve the existing
`TrackAnalysisV2` evidence separation and beat-event timeline truth.

The external observation schema remains at version 1. The report schema is
version 3 because meter candidate availability is now explicit in addition to
the aggregate percentile and availability meanings introduced previously; old
reports must not be compared silently with new reports.
