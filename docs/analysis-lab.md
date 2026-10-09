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

The observation schema is vendor-neutral. It records `sample_id`, the exact
transferred WAV file SHA-256 and optional decoded PCM SHA-256, generic observer
identity/version/environment, public analysis settings,
optional reported BPM, optional beatgrid/downbeat/key/meter results, completion
state, timing, and notes. Optional fields are `Option` values where absence
must remain distinguishable from an observed empty result. For example,
`beatgrid_times_micros: null` means unobserved, while `[]` means a completed
observation explicitly produced zero beats. The same rule applies to
downbeats. Incomplete observations are counted separately and incomplete empty
results are not scored as authoritative failures. A record or document with
an unsupported schema version is rejected. Duplicate sample/observer/version/
settings records are rejected; platform participates in the grouping key.
External observations are accepted only by the exported-WAV evaluator. The
normal in-memory `evaluate` path rejects them explicitly, preventing a WAV
identity from being compared with the generated floating-point fixture hash.
When importing an observation against a `BlackboxManifest`, the evaluator
checks sample ID, `wav_file_sha256`, supplied `pcm_sha256`, and supplied
`ground_truth_sha256` before scoring.

External clean-room references are represented through generic observer
identities. Vendor-specific acquisition details and raw observations are
intentionally kept outside this repository. A future neutral observation
packet can be passed to `--external-observations` after its hashes and schema
have been validated.

The lab contains no vendor-specific implementation or adapter. External
records are generic observations, grouped by observer identity/version/settings
and matched by cryptographic audio identity.

Repository clean-room policy:

- External observers are represented only by generic identities.
- Vendor-specific acquisition details stay outside the repository.
- Raw external observations stay outside the repository.
- Research consumes only neutral observation packets.
- External observations never directly determine production implementation.
- Production changes require independent Wotoha evidence and regression tests.

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
Team A. The manifest labels the exact `wav_file_sha256`, decoded `pcm_sha256`,
generated `generated_float_fixture_sha256`, and `ground_truth_sha256` separately,
so a receiver can reject a changed or mismatched file before analysis. Use the
lab's `package-blackbox` command for deterministic stored-ZIP packaging; the
directory and generated package belong outside the repository.

## Commands

Generate a versioned fixture manifest:

```bash
cargo run --locked -p wotoha-analysis-lab -- generate \
  --output /tmp/wotoha-analysis-fixtures.json --seed 123
```

Evaluate the current Wotoha V2 adapter:

```bash
cargo run --locked -p wotoha-analysis-lab -- evaluate \
  --manifest /tmp/wotoha-analysis-fixtures.json \
  --report /tmp/wotoha-analysis-report.json
```

Use the exported-WAV path for external observations:

```bash
cargo run --locked -p wotoha-analysis-lab -- evaluate-exported \
  --manifest /tmp/wotoha-blackbox-v1/manifest.json \
  --audio-root /tmp/wotoha-blackbox-v1 \
  --external-observations /tmp/observations.json \
  --report /tmp/wotoha-exported-report.json
```

`--mode hybrid` exercises the existing
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

Schema-v2 fixture specs require a present `meter_truth` field. Values `2`,
`3`, `4`, `6`, and explicit `null` are valid; omission is rejected. This
preserves the distinction between unknown evaluation truth and an invalid
fixture.

Runs do not use `.wotoha-analysis/` or the production analysis cache. Generated
manifests, reports, and run directories belong in `/tmp`, `target`, or the
ignored lab paths.

## Ground-truth research pass

The complete exported-WAV research pass writes all required artifacts under a
directory outside Git:

```bash
WOTOHA_SOURCE_COMMIT=$(git rev-parse HEAD) \
WOTOHA_STARTING_COMMIT=c61fb55a0ead23837ee5b1470438324b2b793294 \
cargo run --release --locked -p wotoha-analysis-lab -- research-pass \
  --manifest /tmp/wotoha-blackbox-v1/manifest.json \
  --audio-root /tmp/wotoha-blackbox-v1 \
  --output /tmp/wotoha-groundtruth-research
```

This decodes each exported WAV and runs both backends on the same PCM. It
writes `current-classical.json`, `current-hybrid.json`, `backend-oracle.json`,
`gate-research.json`, `tempo-refinement.json`, `meter-research.json`,
`research-summary.json`, and `research-summary.md`. The source commit is
required in the report metadata.

