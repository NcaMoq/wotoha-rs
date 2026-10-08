//! Research-only competition between fixed-tempo period estimators.
//!
//! This module consumes the neutral, blind real-song analysis report produced
//! by `research-real-songs`.  It never consumes an external BPM while making
//! an inference.  External values may be joined by a separate evaluator after
//! this report has been frozen.

use std::{fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    LabError,
    real_song_research::{RealSongAnalysisReport, RealSongResearchReport, RealSongTrackReport},
    write_json,
};

const REPORT_SCHEMA: u32 = 1;
const PERIOD_CONSENSUS_RELATIVE: f64 = 0.002;
const STRONG_AGREEMENT_RELATIVE: f64 = 0.0005;
const MODERATE_AGREEMENT_RELATIVE: f64 = 0.001;
const WEAK_AGREEMENT_RELATIVE: f64 = 0.002;
const RESIDUAL_MATCH_MICROS: f64 = 35_000.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FixedTempoConsensusReport {
    pub schema_version: u32,
    pub source_commit: String,
    pub input_report_source_commit: String,
    pub research_only: bool,
    pub input_report: String,
    pub configuration: ConsensusConfiguration,
    pub tracks: Vec<FixedTempoConsensusTrack>,
    pub summary: FixedTempoConsensusSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsensusConfiguration {
    pub period_cluster_relative: f64,
    pub strong_agreement_relative: f64,
    pub moderate_agreement_relative: f64,
    pub weak_agreement_relative: f64,
    pub residual_match_micros: f64,
    pub selection_rule: String,
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
    pub physical_period_bpm: Option<f64>,
    pub physical_period_micros: Option<f64>,
    pub period_conflict: Option<PeriodConflict>,
    pub canonical_layer: CanonicalLayerDecision,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FixedTempoPeriodCandidate {
    pub source: String,
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
            canonical_layer_rule: "physical period is resolved first; a metrical alternative is retained only when a runtime tempo hypothesis is an exact half/double layer with substantial weight and no decisive separation".into(),
            external_reference_used_for_inference: false,
        },
        tracks: output_tracks_sorted(tracks),
        summary,
    };
    write_json(&output_dir.join("fixed-tempo-consensus.json"), &output)?;
    fs::write(
        output_dir.join("fixed-tempo-consensus.md"),
        markdown(&output),
    )?;
    Ok(output)
}

fn output_tracks_sorted(
    mut tracks: Vec<FixedTempoConsensusTrack>,
) -> Vec<FixedTempoConsensusTrack> {
    tracks.sort_by(|left, right| left.source_filename.cmp(&right.source_filename));
    tracks
}

fn build_track(track: &RealSongTrackReport) -> FixedTempoConsensusTrack {
    let full = &track.full;
    let grid = &full.grid_method_comparison;
    let mut specs = Vec::new();
    add_spec(
        &mut specs,
        "ADJACENT_MEDIAN",
        grid.adjacent_median_bpm,
        "local",
        1.0,
    );
    add_spec(
        &mut specs,
        "TRIMMED_ADJACENT_MEDIAN",
        grid.trimmed_adjacent_median_bpm,
        "local",
        1.0,
    );
    add_spec(
        &mut specs,
        "EARLY_EVENT_CLOCK",
        grid.early_event_clock_bpm,
        "segment",
        0.5,
    );
    add_spec(
        &mut specs,
        "MIDDLE_EVENT_CLOCK",
        grid.middle_event_clock_bpm,
        "segment",
        0.5,
    );
    add_spec(
        &mut specs,
        "LATE_EVENT_CLOCK",
        grid.late_event_clock_bpm,
        "segment",
        0.5,
    );
    add_spec(
        &mut specs,
        "SEQUENTIAL_GLOBAL_REGRESSION",
        grid.sequential_global_regression_bpm,
        "long_baseline",
        3.0,
    );
    add_spec(
        &mut specs,
        "MISSING_JUMP_GLOBAL_REGRESSION",
        grid.missing_jump_global_regression_bpm,
        "long_baseline",
        3.0,
    );
    add_spec(
        &mut specs,
        "SEQUENTIAL_ENDPOINT",
        grid.sequential_endpoint_bpm,
        "long_baseline",
        2.0,
    );
    add_spec(
        &mut specs,
        "MISSING_JUMP_ENDPOINT",
        grid.missing_jump_endpoint_bpm,
        "long_baseline",
        2.0,
    );
    add_spec(
        &mut specs,
        "ROBUST_GRID_FIT",
        full.refined_grid.fit_bpm,
        "robust_grid",
        2.0,
    );
    if let Some(bpm) = full.classical_candidates_bpm.first() {
        add_spec(
            &mut specs,
            "CLASSICAL_FULL_CANDIDATE",
            Some(f64::from(*bpm)),
            "metrical",
            0.25,
        );
    }
    if let Some(bpm) = full.classical_candidates_bpm.get(1) {
        add_spec(
            &mut specs,
            "CLASSICAL_LOW_CANDIDATE",
            Some(f64::from(*bpm)),
            "metrical",
            0.25,
        );
    }
    let mut candidates = specs
        .into_iter()
        .map(|spec| score_candidate(spec, &full.raw_event_times_micros, full))
        .collect::<Vec<_>>();
    let clusters = make_clusters(&mut candidates);
    let (period_status, physical_period_bpm, period_conflict) =
        select_period(&mut candidates, &clusters, full);
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
    weight: f64,
) {
    if let Some(bpm) = bpm.filter(|value| value.is_finite() && *value > 0.0) {
        specs.push(CandidateSpec {
            source,
            bpm,
            channel,
            weight,
        });
    }
}

