use std::{fs, path::Path, time::Duration};

use serde::Serialize;

use super::{
    BlackboxManifest, DEFAULT_SAMPLE_RATE, FixtureFamily, FixtureSpec, LabError, TempoProfile,
    build_truth, decode_wav_pcm16, downmix, ground_truth_sha256, hash_bytes, resample,
    synthesize_audio,
};

const MARKER_MIN_INTERVALS: usize = 3;
const MARKER_MIN_INLIERS: usize = 3;
const MARKER_MIN_BPM: f32 = 30.0;
const MARKER_MAX_BPM: f32 = 240.0;
const VARIABLE_TEMPO_RELATIVE_SPREAD_LIMIT: f64 = 0.08;

#[derive(Clone, Debug, Serialize)]
pub struct ClassicalTempoResearchReport {
    pub schema_version: u32,
    pub evaluator: String,
    pub source_commit: String,
    pub corpus: CorpusIdentity,
    pub baseline: ScalarMetrics,
    pub confidence_distribution: ConfidenceDistribution,
    pub marker_refinement: ScalarMetrics,
    pub abstention_sweep: Vec<AbstentionResult>,
    pub lower_bound_sweep: Vec<LowerBoundResult>,
    pub candidate_matrix: Vec<CandidateMatrixResult>,
    pub per_fixture: Vec<ClassicalTempoFixtureReport>,
    pub duration_sensitivity: Vec<DurationObservation>,
    pub variable_tempo: Vec<VariableTempoObservation>,
    pub transform_audit: Vec<TransformAudit>,
    pub repeatability: RepeatabilityReport,
    pub promotion_candidates: Vec<PromotionCandidate>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CorpusIdentity {
    pub manifest: String,
    pub schema_version: u32,
    pub corpus_schema_version: u32,
    pub seed: u64,
    pub fixture_count: usize,
    pub exported_wav_identity_verified: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ScalarMetrics {
    pub scored: usize,
    pub returned: usize,
    pub correct: usize,
    pub canonical_accuracy: Option<f64>,
    pub coverage: Option<f64>,
    pub precision: Option<f64>,
    pub false_returns: usize,
    pub abstentions: usize,
    pub half_time: usize,
    pub double_time: usize,
    pub other_wrong: usize,
    pub absent: usize,
    pub absolute_bpm_mae: Option<f64>,
    pub absolute_bpm_median: Option<f64>,
    pub absolute_bpm_p95: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConfidenceDistribution {
    pub correct: ConfidenceStats,
    pub incorrect: ConfidenceStats,
    pub absent: ConfidenceStats,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ConfidenceStats {
    pub count: usize,
    pub values: Vec<f32>,
    pub minimum: Option<f32>,
    pub median: Option<f32>,
    pub maximum: Option<f32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AbstentionResult {
    pub confidence_threshold: f32,
    pub metrics: ScalarMetrics,
    pub returned_sample_ids: Vec<String>,
    pub abstained_sample_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LowerBoundResult {
    pub minimum_bpm: f32,
    pub maximum_bpm: f32,
    pub metrics: ScalarMetrics,
    pub rescued_sample_ids: Vec<String>,
    pub regressed_sample_ids: Vec<String>,
    pub half_double_relation_shifts: Vec<RelationShift>,
    pub mean_confidence_delta: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RelationShift {
    pub sample_id: String,
    pub before: String,
    pub after: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CandidateMatrixResult {
    pub candidate: String,
    pub metrics: ScalarMetrics,
    pub rescued_sample_ids: Vec<String>,
    pub regressed_sample_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClassicalTempoFixtureReport {
    pub sample_id: String,
    pub family: String,
    pub truth_bpm: Option<f32>,
    pub baseline_bpm: Option<f32>,
    pub baseline_confidence: Option<f32>,
    pub baseline_relation: String,
    pub baseline_marker_count: usize,
    pub baseline_marker_bpm: Option<f32>,
    pub baseline_marker_relation: String,
    pub marker_median_interval_micros: Option<f64>,
    pub marker_mad_micros: Option<f64>,
    pub marker_tolerance_micros: Option<f64>,
    pub marker_inlier_count: usize,
    pub marker_available: bool,
    pub marker_rejection_reason: Option<String>,
    pub full_band: BandReport,
    pub low_band: BandReport,
    pub low_energy_ratio: f32,
    pub kick_reliable: bool,
    pub selected_source: Option<String>,
    pub top_peak_harmonics: Vec<String>,
    pub lower_bounds: Vec<BoundFixtureResult>,
    pub marker_rescued: bool,
    pub marker_regressed: bool,
    pub marker_relative_error: Option<f64>,
    pub marker_clock_consistent: bool,
    pub beat_density_per_second: Option<f64>,
    pub phase_micros: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BandReport {
    pub bpm: Option<f32>,
    pub confidence: Option<f32>,
    pub beat_lag_blocks: Option<f32>,
    pub peaks: Vec<PeakReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PeakReport {
    pub lag_blocks: usize,
    pub bpm: f32,
    pub raw_score: f32,
    pub normalized_confidence: f32,
    pub rank: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct BoundFixtureResult {
    pub minimum_bpm: f32,
    pub bpm: Option<f32>,
    pub confidence: Option<f32>,
    pub relation: String,
    pub marker_bpm: Option<f32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DurationObservation {
    pub truth_bpm: f32,
    pub duration_seconds: u32,
    pub master_generated_once: bool,
    pub sample_count: usize,
    pub bpm: Option<f32>,
    pub confidence: Option<f32>,
    pub marker_bpm: Option<f32>,
    pub marker_available: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct VariableTempoObservation {
    pub sample_id: String,
    pub family: String,
    pub baseline_bpm: Option<f32>,
    pub baseline_confidence: Option<f32>,
    pub marker_bpm: Option<f32>,
    pub marker_median_interval_micros: Option<f64>,
    pub marker_mad_micros: Option<f64>,
    pub marker_relative_spread: Option<f64>,
    pub scalar_score_excluded: bool,
    pub global_marker_refinement_would_abstain: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct TransformAudit {
    pub sample_id: String,
    pub base_id: Option<String>,
    pub transform: String,
    pub sample_rate: u32,
    pub duration_micros: u64,
    pub declared_duration_matches_pcm: bool,
    pub baseline_bpm: Option<f32>,
    pub marker_bpm: Option<f32>,
    pub tempo_time_consistent: bool,
    pub invariance_expectation: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RepeatabilityReport {
    pub repetitions_per_fixture: usize,
    pub fixture_count: usize,
    pub mismatch_count: usize,
    pub mismatch_sample_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PromotionCandidate {
    pub name: String,
    pub description: String,
    pub metrics: ScalarMetrics,
    pub max_bpm_range: String,
    pub promotion_status: String,
}

#[derive(Clone, Debug, Default)]
struct MarkerEstimate {
    bpm: Option<f32>,
    median_interval_micros: Option<f64>,
    mad_micros: Option<f64>,
    tolerance_micros: Option<f64>,
    inlier_count: usize,
    relative_spread: Option<f64>,
    available: bool,
    rejection_reason: Option<String>,
    phase_micros: Option<i64>,
}

#[derive(Clone, Debug)]
struct FixtureRun {
    fixture: BlackboxFixtureView,
    audio: Vec<f32>,
    baseline: wotoha_core::automix::TrackAnalysis,
    diagnostics: wotoha_core::audio_analysis::ClassicalTempoDiagnostics,
    marker: MarkerEstimate,
    lower_bounds: Vec<(f32, Option<wotoha_core::automix::TrackAnalysis>)>,
}

#[derive(Clone, Debug)]
struct BlackboxFixtureView {
    id: String,
    spec: FixtureSpec,
    truth: super::AnalysisGroundTruth,
    sample_rate: u32,
    sample_count: usize,
}

pub fn run_classical_tempo_research(
    manifest_path: &Path,
    audio_root: &Path,
    output: &Path,
    source_commit: String,
) -> Result<ClassicalTempoResearchReport, LabError> {
    let blackbox = super::BlackboxManifest::load(manifest_path)?;
    let mut runs = Vec::with_capacity(blackbox.fixtures.len());
    for fixture in &blackbox.fixtures {
        let path = audio_root.join(&fixture.audio_file);
        let bytes = fs::read(&path)?;
        let actual_wav_hash = hash_bytes(&bytes);
        if actual_wav_hash != fixture.audio_sha256 {
            return Err(LabError::HashMismatch {
                sample_id: fixture.sample_id.clone(),
                expected: fixture.audio_sha256.clone(),
                actual: actual_wav_hash,
            });
        }
        let decoded = decode_wav_pcm16(&bytes)?;
        if decoded.pcm_sha256 != fixture.pcm_sha256
            || decoded.sample_rate != fixture.sample_rate
            || decoded.channels != fixture.channels
            || decoded.samples.len() != fixture.sample_count
        {
            return Err(LabError::InvalidInput(format!(
                "WAV or PCM verification failed for {}",
                fixture.sample_id
            )));
        }
        if ground_truth_sha256(&fixture.truth)? != fixture.ground_truth_sha256 {
            return Err(LabError::HashMismatch {
                sample_id: fixture.sample_id.clone(),
                expected: fixture.ground_truth_sha256.clone(),
                actual: ground_truth_sha256(&fixture.truth)?,
            });
        }
        let audio = downmix(&decoded.samples, decoded.channels);
        let audio = if decoded.sample_rate == DEFAULT_SAMPLE_RATE {
            audio
        } else {
            resample(&audio, decoded.sample_rate, DEFAULT_SAMPLE_RATE)
        };
        let baseline = wotoha_core::audio_analysis::analyze_mono_pcm(&audio, DEFAULT_SAMPLE_RATE)
            .ok_or_else(|| {
            LabError::InvalidInput(format!("analysis rejected {}", fixture.sample_id))
        })?;
        let low_band = low_band(&audio);
        let diagnostics = wotoha_core::audio_analysis::diagnose_classical_tempo(
            &audio,
            &low_band,
            DEFAULT_SAMPLE_RATE,
            wotoha_core::audio_analysis::CLASSICAL_MIN_BPM,
            wotoha_core::audio_analysis::CLASSICAL_MAX_BPM,
            5,
        )
        .ok_or_else(|| {
            LabError::InvalidInput(format!("diagnostics rejected {}", fixture.sample_id))
        })?;
        let marker = estimate_markers(&baseline.beat_markers, baseline.duration);
        let mut lower_bounds = Vec::new();
        for minimum_bpm in [60.0, 62.5, 65.0, 67.5, 70.0] {
            let analysis =
                wotoha_core::audio_analysis::analyze_mono_pcm_with_low_band_and_tempo_bounds(
                    &audio,
                    &low_band,
                    DEFAULT_SAMPLE_RATE,
                    minimum_bpm,
                    wotoha_core::audio_analysis::CLASSICAL_MAX_BPM,
                );
            lower_bounds.push((minimum_bpm, analysis));
        }
        runs.push(FixtureRun {
            fixture: BlackboxFixtureView {
                id: fixture.sample_id.clone(),
                spec: fixture.spec.clone(),
                truth: fixture.truth.clone(),
                sample_rate: decoded.sample_rate,
                sample_count: decoded.samples.len(),
            },
            audio,
            baseline,
            diagnostics,
            marker,
            lower_bounds,
        });
    }

    let scalar_indices = runs
        .iter()
        .enumerate()
        .filter(|(_, run)| run.fixture.truth.tempo.is_some())
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let baseline_values = scalar_indices
        .iter()
        .map(|index| runs[*index].baseline.bpm)
        .collect::<Vec<_>>();
    let marker_values = scalar_indices
        .iter()
        .map(|index| runs[*index].marker.bpm)
        .collect::<Vec<_>>();
    let truths = scalar_indices
        .iter()
        .map(|index| {
            runs[*index]
                .fixture
                .truth
                .tempo
                .as_ref()
                .map(|tempo| tempo.primary_bpm)
        })
        .collect::<Vec<_>>();

    let baseline = scalar_metrics(&baseline_values, &truths);
    let confidence_distribution = confidence_distribution(&runs, &scalar_indices);
    let marker_refinement = scalar_metrics(&marker_values, &truths);
    let abstention_sweep = abstention_sweep(&runs, &scalar_indices, &truths);
    let lower_bound_sweep = lower_bound_sweep(&runs, &scalar_indices, &truths);
    let candidate_matrix = candidate_matrix(&runs, &scalar_indices, &truths);
    let per_fixture = runs.iter().map(fixture_report).collect::<Vec<_>>();
    let duration_sensitivity = duration_sensitivity(&blackbox)?;
    let variable_tempo = runs
        .iter()
        .filter(|run| {
            matches!(
                run.fixture.spec.tempo,
                TempoProfile::LinearRamp { .. } | TempoProfile::StepReturn { .. }
            )
        })
        .map(variable_report)
        .collect::<Vec<_>>();
    let transform_audit = runs
        .iter()
        .filter(|run| matches!(run.fixture.spec.family, FixtureFamily::Transform))
        .map(transform_report)
        .collect::<Vec<_>>();
    let repeatability = repeatability(&runs);
    let promotion_candidates = vec![
        PromotionCandidate {
            name: "A1_marker_refinement".into(),
            description: "Use a robust median/MAD estimate from final Classical beat markers."
                .into(),
            metrics: marker_refinement.clone(),
            max_bpm_range: "30-240 research guard".into(),
            promotion_status: "research_only; requires held-out evidence".into(),
        },
        PromotionCandidate {
            name: "A2_confidence_abstention".into(),
            description:
                "Retain the baseline BPM only above a pre-registered confidence threshold.".into(),
            metrics: abstention_sweep
                .iter()
                .min_by(|left, right| {
                    left.metrics
                        .false_returns
                        .cmp(&right.metrics.false_returns)
                        .then_with(|| right.metrics.correct.cmp(&left.metrics.correct))
                })
                .map(|result| result.metrics.clone())
                .unwrap_or_default(),
            max_bpm_range: "default 70-180".into(),
            promotion_status: "research_only; abstention is not a production behavior change"
                .into(),
        },
        PromotionCandidate {
            name: "A3_marker_plus_abstention".into(),
            description:
                "Marker-derived BPM with conservative confidence and clock-consistency guards."
                    .into(),
            metrics: candidate_matrix
                .iter()
                .find(|candidate| candidate.candidate == "A3_marker_plus_abstention")
                .map(|candidate| candidate.metrics.clone())
                .unwrap_or_default(),
            max_bpm_range: "30-240 research guard".into(),
            promotion_status: "research_only; no promotion".into(),
        },
    ];
    let report = ClassicalTempoResearchReport {
        schema_version: 1,
        evaluator: format!(
            "wotoha-analysis-lab/{}/classical-tempo",
            env!("CARGO_PKG_VERSION")
        ),
        source_commit,
        corpus: CorpusIdentity {
            manifest: manifest_path.display().to_string(),
            schema_version: blackbox.schema_version,
            corpus_schema_version: blackbox.corpus_schema_version,
            seed: blackbox.seed,
            fixture_count: blackbox.fixtures.len(),
            exported_wav_identity_verified: true,
        },
        baseline,
        confidence_distribution,
        marker_refinement,
        abstention_sweep,
        lower_bound_sweep,
        candidate_matrix,
        per_fixture,
        duration_sensitivity,
        variable_tempo,
        transform_audit,
        repeatability,
        promotion_candidates,
    };
    fs::create_dir_all(output)?;
    super::write_json(&output.join("classical-tempo-research.json"), &report)?;
    fs::write(
        output.join("classical-tempo-research.md"),
        markdown_report(&report),
    )?;
    Ok(report)
}

fn low_band(samples: &[f32]) -> Vec<f32> {
    let mut filter = wotoha_core::audio_analysis::LowBandFilter::new(DEFAULT_SAMPLE_RATE)
        .expect("fixed research sample rate is valid");
    samples
        .iter()
        .map(|sample| filter.process(*sample))
        .collect()
}

fn scalar_metrics(values: &[Option<f32>], truths: &[Option<f32>]) -> ScalarMetrics {
    let mut metrics = ScalarMetrics {
        scored: truths.iter().filter(|truth| truth.is_some()).count(),
        ..Default::default()
    };
    let mut errors = Vec::new();
    for (value, truth) in values.iter().zip(truths) {
        let Some(truth) = truth else { continue };
        let Some(value) = value else {
            metrics.absent += 1;
            continue;
        };
        metrics.returned += 1;
        errors.push(f64::from((value - truth).abs()));
        match relation(*value, *truth).as_str() {
            "primary" => metrics.correct += 1,
            "half_time" => metrics.half_time += 1,
            "double_time" => metrics.double_time += 1,
            _ => metrics.other_wrong += 1,
        }
    }
    metrics.abstentions = metrics.scored.saturating_sub(metrics.returned);
    metrics.false_returns = metrics.returned.saturating_sub(metrics.correct);
    metrics.canonical_accuracy = ratio(metrics.correct, metrics.scored);
    metrics.coverage = ratio(metrics.returned, metrics.scored);
    metrics.precision = ratio(metrics.correct, metrics.returned);
    metrics.absolute_bpm_mae = mean(&errors);
    metrics.absolute_bpm_median = percentile(&errors, 0.5);
    metrics.absolute_bpm_p95 = percentile(&errors, 0.95);
    metrics
}

fn confidence_distribution(runs: &[FixtureRun], indices: &[usize]) -> ConfidenceDistribution {
    let mut correct = Vec::new();
    let mut incorrect = Vec::new();
    let mut absent = Vec::new();
    for index in indices {
        let run = &runs[*index];
        let confidence = run.baseline.beat_confidence;
        match run.baseline.bpm.zip(
            run.fixture
                .truth
                .tempo
                .as_ref()
                .map(|truth| truth.primary_bpm),
        ) {
            Some((bpm, truth)) if relation(bpm, truth) == "primary" => correct.push(confidence),
            Some(_) => incorrect.push(confidence),
            None => absent.push(confidence),
        }
    }
    ConfidenceDistribution {
        correct: confidence_stats(correct),
        incorrect: confidence_stats(incorrect),
        absent: confidence_stats(absent),
    }
}

fn confidence_stats(mut values: Vec<f32>) -> ConfidenceStats {
    values.sort_by(f32::total_cmp);
    ConfidenceStats {
        count: values.len(),
        minimum: values.first().copied(),
        median: values.get(values.len() / 2).copied(),
        maximum: values.last().copied(),
        values,
    }
}

fn ratio(numerator: usize, denominator: usize) -> Option<f64> {
    (denominator > 0).then_some(numerator as f64 / denominator as f64)
}

fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then_some(values.iter().sum::<f64>() / values.len() as f64)
}

fn percentile(values: &[f64], fraction: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let index = ((sorted.len() - 1) as f64 * fraction.clamp(0.0, 1.0)).round() as usize;
    sorted.get(index).copied()
}

fn relation(value: f32, truth: f32) -> String {
    let within = |expected: f32| expected > 0.0 && (value - expected).abs() / expected < 0.005;
    if within(truth) {
        "primary"
    } else if within(truth / 2.0) {
        "half_time"
    } else if within(truth * 2.0) {
        "double_time"
    } else {
        "other_wrong"
    }
    .into()
}

fn estimate_markers(markers: &[Duration], duration: Duration) -> MarkerEstimate {
    let intervals = markers
        .windows(2)
        .filter_map(|window| window[1].checked_sub(window[0]))
        .map(|interval| interval.as_micros() as f64)
        .filter(|interval| *interval > 0.0)
        .collect::<Vec<_>>();
    if intervals.len() < MARKER_MIN_INTERVALS {
        return MarkerEstimate {
            rejection_reason: Some("insufficient_intervals".into()),
            ..Default::default()
        };
    }
    let central_interval = median(&intervals);
    let deviations = intervals
        .iter()
        .map(|interval| (interval - central_interval).abs())
        .collect::<Vec<_>>();
    let mad = median(&deviations);
    let tolerance = (3.0 * 1.4826 * mad)
        .max(central_interval * 0.02)
        .max(1_000.0);
    let inliers = intervals
        .iter()
        .copied()
        .filter(|interval| (interval - central_interval).abs() <= tolerance)
        .collect::<Vec<_>>();
    if inliers.len() < MARKER_MIN_INLIERS {
        return MarkerEstimate {
            median_interval_micros: Some(central_interval),
            mad_micros: Some(mad),
            tolerance_micros: Some(tolerance),
            inlier_count: inliers.len(),
            rejection_reason: Some("insufficient_inliers".into()),
            ..Default::default()
        };
    }
    let fitted = median(&inliers);
    let bpm = 60_000_000.0 / fitted;
    let relative_spread = mad / fitted.max(1.0);
    if !(MARKER_MIN_BPM..=MARKER_MAX_BPM).contains(&(bpm as f32)) {
        return MarkerEstimate {
            median_interval_micros: Some(fitted),
            mad_micros: Some(mad),
            tolerance_micros: Some(tolerance),
            inlier_count: inliers.len(),
            relative_spread: Some(relative_spread),
            rejection_reason: Some("bpm_out_of_research_range".into()),
            ..Default::default()
        };
    }
    let phase = markers.first().map(|marker| marker.as_micros() as i64);
    let _ = duration;
    MarkerEstimate {
        bpm: Some(bpm as f32),
        median_interval_micros: Some(fitted),
        mad_micros: Some(mad),
        tolerance_micros: Some(tolerance),
        inlier_count: inliers.len(),
        relative_spread: Some(relative_spread),
        available: true,
        phase_micros: phase,
        rejection_reason: None,
    }
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}

fn abstention_sweep(
    runs: &[FixtureRun],
    indices: &[usize],
    truths: &[Option<f32>],
) -> Vec<AbstentionResult> {
    [0.0, 1e-6, 1e-5, 1e-4, 1e-3, 0.005, 0.01, 0.02]
        .into_iter()
        .map(|threshold| {
            let values = indices
                .iter()
                .map(|index| {
                    runs[*index]
                        .baseline
                        .bpm
                        .filter(|_| runs[*index].baseline.beat_confidence >= threshold)
                })
                .collect::<Vec<_>>();
            let metrics = scalar_metrics(&values, truths);
            let returned_sample_ids = indices
                .iter()
                .zip(&values)
                .filter_map(|(index, value)| {
                    value.is_some().then_some(runs[*index].fixture.id.clone())
                })
                .collect();
            let abstained_sample_ids = indices
                .iter()
                .zip(&values)
                .filter_map(|(index, value)| {
                    value.is_none().then_some(runs[*index].fixture.id.clone())
                })
                .collect();
            AbstentionResult {
                confidence_threshold: threshold,
                metrics,
                returned_sample_ids,
                abstained_sample_ids,
            }
        })
        .collect()
}

fn lower_bound_sweep(
    runs: &[FixtureRun],
    indices: &[usize],
    truths: &[Option<f32>],
) -> Vec<LowerBoundResult> {
    [60.0, 62.5, 65.0, 67.5, 70.0]
        .into_iter()
        .map(|minimum| {
            let values = indices
                .iter()
                .map(|index| bound_bpm(&runs[*index], minimum))
                .collect::<Vec<_>>();
            let metrics = scalar_metrics(&values, truths);
            let mut rescued = Vec::new();
            let mut regressed = Vec::new();
            let mut shifts = Vec::new();
            for ((index, value), truth) in indices.iter().zip(&values).zip(truths) {
                let before = runs[*index]
                    .baseline
                    .bpm
                    .map(|bpm| relation(bpm, truth.unwrap_or_default()))
                    .unwrap_or_else(|| "absent".into());
                let after = value
                    .map(|bpm| relation(bpm, truth.unwrap_or_default()))
                    .unwrap_or_else(|| "absent".into());
                if before != "primary" && after == "primary" {
                    rescued.push(runs[*index].fixture.id.clone());
                }
                if before == "primary" && after != "primary" {
                    regressed.push(runs[*index].fixture.id.clone());
                }
                if before != after
                    && (before == "half_time"
                        || before == "double_time"
                        || after == "half_time"
                        || after == "double_time")
                {
                    shifts.push(RelationShift {
                        sample_id: runs[*index].fixture.id.clone(),
                        before,
                        after,
                    });
                }
            }
            let deltas = indices
                .iter()
                .filter_map(|index| {
                    let old = runs[*index].baseline.beat_confidence;
                    let new = runs[*index]
                        .lower_bounds
                        .iter()
                        .find(|(candidate, _)| (*candidate - minimum).abs() < f32::EPSILON)
                        .and_then(|(_, analysis)| analysis.as_ref())
                        .map(|analysis| analysis.beat_confidence)?;
                    Some(f64::from(new - old))
                })
                .collect::<Vec<_>>();
            LowerBoundResult {
                minimum_bpm: minimum,
                maximum_bpm: 180.0,
                metrics,
                rescued_sample_ids: rescued,
                regressed_sample_ids: regressed,
                half_double_relation_shifts: shifts,
                mean_confidence_delta: mean(&deltas),
            }
        })
        .collect()
}

fn bound_bpm(run: &FixtureRun, minimum: f32) -> Option<f32> {
    run.lower_bounds
        .iter()
        .find(|(candidate, _)| (*candidate - minimum).abs() < f32::EPSILON)
        .and_then(|(_, analysis)| analysis.as_ref())
        .and_then(|analysis| analysis.bpm)
}

fn candidate_matrix(
    runs: &[FixtureRun],
    indices: &[usize],
    truths: &[Option<f32>],
) -> Vec<CandidateMatrixResult> {
    let candidates = [
        "A0_baseline",
        "A1_marker_refinement",
        "A2_abstention_0.005",
        "A3_marker_plus_abstention",
        "B60_marker",
        "B65_marker",
        "B70_marker",
    ];
    candidates
        .into_iter()
        .map(|candidate| {
            let values = indices
                .iter()
                .map(|index| candidate_value(&runs[*index], candidate))
                .collect::<Vec<_>>();
            let metrics = scalar_metrics(&values, truths);
            let mut rescued = Vec::new();
            let mut regressed = Vec::new();
            for ((index, value), truth) in indices.iter().zip(&values).zip(truths) {
                let before = runs[*index]
                    .baseline
                    .bpm
                    .map(|bpm| relation(bpm, truth.unwrap_or_default()))
                    .unwrap_or_else(|| "absent".into());
                let after = value
                    .map(|bpm| relation(bpm, truth.unwrap_or_default()))
                    .unwrap_or_else(|| "absent".into());
                if before != "primary" && after == "primary" {
                    rescued.push(runs[*index].fixture.id.clone());
                }
                if before == "primary" && after != "primary" {
                    regressed.push(runs[*index].fixture.id.clone());
                }
            }
            CandidateMatrixResult {
                candidate: candidate.into(),
                metrics,
                rescued_sample_ids: rescued,
                regressed_sample_ids: regressed,
            }
        })
        .collect()
}

fn candidate_value(run: &FixtureRun, candidate: &str) -> Option<f32> {
    match candidate {
        "A0_baseline" => run.baseline.bpm,
        "A1_marker_refinement" => run.marker.bpm,
        "A2_abstention_0.005" => run
            .baseline
            .bpm
            .filter(|_| run.baseline.beat_confidence >= 0.005),
        "A3_marker_plus_abstention" => run.marker.bpm.filter(|_| {
            run.marker.available
                && run.marker.relative_spread.unwrap_or(1.0) <= VARIABLE_TEMPO_RELATIVE_SPREAD_LIMIT
        }),
        "B60_marker" => bound_bpm(run, 60.0),
        "B65_marker" => bound_bpm(run, 65.0),
        "B70_marker" => bound_bpm(run, 70.0),
        _ => None,
    }
}

fn band_report(band: &wotoha_core::audio_analysis::ClassicalTempoBandDiagnostics) -> BandReport {
    BandReport {
        bpm: band.bpm,
        confidence: band.confidence,
        beat_lag_blocks: band.beat_lag_blocks,
        peaks: band
            .peaks
            .iter()
            .enumerate()
            .map(|(rank, peak)| PeakReport {
                lag_blocks: peak.lag_blocks,
                bpm: peak.bpm,
                raw_score: peak.raw_score,
                normalized_confidence: peak.normalized_confidence,
                rank: rank + 1,
            })
            .collect(),
    }
}

fn fixture_report(run: &FixtureRun) -> ClassicalTempoFixtureReport {
    let truth = run
        .fixture
        .truth
        .tempo
        .as_ref()
        .map(|tempo| tempo.primary_bpm);
    let baseline_relation = run
        .baseline
        .bpm
        .zip(truth)
        .map(|(bpm, truth)| relation(bpm, truth))
        .unwrap_or_else(|| "absent".into());
    let marker_relation = run
        .marker
        .bpm
        .zip(truth)
        .map(|(bpm, truth)| relation(bpm, truth))
        .unwrap_or_else(|| "absent".into());
    let marker_rescued = baseline_relation != "primary" && marker_relation == "primary";
    let marker_regressed = baseline_relation == "primary" && marker_relation != "primary";
    let marker_relative_error = run
        .marker
        .bpm
        .zip(truth)
        .map(|(bpm, truth)| f64::from((bpm - truth).abs() / truth));
    let top_peak_harmonics = harmonics(&run.diagnostics.full_band.peaks);
    let phase_micros = run.marker.phase_micros;
    let duration_seconds = run.baseline.duration.as_secs_f64();
    ClassicalTempoFixtureReport {
        sample_id: run.fixture.id.clone(),
        family: run.fixture.spec.family.as_str().into(),
        truth_bpm: truth,
        baseline_bpm: run.baseline.bpm,
        baseline_confidence: Some(run.baseline.beat_confidence),
        baseline_relation,
        baseline_marker_count: run.baseline.beat_markers.len(),
        baseline_marker_bpm: run.marker.bpm,
        baseline_marker_relation: marker_relation,
        marker_median_interval_micros: run.marker.median_interval_micros,
        marker_mad_micros: run.marker.mad_micros,
        marker_tolerance_micros: run.marker.tolerance_micros,
        marker_inlier_count: run.marker.inlier_count,
        marker_available: run.marker.available,
        marker_rejection_reason: run.marker.rejection_reason.clone(),
        full_band: band_report(&run.diagnostics.full_band),
        low_band: band_report(&run.diagnostics.low_band),
        low_energy_ratio: run.diagnostics.low_energy_ratio,
        kick_reliable: run.diagnostics.kick_reliable,
        selected_source: run.diagnostics.selected_source.map(|source| {
            match source {
                wotoha_core::audio_analysis::ClassicalTempoSource::FullBand => "full_band",
                wotoha_core::audio_analysis::ClassicalTempoSource::LowBand => "low_band",
            }
            .into()
        }),
        top_peak_harmonics,
        lower_bounds: run
            .lower_bounds
            .iter()
            .map(|(minimum, analysis)| BoundFixtureResult {
                minimum_bpm: *minimum,
                bpm: analysis.as_ref().and_then(|analysis| analysis.bpm),
                confidence: analysis.as_ref().map(|analysis| analysis.beat_confidence),
                relation: analysis
                    .as_ref()
                    .and_then(|analysis| {
                        analysis
                            .bpm
                            .zip(truth)
                            .map(|(bpm, truth)| relation(bpm, truth))
                    })
                    .unwrap_or_else(|| "absent".into()),
                marker_bpm: analysis.as_ref().and_then(|analysis| {
                    estimate_markers(&analysis.beat_markers, analysis.duration).bpm
                }),
            })
            .collect(),
        marker_rescued,
        marker_regressed,
        marker_relative_error,
        marker_clock_consistent: run
            .marker
            .relative_spread
            .is_some_and(|spread| spread <= VARIABLE_TEMPO_RELATIVE_SPREAD_LIMIT),
        beat_density_per_second: (duration_seconds > 0.0)
            .then_some(run.baseline.beat_markers.len() as f64 / duration_seconds),
        phase_micros,
    }
}

fn harmonics(peaks: &[wotoha_core::audio_analysis::ClassicalTempoPeak]) -> Vec<String> {
    let mut result = Vec::new();
    for left in peaks {
        for right in peaks {
            if left.lag_blocks >= right.lag_blocks {
                continue;
            }
            let ratio = left.bpm / right.bpm.max(f32::EPSILON);
            for (label, expected) in [
                ("2x", 2.0_f32),
                ("half", 0.5),
                ("3:2", 1.5),
                ("2:3", 2.0 / 3.0),
            ] {
                if (ratio - expected).abs() / expected < 0.03 {
                    result.push(format!("{label}:{}->{:.3}", left.bpm, right.bpm));
                }
            }
        }
    }
    result.sort();
    result.dedup();
    result
}

fn variable_report(run: &FixtureRun) -> VariableTempoObservation {
    VariableTempoObservation {
        sample_id: run.fixture.id.clone(),
        family: run.fixture.spec.family.as_str().into(),
        baseline_bpm: run.baseline.bpm,
        baseline_confidence: Some(run.baseline.beat_confidence),
        marker_bpm: run.marker.bpm,
        marker_median_interval_micros: run.marker.median_interval_micros,
        marker_mad_micros: run.marker.mad_micros,
        marker_relative_spread: run.marker.relative_spread,
        scalar_score_excluded: true,
        global_marker_refinement_would_abstain: !run.marker.available
            || run.marker.relative_spread.unwrap_or(1.0) > VARIABLE_TEMPO_RELATIVE_SPREAD_LIMIT,
    }
}

fn transform_report(run: &FixtureRun) -> TransformAudit {
    let expected_samples =
        (run.fixture.spec.duration_micros as f64 * run.fixture.sample_rate as f64 / 1_000_000.0)
            .round() as usize;
    let declared_duration_matches_pcm = expected_samples
        .saturating_mul(usize::from(run.fixture.spec.channels.max(1)))
        == run.fixture.sample_count;
    TransformAudit {
        sample_id: run.fixture.id.clone(),
        base_id: run.fixture.spec.base_id.clone(),
        transform: run.fixture.spec.transform.label().into(),
        sample_rate: run.fixture.sample_rate,
        duration_micros: run.fixture.spec.duration_micros,
        declared_duration_matches_pcm,
        baseline_bpm: run.baseline.bpm,
        marker_bpm: run.marker.bpm,
        tempo_time_consistent: declared_duration_matches_pcm,
        invariance_expectation: format!("{:?}", run.fixture.spec.transform.expectation()),
    }
}

fn repeatability(runs: &[FixtureRun]) -> RepeatabilityReport {
    let mut mismatches = Vec::new();
    for run in runs {
        let mut reference = None;
        let mut mismatch = false;
        for _ in 0..3 {
            let result =
                wotoha_core::audio_analysis::analyze_mono_pcm(&run.audio, DEFAULT_SAMPLE_RATE);
            let fingerprint = result.map(|analysis| {
                (
                    analysis.bpm.map(f32::to_bits),
                    analysis
                        .beat_markers
                        .iter()
                        .map(Duration::as_micros)
                        .collect::<Vec<_>>(),
                    analysis
                        .beat_marker_confidences
                        .iter()
                        .map(|confidence| confidence.to_bits())
                        .collect::<Vec<_>>(),
                )
            });
            if let Some(existing) = &reference {
                if *existing != fingerprint {
                    mismatch = true;
                }
            } else {
                reference = Some(fingerprint);
            }
        }
        if mismatch {
            mismatches.push(run.fixture.id.clone());
        }
    }
    RepeatabilityReport {
        repetitions_per_fixture: 3,
        fixture_count: runs.len(),
        mismatch_count: mismatches.len(),
        mismatch_sample_ids: mismatches,
    }
}

fn duration_sensitivity(manifest: &BlackboxManifest) -> Result<Vec<DurationObservation>, LabError> {
    let bpms = [60.0_f32, 80.0, 120.0, 130.0, 160.0, 180.0];
    let durations = [10_u32, 15, 20, 25, 30, 45, 60, 120];
    let mut result = Vec::new();
    for bpm in bpms {
        let Some(source) = manifest.fixtures.iter().find(|fixture| matches!(fixture.spec.tempo, TempoProfile::Constant { bpm: candidate } if (candidate - bpm).abs() < f32::EPSILON)) else { continue };
        let mut spec = source.spec.clone();
        spec.duration_micros = 120_000_000;
        let truth = build_truth(&spec);
        let master = synthesize_audio(&spec, &truth);
        for seconds in durations {
            let sample_count = (seconds as usize * DEFAULT_SAMPLE_RATE as usize).min(master.len());
            let audio = master[..sample_count].to_vec();
            let analysis =
                wotoha_core::audio_analysis::analyze_mono_pcm(&audio, DEFAULT_SAMPLE_RATE);
            let marker = analysis
                .as_ref()
                .map(|analysis| estimate_markers(&analysis.beat_markers, analysis.duration));
            result.push(DurationObservation {
                truth_bpm: bpm,
                duration_seconds: seconds,
                master_generated_once: true,
                sample_count,
                bpm: analysis.as_ref().and_then(|analysis| analysis.bpm),
                confidence: analysis.as_ref().map(|analysis| analysis.beat_confidence),
                marker_bpm: marker.as_ref().and_then(|marker| marker.bpm),
                marker_available: marker.is_some_and(|marker| marker.available),
            });
        }
    }
    Ok(result)
}

fn markdown_report(report: &ClassicalTempoResearchReport) -> String {
    let mut markdown = String::new();
    markdown.push_str("# Classical tempo research\n\n");
    markdown.push_str("This report is research-only. It does not change production tempo, beat markers, phase, or backend authority.\n\n");
    markdown.push_str(&format!(
        "- Source commit: `{}`\n- Fixtures: {}\n- Exported WAV identity: {}\n\n",
        report.source_commit,
        report.corpus.fixture_count,
        report.corpus.exported_wav_identity_verified
    ));
    markdown.push_str("## Scalar tempo matrix\n\n| Candidate | Correct | Scored | Accuracy | Returned | False returns | MAE BPM |\n|---|---:|---:|---:|---:|---:|---:|\n");
    let mut rows = vec![
        ("A0 baseline", &report.baseline),
        ("A1 marker refinement", &report.marker_refinement),
    ];
    for candidate in &report.candidate_matrix {
        rows.push((&candidate.candidate, &candidate.metrics));
    }
    for (name, metrics) in rows {
        markdown.push_str(&format!(
            "| {} | {} | {} | {:.3?} | {} | {} | {:.3?} |\n",
            name,
            metrics.correct,
            metrics.scored,
            metrics.canonical_accuracy,
            metrics.returned,
            metrics.false_returns,
            metrics.absolute_bpm_mae
        ));
    }
    markdown.push_str("\n## Abstention sweep\n\n| Threshold | Correct | Returned | Abstentions | False returns |\n|---:|---:|---:|---:|---:|\n");
    for result in &report.abstention_sweep {
        markdown.push_str(&format!(
            "| {:.6} | {} | {} | {} | {} |\n",
            result.confidence_threshold,
            result.metrics.correct,
            result.metrics.returned,
            result.metrics.abstentions,
            result.metrics.false_returns
        ));
    }
    markdown.push_str("\n## Confidence distribution\n\n| Class | Count | Min | Median | Max |\n|---|---:|---:|---:|---:|\n");
    for (name, stats) in [
        ("correct", &report.confidence_distribution.correct),
        ("incorrect", &report.confidence_distribution.incorrect),
        ("absent", &report.confidence_distribution.absent),
    ] {
        markdown.push_str(&format!(
            "| {} | {} | {:.6?} | {:.6?} | {:.6?} |\n",
            name, stats.count, stats.minimum, stats.median, stats.maximum
        ));
    }
    markdown.push_str("\n## Lower-bound sweep\n\n| Minimum BPM | Correct | Rescues | Regressions | Half/double shifts |\n|---:|---:|---:|---:|---:|\n");
    for result in &report.lower_bound_sweep {
        markdown.push_str(&format!(
            "| {:.1} | {} | {} | {} | {} |\n",
            result.minimum_bpm,
            result.metrics.correct,
            result.rescued_sample_ids.len(),
            result.regressed_sample_ids.len(),
            result.half_double_relation_shifts.len()
        ));
    }
    markdown.push_str("\n## Remaining Classical scalar failures\n\n");
    for fixture in &report.per_fixture {
        if fixture.truth_bpm.is_some() && fixture.baseline_relation != "primary" {
            markdown.push_str(&format!(
                "- `{}` ({}) truth={:?}, baseline={:?} ({}) marker={:?} ({})\n",
                fixture.sample_id,
                fixture.family,
                fixture.truth_bpm,
                fixture.baseline_bpm,
                fixture.baseline_relation,
                fixture.baseline_marker_bpm,
                fixture.baseline_marker_relation
            ));
        }
    }
    markdown.push_str("\n## Variable-tempo safety\n\n");
    for observation in &report.variable_tempo {
        markdown.push_str(&format!(
            "- `{}`: baseline={:?}, marker={:?}, spread={:?}, abstain={}\n",
            observation.sample_id,
            observation.baseline_bpm,
            observation.marker_bpm,
            observation.marker_relative_spread,
            observation.global_marker_refinement_would_abstain
        ));
    }
    markdown.push_str(&format!(
        "\n## Repeatability\n\n{} repetitions per fixture; mismatches: {}.\n",
        report.repeatability.repetitions_per_fixture, report.repeatability.mismatch_count
    ));
    markdown.push_str("\n## Decision\n\nNo candidate is promoted. The output is evidence for a later held-out decision; production remains unchanged.\n");
    markdown
}

#[cfg(test)]
mod tests {
    use super::estimate_markers;
    use std::time::Duration;

    #[test]
    fn marker_estimator_recovers_regular_clock() {
        let markers = (0..8)
            .map(|index| Duration::from_millis(index * 500))
            .collect::<Vec<_>>();
        let result = estimate_markers(&markers, Duration::from_secs(4));
        assert!(result.available);
        assert!((result.bpm.expect("BPM") - 120.0).abs() < 0.01);
    }

    #[test]
    fn marker_estimator_rejects_insufficient_support() {
        let markers = [Duration::from_millis(0), Duration::from_millis(500)];
        assert!(!estimate_markers(&markers, Duration::from_secs(1)).available);
    }

    #[test]
    fn marker_estimator_is_deterministic_with_an_outlier() {
        let markers = [0, 500, 1_000, 1_500, 2_600, 3_100, 3_600]
            .into_iter()
            .map(Duration::from_millis)
            .collect::<Vec<_>>();
        let first = estimate_markers(&markers, Duration::from_secs(4));
        let second = estimate_markers(&markers, Duration::from_secs(4));
        assert_eq!(first.bpm, second.bpm);
        assert!(first.available);
    }
}