The backend oracle is truth-only and cannot select a production backend. It
reports Always Classical, current Hybrid, Always Neural where native output
exists, and separate beat, tempo, and grid-phase oracle summaries. There is no
single combined oracle score: each dimension selects its own truth-relative
backend. The dimension-aware joint label is `neural_dominates`,
`classical_dominates`, `mixed`, `equal`, or `both_invalid`. A backend dominates
only with no material regression (1 ms beat MAE, 5 ms beat p95, or 0.02
precision/recall thresholds) and at least one material improvement.

The gate uses only pre-decision diagnostics. Primary validation is deterministic
leave-one-connected-leakage-group-out cross-validation. Exact PCM hashes and
base/transform lineage roots are joined transitively into one connected
partition, with a stable group ID. Every fixture receives exactly one primary
OOF decision fitted without its group. Each fold reports direct PCM-hash and
lineage-root overlap checks; both must be empty. Leave-family-out remains a
secondary stress test. Fixture ID, filename, family, transform, expected BPM,
PCM hash, and Ground Truth are not inference features. Full-data refit metrics,
if present, are explicitly non-held-out. Its risk-first threshold search
minimizes false accepts of bad neural output; zero neural coverage is reported
directly as an Always Classical result.

Tempo refinement searches a bounded fractional period neighborhood at
0.1-frame resolution and interpolates activation while jointly searching phase.
Its primary cohort is scalar-tempo fixtures accepted by native neural
analysis, with raw neural observations and a production neural BPM. Classical
fallbacks are excluded from the primary cohort and may only appear in a
separately labeled exploratory section. It preserves the production selection
family (`selection_relation_before/after`) while truth-relative outcomes
(`truth_relation_before/after`) are evaluated independently. It changes only a
research tempo label; production `BeatEvent[]` is never regenerated. Variable
tempo is excluded from global BPM accuracy. The existing PCM-envelope and
activation-domain experiments remain diagnostic and are not combined or
promoted.

### Classical rhythm + Neural tempo advisor

The dedicated research command keeps Classical authoritative for the beat
timeline, grid phase, meter, and downbeats while comparing three label-only
Neural tempo candidates: the production Neural tempo, fractional activation
refinement, and robust BeatEvent interval refinement. It writes all output
outside Git:

```bash
WOTOHA_SOURCE_COMMIT=$(git rev-parse HEAD) \
WOTOHA_STARTING_COMMIT=$(git rev-parse HEAD) \
cargo run --release --locked -p wotoha-analysis-lab -- research-tempo-advisor \
  --manifest /tmp/wotoha-blackbox-v1/manifest.json \
  --audio-root /tmp/wotoha-blackbox-v1 \
  --output /tmp/wotoha-tempo-advisor
```

The event estimator uses decoded Neural event intervals, a median/MAD bounded
trim, fit residual, and an early/late drift check. It requires at least five
robust intervals and abstains on excessive global drift. It preserves the
production half/native/double family relation and never changes event
timestamps. `tempo-advisor-research.json` reports candidate metrics, the
Ground-Truth oracle upper bound, the Classical failure budget, frozen
pre-truth feature inventory, nested grouped OOF advisor decisions, and
component-expanded leave-family-out stress. Full-data refit is explicitly not
held-out evidence. The command is research-only and cannot select a production
backend.

The family stress folds are expanded by connected PCM+lineage components. A
nominal family fold is invalid as generalization evidence if either exact PCM
or lineage overlap is present; the expanded validation sample list is retained
in the report.

### Fixed-tempo period consensus research

`research-real-songs` records a blind, runtime-observable analysis report for
development-only fixed-tempo investigations. `research-fixed-tempo-consensus`
then compares bounded physical-period estimators without consuming an external
tempo during inference:

```bash
WOTOHA_SOURCE_COMMIT=$(git rev-parse HEAD) \
WOTOHA_STARTING_COMMIT=$(git rev-parse HEAD) \
cargo run --release --locked -p wotoha-analysis-lab -- \
  research-real-songs \
  --audio-root /tmp/wotoha-songs \
  --output /tmp/wotoha-real-song-research

cargo run --release --locked -p wotoha-analysis-lab -- \
  research-fixed-tempo-consensus \
  --input /tmp/wotoha-real-song-research/real-song-research.json \
  --output /tmp/wotoha-fixed-tempo-consensus
```

