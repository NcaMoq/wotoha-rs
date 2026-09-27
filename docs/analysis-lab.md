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
must remain distinguishable from an observed empty result. A record or
document with an unsupported schema version is rejected.

The current clean-room target can be represented by an observer identity such
as `Traktor Pro 4`, version `4.1.1 (23)`, macOS. No binary is needed by this
crate. A future Team A packet can be passed to `--external-observations` after
its hashes and schema have been validated.

## Synthetic corpus

Synthetic fixtures generate canonical PCM audio and exact `AnalysisGroundTruth`
together. Truth retains beat times, downbeat indexes, meter, constant tempo
alternates, and variable-tempo segments; it is not reduced to one BPM for
drift fixtures. The default development corpus includes:

- constant tempos from 60 through 180 BPM, including 127.5 BPM;
- half/double-time and accent ambiguity;
- missing beats and extra off-grid transients;
- linear ramps of ±0.1%, ±0.25%, ±0.5%, ±1%, ±2%, and ±4%, plus a step/return;
- kick, snare, hats, attenuated/removed/syncopated kick evidence;
- downbeat ambiguity, breakdown/re-entry, pickup, and 2/4, 3/4, 4/4, 6/8;
- deterministic gain, compression, EQ, high-pass, low-pass, mono, stereo, and
  sample-rate variants.

Transform fixtures retain the same truth and are matched to their base by
fixture identity and audio hash. Lossy codec variants are intentionally not
claimed until a repository-native codec fixture path is available.

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

Runs do not use `.wotoha-analysis/` or the production analysis cache. Generated
manifests, reports, and run directories belong in `/tmp`, `target`, or the
ignored lab paths.

## Metrics and reports

Reports are JSON with concise CLI summaries and the following sections:

`overall`, `by_fixture_family`, `by_tempo_range`, `by_meter`, `by_transform`,
`half_double_errors`, `downbeat_errors`, `variable_tempo`,
`confidence_calibration`, `failure_clusters`, `external`, and `per_track`.

Beat metrics use deterministic one-to-one matching and report MAE, p50, p95,
and precision/recall at 10, 20, 40, and 70 ms. Tempo metrics retain primary
correctness, exact correct-hypothesis top-N credit, relative/absolute error,
and half-time/double-time/other relation counts. Grid phase is measured modulo
the expected beat period. Downbeat/bar phase and meter are evaluated separately;
`Unknown` is not counted as `wrong`.

Variable-tempo metrics report local BPM error, phase drift, and change-tracking
delay where a step change exists. Transform metrics compare beat displacement,
tempo interpretation, downbeat, meter, and confidence changes against the
unmodified base. Confidence calibration bins timing confidence separately from
model score, onset support, low-frequency support, downbeat evidence, and
structure evidence.

When external observations exist, the report separately records:

```text
Wotoha ↔ synthetic truth
External observer/version ↔ synthetic truth
Wotoha ↔ External observer/version
```

Observer versions are grouped separately rather than silently combined. Real
music without exact truth should be labeled `disagreement`, not `error`, and a
future human review may attach `WotohaCorrect`, `ExternalCorrect`,
`BothAcceptable`, `Ambiguous`, or `NeitherCorrect`. No Memory Cue training is
performed here; absence of a human cue is not a negative label.

## Baseline discipline

The first baseline is descriptive. It is not a release gate and it does not
tune Wotoha to agree with an external DJ application. Any future production
change must be justified by synthetic error, signal-processing rationale,
public literature, or human validation, and must preserve the existing
`TrackAnalysisV2` evidence separation and beat-event timeline truth.
