//! Research-only competition between fixed-tempo period estimators.
//!
//! This module consumes the neutral, blind real-song analysis report produced
//! by `research-real-songs`.  It never consumes an external BPM while making
//! an inference.  External values may be joined by a separate evaluator after
//! this report has been frozen.

use std::{collections::BTreeMap, fs, path::Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    LabError,
    real_song_research::{
        RealSongAnalysisReport, RealSongResearchReport, RealSongTrackReport,
        SegmentConsistencyReport, TempoHypothesisReport, analysis_report_from_observed_events,
    },
    write_json,
};

const REPORT_SCHEMA: u32 = 1;
const PERIOD_CONSENSUS_RELATIVE: f64 = 0.002;
const STRONG_AGREEMENT_RELATIVE: f64 = 0.0005;
const MODERATE_AGREEMENT_RELATIVE: f64 = 0.001;
const WEAK_AGREEMENT_RELATIVE: f64 = 0.002;
const RESIDUAL_MATCH_MICROS: f64 = 35_000.0;

const CHANNEL_LOCAL_INTERVAL: &str = "LOCAL_INTERVAL";
const CHANNEL_LONG_BASELINE: &str = "LONG_BASELINE";
const CHANNEL_ROBUST_GRID: &str = "ROBUST_GRID";
const CHANNEL_SEGMENT_CLOCK: &str = "SEGMENT_CLOCK";
const CHANNEL_CLASSICAL_METRICAL: &str = "CLASSICAL_METRICAL";
const ORIGIN_BEAT_EVENTS: &str = "BEAT_EVENTS";
const ORIGIN_INDEPENDENT_METRICAL: &str = "INDEPENDENT_METRICAL_OBSERVATION";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FixedTempoConsensusReport {
    pub schema_version: u32,
    pub source_commit: String,
    pub input_report_source_commit: String,
    pub research_only: bool,
    pub input_report: String,
    pub configuration: ConsensusConfiguration,
    pub synthetic_hardening: SyntheticHardeningSummary,
    pub tracks: Vec<FixedTempoConsensusTrack>,
    pub summary: FixedTempoConsensusSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyntheticHardeningSummary {
    pub suite: String,
    pub phase: String,
    pub source_commit: String,
    pub external_reference_used_for_inference: bool,
    pub truth_used_for_inference: bool,
    pub cases: Vec<SyntheticHardeningCase>,
    pub correct_confident: usize,
    pub correct_acceptable: usize,
    pub safe_retain_multiple: usize,
    pub safe_abstain: usize,
    pub false_confident_selections: usize,
    pub unexpected_failures: usize,
    pub selected_strong: usize,
    pub selected_strong_with_genuine_independent_origins: usize,
    pub strong_independence_passed: bool,
    pub channel_duplication_invariance: bool,
    pub all_passed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyntheticHardeningCase {
    pub fixture_id: String,
    pub truth_bpm: Option<f64>,
    pub valid_physical_bpms: Vec<f64>,
    pub valid_canonical_bpms: Vec<f64>,
    pub stationary_expected: bool,
    pub allowed_period_status: Vec<String>,
    pub period_status: String,
    pub period_confidence: String,
    pub selected_physical_bpm: Option<f64>,
    pub relative_error: Option<f64>,
    pub selected_error_relative: Option<f64>,
    pub candidate_count: usize,
    pub candidates: Vec<FixedTempoPeriodCandidate>,
    pub clusters: Vec<PeriodCluster>,
    pub winning_channels: Vec<String>,
    pub competing_channels: Vec<String>,
    pub period_conflict: bool,
    pub canonical_status: String,
    pub truth_relation: String,
    pub confidently_correct: bool,
    pub safely_ambiguous: bool,
    pub safely_abstained: bool,
    pub evaluation_class: String,
    pub false_confident: bool,
    pub passed: bool,
    pub detail: String,
}

#[derive(Clone, Debug)]
struct SyntheticFixtureTruth {
    truth_bpm: Option<f64>,
    valid_physical_bpms: Vec<f64>,
    valid_canonical_bpms: Vec<f64>,
    stationary_expected: bool,
    allowed_period_status: Vec<String>,
}

#[derive(Clone, Debug)]
struct SyntheticObservedEvidence {
    events: Vec<u64>,
    duration_micros: u64,
    classical_candidates_bpm: Vec<f32>,
    tempo_hypotheses: Vec<TempoHypothesisReport>,
    primary_relation: String,
}

#[derive(Clone, Debug)]
struct SyntheticFixture {
    fixture_id: String,
    observations: SyntheticObservedEvidence,
    truth: SyntheticFixtureTruth,
    detail: String,
}

#[derive(Clone, Debug, Default)]
struct PeriodSelectionEvaluation {
    selected_error_relative: Option<f64>,
    truth_relation: String,
    confidently_correct: bool,
    safely_ambiguous: bool,
    safely_abstained: bool,
    false_confident: bool,
    evaluation_class: String,
    passed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsensusConfiguration {
    pub period_cluster_relative: f64,
    pub strong_agreement_relative: f64,
    pub moderate_agreement_relative: f64,
    pub weak_agreement_relative: f64,
    pub residual_match_micros: f64,
    pub selection_rule: String,
    pub channel_aggregation_rule: String,
    pub phase_estimation_rule: String,
    pub confidence_rule: String,
    pub canonical_layer_rule: String,
    pub external_reference_used_for_inference: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FixedTempoConsensusTrack {
    pub track_id: String,
    pub source_filename: String,
    pub source_sha256: String,
    pub period_candidates: Vec<FixedTempoPeriodCandidate>,
    pub period_clusters: Vec<PeriodCluster>,
    pub period_status: String,
    pub period_confidence: String,
    pub physical_period_bpm: Option<f64>,
    pub physical_period_micros: Option<f64>,
    pub period_conflict: Option<PeriodConflict>,
    pub canonical_layer: CanonicalLayerDecision,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FixedTempoPeriodCandidate {
    pub source: String,
    pub evidence_channel: String,
    pub derived_from: String,
    pub independence_origin: String,
    pub bpm: f64,
    pub period_micros: f64,
    pub support_events: usize,
    pub residual_median_micros: Option<f64>,
    pub residual_p95_micros: Option<f64>,
    pub explained_event_fraction: f64,
    pub missing_beat_fraction: f64,
    pub extra_event_fraction: f64,
    pub early_period_micros: Option<f64>,
    pub middle_period_micros: Option<f64>,
    pub late_period_micros: Option<f64>,
    pub stationarity_error: Option<f64>,
    pub agreement_sources: Vec<String>,
    pub agreement_channels: Vec<String>,
    pub phase_micros: Option<f64>,
    pub cluster_id: Option<usize>,
    pub score: f64,
    pub robust_fit_accepted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeriodCluster {
    pub cluster_id: usize,
    pub representative_bpm: f64,
    pub member_sources: Vec<String>,
    pub member_count: usize,
    pub score: f64,
    pub independent_channels: Vec<String>,
    pub independent_origins: Vec<String>,
    pub channel_scores: BTreeMap<String, f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeriodConflict {
    pub primary_cluster_id: usize,
    pub competing_cluster_id: usize,
    pub relative_difference: f64,
    pub robust_fit_cluster_id: Option<usize>,
    pub long_baseline_cluster_id: Option<usize>,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CanonicalLayerDecision {
    pub status: String,
    pub physical_period_bpm: Option<f64>,
    pub canonical_bpm: Option<f64>,
    pub retained_candidates_bpm: Vec<f64>,
    pub relation: String,
    pub metrical_evidence: Vec<String>,
    pub external_reference_used_for_inference: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FixedTempoConsensusSummary {
    pub tracks: usize,
    pub period_selected: usize,
    pub period_retain_multiple: usize,
    pub period_abstain: usize,
    pub canonical_selected: usize,
    pub canonical_retain_multiple: usize,
    pub canonical_abstain: usize,
    pub period_conflicts: usize,
    pub stationary_input_tracks: usize,
    pub external_reference_used_for_inference: bool,
}

#[derive(Clone, Copy)]
struct CandidateSpec {
    source: &'static str,
    bpm: f64,
    channel: &'static str,
    derived_from: &'static str,
    independence_origin: &'static str,
    weight: f64,
}

/// Re-score an already generated blind real-song report using only runtime
/// observations present in that report.  This function is deliberately a
/// separate research command so the production analyzer and its canonical
/// tempo selector remain untouched.
pub fn run_fixed_tempo_consensus_research(
    input: &Path,
    output_dir: &Path,
) -> Result<FixedTempoConsensusReport, LabError> {
    let input_bytes = fs::read(input)?;
    let report: RealSongResearchReport = serde_json::from_slice(&input_bytes)?;
    let tracks = report.tracks.iter().map(build_track).collect::<Vec<_>>();
    let mut summary = FixedTempoConsensusSummary {
        tracks: tracks.len(),
        stationary_input_tracks: report
            .tracks
            .iter()
            .filter(|track| track.full.stationarity.status == "stationary")
            .count(),
        ..FixedTempoConsensusSummary::default()
    };
    for track in &tracks {
        match track.period_status.as_str() {
            "SELECTED" => summary.period_selected += 1,
            "RETAIN_MULTIPLE" => summary.period_retain_multiple += 1,
            _ => summary.period_abstain += 1,
        }
        match track.canonical_layer.status.as_str() {
            "CANONICAL_SELECTED" => summary.canonical_selected += 1,
            "CANONICAL_RETAIN_MULTIPLE" => summary.canonical_retain_multiple += 1,
            _ => summary.canonical_abstain += 1,
        }
        if track.period_conflict.is_some() {
            summary.period_conflicts += 1;
        }
    }
    let source_commit =
        std::env::var("WOTOHA_SOURCE_COMMIT").unwrap_or_else(|_| report.source_commit.clone());
    let output = FixedTempoConsensusReport {
        schema_version: REPORT_SCHEMA,
        source_commit,
        input_report_source_commit: report.source_commit,
        research_only: true,
        input_report: input.display().to_string(),
        configuration: ConsensusConfiguration {
            period_cluster_relative: PERIOD_CONSENSUS_RELATIVE,
            strong_agreement_relative: STRONG_AGREEMENT_RELATIVE,
            moderate_agreement_relative: MODERATE_AGREEMENT_RELATIVE,
            weak_agreement_relative: WEAK_AGREEMENT_RELATIVE,
            residual_match_micros: RESIDUAL_MATCH_MICROS,
            selection_rule: "fixed representative clusters; long-baseline and robust channels are scored independently; unresolved material conflict retains multiple or abstains".into(),
            channel_aggregation_rule: "within each fixed representative cluster, each evidence channel contributes only its highest-scoring candidate; correlated estimators share one bounded vote and cannot inflate the cluster score".into(),
            phase_estimation_rule: "candidate phase is selected from bounded event remainders and fixed phase buckets using a trimmed residual objective; it is never anchored to the first BeatEvent".into(),
            confidence_rule: "SELECTED_STRONG requires two or more genuine evidence origins; SELECTED_MODERATE is reserved for a selected cluster supported only by one origin; algorithmic channels derived from the same BeatEvent stream do not create a second origin; unresolved conflict is RETAIN_MULTIPLE and non-stationary or insufficient evidence is ABSTAIN".into(),
            canonical_layer_rule: "physical period is resolved first; a metrical alternative is retained only when a runtime tempo hypothesis is an exact half/double layer with substantial weight and no decisive separation".into(),
            external_reference_used_for_inference: false,
        },
        synthetic_hardening: synthetic_hardening_summary(),
        tracks: output_tracks_sorted(tracks),
        summary,
    };
    write_json(&output_dir.join("fixed-tempo-consensus.json"), &output)?;
    fs::write(
        output_dir.join("fixed-tempo-consensus.md"),
        markdown(&output),
    )?;
    write_json(
        &output_dir.join("synthetic-hardening.json"),
        &output.synthetic_hardening,
    )?;
    Ok(output)
}

/// Run the end-to-end synthetic selector hardening suite.  The fixture truth
/// is retained only by the evaluator; `synthetic_track_from_observations`
/// receives observations without the truth object and enters `build_track`.
pub fn run_fixed_tempo_consensus_synthetic_research(
    output_dir: &Path,
    source_commit: String,
) -> Result<SyntheticHardeningSummary, LabError> {
    let mut summary = synthetic_hardening_summary();
    summary.source_commit = source_commit.clone();
    write_json(&output_dir.join("synthetic-hardening-e2e.json"), &summary)?;
    fs::write(
        output_dir.join("synthetic-hardening-e2e.md"),
        synthetic_markdown(&summary, &source_commit),
    )?;
    Ok(summary)
}

fn output_tracks_sorted(
    mut tracks: Vec<FixedTempoConsensusTrack>,
) -> Vec<FixedTempoConsensusTrack> {
    tracks.sort_by(|left, right| left.source_filename.cmp(&right.source_filename));
    tracks
}

fn build_track(track: &RealSongTrackReport) -> FixedTempoConsensusTrack {
    let full = &track.full;
    let derived_grid;
    let grid = if full.grid_method_comparison.is_empty() {
        derived_grid = analysis_report_from_observed_events(
            full.raw_event_times_micros.clone(),
            track.duration_micros,
            full.classical_selected_bpm,
            full.classical_candidates_bpm.clone(),
            full.tempo_hypotheses.clone(),
            full.primary_relation.clone(),
            &full.analysis_backend,
        )
        .grid_method_comparison;
        &derived_grid
    } else {
        &full.grid_method_comparison
    };
    let mut specs = Vec::new();
    add_spec(
        &mut specs,
        "ADJACENT_MEDIAN",
        grid.adjacent_median_bpm,
        CHANNEL_LOCAL_INTERVAL,
        "RAW_ADJACENT_INTERVALS",
        ORIGIN_BEAT_EVENTS,
        1.0,
    );
    add_spec(
        &mut specs,
        "TRIMMED_ADJACENT_MEDIAN",
        grid.trimmed_adjacent_median_bpm,
        CHANNEL_LOCAL_INTERVAL,
        "TRIMMED_ADJACENT_INTERVALS",
        ORIGIN_BEAT_EVENTS,
        1.0,
    );
    add_spec(
        &mut specs,
        "EARLY_EVENT_CLOCK",
        grid.early_event_clock_bpm,
        CHANNEL_SEGMENT_CLOCK,
        "RAW_EVENT_SEGMENT_CLOCK",
        ORIGIN_BEAT_EVENTS,
        0.5,
    );
    add_spec(
        &mut specs,
        "MIDDLE_EVENT_CLOCK",
        grid.middle_event_clock_bpm,
        CHANNEL_SEGMENT_CLOCK,
        "RAW_EVENT_SEGMENT_CLOCK",
        ORIGIN_BEAT_EVENTS,
        0.5,
    );
    add_spec(
        &mut specs,
        "LATE_EVENT_CLOCK",
        grid.late_event_clock_bpm,
        CHANNEL_SEGMENT_CLOCK,
        "RAW_EVENT_SEGMENT_CLOCK",
        ORIGIN_BEAT_EVENTS,
        0.5,
    );
    add_spec(
        &mut specs,
        "SEQUENTIAL_GLOBAL_REGRESSION",
        grid.sequential_global_regression_bpm,
        CHANNEL_LONG_BASELINE,
        "SEQUENTIAL_EVENT_INDEX_GLOBAL_FIT",
        ORIGIN_BEAT_EVENTS,
        3.0,
    );
    add_spec(
        &mut specs,
        "MISSING_JUMP_GLOBAL_REGRESSION",
        grid.missing_jump_global_regression_bpm,
        CHANNEL_LONG_BASELINE,
        "MISSING_JUMP_GLOBAL_FIT",
        ORIGIN_BEAT_EVENTS,
        3.0,
    );
    add_spec(
        &mut specs,
        "SEQUENTIAL_ENDPOINT",
        grid.sequential_endpoint_bpm,
        CHANNEL_LONG_BASELINE,
        "SEQUENTIAL_ENDPOINT_FIT",
        ORIGIN_BEAT_EVENTS,
        2.0,
    );
    add_spec(
        &mut specs,
        "MISSING_JUMP_ENDPOINT",
        grid.missing_jump_endpoint_bpm,
        CHANNEL_LONG_BASELINE,
        "MISSING_JUMP_ENDPOINT_FIT",
        ORIGIN_BEAT_EVENTS,
        2.0,
    );
    add_spec(
        &mut specs,
        "ROBUST_GRID_FIT",
        full.refined_grid.fit_bpm,
        CHANNEL_ROBUST_GRID,
        "RUNTIME_REFINED_GRID_FIT",
        ORIGIN_BEAT_EVENTS,
        2.0,
    );
    if let Some(bpm) = full.classical_candidates_bpm.first() {
        add_spec(
            &mut specs,
            "CLASSICAL_FULL_CANDIDATE",
            Some(f64::from(*bpm)),
            CHANNEL_CLASSICAL_METRICAL,
            "CLASSICAL_TEMPO_HYPOTHESIS",
            ORIGIN_INDEPENDENT_METRICAL,
            0.25,
        );
    }
    if let Some(bpm) = full.classical_candidates_bpm.get(1) {
        add_spec(
            &mut specs,
            "CLASSICAL_LOW_CANDIDATE",
            Some(f64::from(*bpm)),
            CHANNEL_CLASSICAL_METRICAL,
            "CLASSICAL_TEMPO_HYPOTHESIS",
            ORIGIN_INDEPENDENT_METRICAL,
            0.25,
        );
    }
    let mut candidates = specs
        .into_iter()
        .map(|spec| score_candidate(spec, &full.raw_event_times_micros, full, grid))
        .collect::<Vec<_>>();
    let clusters = make_clusters(&mut candidates);
    let (period_status, physical_period_bpm, period_conflict) =
        select_period(&mut candidates, &clusters, full);
    let period_confidence = period_confidence(&period_status, &clusters, physical_period_bpm);
    let canonical_layer = resolve_canonical_layer(
        physical_period_bpm,
        &full.tempo_hypotheses,
        &full.primary_relation,
    );
    FixedTempoConsensusTrack {
        track_id: track.track_id.clone(),
        source_filename: track.source_filename.clone(),
        source_sha256: track.source_sha256.clone(),
        period_candidates: candidates,
        period_clusters: clusters,
        period_status,
        period_confidence,
        physical_period_bpm,
        physical_period_micros: physical_period_bpm.map(|bpm| 60_000_000.0 / bpm),
        period_conflict,
        canonical_layer,
    }
}

fn add_spec(
    specs: &mut Vec<CandidateSpec>,
    source: &'static str,
    bpm: Option<f64>,
    channel: &'static str,
    derived_from: &'static str,
    independence_origin: &'static str,
    weight: f64,
) {
    if let Some(bpm) = bpm.filter(|value| value.is_finite() && *value > 0.0) {
        specs.push(CandidateSpec {
            source,
            bpm,
            channel,
            derived_from,
            independence_origin,
            weight,
        });
    }
}

fn score_candidate(
    spec: CandidateSpec,
    events: &[u64],
    full: &RealSongAnalysisReport,
    grid: &crate::real_song_research::GridMethodComparisonReport,
) -> FixedTempoPeriodCandidate {
    let period = 60_000_000.0 / spec.bpm;
    let support = events.len();
    let phase_fit = fit_period_phase(events, period);
    let residuals = phase_fit.residuals.clone();
    let indices = phase_fit.indices.clone();
    let residual_median = median(&residuals);
    let residual_p95 = percentile(&residuals, 0.95);
    let explained = residuals
        .iter()
        .filter(|residual| **residual <= RESIDUAL_MATCH_MICROS)
        .count() as f64
        / support.max(1) as f64;
    let missing = missing_fraction(&indices);
    let extra = extra_fraction(events, period);
    let score = spec.weight + 2.0 * explained
        - residual_median.unwrap_or(1_000_000.0) / period.max(1.0)
        - missing
        - extra;
    FixedTempoPeriodCandidate {
        source: spec.source.into(),
        evidence_channel: spec.channel.into(),
        derived_from: spec.derived_from.into(),
        independence_origin: spec.independence_origin.into(),
        bpm: spec.bpm,
        period_micros: period,
        support_events: support,
        residual_median_micros: residual_median,
        residual_p95_micros: residual_p95,
        explained_event_fraction: explained,
        missing_beat_fraction: missing,
        extra_event_fraction: extra,
        early_period_micros: grid.early_event_clock_bpm.map(|bpm| 60_000_000.0 / bpm),
        middle_period_micros: grid.middle_event_clock_bpm.map(|bpm| 60_000_000.0 / bpm),
        late_period_micros: grid.late_event_clock_bpm.map(|bpm| 60_000_000.0 / bpm),
        stationarity_error: full.stationarity.relative_period_drift,
        agreement_sources: Vec::new(),
        agreement_channels: vec![spec.channel.into()],
        phase_micros: Some(phase_fit.phase_micros),
        cluster_id: None,
        score,
        robust_fit_accepted: spec.source == "ROBUST_GRID_FIT" && full.refined_grid.accepted,
    }
}

#[derive(Clone, Debug)]
struct PeriodPhaseFit {
    phase_micros: f64,
    residuals: Vec<f64>,
    indices: Vec<i64>,
}

fn fit_period_phase(events: &[u64], period: f64) -> PeriodPhaseFit {
    if events.is_empty() {
        return PeriodPhaseFit {
            phase_micros: 0.0,
            residuals: Vec::new(),
            indices: Vec::new(),
        };
    }
    let mut phases = vec![0.0];
    let sample_count = events.len().min(32);
    for sample in 0..sample_count {
        let index = if sample_count <= 1 {
            0
        } else {
            sample * (events.len() - 1) / (sample_count - 1)
        };
        phases.push((events[index] as f64).rem_euclid(period));
    }
    let mut remainders = events
        .iter()
        .map(|time| (*time as f64).rem_euclid(period))
        .collect::<Vec<_>>();
    remainders.sort_by(f64::total_cmp);
    if let Some(middle) = remainders.get(remainders.len() / 2) {
        phases.push(*middle);
    }
    for fraction in [0.25, 0.5, 0.75] {
        let index = ((remainders.len() - 1) as f64 * fraction).round() as usize;
        if let Some(phase) = remainders.get(index) {
            phases.push(*phase);
        }
    }
    // The fixed grid makes endpoint and quantization behavior deterministic even
    // when all candidate remainders come from a corrupted short segment.
    for bucket in 0..32 {
        phases.push(period * bucket as f64 / 32.0);
    }
    phases.sort_by(f64::total_cmp);
    phases.dedup_by(|left, right| (*left - *right).abs() <= 1.0);

    let mut best: Option<(f64, f64, PeriodPhaseFit)> = None;
    for phase in phases {
        let (residuals, indices) = fixed_period_residuals_with_phase(events, period, phase);
        let objective = robust_phase_objective(&residuals);
        let median_residual = median(&residuals).unwrap_or(f64::INFINITY);
        let fit = PeriodPhaseFit {
            phase_micros: phase,
            residuals,
            indices,
        };
        let replace = match best.as_ref() {
            None => true,
            Some((best_objective, best_median, best_fit)) => {
                objective.total_cmp(best_objective).is_lt()
                    || (objective.total_cmp(best_objective).is_eq()
                        && (median_residual.total_cmp(best_median).is_lt()
                            || (median_residual.total_cmp(best_median).is_eq()
                                && phase.total_cmp(&best_fit.phase_micros).is_lt())))
            }
        };
        if replace {
            best = Some((objective, median_residual, fit));
        }
    }
    best.map(|(_, _, fit)| fit).unwrap_or(PeriodPhaseFit {
        phase_micros: 0.0,
        residuals: Vec::new(),
        indices: Vec::new(),
    })
}

fn fixed_period_residuals_with_phase(
    events: &[u64],
    period: f64,
    phase: f64,
) -> (Vec<f64>, Vec<i64>) {
    let phase = phase.rem_euclid(period);
    let mut indices = Vec::with_capacity(events.len());
    let mut residuals = Vec::with_capacity(events.len());
    let mut previous = 0_i64;
    for (position, time) in events.iter().enumerate() {
        let proposed = ((*time as f64 - phase) / period).round() as i64;
        let index = if position == 0 {
            proposed
        } else {
            // A corrupted endpoint or subdivision can share the nearest grid
            // index with its neighbor.  Requiring a strictly increasing index
            // here would shift every later event by one beat and turn a local
            // outlier into an apparent global phase error.  Missing-beat and
            // extra-event diagnostics remain separate from this phase fit.
            proposed.max(previous)
        };
        previous = index;
        indices.push(index);
        residuals.push((*time as f64 - (phase + index as f64 * period)).abs());
    }
    (residuals, indices)
}

fn robust_phase_objective(residuals: &[f64]) -> f64 {
    if residuals.is_empty() {
        return f64::INFINITY;
    }
    let mut sorted = residuals.to_vec();
    sorted.sort_by(f64::total_cmp);
    let trim = (sorted.len() / 10).min(sorted.len().saturating_sub(1));
    let retained = &sorted[..sorted.len() - trim];
    let mean = retained.iter().sum::<f64>() / retained.len() as f64;
    let median = median(&sorted).unwrap_or(f64::INFINITY);
    let p95 = percentile(&sorted, 0.95).unwrap_or(f64::INFINITY);
    mean + 0.25 * median + 0.25 * p95
}

fn make_clusters(candidates: &mut [FixedTempoPeriodCandidate]) -> Vec<PeriodCluster> {
    let mut order = (0..candidates.len()).collect::<Vec<_>>();
    order.sort_by(|left, right| candidates[*left].bpm.total_cmp(&candidates[*right].bpm));
    let mut clusters = Vec::new();
    for candidate_index in order {
        let bpm = candidates[candidate_index].bpm;
        let cluster_id = clusters.iter().position(|cluster: &PeriodCluster| {
            (bpm / cluster.representative_bpm - 1.0).abs() <= PERIOD_CONSENSUS_RELATIVE
        });
        let id = if let Some(id) = cluster_id {
            id
        } else {
            let id = clusters.len();
            clusters.push(PeriodCluster {
                cluster_id: id,
                representative_bpm: bpm,
                member_sources: Vec::new(),
                member_count: 0,
                score: 0.0,
                independent_channels: Vec::new(),
                independent_origins: Vec::new(),
                channel_scores: BTreeMap::new(),
            });
            id
        };
        candidates[candidate_index].cluster_id = Some(id);
        let cluster = &mut clusters[id];
        cluster
            .member_sources
            .push(candidates[candidate_index].source.clone());
        cluster.member_count += 1;
        let candidate = &candidates[candidate_index];
        let channel = candidate.evidence_channel.clone();
        cluster
            .channel_scores
            .entry(channel.clone())
            .and_modify(|score| *score = score.max(candidate.score))
            .or_insert(candidate.score);
        if !cluster.independent_channels.contains(&channel) {
            cluster.independent_channels.push(channel);
        }
        let origin = candidate.independence_origin.clone();
        if !cluster.independent_origins.contains(&origin) {
            cluster.independent_origins.push(origin);
        }
        cluster.score = cluster.channel_scores.values().sum();
    }
    for candidate in candidates.iter_mut() {
        if let Some(id) = candidate.cluster_id {
            candidate.agreement_sources = clusters[id].member_sources.clone();
            candidate.agreement_channels = clusters[id].independent_channels.clone();
        }
    }
    clusters
}

fn select_period(
    candidates: &mut [FixedTempoPeriodCandidate],
    clusters: &[PeriodCluster],
    full: &RealSongAnalysisReport,
) -> (String, Option<f64>, Option<PeriodConflict>) {
    if full.stationarity.status != "stationary" || clusters.is_empty() {
        return ("ABSTAIN".into(), None, None);
    }
    let mut ranked = clusters.iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.representative_bpm.total_cmp(&right.representative_bpm))
    });
    let best = ranked[0];
    let second = ranked.get(1).copied();
    let robust_candidate = candidates
        .iter()
        .find(|candidate| candidate.source == "ROBUST_GRID_FIT");
    let robust_cluster = robust_candidate.and_then(|candidate| candidate.cluster_id);
    let long_cluster = candidates
        .iter()
        .filter(|candidate| {
            candidate.source.contains("GLOBAL") || candidate.source.contains("ENDPOINT")
        })
        .max_by(|left, right| left.score.total_cmp(&right.score))
        .and_then(|candidate| candidate.cluster_id);
    let robust_conflict = robust_candidate
        .filter(|candidate| candidate.robust_fit_accepted)
        .and_then(|candidate| candidate.cluster_id)
        .zip(long_cluster)
        .filter(|(robust, long)| robust != long)
        .and_then(|(robust, long)| {
            let a = clusters.get(robust)?.representative_bpm;
            let b = clusters.get(long)?.representative_bpm;
            let relative = (a / b - 1.0).abs();
            (relative > WEAK_AGREEMENT_RELATIVE).then_some((robust, long, relative))
        });
    let competing_conflict = second.filter(|cluster| {
        (cluster.representative_bpm / best.representative_bpm - 1.0).abs() > WEAK_AGREEMENT_RELATIVE
            && (best.score - cluster.score).abs() <= 2.0
    });
    let conflict = robust_conflict.or_else(|| {
        competing_conflict.map(|cluster| {
            let relative = (cluster.representative_bpm / best.representative_bpm - 1.0).abs();
            (best.cluster_id, cluster.cluster_id, relative)
        })
    });
    let period_conflict = conflict.map(|(primary, competing, relative)| PeriodConflict {
        primary_cluster_id: primary,
        competing_cluster_id: competing,
        relative_difference: relative,
        robust_fit_cluster_id: robust_cluster,
        long_baseline_cluster_id: long_cluster,
        reason:
            "independent fixed-period channels disagree beyond the weak 0.20% consensus tolerance"
                .into(),
    });
    if period_conflict.is_some() {
        return ("RETAIN_MULTIPLE".into(), None, period_conflict);
    }
    if best.member_count < 2 || best.independent_channels.len() < 2 {
        return ("ABSTAIN".into(), None, None);
    }
    let selected = weighted_cluster_bpm(candidates, best.cluster_id);
    ("SELECTED".into(), selected, None)
}

fn period_confidence(
    status: &str,
    clusters: &[PeriodCluster],
    selected_bpm: Option<f64>,
) -> String {
    if status == "RETAIN_MULTIPLE" {
        return "RETAIN_MULTIPLE".into();
    }
    if status != "SELECTED" || selected_bpm.is_none() {
        return "ABSTAIN".into();
    }
    let independent_origins = clusters
        .iter()
        .find(|cluster| {
            (cluster.representative_bpm / selected_bpm.unwrap_or(0.0) - 1.0).abs()
                <= PERIOD_CONSENSUS_RELATIVE
        })
        .map_or(0, |cluster| cluster.independent_origins.len());
    if independent_origins >= 2 {
        "SELECTED_STRONG".into()
    } else {
        "SELECTED_MODERATE".into()
    }
}

fn weighted_cluster_bpm(
    candidates: &[FixedTempoPeriodCandidate],
    cluster_id: usize,
) -> Option<f64> {
    let mut best_by_channel = BTreeMap::<&str, &FixedTempoPeriodCandidate>::new();
    for candidate in candidates
        .iter()
        .filter(|candidate| candidate.cluster_id == Some(cluster_id))
    {
        best_by_channel
            .entry(candidate.evidence_channel.as_str())
            .and_modify(|current| {
                if candidate.score > current.score {
                    *current = candidate;
                }
            })
            .or_insert(candidate);
    }
    let members = best_by_channel.into_values().collect::<Vec<_>>();
    if members.is_empty() {
        return None;
    }
    let weight = members
        .iter()
        .map(|candidate| candidate.score.max(0.1))
        .sum::<f64>();
    Some(
        members
            .iter()
            .map(|candidate| candidate.bpm * candidate.score.max(0.1))
            .sum::<f64>()
            / weight,
    )
}

fn resolve_canonical_layer(
    physical_period_bpm: Option<f64>,
    hypotheses: &[crate::real_song_research::TempoHypothesisReport],
    primary_relation: &str,
) -> CanonicalLayerDecision {
    let Some(physical) = physical_period_bpm else {
        return CanonicalLayerDecision {
            status: "CANONICAL_ABSTAIN".into(),
            physical_period_bpm: None,
            canonical_bpm: None,
            retained_candidates_bpm: Vec::new(),
            relation: "unknown".into(),
            metrical_evidence: vec!["physical period was not selected".into()],
            external_reference_used_for_inference: false,
        };
    };
    let primary = hypotheses.iter().find(|hypothesis| {
        (f64::from(hypothesis.bpm) / physical - 1.0).abs() <= STRONG_AGREEMENT_RELATIVE
    });
    let alternative = hypotheses.iter().find(|hypothesis| {
        let ratio = f64::from(hypothesis.bpm) / physical;
        (ratio - 2.0).abs() <= STRONG_AGREEMENT_RELATIVE
            || (ratio - 0.5).abs() <= STRONG_AGREEMENT_RELATIVE
    });
    let ambiguous = primary.is_some_and(|primary| {
        alternative.is_some_and(|alternative| {
            alternative.relative_weight >= 0.35
                && (primary.relative_weight - alternative.relative_weight).abs() <= 0.20
        })
    });
    if ambiguous {
        let mut retained = vec![physical];
        if let Some(alternative) = alternative {
            retained.push(f64::from(alternative.bpm));
        }
        retained.sort_by(f64::total_cmp);
        return CanonicalLayerDecision {
            status: "CANONICAL_RETAIN_MULTIPLE".into(),
            physical_period_bpm: Some(physical),
            canonical_bpm: None,
            retained_candidates_bpm: retained,
            relation: "multiple_metrical_layers".into(),
            metrical_evidence: vec![
                "primary and half/double hypothesis are both runtime-supported".into(),
                format!("input primary relation: {primary_relation}"),
            ],
            external_reference_used_for_inference: false,
        };
    }
    CanonicalLayerDecision {
        status: "CANONICAL_SELECTED".into(),
        physical_period_bpm: Some(physical),
        canonical_bpm: Some(physical),
        retained_candidates_bpm: vec![physical],
        relation: "primary".into(),
        metrical_evidence: vec!["no decisive exact half/double alternative was supported".into()],
        external_reference_used_for_inference: false,
    }
}

fn missing_fraction(indices: &[i64]) -> f64 {
    if indices.len() < 2 {
        return 0.0;
    }
    let missing = indices
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).saturating_sub(1).max(0) as f64)
        .sum::<f64>();
    missing / (indices.last().copied().unwrap_or(0) - indices[0]).max(1) as f64
}

fn extra_fraction(events: &[u64], period: f64) -> f64 {
    if events.len() < 2 {
        return 0.0;
    }
    events
        .windows(2)
        .filter(|pair| pair[1] > pair[0] && ((pair[1] - pair[0]) as f64) < period * 0.5)
        .count() as f64
        / (events.len() - 1) as f64
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    Some(sorted[sorted.len() / 2])
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

fn synthetic_grid_events(period: f64, count: usize, phase: f64) -> Vec<u64> {
    (0..count)
        .map(|index| (phase + index as f64 * period).round() as u64)
        .collect()
}

fn synthetic_candidate(
    source: &str,
    bpm: f64,
    channel: &str,
    score: f64,
) -> FixedTempoPeriodCandidate {
    FixedTempoPeriodCandidate {
        source: source.into(),
        evidence_channel: channel.into(),
        derived_from: "SYNTHETIC_TEST".into(),
        independence_origin: ORIGIN_BEAT_EVENTS.into(),
        bpm,
        period_micros: 60_000_000.0 / bpm,
        support_events: 32,
        residual_median_micros: Some(0.0),
        residual_p95_micros: Some(0.0),
        explained_event_fraction: 1.0,
        missing_beat_fraction: 0.0,
        extra_event_fraction: 0.0,
        early_period_micros: None,
        middle_period_micros: None,
        late_period_micros: None,
        stationarity_error: Some(0.0),
        agreement_sources: Vec::new(),
        agreement_channels: vec![channel.into()],
        phase_micros: Some(0.0),
        cluster_id: None,
        score,
        robust_fit_accepted: channel == CHANNEL_ROBUST_GRID,
    }
}

fn patterned_events(period: f64, count: usize, phase: f64, pattern: &[f64]) -> Vec<u64> {
    let mut events = vec![phase.round() as u64];
    let mut time = phase;
    for index in 0..count.saturating_sub(1) {
        time += period * pattern[index % pattern.len()];
        events.push(time.round() as u64);
    }
    events
}

fn offset_events(period: f64, count: usize, phase: f64, offsets: &[f64]) -> Vec<u64> {
    (0..count)
        .map(|index| {
            (phase + index as f64 * period + offsets[index % offsets.len()]).round() as u64
        })
        .collect()
}

fn remove_events(events: &[u64], predicate: impl Fn(usize) -> bool) -> Vec<u64> {
    events
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(index, event)| (!predicate(index)).then_some(event))
        .collect()
}

fn synthetic_identity(events: &[u64]) -> String {
    let mut digest = Sha256::new();
    for event in events {
        digest.update(event.to_le_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn synthetic_track_from_observations(
    fixture_id: &str,
    observations: &SyntheticObservedEvidence,
) -> RealSongTrackReport {
    // The event-derived estimators are reconstructed inside `build_track` from
    // the observed event stream.  Keep the classical candidate list limited to
    // explicitly injected metrical observations; copying event-derived BPMs
    // into this field would manufacture a second evidence origin.
    let full = analysis_report_from_observed_events(
        observations.events.clone(),
        observations.duration_micros,
        observations.classical_candidates_bpm.first().copied(),
        observations.classical_candidates_bpm.clone(),
        observations.tempo_hypotheses.clone(),
        observations.primary_relation.clone(),
        "synthetic_observed_events",
    );
    let source_sha256 = synthetic_identity(&observations.events);
    RealSongTrackReport {
        track_id: fixture_id.into(),
        source_filename: format!("{fixture_id}.synthetic"),
        source_sha256: source_sha256.clone(),
        source_bytes: observations.events.len() * std::mem::size_of::<u64>(),
        decoded_pcm_sha256: source_sha256,
        container_extension: "synthetic".into(),
        codec: "observed-event-clock".into(),
        sample_rate: 1_000_000,
        channels: 1,
        decoded_frames: observations.duration_micros as usize,
        duration_micros: observations.duration_micros,
        full,
        segments: Vec::new(),
        segment_consistency: SegmentConsistencyReport {
            segment_count: 0,
            canonical_bpm_consistent: 0,
            family_consistent: 0,
            canonical_threshold_relative: 0.01,
            family_threshold_relative: 0.01,
            corpus_constant_tempo_assertion_used_only_after_inference: true,
        },
        failure_taxonomy: Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn synthetic_fixture(
    fixture_id: &str,
    events: Vec<u64>,
    truth_bpm: Option<f64>,
    valid_physical_bpms: Vec<f64>,
    valid_canonical_bpms: Vec<f64>,
    stationary_expected: bool,
    allowed_period_status: &[&str],
    detail: &str,
) -> SyntheticFixture {
    let duration_micros = events
        .last()
        .copied()
        .unwrap_or_default()
        .saturating_add(1_000_000);
    SyntheticFixture {
        fixture_id: fixture_id.into(),
        observations: SyntheticObservedEvidence {
            events,
            duration_micros,
            classical_candidates_bpm: Vec::new(),
            tempo_hypotheses: Vec::new(),
            primary_relation: "unknown".into(),
        },
        truth: SyntheticFixtureTruth {
            truth_bpm,
            valid_physical_bpms,
            valid_canonical_bpms,
            stationary_expected,
            allowed_period_status: allowed_period_status
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        },
        detail: detail.into(),
    }
}

fn with_observed_hypotheses(
    mut fixture: SyntheticFixture,
    hypotheses: &[(f32, &str, f32)],
    primary_relation: &str,
) -> SyntheticFixture {
    // These fixtures intentionally model an independent metrical observation.
    // Ordinary event-clock fixtures leave this list empty, so no
    // CLASSICAL_METRICAL candidate can be manufactured from BeatEvents.
    fixture.observations.classical_candidates_bpm =
        hypotheses.iter().map(|(bpm, _, _)| *bpm).collect();
    fixture.observations.tempo_hypotheses = hypotheses
        .iter()
        .map(|(bpm, relation, relative_weight)| TempoHypothesisReport {
            bpm: *bpm,
            relation: (*relation).into(),
            relative_weight: *relative_weight,
        })
        .collect();
    fixture.observations.primary_relation = primary_relation.into();
    fixture
}

fn period_selection_evaluation(
    result: &FixedTempoConsensusTrack,
    truth: &SyntheticFixtureTruth,
) -> PeriodSelectionEvaluation {
    let selected = result.physical_period_bpm;
    let selected_error_relative = selected.and_then(|bpm| {
        truth
            .valid_physical_bpms
            .iter()
            .map(|allowed| (bpm / allowed - 1.0).abs())
            .min_by(f64::total_cmp)
    });
    let selected_confident = result.period_status == "SELECTED"
        && matches!(
            result.period_confidence.as_str(),
            "SELECTED_STRONG" | "SELECTED_MODERATE"
        );
    let false_confident =
        selected_confident && selected_error_relative.is_none_or(|error| error > 0.002);
    let confidently_correct =
        selected_confident && selected_error_relative.is_some_and(|error| error <= 0.0005);
    let correct_acceptable =
        selected_confident && selected_error_relative.is_some_and(|error| error <= 0.002);
    let safely_ambiguous = result.period_status == "RETAIN_MULTIPLE";
    let safely_abstained = result.period_status == "ABSTAIN";
    let truth_relation = if selected_error_relative.is_some() {
        if truth
            .valid_canonical_bpms
            .iter()
            .any(|allowed| selected.is_some_and(|bpm| (bpm / allowed - 1.0).abs() <= 0.002))
        {
            "allowed_physical_or_canonical_layer"
        } else {
            "allowed_physical_layer"
        }
    } else {
        "outside_allowed_physical_layer"
    }
    .into();
    let evaluation_class = if false_confident {
        "FALSE_CONFIDENT"
    } else if confidently_correct {
        "CORRECT_CONFIDENT"
    } else if correct_acceptable {
        "CORRECT_ACCEPTABLE"
    } else if safely_ambiguous
        && truth
            .allowed_period_status
            .iter()
            .any(|status| status == "RETAIN_MULTIPLE")
    {
        "SAFE_RETAIN_MULTIPLE"
    } else if safely_abstained
        && truth
            .allowed_period_status
            .iter()
            .any(|status| status == "ABSTAIN")
    {
        "SAFE_ABSTAIN"
    } else {
        "UNEXPECTED_FAILURE"
    };
    let passed = truth
        .allowed_period_status
        .iter()
        .any(|status| status == &result.period_status)
        && evaluation_class != "FALSE_CONFIDENT"
        && evaluation_class != "UNEXPECTED_FAILURE";
    PeriodSelectionEvaluation {
        selected_error_relative,
        truth_relation,
        confidently_correct,
        safely_ambiguous,
        safely_abstained,
        false_confident,
        evaluation_class: evaluation_class.into(),
        passed,
    }
}

fn synthetic_case_from_result(
    fixture: &SyntheticFixture,
    result: &FixedTempoConsensusTrack,
) -> SyntheticHardeningCase {
    let evaluation = period_selection_evaluation(result, &fixture.truth);
    let winning_cluster = result
        .physical_period_bpm
        .and_then(|bpm| {
            result.period_clusters.iter().min_by(|left, right| {
                ((left.representative_bpm / bpm - 1.0).abs())
                    .total_cmp(&((right.representative_bpm / bpm - 1.0).abs()))
            })
        })
        .or_else(|| result.period_clusters.first());
    let winning_channels = winning_cluster
        .map(|cluster| cluster.independent_channels.clone())
        .unwrap_or_default();
    let winning_id = winning_cluster.map(|cluster| cluster.cluster_id);
    let competing_channels = result
        .period_clusters
        .iter()
        .filter(|cluster| Some(cluster.cluster_id) != winning_id)
        .flat_map(|cluster| cluster.independent_channels.iter().cloned())
        .collect::<Vec<_>>();
    SyntheticHardeningCase {
        fixture_id: fixture.fixture_id.clone(),
        truth_bpm: fixture.truth.truth_bpm,
        valid_physical_bpms: fixture.truth.valid_physical_bpms.clone(),
        valid_canonical_bpms: fixture.truth.valid_canonical_bpms.clone(),
        stationary_expected: fixture.truth.stationary_expected,
        allowed_period_status: fixture.truth.allowed_period_status.clone(),
        period_status: result.period_status.clone(),
        period_confidence: result.period_confidence.clone(),
        selected_physical_bpm: result.physical_period_bpm,
        relative_error: evaluation.selected_error_relative,
        selected_error_relative: evaluation.selected_error_relative,
        candidate_count: result.period_candidates.len(),
        candidates: result.period_candidates.clone(),
        clusters: result.period_clusters.clone(),
        winning_channels,
        competing_channels,
        period_conflict: result.period_conflict.is_some(),
        canonical_status: result.canonical_layer.status.clone(),
        truth_relation: evaluation.truth_relation,
        confidently_correct: evaluation.confidently_correct,
        safely_ambiguous: evaluation.safely_ambiguous,
        safely_abstained: evaluation.safely_abstained,
        evaluation_class: evaluation.evaluation_class.clone(),
        false_confident: evaluation.false_confident,
        passed: evaluation.passed,
        detail: fixture.detail.clone(),
    }
}

fn synthetic_hardening_fixtures() -> Vec<SyntheticFixture> {
    let allow_selected = ["SELECTED"];
    let allow_safe = ["SELECTED", "RETAIN_MULTIPLE", "ABSTAIN"];
    let mut fixtures = Vec::new();
    for bpm in [95.7, 119.8, 127.35, 129.7, 133.4, 174.2] {
        fixtures.push(synthetic_fixture(
            &format!("clean_stationary_{bpm}"),
            synthetic_grid_events(60_000_000.0 / bpm, 64, 123_456.0),
            Some(bpm),
            vec![bpm],
            vec![bpm],
            true,
            &allow_selected,
            "clean stationary clock with an exact non-integer period",
        ));
    }
    fixtures.push(synthetic_fixture(
        "biased_adjacent_median",
        patterned_events(500_000.0, 64, 10_000.0, &[0.97, 0.97, 1.06]),
        Some(120.0),
        vec![120.0],
        vec![120.0],
        true,
        &allow_safe,
        "adjacent median is biased while the repeating long-span clock is 500 ms",
    ));
    fixtures.push(synthetic_fixture(
        "alternating_quantization_bias",
        offset_events(60_000_000.0 / 119.8, 64, 20_000.0, &[8_000.0, -8_000.0]),
        Some(119.8),
        vec![119.8],
        vec![119.8],
        true,
        &allow_safe,
        "bounded alternating timing offsets",
    ));
    fixtures.push(synthetic_fixture(
        "compensating_local_bias",
        patterned_events(
            60_000_000.0 / 127.35,
            64,
            30_000.0,
            &[0.96, 0.96, 0.96, 1.12],
        ),
        Some(127.35),
        vec![127.35],
        vec![127.35],
        true,
        &allow_safe,
        "short consecutive intervals are compensated by a longer interval",
    ));
    for offset in [80_000_u64, 150_000, 250_000] {
        let mut first = synthetic_grid_events(60_000_000.0 / 129.7, 64, 40_000.0);
        first[0] += offset;
        fixtures.push(synthetic_fixture(
            &format!("bad_first_endpoint_{offset}"),
            first,
            Some(129.7),
            vec![129.7],
            vec![129.7],
            true,
            &allow_safe,
            "only the first observed event is corrupted",
        ));
        let mut last = synthetic_grid_events(60_000_000.0 / 129.7, 64, 40_000.0);
        *last.last_mut().expect("synthetic endpoint") += offset;
        fixtures.push(synthetic_fixture(
            &format!("bad_last_endpoint_{offset}"),
            last,
            Some(129.7),
            vec![129.7],
            vec![129.7],
            true,
            &allow_safe,
            "only the final observed event is corrupted",
        ));
    }
    let random_base = synthetic_grid_events(60_000_000.0 / 133.4, 80, 50_000.0);
    for (label, fraction) in [
        ("random_missing_5pct", 20_usize),
        ("random_missing_10pct", 10),
        ("random_missing_20pct", 5),
    ] {
        fixtures.push(synthetic_fixture(
            label,
            remove_events(&random_base, |index| index > 0 && index % fraction == 0),
            Some(133.4),
            vec![133.4],
            vec![133.4],
            true,
            &allow_safe,
            "deterministic sparse missing-event pattern",
        ));
    }
    let periodic_base = synthetic_grid_events(60_000_000.0 / 174.2, 96, 60_000.0);
    for divisor in [4, 7, 8] {
        fixtures.push(synthetic_fixture(
            &format!("periodic_missing_every_{divisor}"),
            remove_events(&periodic_base, |index| index > 0 && index % divisor == 0),
            Some(174.2),
            vec![174.2],
            vec![174.2],
            true,
            &allow_safe,
            "periodic missing events test alternate-period support",
        ));
    }
    let subdivision_base = synthetic_grid_events(60_000_000.0 / 95.7, 64, 70_000.0);
    let mut subdivision = subdivision_base.clone();
    subdivision.insert(20, subdivision_base[20] - 300_000);
    fixtures.push(synthetic_fixture(
        "subdivision_contamination",
        subdivision,
        Some(95.7),
        vec![95.7],
        vec![95.7],
        true,
        &allow_safe,
        "one half-beat subdivision is inserted into an otherwise stationary clock",
    ));
    let duplicate_base = synthetic_grid_events(60_000_000.0 / 119.8, 64, 80_000.0);
    let mut duplicate = duplicate_base.clone();
    duplicate.insert(24, duplicate_base[24] + 1_000);
    fixtures.push(synthetic_fixture(
        "near_duplicate_event",
        duplicate,
        Some(119.8),
        vec![119.8],
        vec![119.8],
        true,
        &allow_safe,
        "one near-duplicate event is inserted without changing the physical clock",
    ));
    fixtures.push(synthetic_fixture(
        "nearby_false_period_174_vs_175",
        synthetic_grid_events(60_000_000.0 / 174.0, 96, 91_000.0),
        Some(174.0),
        vec![174.0],
        vec![174.0],
        true,
        &allow_safe,
        "long baseline must reject a plausible 175 BPM basin",
    ));
    for (id, bpm) in [
        ("nearby_129_vs_130", 129.0),
        ("nearby_95_7_vs_96", 95.7),
        ("nearby_133_vs_134", 133.0),
    ] {
        fixtures.push(synthetic_fixture(
            id,
            synthetic_grid_events(60_000_000.0 / bpm, 80, 100_000.0),
            Some(bpm),
            vec![bpm],
            vec![bpm],
            true,
            &allow_safe,
            "nearby-period robustness without a BPM-specific inference rule",
        ));
    }
    let alias = with_observed_hypotheses(
        synthetic_fixture(
            "half_double_60_120",
            synthetic_grid_events(1_000_000.0, 64, 110_000.0),
            Some(60.0),
            vec![60.0],
            vec![60.0, 120.0],
            true,
            &allow_selected,
            "physical 60 BPM with an observed exact double-time metrical layer",
        ),
        &[(60.0, "primary", 0.55), (120.0, "double_time", 0.45)],
        "primary",
    );
    fixtures.push(alias);
    fixtures.push(with_observed_hypotheses(
        synthetic_fixture(
            "clear_primary_layer",
            synthetic_grid_events(1_000_000.0, 64, 120_000.0),
            Some(60.0),
            vec![60.0],
            vec![60.0],
            true,
            &allow_selected,
            "one observed primary metrical layer has decisive weight",
        ),
        &[(60.0, "primary", 0.9)],
        "primary",
    ));
    let mut nonstationary = synthetic_grid_events(500_000.0, 64, 130_000.0);
    for (index, event) in nonstationary.iter_mut().enumerate() {
        *event += (index * index * 2_500) as u64;
    }
    fixtures.push(synthetic_fixture(
        "nonstationary_clock",
        nonstationary,
        Some(120.0),
        Vec::new(),
        Vec::new(),
        false,
        &["ABSTAIN"],
        "tempo ramp is not eligible for fixed-period authority",
    ));
    fixtures
}

fn synthetic_hardening_summary() -> SyntheticHardeningSummary {
    let cases = synthetic_hardening_fixtures()
        .into_iter()
        .map(|fixture| {
            let observed =
                synthetic_track_from_observations(&fixture.fixture_id, &fixture.observations);
            let result = build_track(&observed);
            synthetic_case_from_result(&fixture, &result)
        })
        .collect::<Vec<_>>();
    let correct_confident = cases
        .iter()
        .filter(|case| case.evaluation_class == "CORRECT_CONFIDENT")
        .count();
    let correct_acceptable = cases
        .iter()
        .filter(|case| case.evaluation_class == "CORRECT_ACCEPTABLE")
        .count();
    let safe_retain_multiple = cases
        .iter()
        .filter(|case| case.evaluation_class == "SAFE_RETAIN_MULTIPLE")
        .count();
    let safe_abstain = cases
        .iter()
        .filter(|case| case.evaluation_class == "SAFE_ABSTAIN")
        .count();
    let false_confident = cases.iter().filter(|case| case.false_confident).count();
    let unexpected_failures = cases
        .iter()
        .filter(|case| case.evaluation_class == "UNEXPECTED_FAILURE")
        .count();
    let selected_strong = cases
        .iter()
        .filter(|case| case.period_confidence == "SELECTED_STRONG")
        .count();
    let selected_strong_with_genuine_independent_origins = cases
        .iter()
        .filter(|case| {
            case.period_confidence == "SELECTED_STRONG"
                && case
                    .clusters
                    .iter()
                    .any(|cluster| cluster.independent_origins.len() >= 2)
        })
        .count();
    let strong_independence_passed =
        selected_strong == selected_strong_with_genuine_independent_origins;
    let channel_duplication_invariance = synthetic_channel_duplication_invariance();
    let all_passed = cases.iter().all(|case| case.passed)
        && false_confident == 0
        && strong_independence_passed
        && channel_duplication_invariance;
    SyntheticHardeningSummary {
        suite: "fixed_tempo_consensus_synthetic_hardening_e2e".into(),
        phase: "PRE_HARDENING_E2E_BASELINE".into(),
        source_commit: String::new(),
        external_reference_used_for_inference: false,
        truth_used_for_inference: false,
        cases,
        correct_confident,
        correct_acceptable,
        safe_retain_multiple,
        safe_abstain,
        false_confident_selections: false_confident,
        unexpected_failures,
        selected_strong,
        selected_strong_with_genuine_independent_origins,
        strong_independence_passed,
        channel_duplication_invariance,
        all_passed,
    }
}

fn synthetic_channel_duplication_invariance() -> bool {
    let observed = synthetic_track_from_observations(
        "channel_duplication_observation",
        &SyntheticObservedEvidence {
            events: synthetic_grid_events(60_000_000.0 / 129.0, 64, 0.0),
            duration_micros: 32_000_000,
            classical_candidates_bpm: Vec::new(),
            tempo_hypotheses: Vec::new(),
            primary_relation: "unknown".into(),
        },
    );
    let mut with_duplicates = vec![
        synthetic_candidate("local", 129.0, CHANNEL_LOCAL_INTERVAL, 1.0),
        synthetic_candidate("long-a", 129.0, CHANNEL_LONG_BASELINE, 3.0),
        synthetic_candidate("long-b", 129.0, CHANNEL_LONG_BASELINE, 3.0),
        synthetic_candidate("long-c", 129.0, CHANNEL_LONG_BASELINE, 3.0),
        synthetic_candidate("robust", 129.0, CHANNEL_ROBUST_GRID, 2.0),
    ];
    let duplicated_clusters = make_clusters(&mut with_duplicates);
    let duplicated_score = duplicated_clusters[0].score;
    let duplicated_bpm = weighted_cluster_bpm(&with_duplicates, 0);
    let duplicated_channels = duplicated_clusters[0].independent_channels.clone();
    let duplicated_selection = select_signature(&mut with_duplicates, &observed.full);
    let mut single = with_duplicates
        .into_iter()
        .filter(|candidate| {
            candidate.source == "local"
                || candidate.source == "long-a"
                || candidate.source == "robust"
        })
        .collect::<Vec<_>>();
    let single_clusters = make_clusters(&mut single);
    let single_selection = select_signature(&mut single, &observed.full);
    duplicated_score == single_clusters[0].score
        && duplicated_bpm == weighted_cluster_bpm(&single, 0)
        && duplicated_channels == single_clusters[0].independent_channels
        && duplicated_selection == single_selection
}

fn select_signature(
    candidates: &mut [FixedTempoPeriodCandidate],
    full: &RealSongAnalysisReport,
) -> (String, Option<f64>, String) {
    let clusters = make_clusters(candidates);
    let (status, bpm, conflict) = select_period(candidates, &clusters, full);
    let confidence = period_confidence(&status, &clusters, bpm);
    (
        status,
        bpm,
        confidence
            + if conflict.is_some() {
                ":CONFLICT"
            } else {
                ":CLEAR"
            },
    )
}

fn synthetic_markdown(summary: &SyntheticHardeningSummary, source_commit: &str) -> String {
    format!(
        "# Synthetic fixed-tempo consensus E2E validation\n\n\
         - source commit: `{source_commit}`\n\
         - phase: `{}`\n\
         - truth used for inference: NO\n\
         - external reference used for inference: NO\n\
         - fixtures: {}\n\
         - CORRECT_CONFIDENT: {}\n\
         - CORRECT_ACCEPTABLE: {}\n\
         - SAFE_RETAIN_MULTIPLE: {}\n\
         - SAFE_ABSTAIN: {}\n\
         - FALSE_CONFIDENT: {}\n\
         - UNEXPECTED_FAILURE: {}\n\
         - SELECTED_STRONG: {}\n\
         - SELECTED_STRONG with >=2 genuine independent origins: {}/{}\n\
         - strong independence: {}\n\
         - channel duplication invariance: {}\n\
         - all passed: {}\n\n\
         Each case entered the same candidate generation, channel aggregation,\
         clustering, conflict detection, selection, confidence, and canonical\
         decision path used by the blind real-song report. Truth is joined only\
         after inference by the evaluator.\n",
        summary.phase,
        summary.cases.len(),
        summary.correct_confident,
        summary.correct_acceptable,
        summary.safe_retain_multiple,
        summary.safe_abstain,
        summary.false_confident_selections,
        summary.unexpected_failures,
        summary.selected_strong,
        summary.selected_strong_with_genuine_independent_origins,
        summary.selected_strong,
        summary.strong_independence_passed,
        summary.channel_duplication_invariance,
        summary.all_passed,
    )
}

fn markdown(report: &FixedTempoConsensusReport) -> String {
    let summary = &report.summary;
    format!(
        "# Fixed-tempo period consensus research\n\n\
         - source commit: `{}`\n\
         - input report source commit: `{}`\n\
         - tracks: {}\n\
         - period SELECTED: {}\n\
         - period RETAIN_MULTIPLE: {}\n\
         - period ABSTAIN: {}\n\
         - canonical SELECTED: {}\n\
         - canonical RETAIN_MULTIPLE: {}\n\
         - period conflicts: {}\n\
         - synthetic hardening: {}/{} passed\n\
         - external reference used for inference: NO\n\n\
         This is a research-only candidate competition. Physical period selection\
         and canonical metrical-layer selection are intentionally separate.\n",
        report.source_commit,
        report.input_report_source_commit,
        summary.tracks,
        summary.period_selected,
        summary.period_retain_multiple,
        summary.period_abstain,
        summary.canonical_selected,
        summary.canonical_retain_multiple,
        summary.period_conflicts,
        report
            .synthetic_hardening
            .cases
            .iter()
            .filter(|case| case.passed)
            .count(),
        report.synthetic_hardening.cases.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_representative_clusters_do_not_chain() {
        let base = FixedTempoPeriodCandidate {
            source: "a".into(),
            evidence_channel: CHANNEL_LONG_BASELINE.into(),
            derived_from: "TEST".into(),
            independence_origin: ORIGIN_BEAT_EVENTS.into(),
            bpm: 120.0,
            period_micros: 0.0,
            support_events: 0,
            residual_median_micros: None,
            residual_p95_micros: None,
            explained_event_fraction: 0.0,
            missing_beat_fraction: 0.0,
            extra_event_fraction: 0.0,
            early_period_micros: None,
            middle_period_micros: None,
            late_period_micros: None,
            stationarity_error: None,
            agreement_sources: Vec::new(),
            agreement_channels: vec![CHANNEL_LONG_BASELINE.into()],
            phase_micros: None,
            cluster_id: None,
            score: 1.0,
            robust_fit_accepted: false,
        };
        let mut candidates = vec![
            base.clone(),
            FixedTempoPeriodCandidate {
                bpm: 120.2,
                source: "b".into(),
                ..base.clone()
            },
            FixedTempoPeriodCandidate {
                bpm: 120.4,
                source: "c".into(),
                ..base
            },
        ];
        let clusters = make_clusters(&mut candidates);
        assert_eq!(clusters.len(), 2);
        assert_ne!(candidates[0].cluster_id, candidates[2].cluster_id);
    }

    #[test]
    fn exact_half_double_alternative_is_retained_without_external_truth() {
        let hypotheses = vec![
            crate::real_song_research::TempoHypothesisReport {
                bpm: 75.0,
                relation: "primary".into(),
                relative_weight: 0.57,
            },
            crate::real_song_research::TempoHypothesisReport {
                bpm: 150.0,
                relation: "double_time".into(),
                relative_weight: 0.43,
            },
        ];
        let result = resolve_canonical_layer(Some(75.0), &hypotheses, "primary");
        assert_eq!(result.status, "CANONICAL_RETAIN_MULTIPLE");
        assert!(!result.external_reference_used_for_inference);
    }

    #[test]
    fn missing_and_extra_diagnostics_are_bounded() {
        let events = [0, 500_000, 1_500_000, 1_600_000, 2_000_000];
        let fit = fit_period_phase(&events, 500_000.0);
        assert_eq!(fit.residuals.len(), events.len());
        assert!(missing_fraction(&fit.indices) <= 1.0);
        assert!(extra_fraction(&events, 500_000.0) <= 1.0);
    }

    #[test]
    fn robust_phase_does_not_anchor_to_corrupted_endpoints() {
        let period = 500_000.0;
        let mut events = synthetic_grid_events(period, 32, 123_456.0);
        events[0] += 250_000;
        *events.last_mut().expect("synthetic endpoint") += 150_000;
        let fit = fit_period_phase(&events, period);
        assert!(median(&fit.residuals).expect("residuals") <= 2_000.0);
        assert!(
            fit.residuals
                .iter()
                .filter(|residual| **residual <= 2_000.0)
                .count()
                >= 30
        );
    }

    #[test]
    fn channel_duplication_does_not_inflate_consensus() {
        let mut with_duplicate = vec![
            synthetic_candidate("local", 129.0, CHANNEL_LOCAL_INTERVAL, 1.0),
            synthetic_candidate("long-a", 129.0, CHANNEL_LONG_BASELINE, 3.0),
            synthetic_candidate("long-b", 129.0, CHANNEL_LONG_BASELINE, 3.0),
            synthetic_candidate("robust", 129.0, CHANNEL_ROBUST_GRID, 2.0),
        ];
        let duplicate_clusters = make_clusters(&mut with_duplicate);
        let mut without_duplicate = with_duplicate
            .iter()
            .filter(|candidate| candidate.source != "long-b")
            .cloned()
            .collect::<Vec<_>>();
        let single_clusters = make_clusters(&mut without_duplicate);
        assert_eq!(duplicate_clusters[0].score, single_clusters[0].score);
        assert_eq!(
            weighted_cluster_bpm(&with_duplicate, 0),
            weighted_cluster_bpm(&without_duplicate, 0)
        );
        assert_eq!(duplicate_clusters[0].independent_channels.len(), 3);
    }

    #[test]
    fn long_duration_residuals_reject_a_nearby_false_period() {
        let events = synthetic_grid_events(60_000_000.0 / 174.0, 96, 91_000.0);
        let truth = fit_period_phase(&events, 60_000_000.0 / 174.0);
        let nearby = fit_period_phase(&events, 60_000_000.0 / 175.0);
        assert!(
            robust_phase_objective(&truth.residuals) < robust_phase_objective(&nearby.residuals)
        );
    }

    #[test]
    fn independent_nearby_clusters_are_not_collapsed() {
        let mut candidates = vec![
            synthetic_candidate("truth-long", 174.0, CHANNEL_LONG_BASELINE, 3.0),
            synthetic_candidate("truth-robust", 174.0, CHANNEL_ROBUST_GRID, 2.0),
            synthetic_candidate("wrong-long", 175.0, CHANNEL_LONG_BASELINE, 3.0),
            synthetic_candidate("wrong-local", 175.0, CHANNEL_LOCAL_INTERVAL, 1.0),
        ];
        let clusters = make_clusters(&mut candidates);
        assert_eq!(clusters.len(), 2);
        assert!((clusters[0].score - clusters[1].score).abs() <= 2.0);
    }

    #[test]
    fn synthetic_hardening_suite_has_no_false_confident_case() {
        let summary = synthetic_hardening_summary();
        assert!(summary.cases.len() >= 19);
        assert_eq!(summary.false_confident_selections, 0);
        assert_eq!(summary.unexpected_failures, 0);
        assert!(summary.strong_independence_passed);
        assert_eq!(
            summary.selected_strong,
            summary.selected_strong_with_genuine_independent_origins
        );
        assert!(summary.channel_duplication_invariance);
        assert!(summary.all_passed);
        assert_eq!(
            summary.cases.len(),
            summary.correct_confident
                + summary.correct_acceptable
                + summary.safe_retain_multiple
                + summary.safe_abstain
                + summary.false_confident_selections
                + summary.unexpected_failures
        );
        assert!(!summary.external_reference_used_for_inference);
        assert!(!summary.truth_used_for_inference);
    }

    #[test]
    fn event_only_synthetic_fixtures_have_one_genuine_origin() {
        let fixture = synthetic_hardening_fixtures()
            .into_iter()
            .find(|fixture| fixture.fixture_id == "clean_stationary_119.8")
            .expect("event-only fixture");
        assert!(fixture.observations.classical_candidates_bpm.is_empty());
        let observed =
            synthetic_track_from_observations(&fixture.fixture_id, &fixture.observations);
        let result = build_track(&observed);
        assert!(
            result
                .period_candidates
                .iter()
                .all(|candidate| candidate.evidence_channel != CHANNEL_CLASSICAL_METRICAL)
        );
        assert!(
            result
                .period_clusters
                .iter()
                .all(|cluster| cluster.independent_origins == vec![ORIGIN_BEAT_EVENTS])
        );
        assert_eq!(result.period_confidence, "SELECTED_MODERATE");
    }

    #[test]
    fn explicit_metrical_fixture_records_a_second_origin() {
        let fixture = synthetic_hardening_fixtures()
            .into_iter()
            .find(|fixture| fixture.fixture_id == "clear_primary_layer")
            .expect("explicit metrical fixture");
        assert!(!fixture.observations.classical_candidates_bpm.is_empty());
        let observed =
            synthetic_track_from_observations(&fixture.fixture_id, &fixture.observations);
        let result = build_track(&observed);
        assert!(result.period_candidates.iter().any(|candidate| {
            candidate.evidence_channel == CHANNEL_CLASSICAL_METRICAL
                && candidate.independence_origin == ORIGIN_INDEPENDENT_METRICAL
        }));
        assert!(
            result
                .period_clusters
                .iter()
                .any(|cluster| cluster.independent_origins.len() >= 2)
        );
        assert_eq!(result.period_confidence, "SELECTED_STRONG");
    }

    #[test]
    fn false_confidence_is_derived_from_selector_result() {
        let fixture = synthetic_hardening_fixtures()
            .into_iter()
            .find(|fixture| fixture.fixture_id == "clean_stationary_119.8")
            .expect("synthetic fixture");
        let observed =
            synthetic_track_from_observations(&fixture.fixture_id, &fixture.observations);
        let mut selected = build_track(&observed);
        selected.period_status = "SELECTED".into();
        selected.period_confidence = "SELECTED_STRONG".into();
        selected.physical_period_bpm = Some(121.0);
        let evaluation = period_selection_evaluation(&selected, &fixture.truth);
        assert!(evaluation.false_confident);
        assert_eq!(evaluation.evaluation_class, "FALSE_CONFIDENT");
        assert!(!evaluation.passed);
    }
}