The consensus report keeps physical-period estimation separate from canonical
primary/half/double layer selection. It compares local interval medians,
segment clocks, bounded global regressions, endpoint estimates, the existing
robust grid fit, and Classical candidate evidence. Each candidate records its
source, derivation, and evidence channel. Fixed representative clusters use
only the best candidate from each channel, so sequential and missing-jump
estimators cannot masquerade as independent votes. Unresolved independent
conflicts produce `RETAIN_MULTIPLE` or `ABSTAIN`.

Candidate phase is fitted from a bounded sample of event remainders and fixed
phase buckets with a trimmed residual objective. The first BeatEvent is not a
phase anchor, so isolated endpoint errors do not shift the global candidate.
The report exposes `SELECTED_STRONG`, `SELECTED_MODERATE`, `RETAIN_MULTIPLE`,
and `ABSTAIN` confidence states, plus a deterministic synthetic hardening
suite covering non-integer periods, missing/extra events, endpoint corruption,
nearby false periods, correlated-channel duplication, metrical alternatives,
and non-stationarity. The synthetic suite enters the same report construction,
candidate scoring, channel aggregation, clustering, conflict detection,
selection, confidence, and canonical-layer path as the real-song report. Truth
is joined only afterward by an evaluator; `false_confident` is therefore
derived from the actual selector result, not from a fixture label. Run it with:

```bash
WOTOHA_SOURCE_COMMIT=$(git rev-parse HEAD) \
cargo run --release --locked -p wotoha-analysis-lab -- \
  research-fixed-tempo-consensus-synthetic \
  --output /tmp/wotoha-fixed-tempo-consensus-synthetic
```

The E2E report records `CORRECT_CONFIDENT`, `CORRECT_ACCEPTABLE`,
`SAFE_RETAIN_MULTIPLE`, `SAFE_ABSTAIN`, `FALSE_CONFIDENT`, and
`UNEXPECTED_FAILURE` per fixture. It also records a selector-level channel
duplication invariance check. The command is research-only, uses no external
observer value as an inference feature, and does not change beat events,
production tempo authority, or AutoMix behavior.

### Conservative tempo shadow

`research-tempo-conservative-shadow` is a follow-up research command. It
reuses the long-duration candidate-flow audit, then evaluates bounded Classical
candidate propagation, source-aware ranking, explicit `Select` /
`RetainMultiple` / `Abstain` decisions, variable-tempo stationarity, and a
separate metrical-consistency guard. The guard compares a candidate hypothesis
with the observed BeatEvent clock through primary/half/double relations; it is
not a replacement for the production quality guard.

```bash
WOTOHA_SOURCE_COMMIT=$(git rev-parse HEAD) \
WOTOHA_STARTING_COMMIT=$(git rev-parse HEAD) \
cargo run --release --locked -p wotoha-analysis-lab -- \
  research-tempo-conservative-shadow \
  --output /tmp/wotoha-tempo-conservative-shadow
```

The command writes candidate provenance, ranking and abstention operating
points, evidence attribution, stationarity results, adversarial metrical
consistency cases, candidate-count pressure, pruning summaries, and
component-aware family stress reports outside Git. All thresholds are fixed
research rules; no Ground Truth, fixture identity, external observation, or
production output is used as an inference feature. Classical remains the
production beat/grid/tempo authority and all AutoMix results are shadow-only.
The effective shadow matrix now sends the selected, retained, or abstained
tempo hypotheses through the existing V2 planner and reports baseline and
effective transitions separately. It also records runtime-feasible versus
offline-only features, single-pass interval-consistency diagnostics, and a
reason for every non-selected decision. Invalid metrical pairs must end in a
non-BeatMatched effective transition, while declared harmonic aliases remain
representable without requiring that the shadow planner select them.

The bounded arrangement-like corpus can also be run independently when the
full conservative report is not needed:

```bash
WOTOHA_SOURCE_COMMIT=$(git rev-parse HEAD) \
WOTOHA_STARTING_COMMIT=$(git rev-parse HEAD) \
cargo run --release --locked -p wotoha-analysis-lab -- \
  research-tempo-realistic-shadow \
  --output /tmp/wotoha-realistic-shadow
```