fn score_candidate(
    spec: CandidateSpec,
    events: &[u64],
    full: &RealSongAnalysisReport,
) -> FixedTempoPeriodCandidate {
    let period = 60_000_000.0 / spec.bpm;
    let support = events.len();
    let (residuals, indices) = fixed_period_residuals(events, period);
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
        bpm: spec.bpm,
        period_micros: period,
        support_events: support,
        residual_median_micros: residual_median,
        residual_p95_micros: residual_p95,
        explained_event_fraction: explained,
        missing_beat_fraction: missing,
        extra_event_fraction: extra,
        early_period_micros: full
            .grid_method_comparison
            .early_event_clock_bpm
            .map(|bpm| 60_000_000.0 / bpm),
        middle_period_micros: full
            .grid_method_comparison
            .middle_event_clock_bpm
            .map(|bpm| 60_000_000.0 / bpm),
        late_period_micros: full
            .grid_method_comparison
            .late_event_clock_bpm
            .map(|bpm| 60_000_000.0 / bpm),
        stationarity_error: full.stationarity.relative_period_drift,
        agreement_sources: Vec::new(),
        agreement_channels: vec![spec.channel.into()],
        cluster_id: None,
        score,
        robust_fit_accepted: spec.source == "ROBUST_GRID_FIT" && full.refined_grid.accepted,
    }
}

fn fixed_period_residuals(events: &[u64], period: f64) -> (Vec<f64>, Vec<i64>) {
    if events.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let phase = events[0] as f64;
    let mut indices = Vec::with_capacity(events.len());
    let mut residuals = Vec::with_capacity(events.len());
    let mut previous = 0_i64;
    for (position, time) in events.iter().enumerate() {
        let proposed = ((*time as f64 - phase) / period).round() as i64;
        let index = if position == 0 {
            proposed
        } else {
            proposed.max(previous + 1)
        };
        previous = index;
        indices.push(index);
        residuals.push((*time as f64 - (phase + index as f64 * period)).abs());
    }
    (residuals, indices)
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
            });
            id
        };
        candidates[candidate_index].cluster_id = Some(id);
        let cluster = &mut clusters[id];
        cluster
            .member_sources
            .push(candidates[candidate_index].source.clone());
        cluster.member_count += 1;
        cluster.score += candidates[candidate_index].score;
        for channel in &candidates[candidate_index].agreement_channels {
            if !cluster.independent_channels.contains(channel) {
                cluster.independent_channels.push(channel.clone());
            }
        }
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

fn weighted_cluster_bpm(
    candidates: &[FixedTempoPeriodCandidate],
    cluster_id: usize,
) -> Option<f64> {
    let members = candidates
        .iter()
        .filter(|candidate| candidate.cluster_id == Some(cluster_id))
        .collect::<Vec<_>>();
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
        .map(|pair| (pair[1] - pair[0] - 1).max(0) as f64)
        .sum::<f64>();
    missing / (indices.last().copied().unwrap_or(0) - indices[0]).max(1) as f64
}

fn extra_fraction(events: &[u64], period: f64) -> f64 {
    if events.len() < 2 {
        return 0.0;
    }
    events
        .windows(2)
        .filter(|pair| ((pair[1] - pair[0]) as f64) < period * 0.5)
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
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_representative_clusters_do_not_chain() {
        let base = FixedTempoPeriodCandidate {
            source: "a".into(),
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
            agreement_channels: vec!["long_baseline".into()],
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
        let (residuals, indices) = fixed_period_residuals(&events, 500_000.0);
        assert_eq!(residuals.len(), events.len());
        assert!(missing_fraction(&indices) <= 1.0);
        assert!(extra_fraction(&events, 500_000.0) <= 1.0);
    }
}