This command generates deterministic 30-second and 60-second mono fixtures
with intro/build/drop/breakdown/outro evidence-density changes, sparse and
syncopated variants, bounded candidate-cap measurements, and a small set of
transition pairs. It records baseline and effective planner outcomes and
requires safe fallback for tempo-mismatched pairs. The generated corpus is
research-only: its known beat clock is useful for safety checks but is not a
replacement for ecological or external validation.

Meter research evaluates fixed 2/3/4/6 meter × phase candidates using target
downbeat evidence, off-phase leakage, periodic consistency, bar-cycle
consistency, and beat-event confidence. Its primary set is only the `Meter`
fixture family: explicit `meter_truth` values are clear scored fixtures and
explicit `meter_truth: null` values are ambiguous. Ordinary 4/4 constant and
transform fixtures are excluded, and no fixture ID has semantic meaning. A
minimum score and margin may return `Unknown`. Recovering or failing to recover
3/4 or 6/8 in this downstream experiment does not by itself prove that the
upstream four-phase prior is causal; the report records descriptive evidence
and retains the production prior/resolver unchanged.

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
winner. `tempo_experiment` contains two explicitly named lab-only resolvers
on every fixture: `pcm` for the PCM-envelope experiment and `activation` for
the raw Beat This activation experiment. Candidate BPM/half/double relations
are scored with activation support, coverage, periodic consistency, phase,
off-grid leakage where applicable, and ambiguity. Truth-relative relations
are reported separately as `primary`, `half_time`, `double_time`,
`other_wrong`, `ambiguous`, or `absent`; internal candidate labels are never
used as truth relations. These results are observational and do not alter the
production tempo hypotheses.

Beat metrics use a deterministic monotonic one-to-one matcher and report MAE,
p50, p95, and precision/recall at 10, 20, 40, and 70 ms. Per-track percentiles
remain per-track; overall p50/p95 are pooled over every matched beat error, and
overall MAE and precision/recall are observation-weighted micro metrics. The
same canonical pooled/micro aggregator is used for selected-backend research
baselines, OOF gate results, and the beat oracle; no per-track macro average is
substituted. Tempo
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

Observer product, version, platform, and materially different public analysis
settings are grouped separately rather than silently combined. Platform is
serialized in every observer report. The report includes total,
complete/incomplete, beat, tempo, meter, downbeat, and grid-phase observation
counts per group. Real
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

The synthetic corpus schema is version 2 because meter truth is now explicit;
the external observation schema is version 2 because transferred identity is
named `wav_file_sha256`; the black-box schema is version 2 for the same explicit
hash semantics. The report schema is version 6 because neural tempo candidate
evidence and truth-relative relation fields have candidate-specific semantics.
Research artifacts use research schema version 2 because their gate, oracle,
meter, and tempo semantics changed in this audit. Old manifests, observations,
and reports must not be compared silently with new schemas.

## Classical tempo research

`research-classical-tempo` is a bounded, research-only command for studying
the existing Classical analyzer on the exported-WAV corpus. It verifies each
WAV, decoded PCM identity, and Ground Truth hash before analysis, then writes
JSON and Markdown outside Git:

```bash
WOTOHA_SOURCE_COMMIT=$(git rev-parse HEAD) \
  cargo run --release --locked -p wotoha-analysis-lab -- \
  research-classical-tempo \
  --manifest /tmp/wotoha-blackbox-v1/manifest.json \
  --audio-root /tmp/wotoha-blackbox-v1 \
  --output /tmp/wotoha-classical-tempo-research
```

The report includes the baseline, robust median/MAD tempo estimates derived
only from final Classical markers, residual classifications, full/low-band
top-eight autocorrelation diagnostics with adjacent-lag clustering, harmonic
families, lower-bound search counterfactuals, confidence distributions,
research-only two-dimensional abstention selectors, marker-clock consistency,
same-master-prefix duration sensitivity, variable-tempo safety, transform
metadata, and three-run determinism checks. Selector thresholds are frozen
research parameters and are evaluated on every scalar-tempo fixture; they are
not production logic. Variable-tempo fixtures are retained as diagnostics and
excluded from scalar-tempo accuracy. The command never changes beat markers,
phase, downbeats, meter, backend selection, or production tempo behavior.

## Long-duration tempo ambiguity research

`research-tempo-ambiguity` generates a bounded, deterministic, vendor-neutral
30-second and 60-second benchmark. It records Classical full/low-band peaks,
Neural selected-grid and half/native/double candidate evidence, V2 beat-event
intervals and tempo hypotheses, and controlled V1/V2 AutoMix planner outcomes.
The benchmark includes a fine tempo sweep around the lower Classical boundary
and 130 BPM, plus generic accent/subdivision archetypes and a small percussion
robustness slice. Durations below 20 seconds are not part of its main score.

```bash
WOTOHA_SOURCE_COMMIT=$(git rev-parse HEAD) \
WOTOHA_STARTING_COMMIT=$(git rev-parse HEAD) \
  cargo run --release --locked -p wotoha-analysis-lab -- \
  research-tempo-ambiguity \
  --output /tmp/wotoha-tempo-ambiguity
```

The command writes the required JSON, CSV, and Markdown reports under the
requested output directory. It is research-only: no production tempo bound,
selector, beat-event timeline, AutoMix threshold, or authority is changed.
The V2 planner is observed through its existing hypothesis cross-product,
physical eligibility, phase evidence, and quality guard. Any follow-up
ranking work must be validated on held-out rhythm families and must preserve
the default production behavior until separately promoted.

## Tempo shadow follow-up

The research-tempo-shadow command is the next-stage, research-only audit for
long-duration tempo ambiguity. It keeps candidate generation, propagation,
ranking, and event-clock refinement in separate reported pools. Event-clock
variants preserve the selected Neural half/native/double relation; they do
not resolve that relation from Ground Truth.

Command:

    WOTOHA_SOURCE_COMMIT=$(git rev-parse HEAD) \
    WOTOHA_STARTING_COMMIT=$(git rev-parse HEAD) \
      cargo run --release --locked -p wotoha-analysis-lab -- \
      research-tempo-shadow \
      --output /tmp/wotoha-tempo-shadow

The command writes candidate-flow CSV/JSON, re-anchor and failure taxonomy
reports, duration-invariance decomposition, a strict known-positive
BeatMatched planner harness, an alias shadow matrix with deterministic preview
quality metrics, fixed-rule component-expanded family stress results, feature
inventory, and the final Markdown/JSON summary. The positive harness uses
synthetic truth only to construct a planner sanity case; it is not an inference
feature or a production label. The shadow output can change only a reported
tempo candidate. Classical beat events, grid phase, meter, downbeats, AutoMix,
runtime, and playback remain unchanged.

## Final clean-room handoff

The lab keeps three tempo interpretations separate: the current production
resolver, the PCM-envelope research resolver, and the raw Beat This activation
resolver. The latter scores each half/native/double candidate directly from
the activation stream with its own phase search, coverage, off-grid leakage,
periodic consistency, and bounded score. It is observational only and cannot
change production beat events, tempo selection, or confidence.

Synthetic meter truth is explicit in `FixtureSpec.meter_truth`. The synthesis
meter may be known while evaluation truth is unknown; fixture IDs have no
semantic effect. The current neural downbeat decoder still uses a four-phase
prior, so 3/4 and 6/8 results must be interpreted with that limitation in
mind.

Generate and package the handoff with the same fixed seed:

```bash
cargo run --release --locked -p wotoha-analysis-lab -- \
  export-blackbox --output /tmp/wotoha-blackbox-v1 --seed 246813579
cargo run --release --locked -p wotoha-analysis-lab -- \
  package-blackbox --input /tmp/wotoha-blackbox-v1 \
  --output /tmp/wotoha-blackbox-v1.zip
cargo run --release --locked -p wotoha-analysis-lab -- \
  verify-blackbox /tmp/wotoha-blackbox-v1.zip
```

Packaging uses stable ordering, normalized timestamps, stored entries, bounded
file sizes, safe relative paths, duplicate detection, CRC checks, manifest and
PCM/Ground Truth hash verification, and observation-template identity checks.
The packaged README explicitly instructs observers: **Record external analysis
output before inspecting Ground Truth.**
The ZIP, WAVs, and reports belong under `/tmp` and are not repository or
container artifacts.

Final exported-WAV baselines use `evaluate-exported` in both Hybrid and
Classical modes. Reports contain schema version, analyzer mode, source commit,
production/PCM/activation tempo results, explicit regressions, meter evidence,
and high-pass evidence-ablation metrics. External DJ software remains a
reference observation, never Ground Truth.
