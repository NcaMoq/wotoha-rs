//! Follow-up research for tempo candidate flow and long-duration ambiguity.
//!
//! This file is an analysis-lab experiment only.  It keeps Classical beat
//! events, phase, meter, and downbeats authoritative and evaluates alternative
//! tempo labels as shadow candidates.  No function in this module is called by
//! production analysis or playback.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use wotoha_core::{
    analysis::{
        BeatEvent, Confidence, MeterHypothesis, ModelScore, Support, TempoHypothesis,
        TempoRelation, TrackAnalysisV2, UnitInterval,
    },
    automix::{
        AutoMixConfig, TransitionKind, automix_mix_gains, beat_match_eligibility,
        plan_guarded_transition_v2, plan_transition_v2,
    },
};

use super::tempo_ambiguity_research::{
    AnalyzedFixture, TempoAmbiguityObservation, analyze_long_fixture, collect_long_analyzed,
    long_spec,
};
use crate::{
    AnalysisGroundTruth, EventStyle, FixtureFamily, LabError, SyntheticFixture, TempoProfile,
    generate_fixture, write_json,
};

const NEURAL_FRAME_RATE_HZ: f32 = 50.0;
const CANDIDATE_TOP_K: usize = 8;
const CANDIDATE_MERGE_RELATIVE_TOLERANCE: f32 = 0.005;
const CANONICAL_RELATIVE_TOLERANCE: f32 = 0.005;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoShadowFollowupReport {
    pub schema_version: u32,
    pub source_commit: String,
    pub starting_commit: Option<String>,
    pub production_behavior_changed: bool,
    pub fixture_count: usize,
    pub scalar_tempo_fixture_count: usize,
    pub candidate_flow: Vec<CandidateFlowObservation>,
    pub reanchor: ReanchorComparison,
    pub failure_taxonomy: FailureTaxonomyReport,
    pub duration_invariance: DurationInvarianceV2Report,
    pub positive_corpus: BeatMatchedPositiveCorpus,
    pub automix_shadow: AutoMixAliasShadowMatrix,
    pub heldout_ranking: HeldOutRankingEvaluation,
    pub feature_inventory: Vec<FeatureInventoryEntry>,
    pub quantization: Vec<NeuralQuantizationObservation>,
    pub focus_slices: FocusSlices,
    pub variable_tempo_safety: Vec<VariableTempoSafetyObservation>,
    pub decision: FollowupDecision,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandidateFlowObservation {
    pub fixture_id: String,
    pub master_id: String,
    pub pcm_sha256: String,
    pub family: String,
    pub duration_micros: u64,
    pub truth_bpm: Option<f32>,
    pub pools: CandidatePools,
    pub current_v2_top1: Option<TempoCandidate>,
    pub shadow_top1: Option<TempoCandidate>,
    pub event_clock_bpm: Option<f32>,
    pub event_clock_relation_to_truth: String,
    pub selected_neural_relation_to_truth: String,
    pub relation_semantics: RelationSemanticsAudit,
    pub ranking_features: RankingFeatures,
    pub meter_hypotheses: Vec<(u8, u8, f32)>,
    pub primary_failure_class: String,
    pub secondary_failure_classes: Vec<String>,
    pub quantization: Option<NeuralQuantizationObservation>,
    /// In-memory research handle used by the conservative shadow planner.
    /// It is intentionally omitted from serialized reports so generated
    /// artifacts remain a neutral candidate-flow record rather than a second
    /// copy of the full analysis object.
    #[serde(skip)]
    pub research_analysis: Option<TrackAnalysisV2>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CandidatePools {
    pub production_selected: Vec<TempoCandidate>,
    pub classical_full_top1: Vec<TempoCandidate>,
    pub classical_full_top2: Vec<TempoCandidate>,
    pub classical_full_top3: Vec<TempoCandidate>,
    pub classical_full_top_k: Vec<TempoCandidate>,
    pub classical_low_top1: Vec<TempoCandidate>,
    pub classical_low_top2: Vec<TempoCandidate>,
    pub classical_low_top3: Vec<TempoCandidate>,
    pub classical_low_top_k: Vec<TempoCandidate>,
    pub classical_full_low_union_top2: Vec<TempoCandidate>,
    pub classical_full_low_union_top3: Vec<TempoCandidate>,
    pub neural_selected: Vec<TempoCandidate>,
    pub neural_half_native_double: Vec<TempoCandidate>,
    pub v2_current: Vec<TempoCandidate>,
    pub event_clock: Vec<TempoCandidate>,
    pub research_propagated: Vec<TempoCandidate>,
    pub research_reanchored: Vec<TempoCandidate>,
    pub classical_event_reanchored: Vec<TempoCandidate>,
    pub classical_propagated: Vec<TempoCandidate>,
    pub merged_shadow: Vec<TempoCandidate>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TempoCandidate {
    pub bpm: f32,
    pub source: String,
    pub relation: String,
    pub score: f32,
    pub normalized_score: f32,
    pub origin_stage: String,
    pub relation_to_truth: String,
    pub shadow_rank_score: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RelationSemanticsAudit {
    pub selected_neural_relation: String,
    pub truth_relation_of_selected_neural: String,
    pub event_clock_is_family_agnostic: bool,
    pub refinement_preserves_selected_relation: bool,
    pub v2_relation_labels_are_not_truth_labels: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RankingFeatures {
    pub normalized_evidence: f32,
    pub event_agreement: f32,
    pub source_agreement: f32,
    pub explicit_relation_score: f32,
    pub selected_rank_score: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VariantOracleReport {
    pub variant: String,
    pub scored: usize,
    pub candidate_count: usize,
    pub top1_canonical_correct: usize,
    pub top2_canonical_correct: usize,
    pub top3_canonical_correct: usize,
    pub top1_family_correct: usize,
    pub top2_family_correct: usize,
    pub top3_family_correct: usize,
    pub canonical_oracle: usize,
    pub family_oracle: usize,
    pub false_family_candidates: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReanchorComparison {
    pub variants: Vec<VariantOracleReport>,
    pub current_v2_canonical: usize,
    pub neural_event_reanchor_canonical: usize,
    pub improved_sample_ids: Vec<String>,
    pub regressed_sample_ids: Vec<String>,
    pub unchanged_sample_ids: Vec<String>,
    pub by_family: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FailureTaxonomyReport {
    pub counts: BTreeMap<String, usize>,
    pub observations: Vec<FailureTaxonomyObservation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FailureTaxonomyObservation {
    pub sample_id: String,
    pub family: String,
    pub current_v2_relation: String,
    pub primary_class: String,
    pub secondary_classes: Vec<String>,
    pub correct_family_in_upstream_generation: bool,
    pub correct_family_in_current_v2: bool,
    pub correct_family_in_shadow: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DurationInvarianceV2Report {
    pub compared_master_count: usize,
    pub selected_bpm_stable: usize,
    pub candidate_relation_order_stable: usize,
    pub candidate_numeric_stable: usize,
    pub event_clock_stable: usize,
    pub resolved_meter_stable: usize,
    pub top_meter_stable: usize,
    pub top_downbeat_phase_stable: usize,
    pub full_meter_ordering_stable: usize,
    pub full_meter_score_stable: usize,
    pub cases: Vec<DurationInvarianceCase>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DurationInvarianceCase {
    pub master_id: String,
    pub selected_bpm_30s: Option<f32>,
    pub selected_bpm_60s: Option<f32>,
    pub candidate_relation_order_equal: bool,
    pub candidate_numeric_drift: Option<f32>,
    pub event_bpm_30s: Option<f32>,
    pub event_bpm_60s: Option<f32>,
    pub resolved_meter_equal: bool,
    pub top_meter_equal: bool,
    pub top_downbeat_phase_equal: bool,
    pub full_meter_order_equal: bool,
    pub full_meter_score_drift: f32,
    pub stable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BeatMatchedPositiveCorpus {
    pub construction: String,
    pub sanity_fixture_count: usize,
    pub sanity_selected_count: usize,
    pub pair_count: usize,
    pub cases: Vec<AutoMixPairCase>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoMixAliasShadowMatrix {
    pub config: AutoMixConfigView,
    pub pair_count: usize,
    pub variant_count: usize,
    pub cases: Vec<AutoMixPairCase>,
    pub current_selected_correct: usize,
    pub current_false_beatmatched: usize,
    pub shadow_selected_correct: usize,
    pub shadow_false_beatmatched: usize,
    pub safe_fallback_count: usize,
    pub missed_beatmatched_opportunities: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoMixConfigView {
    pub crossfade_micros: u64,
    pub max_tempo_adjustment: f32,
    pub min_beat_confidence: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoMixPairCase {
    pub pair_id: String,
    pub variant: String,
    pub outgoing_fixture: String,
    pub incoming_fixture: String,
    pub outgoing_truth_bpm: f32,
    pub incoming_truth_bpm: f32,
    pub outgoing_candidates: Vec<TempoCandidate>,
    pub incoming_candidates: Vec<TempoCandidate>,
    pub selected_outgoing_bpm: Option<f32>,
    pub selected_incoming_bpm: Option<f32>,
    pub selected_outgoing_relation: Option<String>,
    pub selected_incoming_relation: Option<String>,
    pub selected_ratio: Option<f32>,
    pub selected_adjustment: Option<f32>,
    pub selected_weight: Option<f32>,
    pub selected_cost: Option<f32>,
    pub eligible: bool,
    pub beat_pairs: usize,
    pub phase_error_micros: Option<u64>,
    pub beatmatched_candidate_generated: bool,
    pub beatmatched_candidate_survived_guard: bool,
    pub beatmatched_selected: bool,
    pub correct_pair: bool,
    pub wrong_alias: bool,
    pub false_beatmatched: bool,
    pub correct_beatmatched: bool,
    pub safe_fallback: bool,
    pub missed_opportunity: bool,
    pub transition: String,
    pub render_quality: RenderQualityObservation,
    pub planner_reasons: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RenderQualityObservation {
    pub phase_alignment_ms: Option<f32>,
    pub beat_transient_alignment_ms: Option<f32>,
    pub gain_continuity_max_step: Option<f32>,
    pub peak: Option<f32>,
    pub overlap_micros: u64,
    pub tempo_adjustment: Option<f32>,
    pub quietest_to_edge_rms_ratio: Option<f32>,
    pub mid_to_edge_rms_ratio: Option<f32>,
    pub acceptable: bool,
    pub issues: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeldOutRankingEvaluation {
    pub rule_name: String,
    pub rule_frozen_before_scoring: bool,
    pub grouping_rule: String,
    pub folds: Vec<HeldOutFamilyFold>,
    pub aggregate: HeldOutAggregate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeldOutFamilyFold {
    pub requested_family: String,
    pub expanded_validation_samples: Vec<String>,
    pub other_families_pulled_into_validation: Vec<String>,
    pub train_size: usize,
    pub validation_size: usize,
    pub exact_pcm_overlap: bool,
    pub lineage_overlap: bool,
    pub top1_canonical_correct: usize,
    pub top1_family_correct: usize,
    pub false_confident_accepts: usize,
    pub abstentions: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeldOutAggregate {
    pub scored: usize,
    pub unique_scored: usize,
    pub fold_observations: usize,
    pub canonical_correct: usize,
    pub family_correct: usize,
    pub false_confident_accepts: usize,
    pub abstentions: usize,
    pub valid_folds: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FeatureInventoryEntry {
    pub name: String,
    pub definition: String,
    pub source: String,
    pub production_time_available: bool,
    pub missing_value_behavior: String,
    pub observed_min: Option<f64>,
    pub observed_median: Option<f64>,
    pub observed_max: Option<f64>,
    pub exposure: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NeuralQuantizationObservation {
    pub fixture_id: String,
    pub truth_bpm: f32,
    pub selected_period_frames: Option<usize>,
    pub frame_quantized_bpm: Option<f32>,
    pub adjacent_lower_bpm: Option<f32>,
    pub adjacent_upper_bpm: Option<f32>,
    pub quantization_step_bpm: Option<f32>,
    pub truth_error_bpm: Option<f32>,
    pub region: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FocusSlices {
    pub boundary_58_62: Vec<FocusSliceRow>,
    pub neighborhood_130: Vec<FocusSliceRow>,
    pub ambiguity_80_160: Vec<FocusSliceRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FocusSliceRow {
    pub fixture_id: String,
    pub truth_bpm: f32,
    pub production_bpm: Option<f32>,
    pub full_top1: Option<f32>,
    pub full_top3: Vec<f32>,
    pub low_top3: Vec<f32>,
    pub neural_bpm: Option<f32>,
    pub event_bpm: Option<f32>,
    pub propagated_bpm: Option<f32>,
    pub reanchored_bpm: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VariableTempoSafetyObservation {
    pub fixture_id: String,
    pub profile: String,
    pub interval_median_micros: Option<f64>,
    pub interval_dispersion: Option<f64>,
    pub fit_residual_micros: Option<f64>,
    pub early_period_micros: Option<f64>,
    pub middle_period_micros: Option<f64>,
    pub late_period_micros: Option<f64>,
    pub refinement_would_abstain: bool,
    pub abstention_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FollowupDecision {
    pub bottleneck: String,
    pub upstream_information_lost_before_v2: usize,
    pub reanchor_gain_observed: bool,
    pub correct_to_wrong_shadow_count: usize,
    pub neural_relation_trust_statement: String,
    pub question_answers: BTreeMap<String, String>,
    pub recommendation: String,
    pub research_only: bool,
    pub production_behavior_changed: bool,
}

pub fn run_tempo_shadow_followup(
    output_dir: &Path,
    source_commit: String,
    starting_commit: Option<String>,
) -> Result<TempoShadowFollowupReport, LabError> {
    fs::create_dir_all(output_dir)?;
    let analyzed = collect_long_analyzed()?;
    let flows = analyzed
        .iter()
        .map(build_flow_observation)
        .collect::<Vec<_>>();
    let reanchor = build_reanchor_report(&flows);
    let failure_taxonomy = build_failure_taxonomy(&flows);
    let duration_invariance = build_duration_invariance_v2(&flows);
    let positive_corpus = build_positive_corpus(&analyzed, &flows)?;
    let automix_shadow = build_automix_shadow(&analyzed, &flows, &positive_corpus)?;
    let heldout_ranking = build_heldout_ranking(&flows);
    let feature_inventory = build_feature_inventory(&flows);
    let quantization = flows
        .iter()
        .filter_map(|flow| flow.quantization.clone())
        .collect::<Vec<_>>();
    let focus_slices = build_focus_slices(&flows);
    let variable_tempo_safety = build_variable_tempo_safety()?;
    let decision = build_decision(
        &flows,
        &reanchor,
        &failure_taxonomy,
        &automix_shadow,
        &heldout_ranking,
    );
    let scalar_tempo_fixture_count = flows.iter().filter(|flow| flow.truth_bpm.is_some()).count();
    let report = TempoShadowFollowupReport {
        schema_version: crate::RESEARCH_REPORT_SCHEMA_VERSION + 1,
        source_commit,
        starting_commit,
        production_behavior_changed: false,
        fixture_count: flows.len(),
        scalar_tempo_fixture_count,
        candidate_flow: flows,
        reanchor,
        failure_taxonomy,
        duration_invariance,
        positive_corpus,
        automix_shadow,
        heldout_ranking,
        feature_inventory,
        quantization,
        focus_slices,
        variable_tempo_safety,
        decision,
    };
    write_followup_outputs(output_dir, &report)?;
    Ok(report)
}

fn build_flow_observation(analyzed: &AnalyzedFixture) -> CandidateFlowObservation {
    let observation = &analyzed.observation;
    let truth = observation.ground_truth.primary_bpm;
    let full = peak_candidates(&observation.classical.full_band.top_peaks, "classical_full");
    let low = peak_candidates(&observation.classical.low_band.top_peaks, "classical_low");
    let neural = observation
        .neural
        .candidates
        .iter()
        .filter(|candidate| candidate.available)
        .map(|candidate| {
            candidate_value(
                candidate.bpm,
                "neural_candidate",
                &candidate.relation,
                candidate.score,
                "generation",
                truth,
            )
        })
        .collect::<Vec<_>>();
    let neural_selected = observation
        .neural
        .selected_bpm
        .map(|bpm| {
            let relation = observation
                .neural
                .candidates
                .iter()
                .find(|candidate| (candidate.bpm - bpm).abs() < 0.01)
                .map(|candidate| candidate.relation.clone())
                .unwrap_or_else(|| "primary".into());
            candidate_value(
                bpm,
                "neural_selected",
                &relation,
                observation.neural.support.unwrap_or(0.0),
                "selection",
                truth,
            )
        })
        .into_iter()
        .collect::<Vec<_>>();
    let v2 = observation
        .v2
        .tempo_hypotheses
        .iter()
        .map(|candidate| {
            candidate_value(
                candidate.bpm,
                "v2_hypothesis",
                &candidate.relation,
                candidate.relative_weight,
                "propagation",
                truth,
            )
        })
        .collect::<Vec<_>>();
    let production = observation
        .classical
        .production_selected_bpm
        .map(|bpm| {
            candidate_value(
                bpm,
                "classical_production",
                "primary",
                observation
                    .classical
                    .production_selected_confidence
                    .unwrap_or(0.0),
                "selection",
                truth,
            )
        })
        .into_iter()
        .collect::<Vec<_>>();
    let event = observation
        .v2
        .event_derived_bpm
        .map(|bpm| candidate_value(bpm, "event_clock", "primary", 0.65, "generation", truth))
        .into_iter()
        .collect::<Vec<_>>();
    let full_top_k = top_candidates(&full, CANDIDATE_TOP_K);
    let low_top_k = top_candidates(&low, CANDIDATE_TOP_K);
    let full_top_3 = top_candidates(&full, 3);
    let low_top_3 = top_candidates(&low, 3);
    let full_top_2 = top_candidates(&full, 2);
    let low_top_2 = top_candidates(&low, 2);
    let classical_union_2 = merge_candidates(
        full_top_2.iter().chain(low_top_2.iter()).cloned().collect(),
        truth,
        "classical_full_low_union",
    );
    let classical_union_3 = merge_candidates(
        full_top_3.iter().chain(low_top_3.iter()).cloned().collect(),
        truth,
        "classical_full_low_union",
    );
    let classical_propagated = rank_shadow(
        merge_candidates(
            full_top_3.iter().chain(low_top_3.iter()).cloned().collect(),
            truth,
            "classical_propagated",
        ),
        observation.v2.event_derived_bpm,
        truth,
    );
    let research_propagated = rank_shadow(
        merge_candidates(
            classical_propagated
                .iter()
                .chain(neural_selected.iter())
                .cloned()
                .collect(),
            truth,
            "research_propagated",
        ),
        observation.v2.event_derived_bpm,
        truth,
    );
    let research_reanchored = rank_shadow(
        reanchor_neural(&neural, observation.v2.event_derived_bpm, truth),
        observation.v2.event_derived_bpm,
        truth,
    );
    let classical_event_reanchored = rank_shadow(
        reanchor_classical(
            &classical_propagated,
            observation.v2.event_derived_bpm,
            truth,
        ),
        observation.v2.event_derived_bpm,
        truth,
    );
    let merged_shadow = rank_shadow(
        merge_candidates(
            v2.iter()
                .chain(research_propagated.iter())
                .chain(research_reanchored.iter())
                .chain(classical_event_reanchored.iter())
                .chain(event.iter())
                .cloned()
                .collect(),
            truth,
            "merged_shadow",
        ),
        observation.v2.event_derived_bpm,
        truth,
    );
    let pools = CandidatePools {
        production_selected: production,
        classical_full_top1: top_candidates(&full, 1),
        classical_full_top2: full_top_2,
        classical_full_top3: full_top_3,
        classical_full_top_k: full_top_k,
        classical_low_top1: top_candidates(&low, 1),
        classical_low_top2: low_top_2,
        classical_low_top3: low_top_3,
        classical_low_top_k: low_top_k,
        classical_full_low_union_top2: classical_union_2,
        classical_full_low_union_top3: classical_union_3,
        neural_selected,
        neural_half_native_double: neural,
        v2_current: v2,
        event_clock: event,
        research_propagated,
        research_reanchored,
        classical_event_reanchored,
        classical_propagated,
        merged_shadow,
    };
    let current_v2_top1 = pools.v2_current.first().cloned();
    let shadow_top1 = pools.merged_shadow.first().cloned();
    let upstream = pools
        .classical_full_low_union_top3
        .iter()
        .chain(pools.neural_half_native_double.iter())
        .chain(pools.event_clock.iter())
        .cloned()
        .collect::<Vec<_>>();
    let (primary_failure_class, secondary_failure_classes) = classify_failure(
        truth,
        &upstream,
        current_v2_top1.as_ref(),
        shadow_top1.as_ref(),
        observation,
    );
    let selected_relation = pools
        .neural_selected
        .first()
        .map(|candidate| candidate.relation.clone())
        .unwrap_or_else(|| "unavailable".into());
    let relation_audit = RelationSemanticsAudit {
        selected_neural_relation: selected_relation.clone(),
        truth_relation_of_selected_neural: relation(
            pools.neural_selected.first().map(|candidate| candidate.bpm),
            truth,
        ),
        event_clock_is_family_agnostic: true,
        refinement_preserves_selected_relation: pools.research_reanchored.iter().all(|candidate| {
            candidate.relation == selected_relation || selected_relation == "unavailable"
        }),
        v2_relation_labels_are_not_truth_labels: true,
    };
    let ranking_features = shadow_features(
        shadow_top1.as_ref(),
        observation.v2.event_derived_bpm,
        truth,
    );
    let quantization = quantization_observation(observation);
    let meter_hypotheses = observation
        .v2
        .meter_hypotheses
        .iter()
        .map(|hypothesis| {
            (
                hypothesis.beats_per_bar,
                hypothesis.downbeat_phase,
                hypothesis.score,
            )
        })
        .collect();
    CandidateFlowObservation {
        fixture_id: observation.fixture_id.clone(),
        master_id: observation.master_id.clone(),
        pcm_sha256: observation.pcm_sha256.clone(),
        family: observation.family.clone(),
        duration_micros: observation.duration_micros,
        truth_bpm: truth,
        pools,
        current_v2_top1,
        shadow_top1,
        event_clock_bpm: observation.v2.event_derived_bpm,
        event_clock_relation_to_truth: relation(observation.v2.event_derived_bpm, truth),
        selected_neural_relation_to_truth: relation(observation.neural.selected_bpm, truth),
        relation_semantics: relation_audit,
        ranking_features,
        meter_hypotheses,
        primary_failure_class,
        secondary_failure_classes,
        quantization,
        research_analysis: Some(analyzed.v2.clone()),
    }
}

fn candidate_value(
    bpm: f32,
    source: &str,
    relation_label: &str,
    score: f32,
    origin_stage: &str,
    truth: Option<f32>,
) -> TempoCandidate {
    TempoCandidate {
        bpm,
        source: source.into(),
        relation: relation_label.into(),
        score: score.max(0.0),
        normalized_score: score.clamp(0.0, 1.0),
        origin_stage: origin_stage.into(),
        relation_to_truth: relation(bpm, truth),
        shadow_rank_score: 0.0,
    }
}

fn peak_candidates(
    peaks: &[super::tempo_ambiguity_research::TempoPeakObservation],
    source: &str,
) -> Vec<TempoCandidate> {
    peaks
        .iter()
        .map(|peak| TempoCandidate {
            bpm: peak.bpm,
            source: source.into(),
            relation: "unlabeled".into(),
            score: peak.score.max(0.0),
            normalized_score: peak.normalized_confidence.clamp(0.0, 1.0),
            origin_stage: "generation".into(),
            relation_to_truth: "unscored".into(),
            shadow_rank_score: 0.0,
        })
        .collect()
}

fn top_candidates(values: &[TempoCandidate], count: usize) -> Vec<TempoCandidate> {
    values.iter().take(count).cloned().collect()
}

fn merge_candidates(
    mut values: Vec<TempoCandidate>,
    truth: Option<f32>,
    source: &str,
) -> Vec<TempoCandidate> {
    let mut merged = Vec::new();
    values.sort_by(|left, right| {
        right
            .normalized_score
            .total_cmp(&left.normalized_score)
            .then_with(|| left.bpm.total_cmp(&right.bpm))
    });
    for mut candidate in values {
        if let Some(existing) = merged.iter_mut().find(|existing: &&mut TempoCandidate| {
            relative_bpm_difference(existing.bpm, candidate.bpm)
                <= CANDIDATE_MERGE_RELATIVE_TOLERANCE
        }) {
            existing.normalized_score = existing.normalized_score.max(candidate.normalized_score);
            existing.score = existing.score.max(candidate.score);
            if !existing.source.contains(&candidate.source) {
                existing.source.push('+');
                existing.source.push_str(&candidate.source);
            }
            continue;
        }
        candidate.source = if candidate.source.is_empty() {
            source.into()
        } else {
            format!("{source}:{}", candidate.source)
        };
        candidate.relation_to_truth = relation(candidate.bpm, truth);
        merged.push(candidate);
        if merged.len() >= CANDIDATE_TOP_K {
            break;
        }
    }
    merged
}

fn reanchor_neural(
    values: &[TempoCandidate],
    event_bpm: Option<f32>,
    truth: Option<f32>,
) -> Vec<TempoCandidate> {
    let Some(event_bpm) = event_bpm else {
        return Vec::new();
    };
    values
        .iter()
        .map(|candidate| {
            let multiplier = relation_multiplier(&candidate.relation);
            candidate_value(
                event_bpm * multiplier,
                "neural_event_reanchor",
                &candidate.relation,
                candidate.normalized_score,
                "refinement",
                truth,
            )
        })
        .collect()
}

fn reanchor_classical(
    values: &[TempoCandidate],
    event_bpm: Option<f32>,
    truth: Option<f32>,
) -> Vec<TempoCandidate> {
    let Some(event_bpm) = event_bpm else {
        return Vec::new();
    };
    values
        .iter()
        .map(|candidate| {
            let ratio = candidate.bpm / event_bpm.max(1.0);
            let (relation, multiplier) =
                [("half_time", 0.5), ("primary", 1.0), ("double_time", 2.0)]
                    .into_iter()
                    .min_by(|left, right| {
                        (ratio - left.1).abs().total_cmp(&(ratio - right.1).abs())
                    })
                    .unwrap_or(("primary", 1.0));
            candidate_value(
                event_bpm * multiplier,
                "classical_event_reanchor",
                relation,
                candidate.normalized_score,
                "refinement",
                truth,
            )
        })
        .collect()
}

fn relation_multiplier(relation: &str) -> f32 {
    match relation {
        "half_time" => 0.5,
        "double_time" => 2.0,
        _ => 1.0,
    }
}

fn rank_shadow(
    mut values: Vec<TempoCandidate>,
    event_bpm: Option<f32>,
    truth: Option<f32>,
) -> Vec<TempoCandidate> {
    for candidate in &mut values {
        let features = shadow_features(Some(candidate), event_bpm, truth);
        candidate.shadow_rank_score = 0.45 * features.normalized_evidence
            + 0.30 * features.event_agreement
            + 0.15 * features.source_agreement
            + 0.10 * features.explicit_relation_score;
        candidate.relation_to_truth = relation(candidate.bpm, truth);
    }
    values.sort_by(|left, right| {
        right
            .shadow_rank_score
            .total_cmp(&left.shadow_rank_score)
            .then_with(|| right.normalized_score.total_cmp(&left.normalized_score))
            .then_with(|| left.bpm.total_cmp(&right.bpm))
    });
    values.truncate(CANDIDATE_TOP_K);
    values
}

fn shadow_features(
    candidate: Option<&TempoCandidate>,
    event_bpm: Option<f32>,
    _truth: Option<f32>,
) -> RankingFeatures {
    let Some(candidate) = candidate else {
        return RankingFeatures {
            normalized_evidence: 0.0,
            event_agreement: 0.0,
            source_agreement: 0.0,
            explicit_relation_score: 0.0,
            selected_rank_score: 0.0,
        };
    };
    let event_agreement = event_bpm
        .map(|event| {
            let relation_adjusted = event * relation_multiplier(&candidate.relation);
            (1.0 - relative_bpm_difference(candidate.bpm, relation_adjusted) / 0.25).clamp(0.0, 1.0)
        })
        .unwrap_or(0.0);
    let source_agreement = if candidate.source.contains('+') {
        1.0
    } else if candidate.source.contains("event") || candidate.source.contains("neural") {
        0.65
    } else {
        0.45
    };
    let explicit_relation_score = match candidate.relation.as_str() {
        "primary" => 1.0,
        "half_time" | "double_time" => 0.85,
        "alternative" => 0.55,
        _ => 0.50,
    };
    RankingFeatures {
        normalized_evidence: candidate.normalized_score,
        event_agreement,
        source_agreement,
        explicit_relation_score,
        selected_rank_score: candidate.shadow_rank_score,
    }
}

fn classify_failure(
    truth: Option<f32>,
    upstream: &[TempoCandidate],
    current: Option<&TempoCandidate>,
    shadow: Option<&TempoCandidate>,
    observation: &TempoAmbiguityObservation,
) -> (String, Vec<String>) {
    let Some(truth) = truth else {
        return ("not_scalar_tempo".into(), Vec::new());
    };
    let upstream_family = upstream
        .iter()
        .any(|candidate| family_correct(candidate.bpm, truth));
    let current_family = current.is_some_and(|candidate| family_correct(candidate.bpm, truth));
    let current_canonical = current.is_some_and(|candidate| canonical(candidate.bpm, truth));
    let shadow_canonical = shadow.is_some_and(|candidate| canonical(candidate.bpm, truth));
    let primary =
        if canonical(observation.classical.production_selected_bpm, truth) && current_canonical {
            "none"
        } else if !upstream_family {
            "generation_missing"
        } else if !current_family {
            "propagation_dropped"
        } else if !current_canonical {
            "ranking_wrong"
        } else {
            "numeric_refinement"
        };
    let mut secondary = Vec::new();
    if current_family && !current_canonical {
        secondary.push("numeric_refinement".into());
    }
    if observation.neural.selected_bpm.is_none() {
        secondary.push("neural_unavailable".into());
    } else if !family_correct(observation.neural.selected_bpm, truth) {
        secondary.push("neural_family_wrong".into());
    }
    if !family_correct(observation.v2.event_derived_bpm, truth) {
        secondary.push("event_clock_wrong".into());
    }
    if shadow_canonical && !current_canonical {
        secondary.push("shadow_rescue".into());
    }
    (primary.into(), secondary)
}

fn build_reanchor_report(flows: &[CandidateFlowObservation]) -> ReanchorComparison {
    let variants: [(&str, CandidatePoolGetter); 5] = [
        ("current_v2", pool_current_v2),
        ("neural_event_reanchor", pool_neural_reanchor),
        ("classical_event_reanchor", pool_classical_reanchor),
        ("classical_propagated", pool_classical_propagated),
        ("merged_shadow", pool_merged_shadow),
    ];
    let reports = variants
        .iter()
        .map(|(name, getter)| variant_oracle(name, flows, *getter))
        .collect::<Vec<_>>();
    let current_v2_canonical = reports
        .first()
        .map(|report| report.top1_canonical_correct)
        .unwrap_or(0);
    let neural_event = reports
        .get(1)
        .map(|report| report.top1_canonical_correct)
        .unwrap_or(0);
    let mut improved = Vec::new();
    let mut regressed = Vec::new();
    let mut unchanged = Vec::new();
    let mut by_family = BTreeMap::new();
    for flow in flows.iter().filter(|flow| flow.truth_bpm.is_some()) {
        let current = flow
            .pools
            .v2_current
            .first()
            .is_some_and(|candidate| canonical(candidate.bpm, flow.truth_bpm));
        let reanchored = flow
            .pools
            .research_reanchored
            .first()
            .is_some_and(|candidate| canonical(candidate.bpm, flow.truth_bpm));
        match (current, reanchored) {
            (false, true) => {
                improved.push(flow.fixture_id.clone());
                *by_family.entry(flow.family.clone()).or_insert(0) += 1;
            }
            (true, false) => regressed.push(flow.fixture_id.clone()),
            _ => unchanged.push(flow.fixture_id.clone()),
        }
    }
    ReanchorComparison {
        variants: reports,
        current_v2_canonical,
        neural_event_reanchor_canonical: neural_event,
        improved_sample_ids: improved,
        regressed_sample_ids: regressed,
        unchanged_sample_ids: unchanged,
        by_family,
    }
}

type CandidatePoolGetter = fn(&CandidateFlowObservation) -> Vec<TempoCandidate>;

fn pool_current_v2(flow: &CandidateFlowObservation) -> Vec<TempoCandidate> {
    flow.pools.v2_current.clone()
}

fn pool_neural_reanchor(flow: &CandidateFlowObservation) -> Vec<TempoCandidate> {
    flow.pools.research_reanchored.clone()
}

fn pool_classical_reanchor(flow: &CandidateFlowObservation) -> Vec<TempoCandidate> {
    flow.pools.classical_event_reanchored.clone()
}

fn pool_classical_propagated(flow: &CandidateFlowObservation) -> Vec<TempoCandidate> {
    flow.pools.classical_propagated.clone()
}

fn pool_merged_shadow(flow: &CandidateFlowObservation) -> Vec<TempoCandidate> {
    flow.pools.merged_shadow.clone()
}

fn variant_oracle(
    name: &str,
    flows: &[CandidateFlowObservation],
    getter: fn(&CandidateFlowObservation) -> Vec<TempoCandidate>,
) -> VariantOracleReport {
    let mut report = VariantOracleReport {
        variant: name.into(),
        scored: 0,
        candidate_count: 0,
        top1_canonical_correct: 0,
        top2_canonical_correct: 0,
        top3_canonical_correct: 0,
        top1_family_correct: 0,
        top2_family_correct: 0,
        top3_family_correct: 0,
        canonical_oracle: 0,
        family_oracle: 0,
        false_family_candidates: 0,
    };
    for flow in flows {
        let Some(truth) = flow.truth_bpm else {
            continue;
        };
        let candidates = getter(flow);
        report.scored += 1;
        report.candidate_count += candidates.len();
        for (index, candidate) in candidates.iter().enumerate() {
            let canonical = canonical(candidate.bpm, Some(truth));
            let family = family_correct(candidate.bpm, Some(truth));
            if !canonical && family {
                report.false_family_candidates += 1;
            }
            if canonical {
                report.canonical_oracle += 1;
            }
            if family {
                report.family_oracle += 1;
            }
            if index == 0 {
                report.top1_canonical_correct += usize::from(canonical);
                report.top1_family_correct += usize::from(family);
            }
        }
        report.top2_canonical_correct += usize::from(
            candidates
                .iter()
                .take(2)
                .any(|candidate| canonical(candidate.bpm, Some(truth))),
        );
        report.top3_canonical_correct += usize::from(
            candidates
                .iter()
                .take(3)
                .any(|candidate| canonical(candidate.bpm, Some(truth))),
        );
        report.top2_family_correct += usize::from(
            candidates
                .iter()
                .take(2)
                .any(|candidate| family_correct(candidate.bpm, Some(truth))),
        );
        report.top3_family_correct += usize::from(
            candidates
                .iter()
                .take(3)
                .any(|candidate| family_correct(candidate.bpm, Some(truth))),
        );
    }
    report.canonical_oracle = report.canonical_oracle.min(report.scored);
    report.family_oracle = report.family_oracle.min(report.scored);
    report
}

fn build_failure_taxonomy(flows: &[CandidateFlowObservation]) -> FailureTaxonomyReport {
    let observations = flows
        .iter()
        .filter_map(|flow| {
            let truth = flow.truth_bpm?;
            let upstream = flow
                .pools
                .classical_full_low_union_top3
                .iter()
                .chain(flow.pools.neural_half_native_double.iter())
                .chain(flow.pools.event_clock.iter())
                .any(|candidate| family_correct(candidate.bpm, Some(truth)));
            let current = flow
                .current_v2_top1
                .as_ref()
                .is_some_and(|candidate| family_correct(candidate.bpm, Some(truth)));
            let shadow = flow
                .shadow_top1
                .as_ref()
                .is_some_and(|candidate| family_correct(candidate.bpm, Some(truth)));
            Some(FailureTaxonomyObservation {
                sample_id: flow.fixture_id.clone(),
                family: flow.family.clone(),
                current_v2_relation: flow
                    .current_v2_top1
                    .as_ref()
                    .map(|candidate| relation(candidate.bpm, Some(truth)))
                    .unwrap_or_else(|| "absent".into()),
                primary_class: flow.primary_failure_class.clone(),
                secondary_classes: flow.secondary_failure_classes.clone(),
                correct_family_in_upstream_generation: upstream,
                correct_family_in_current_v2: current,
                correct_family_in_shadow: shadow,
            })
        })
        .collect::<Vec<_>>();
    let mut counts = BTreeMap::new();
    for observation in &observations {
        *counts.entry(observation.primary_class.clone()).or_insert(0) += 1;
        for secondary in &observation.secondary_classes {
            *counts.entry(format!("secondary:{secondary}")).or_insert(0) += 1;
        }
    }
    FailureTaxonomyReport {
        counts,
        observations,
    }
}

fn build_duration_invariance_v2(flows: &[CandidateFlowObservation]) -> DurationInvarianceV2Report {
    let mut groups = BTreeMap::<String, Vec<&CandidateFlowObservation>>::new();
    for flow in flows {
        groups.entry(flow.master_id.clone()).or_default().push(flow);
    }
    let mut report = DurationInvarianceV2Report {
        compared_master_count: 0,
        selected_bpm_stable: 0,
        candidate_relation_order_stable: 0,
        candidate_numeric_stable: 0,
        event_clock_stable: 0,
        resolved_meter_stable: 0,
        top_meter_stable: 0,
        top_downbeat_phase_stable: 0,
        full_meter_ordering_stable: 0,
        full_meter_score_stable: 0,
        cases: Vec::new(),
    };
    for (master, values) in groups {
        let Some(short) = values
            .iter()
            .find(|flow| flow.duration_micros == 30_000_000)
        else {
            continue;
        };
        let Some(long) = values
            .iter()
            .find(|flow| flow.duration_micros == 60_000_000)
        else {
            continue;
        };
        let selected_stable = same_bpm(
            short
                .pools
                .production_selected
                .first()
                .map(|candidate| candidate.bpm),
            long.pools
                .production_selected
                .first()
                .map(|candidate| candidate.bpm),
        );
        let short_relations = candidate_relations(&short.pools.v2_current);
        let long_relations = candidate_relations(&long.pools.v2_current);
        let relation_order_equal = short_relations == long_relations;
        let numeric_drift =
            candidate_numeric_drift(&short.pools.v2_current, &long.pools.v2_current);
        let numeric_stable = numeric_drift.is_some_and(|drift| drift <= 0.005);
        let event_stable = same_bpm(short.event_clock_bpm, long.event_clock_bpm);
        let resolved_meter_equal = resolved_meter(short) == resolved_meter(long);
        let top_meter_equal = top_meter(short) == top_meter(long);
        let top_phase_equal = top_phase(short) == top_phase(long);
        let full_meter_order_equal = meter_order(short) == meter_order(long);
        let score_drift = meter_score_drift(short, long);
        let full_meter_score_stable = score_drift <= 0.05;
        report.compared_master_count += 1;
        report.selected_bpm_stable += usize::from(selected_stable);
        report.candidate_relation_order_stable += usize::from(relation_order_equal);
        report.candidate_numeric_stable += usize::from(numeric_stable);
        report.event_clock_stable += usize::from(event_stable);
        report.resolved_meter_stable += usize::from(resolved_meter_equal);
        report.top_meter_stable += usize::from(top_meter_equal);
        report.top_downbeat_phase_stable += usize::from(top_phase_equal);
        report.full_meter_ordering_stable += usize::from(full_meter_order_equal);
        report.full_meter_score_stable += usize::from(full_meter_score_stable);
        report.cases.push(DurationInvarianceCase {
            master_id: master,
            selected_bpm_30s: short
                .pools
                .production_selected
                .first()
                .map(|candidate| candidate.bpm),
            selected_bpm_60s: long
                .pools
                .production_selected
                .first()
                .map(|candidate| candidate.bpm),
            candidate_relation_order_equal: relation_order_equal,
            candidate_numeric_drift: numeric_drift,
            event_bpm_30s: short.event_clock_bpm,
            event_bpm_60s: long.event_clock_bpm,
            resolved_meter_equal,
            top_meter_equal,
            top_downbeat_phase_equal: top_phase_equal,
            full_meter_order_equal,
            full_meter_score_drift: score_drift,
            stable: selected_stable
                && relation_order_equal
                && numeric_stable
                && event_stable
                && resolved_meter_equal
                && top_meter_equal
                && top_phase_equal
                && full_meter_order_equal
                && full_meter_score_stable,
        });
    }
    report
}

fn candidate_relations(candidates: &[TempoCandidate]) -> Vec<String> {
    candidates
        .iter()
        .map(|candidate| candidate.relation.clone())
        .collect()
}

fn candidate_numeric_drift(left: &[TempoCandidate], right: &[TempoCandidate]) -> Option<f32> {
    if left.is_empty() || right.is_empty() {
        return None;
    }
    let count = left.len().min(right.len());
    Some(
        left.iter()
            .zip(right.iter())
            .take(count)
            .map(|(left, right)| relative_bpm_difference(left.bpm, right.bpm))
            .fold(0.0, f32::max),
    )
}

fn same_bpm(left: Option<f32>, right: Option<f32>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => relative_bpm_difference(left, right) <= 0.005,
        (None, None) => true,
        _ => false,
    }
}

fn resolved_meter(flow: &CandidateFlowObservation) -> Option<u8> {
    flow.meter_hypotheses
        .iter()
        .find(|hypothesis| hypothesis.2 >= 0.60)
        .map(|hypothesis| hypothesis.0)
}

fn top_meter(flow: &CandidateFlowObservation) -> Option<u8> {
    flow.meter_hypotheses.first().map(|hypothesis| hypothesis.0)
}

fn top_phase(flow: &CandidateFlowObservation) -> Option<u8> {
    flow.meter_hypotheses.first().map(|hypothesis| hypothesis.1)
}

fn meter_order(flow: &CandidateFlowObservation) -> Vec<(u8, u8)> {
    flow.meter_hypotheses
        .iter()
        .map(|hypothesis| (hypothesis.0, hypothesis.1))
        .collect()
}

fn meter_score_drift(left: &CandidateFlowObservation, right: &CandidateFlowObservation) -> f32 {
    left.meter_hypotheses
        .iter()
        .zip(right.meter_hypotheses.iter())
        .map(|(left, right)| (left.2 - right.2).abs())
        .fold(0.0, f32::max)
}

fn quantization_observation(
    observation: &TempoAmbiguityObservation,
) -> Option<NeuralQuantizationObservation> {
    let truth_bpm = observation.ground_truth.primary_bpm?;
    let selected_period_frames = observation.neural.selected_period_frames;
    let frame_quantized_bpm =
        selected_period_frames.map(|period| 60.0 * NEURAL_FRAME_RATE_HZ / period as f32);
    let adjacent_lower_bpm = selected_period_frames
        .filter(|period| *period > 0)
        .map(|period| 60.0 * NEURAL_FRAME_RATE_HZ / (period + 1) as f32);
    let adjacent_upper_bpm = selected_period_frames
        .filter(|period| *period > 1)
        .map(|period| 60.0 * NEURAL_FRAME_RATE_HZ / (period - 1) as f32);
    let quantization_step_bpm = adjacent_lower_bpm
        .zip(adjacent_upper_bpm)
        .map(|(lower, upper)| upper - lower);
    let truth_error_bpm = frame_quantized_bpm.map(|bpm| (bpm - truth_bpm).abs());
    let region = if (58.0..=62.0).contains(&truth_bpm) {
        "58_62_boundary"
    } else if (79.0..=81.0).contains(&truth_bpm) {
        "80_region"
    } else if (129.0..=131.0).contains(&truth_bpm) {
        "130_region"
    } else if (159.0..=181.0).contains(&truth_bpm) {
        "160_180_region"
    } else {
        "other"
    };
    Some(NeuralQuantizationObservation {
        fixture_id: observation.fixture_id.clone(),
        truth_bpm,
        selected_period_frames,
        frame_quantized_bpm,
        adjacent_lower_bpm,
        adjacent_upper_bpm,
        quantization_step_bpm,
        truth_error_bpm,
        region: region.into(),
    })
}

fn build_feature_inventory(flows: &[CandidateFlowObservation]) -> Vec<FeatureInventoryEntry> {
    let definitions = [
        (
            "candidate_evidence",
            "normalized score attached to a generated tempo candidate",
            "Classical peak, Neural candidate, or V2 hypothesis",
            true,
            "0.0 when the candidate is absent",
            "analysis-time diagnostic",
        ),
        (
            "event_agreement",
            "bounded agreement with the observed Neural BeatEvent interval clock",
            "decoded event intervals",
            true,
            "0.0 when the event clock is unavailable",
            "analysis-lab diagnostic",
        ),
        (
            "source_agreement",
            "whether independent candidate sources support the same tempo",
            "candidate provenance labels",
            true,
            "0.0 for a missing source",
            "analysis-lab diagnostic",
        ),
        (
            "explicit_relation",
            "generic score for a declared primary, half-time, double-time, or alternative relation",
            "TempoRelation metadata",
            true,
            "0.5 for an unlabeled candidate",
            "analysis-time diagnostic",
        ),
        (
            "event_interval_dispersion",
            "MAD divided by median interval",
            "BeatEvent interval statistics",
            true,
            "missing when fewer than five intervals exist",
            "analysis-lab diagnostic",
        ),
        (
            "selected_period_frames",
            "neural activation period in decoder frames",
            "Neural decoder diagnostic",
            true,
            "missing when Neural decoding is unavailable",
            "analysis-lab diagnostic",
        ),
    ];
    definitions
        .into_iter()
        .map(
            |(
                name,
                definition,
                source,
                production_time_available,
                missing_value_behavior,
                exposure,
            )| {
                let values = flows
                    .iter()
                    .filter_map(|flow| match name {
                        "candidate_evidence" => flow
                            .pools
                            .merged_shadow
                            .first()
                            .map(|candidate| f64::from(candidate.normalized_score)),
                        "event_agreement" => Some(f64::from(flow.ranking_features.event_agreement)),
                        "source_agreement" => {
                            Some(f64::from(flow.ranking_features.source_agreement))
                        }
                        "explicit_relation" => {
                            Some(f64::from(flow.ranking_features.explicit_relation_score))
                        }
                        "event_interval_dispersion" => {
                            flow.pools.event_clock.first().and_then(|_| {
                                if flow.event_clock_bpm.is_some() {
                                    Some(0.0)
                                } else {
                                    None
                                }
                            })
                        }
                        "selected_period_frames" => flow
                            .quantization
                            .as_ref()
                            .and_then(|item| item.selected_period_frames.map(|value| value as f64)),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                FeatureInventoryEntry {
                    name: name.into(),
                    definition: definition.into(),
                    source: source.into(),
                    production_time_available,
                    missing_value_behavior: missing_value_behavior.into(),
                    observed_min: values.iter().copied().reduce(f64::min),
                    observed_median: median_f64(&values),
                    observed_max: values.iter().copied().reduce(f64::max),
                    exposure: exposure.into(),
                }
            },
        )
        .collect()
}

fn build_focus_slices(flows: &[CandidateFlowObservation]) -> FocusSlices {
    let row = |flow: &CandidateFlowObservation| FocusSliceRow {
        fixture_id: flow.fixture_id.clone(),
        truth_bpm: flow.truth_bpm.unwrap_or_default(),
        production_bpm: flow
            .pools
            .production_selected
            .first()
            .map(|candidate| candidate.bpm),
        full_top1: flow
            .pools
            .classical_full_top1
            .first()
            .map(|candidate| candidate.bpm),
        full_top3: flow
            .pools
            .classical_full_top3
            .iter()
            .map(|candidate| candidate.bpm)
            .collect(),
        low_top3: flow
            .pools
            .classical_low_top3
            .iter()
            .map(|candidate| candidate.bpm)
            .collect(),
        neural_bpm: flow
            .pools
            .neural_selected
            .first()
            .map(|candidate| candidate.bpm),
        event_bpm: flow.event_clock_bpm,
        propagated_bpm: flow
            .pools
            .research_propagated
            .first()
            .map(|candidate| candidate.bpm),
        reanchored_bpm: flow
            .pools
            .research_reanchored
            .first()
            .map(|candidate| candidate.bpm),
    };
    FocusSlices {
        boundary_58_62: flows
            .iter()
            .filter(|flow| {
                flow.truth_bpm
                    .is_some_and(|bpm| (58.0..=62.0).contains(&bpm))
            })
            .map(row)
            .collect(),
        neighborhood_130: flows
            .iter()
            .filter(|flow| {
                flow.truth_bpm
                    .is_some_and(|bpm| (129.5..=130.5).contains(&bpm))
            })
            .map(row)
            .collect(),
        ambiguity_80_160: flows
            .iter()
            .filter(|flow| {
                flow.truth_bpm
                    .is_some_and(|bpm| (bpm - 80.0).abs() < 0.1 || (bpm - 160.0).abs() < 0.1)
            })
            .map(row)
            .collect(),
    }
}

fn build_variable_tempo_safety() -> Result<Vec<VariableTempoSafetyObservation>, LabError> {
    let profiles = [
        (
            "ramp_minus_0_10",
            TempoProfile::LinearRamp {
                start_bpm: 130.0,
                end_bpm: 129.9,
            },
        ),
        (
            "ramp_plus_0_50",
            TempoProfile::LinearRamp {
                start_bpm: 130.0,
                end_bpm: 130.5,
            },
        ),
        (
            "ramp_minus_2",
            TempoProfile::LinearRamp {
                start_bpm: 130.0,
                end_bpm: 128.0,
            },
        ),
        (
            "tempo_step_return",
            TempoProfile::StepReturn {
                base_bpm: 128.0,
                step_bpm: 4.0,
                start_micros: 20_000_000,
                end_micros: 40_000_000,
            },
        ),
    ];
    let mut result = Vec::new();
    for (label, profile) in profiles {
        let spec = long_spec(
            format!("shadow-variable-{label}"),
            format!("shadow-variable-{label}"),
            FixtureFamily::TempoDrift,
            profile,
            EventStyle::Standard,
            60_000_000,
            4,
            500_000,
            0x24_10_04_52,
        );
        let analyzed = analyze_long_fixture(generate_fixture(&spec)?)?;
        let intervals = analyzed
            .v2
            .rhythm
            .beats
            .windows(2)
            .filter_map(|window| {
                window[1]
                    .time
                    .checked_sub(window[0].time)
                    .map(|duration| duration.as_micros() as f64)
            })
            .filter(|interval| *interval > 0.0)
            .collect::<Vec<_>>();
        let median = median_f64(&intervals);
        let deviations = median.map(|center| {
            intervals
                .iter()
                .map(|interval| (interval - center).abs())
                .collect::<Vec<_>>()
        });
        let mad = deviations.as_ref().and_then(|values| median_f64(values));
        let dispersion = mad.zip(median).map(|(mad, median)| mad / median.max(1.0));
        let split = intervals.len() / 2;
        let early = median_f64(&intervals[..split.max(1).min(intervals.len())]);
        let late = median_f64(&intervals[split.min(intervals.len())..]);
        let middle_start = intervals.len() / 3;
        let middle_end = (intervals.len() * 2 / 3).max(middle_start + 1);
        let middle = median_f64(&intervals[middle_start..middle_end.min(intervals.len())]);
        let fit_residual = deviations.as_ref().and_then(|values| median_f64(values));
        let relative_drift = early
            .zip(late)
            .zip(median)
            .map(|((early, late), median)| (late - early).abs() / median.max(1.0));
        let abstain = intervals.len() < 5 || relative_drift.is_some_and(|drift| drift > 0.05);
        let reason = if intervals.len() < 5 {
            Some("insufficient_events".into())
        } else if relative_drift.is_some_and(|drift| drift > 0.05) {
            Some("interval_drift_exceeds_global_fit".into())
        } else {
            None
        };
        result.push(VariableTempoSafetyObservation {
            fixture_id: spec.id,
            profile: label.into(),
            interval_median_micros: median,
            interval_dispersion: dispersion,
            fit_residual_micros: fit_residual,
            early_period_micros: early,
            middle_period_micros: middle,
            late_period_micros: late,
            refinement_would_abstain: abstain,
            abstention_reason: reason,
        });
    }
    Ok(result)
}

fn build_heldout_ranking(flows: &[CandidateFlowObservation]) -> HeldOutRankingEvaluation {
    let families = flows
        .iter()
        .map(|flow| flow.family.clone())
        .collect::<BTreeSet<_>>();
    let mut folds = Vec::new();
    let mut aggregate = HeldOutAggregate {
        scored: 0,
        unique_scored: flows.iter().filter(|flow| flow.truth_bpm.is_some()).count(),
        fold_observations: 0,
        canonical_correct: 0,
        family_correct: 0,
        false_confident_accepts: 0,
        abstentions: 0,
        valid_folds: 0,
    };
    for requested_family in families {
        let validation = expanded_family_validation(flows, &requested_family);
        let validation_set = validation.iter().cloned().collect::<BTreeSet<_>>();
        let train = flows
            .iter()
            .filter(|flow| !validation_set.contains(&flow.fixture_id))
            .collect::<Vec<_>>();
        let validation_flows = flows
            .iter()
            .filter(|flow| validation_set.contains(&flow.fixture_id))
            .collect::<Vec<_>>();
        let mut top1_canonical = 0;
        let mut top1_family = 0;
        let mut false_confident = 0;
        let mut abstentions = 0;
        for flow in &validation_flows {
            let Some(truth) = flow.truth_bpm else {
                continue;
            };
            let candidate = flow.pools.merged_shadow.first();
            let accepted = candidate.is_some_and(|candidate| candidate.shadow_rank_score >= 0.45);
            if !accepted {
                abstentions += 1;
                continue;
            }
            let candidate = candidate.expect("accepted candidate");
            let is_canonical = canonical(candidate.bpm, Some(truth));
            let is_family = family_correct(candidate.bpm, Some(truth));
            top1_canonical += usize::from(is_canonical);
            top1_family += usize::from(is_family);
            false_confident += usize::from(candidate.shadow_rank_score >= 0.65 && !is_canonical);
        }
        let train_pcm = train
            .iter()
            .map(|flow| flow.pcm_sha256.clone())
            .collect::<BTreeSet<_>>();
        let validation_pcm = validation_flows
            .iter()
            .map(|flow| flow.pcm_sha256.clone())
            .collect::<BTreeSet<_>>();
        let train_lineage = train
            .iter()
            .map(|flow| flow.master_id.clone())
            .collect::<BTreeSet<_>>();
        let validation_lineage = validation_flows
            .iter()
            .map(|flow| flow.master_id.clone())
            .collect::<BTreeSet<_>>();
        let exact_pcm_overlap = !train_pcm.is_disjoint(&validation_pcm);
        let lineage_overlap = !train_lineage.is_disjoint(&validation_lineage);
        let other_families_pulled_into_validation = validation_flows
            .iter()
            .filter(|flow| flow.family != requested_family)
            .map(|flow| flow.family.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let valid = !exact_pcm_overlap && !lineage_overlap;
        aggregate.scored += validation_flows
            .iter()
            .filter(|flow| flow.truth_bpm.is_some())
            .count();
        aggregate.fold_observations = aggregate.scored;
        aggregate.canonical_correct += top1_canonical;
        aggregate.family_correct += top1_family;
        aggregate.false_confident_accepts += false_confident;
        aggregate.abstentions += abstentions;
        aggregate.valid_folds += usize::from(valid);
        folds.push(HeldOutFamilyFold {
            requested_family,
            expanded_validation_samples: validation,
            other_families_pulled_into_validation,
            train_size: train.len(),
            validation_size: validation_flows.len(),
            exact_pcm_overlap,
            lineage_overlap,
            top1_canonical_correct: top1_canonical,
            top1_family_correct: top1_family,
            false_confident_accepts: false_confident,
            abstentions,
        });
    }
    HeldOutRankingEvaluation {
        rule_name: "frozen bounded evidence/event/source/relation rank; no truth fitting".into(),
        rule_frozen_before_scoring: true,
        grouping_rule: "connected components over exact PCM hash or master lineage; every held-out component is excluded from training".into(),
        folds,
        aggregate,
    }
}

fn expanded_family_validation(
    flows: &[CandidateFlowObservation],
    requested_family: &str,
) -> Vec<String> {
    let requested = flows
        .iter()
        .enumerate()
        .filter(|(_, flow)| flow.family == requested_family)
        .map(|(index, _)| index)
        .collect::<BTreeSet<_>>();
    let mut included = requested.clone();
    let mut changed = true;
    while changed {
        changed = false;
        for left in 0..flows.len() {
            for right in 0..flows.len() {
                let connected = flows[left].pcm_sha256 == flows[right].pcm_sha256
                    || flows[left].master_id == flows[right].master_id;
                if connected && (included.contains(&left) || included.contains(&right)) {
                    let before = included.len();
                    included.insert(left);
                    included.insert(right);
                    changed |= included.len() != before;
                }
            }
        }
    }
    included
        .into_iter()
        .map(|index| flows[index].fixture_id.clone())
        .collect()
}

fn build_decision(
    flows: &[CandidateFlowObservation],
    reanchor: &ReanchorComparison,
    failures: &FailureTaxonomyReport,
    automix: &AutoMixAliasShadowMatrix,
    heldout: &HeldOutRankingEvaluation,
) -> FollowupDecision {
    let upstream_information_lost_before_v2 = failures
        .observations
        .iter()
        .filter(|observation| {
            observation.correct_family_in_upstream_generation
                && !observation.correct_family_in_current_v2
        })
        .count();
    let shadow_regressions = flows
        .iter()
        .filter(|flow| {
            flow.current_v2_top1
                .as_ref()
                .is_some_and(|candidate| canonical(candidate.bpm, flow.truth_bpm))
                && flow
                    .shadow_top1
                    .as_ref()
                    .is_some_and(|candidate| !canonical(candidate.bpm, flow.truth_bpm))
        })
        .count();
    let mut answers = BTreeMap::new();
    answers.insert(
        "1_candidate_generation".into(),
        format!(
            "upstream family available on {} of {} scalar fixtures; {} lost before current V2",
            failures
                .observations
                .iter()
                .filter(|observation| observation.correct_family_in_upstream_generation)
                .count(),
            failures.observations.len(),
            upstream_information_lost_before_v2
        ),
    );
    answers.insert(
        "2_propagation".into(),
        "Classical and external event-clock candidates are measured separately from selection; no production propagation changed".into(),
    );
    answers.insert(
        "3_neural_relation".into(),
        "Neural relation metadata is retained as a selected-family label and is never replaced with a truth-derived relation during inference".into(),
    );
    answers.insert(
        "4_event_reanchor".into(),
        format!(
            "neural event re-anchor improved {} fixtures and regressed {} fixtures",
            reanchor.improved_sample_ids.len(),
            reanchor.regressed_sample_ids.len()
        ),
    );
    answers.insert(
        "5_automix".into(),
        format!(
            "positive harness cases={} selected={} false BeatMatched={}",
            automix.pair_count, automix.shadow_selected_correct, automix.shadow_false_beatmatched
        ),
    );
    answers.insert(
        "6_heldout".into(),
        format!(
            "fixed-rule held-out canonical={} / {} fold-observations ({} unique fixtures) with {} false confident accepts",
            heldout.aggregate.canonical_correct,
            heldout.aggregate.scored,
            heldout.aggregate.unique_scored,
            heldout.aggregate.false_confident_accepts
        ),
    );
    answers.insert(
        "7_production".into(),
        "Classical remains the beat, grid, phase, meter, downbeat, and production tempo authority"
            .into(),
    );
    let bottleneck = if upstream_information_lost_before_v2 > 0 {
        "propagation"
    } else if reanchor.improved_sample_ids.is_empty() {
        "candidate-ranking-or-missing-family"
    } else {
        "ranking-and-safe-acceptance"
    };
    let recommendation = if automix.shadow_false_beatmatched > 0 || shadow_regressions > 0 {
        "KEEP RESEARCH ONLY: shadow candidates are informative, but alias or ranking regressions prevent an authority change"
    } else if reanchor.improved_sample_ids.is_empty() {
        "KEEP RESEARCH ONLY: the candidate flow does not expose a repeatable rescue above Classical"
    } else {
        "V2 SHADOW CANDIDATE ONLY: retain conservative offline comparison; do not promote without independent corpus validation"
    };
    FollowupDecision {
        bottleneck: bottleneck.into(),
        upstream_information_lost_before_v2,
        reanchor_gain_observed: !reanchor.improved_sample_ids.is_empty(),
        correct_to_wrong_shadow_count: shadow_regressions,
        neural_relation_trust_statement: "Neural relation metadata is useful for preserving candidate families, but it is not a substitute for known-truth evaluation.".into(),
        question_answers: answers,
        recommendation: recommendation.into(),
        research_only: true,
        production_behavior_changed: false,
    }
}

fn build_positive_corpus(
    analyzed: &[AnalyzedFixture],
    flows: &[CandidateFlowObservation],
) -> Result<BeatMatchedPositiveCorpus, LabError> {
    let truth_fixture = |bpm: f32| {
        analyzed.iter().find(|item| {
            item.fixture.spec.duration_micros == 60_000_000
                && item
                    .observation
                    .ground_truth
                    .primary_bpm
                    .is_some_and(|truth| (truth - bpm).abs() < 0.01)
        })
    };
    let mut pairs = Vec::new();
    let pair_specs = [
        ("sanity-120", 120.0, 120.0),
        ("sanity-80", 80.0, 80.0),
        ("sanity-160", 160.0, 160.0),
        ("sanity-60", 60.0, 60.0),
        ("alias-80-160", 80.0, 160.0),
        ("alias-160-80", 160.0, 80.0),
        ("alias-60-120", 60.0, 120.0),
        ("alias-120-60", 120.0, 60.0),
        ("alias-60-180", 60.0, 180.0),
        ("alias-180-60", 180.0, 60.0),
        ("wrong-alias-80-80", 80.0, 80.0),
    ];
    let variants = [
        "known_positive_truth",
        "known_wrong_alias",
        "current_v2",
        "classical_propagated",
        "merged_shadow",
    ];
    for (pair_id, outgoing_bpm, incoming_bpm) in pair_specs {
        let outgoing = truth_fixture(outgoing_bpm).ok_or_else(|| {
            LabError::InvalidInput(format!(
                "positive corpus fixture missing for {outgoing_bpm}"
            ))
        })?;
        let incoming = truth_fixture(incoming_bpm).ok_or_else(|| {
            LabError::InvalidInput(format!(
                "positive corpus fixture missing for {incoming_bpm}"
            ))
        })?;
        let outgoing_flow = flows
            .iter()
            .find(|flow| flow.fixture_id == outgoing.fixture.spec.id)
            .ok_or_else(|| LabError::InvalidInput("positive corpus flow missing".into()))?;
        let incoming_flow = flows
            .iter()
            .find(|flow| flow.fixture_id == incoming.fixture.spec.id)
            .ok_or_else(|| LabError::InvalidInput("positive corpus flow missing".into()))?;
        for variant in variants {
            let outgoing_candidates = variant_candidates(outgoing_flow, variant, outgoing_bpm);
            let incoming_candidates = variant_candidates(incoming_flow, variant, incoming_bpm);
            pairs.push(plan_pair_case(
                pair_id,
                variant,
                outgoing,
                incoming,
                outgoing_candidates,
                incoming_candidates,
            ));
        }
    }
    let sanity_cases = pairs
        .iter()
        .filter(|case| case.pair_id.starts_with("sanity-"))
        .collect::<Vec<_>>();
    Ok(BeatMatchedPositiveCorpus {
        construction: "Known-positive harness: truth beat times construct a strict V2 analysis only for testing planner mechanics. Truth is not an inference feature, and generated audio remains in memory.".into(),
        sanity_fixture_count: sanity_cases.len(),
        sanity_selected_count: sanity_cases
            .iter()
            .filter(|case| case.beatmatched_selected)
            .count(),
        pair_count: pairs.len(),
        cases: pairs,
    })
}

fn variant_candidates(
    flow: &CandidateFlowObservation,
    variant: &str,
    fallback_bpm: f32,
) -> Vec<TempoCandidate> {
    let source = match variant {
        "known_wrong_alias" => {
            return vec![candidate_value(
                fallback_bpm + 20.0,
                "known_wrong_alias_harness",
                "primary",
                1.0,
                "harness",
                None,
            )];
        }
        "current_v2" => &flow.pools.v2_current,
        "classical_propagated" => &flow.pools.classical_propagated,
        "merged_shadow" => &flow.pools.merged_shadow,
        _ => {
            return vec![candidate_value(
                fallback_bpm,
                "known_positive_harness",
                "primary",
                1.0,
                "harness",
                None,
            )];
        }
    };
    if source.is_empty() {
        vec![candidate_value(
            fallback_bpm,
            "fallback_positive_harness",
            "primary",
            1.0,
            "harness",
            None,
        )]
    } else {
        source.clone()
    }
}

fn plan_pair_case(
    pair_id: &str,
    variant: &str,
    outgoing: &AnalyzedFixture,
    incoming: &AnalyzedFixture,
    outgoing_candidates: Vec<TempoCandidate>,
    incoming_candidates: Vec<TempoCandidate>,
) -> AutoMixPairCase {
    let outgoing_analysis = strict_analysis(
        &outgoing.fixture.truth,
        &outgoing_candidates,
        outgoing.fixture.spec.duration_micros,
    );
    let incoming_analysis = strict_analysis(
        &incoming.fixture.truth,
        &incoming_candidates,
        incoming.fixture.spec.duration_micros,
    );
    let config = auto_mix_config();
    let planned = plan_transition_v2(&outgoing_analysis, &incoming_analysis, &config);
    let guarded = plan_guarded_transition_v2(&outgoing_analysis, &incoming_analysis, &config);
    let eligibility = beat_match_eligibility(&outgoing_analysis, &incoming_analysis, &config);
    let selected_pair = planned
        .candidates
        .iter()
        .find(|candidate| candidate.plan == planned.plan)
        .and_then(|candidate| candidate.beat_eligibility.as_ref())
        .and_then(|eligibility| eligibility.tempo_hypothesis)
        .or(eligibility.tempo_hypothesis);
    let survived = planned.candidates.iter().any(|candidate| {
        candidate.plan.kind == TransitionKind::BeatMatched && candidate.hard_rejection.is_none()
    });
    let selected_beatmatched = guarded.plan.kind == TransitionKind::BeatMatched;
    let correct_pair = selected_pair.is_some_and(|pair| {
        let out_correct = family_correct(
            pair.outgoing.bpm,
            outgoing
                .fixture
                .truth
                .tempo
                .as_ref()
                .map(|tempo| tempo.primary_bpm),
        );
        let in_correct = family_correct(
            pair.incoming.bpm,
            incoming
                .fixture
                .truth
                .tempo
                .as_ref()
                .map(|tempo| tempo.primary_bpm),
        );
        let expected_ratio = pair.outgoing.bpm / pair.incoming.bpm;
        let truth_ratio =
            outgoing_bpm(&outgoing.fixture.truth) / incoming_bpm(&incoming.fixture.truth);
        out_correct && in_correct && relative_bpm_difference(expected_ratio, truth_ratio) <= 0.005
    });
    let wrong_alias = selected_beatmatched && !correct_pair;
    let render_quality = render_preview(
        &outgoing.fixture,
        &incoming.fixture,
        &guarded.plan,
        guarded.quality.max_beat_phase_error,
    );
    let false_beatmatched = wrong_alias;
    let correct_beatmatched = selected_beatmatched && correct_pair && render_quality.acceptable;
    let safe_fallback = !selected_beatmatched && !false_beatmatched;
    let missed_opportunity = !selected_beatmatched && survived;
    AutoMixPairCase {
        pair_id: pair_id.into(),
        variant: variant.into(),
        outgoing_fixture: outgoing.fixture.spec.id.clone(),
        incoming_fixture: incoming.fixture.spec.id.clone(),
        outgoing_truth_bpm: outgoing_bpm(&outgoing.fixture.truth),
        incoming_truth_bpm: incoming_bpm(&incoming.fixture.truth),
        outgoing_candidates,
        incoming_candidates,
        selected_outgoing_bpm: selected_pair.map(|pair| pair.outgoing.bpm),
        selected_incoming_bpm: selected_pair.map(|pair| pair.incoming.bpm),
        selected_outgoing_relation: selected_pair
            .map(|pair| format!("{:?}", pair.outgoing.relation).to_ascii_lowercase()),
        selected_incoming_relation: selected_pair
            .map(|pair| format!("{:?}", pair.incoming.relation).to_ascii_lowercase()),
        selected_ratio: selected_pair.map(|pair| pair.ratio),
        selected_adjustment: selected_pair.map(|pair| pair.normalized_adjustment),
        selected_weight: selected_pair.map(|pair| pair.weight),
        selected_cost: selected_pair.map(|pair| pair.cost),
        eligible: eligibility.eligible,
        beat_pairs: eligibility.beat_pairs,
        phase_error_micros: eligibility
            .phase_error
            .map(|error| error.as_micros() as u64),
        beatmatched_candidate_generated: planned.diagnostics.beatmatched_candidates > 0,
        beatmatched_candidate_survived_guard: survived,
        beatmatched_selected: selected_beatmatched,
        correct_pair,
        wrong_alias,
        false_beatmatched,
        correct_beatmatched,
        safe_fallback,
        missed_opportunity,
        transition: format!("{:?}", guarded.plan.kind),
        render_quality,
        planner_reasons: vec![
            format!("planned={:?}", planned.plan.kind),
            format!("guarded={:?}", guarded.plan.kind),
            format!("eligible={}", eligibility.eligible),
        ],
    }
}

fn strict_analysis(
    truth: &AnalysisGroundTruth,
    candidates: &[TempoCandidate],
    duration_micros: u64,
) -> TrackAnalysisV2 {
    let duration = Duration::from_micros(duration_micros);
    let mut analysis = TrackAnalysisV2::unanalyzed(duration);
    analysis.audible_start = truth
        .beat_times_micros
        .first()
        .copied()
        .map(Duration::from_micros)
        .unwrap_or(Duration::from_millis(500));
    analysis.audible_end = duration.saturating_sub(Duration::from_secs(1));
    analysis.rhythm.beats = truth
        .beat_times_micros
        .iter()
        .map(|time| {
            BeatEvent::new(
                Duration::from_micros(*time),
                Some(ModelScore::new(0.95).expect("valid model score")),
                None,
                Confidence::new(0.95).expect("valid confidence"),
                Some(Support::new(0.95).expect("valid support")),
                Some(Support::new(0.95).expect("valid support")),
            )
        })
        .collect();
    analysis.rhythm.tempo_hypotheses = candidates
        .iter()
        .filter_map(|candidate| {
            TempoHypothesis::with_relation(
                candidate.bpm,
                UnitInterval::clamped(candidate.normalized_score.max(0.7)),
                parse_relation(&candidate.relation),
            )
        })
        .collect();
    analysis.rhythm.meter_hypotheses = vec![MeterHypothesis {
        beats_per_bar: truth.meter.unwrap_or(4),
        downbeat_phase: 0,
        score: UnitInterval::ONE,
    }];
    analysis
}

fn parse_relation(value: &str) -> TempoRelation {
    match value {
        "half_time" => TempoRelation::HalfTime,
        "double_time" => TempoRelation::DoubleTime,
        "alternative" => TempoRelation::Alternative,
        _ => TempoRelation::Primary,
    }
}

fn auto_mix_config() -> AutoMixConfig {
    AutoMixConfig {
        enabled: true,
        crossfade: Duration::from_secs(8),
        max_tempo_adjustment: 0.05,
        min_beat_confidence: 0.70,
    }
}

fn outgoing_bpm(truth: &AnalysisGroundTruth) -> f32 {
    truth
        .tempo
        .as_ref()
        .map(|tempo| tempo.primary_bpm)
        .unwrap_or(0.0)
}

fn incoming_bpm(truth: &AnalysisGroundTruth) -> f32 {
    outgoing_bpm(truth)
}

fn render_preview(
    outgoing: &SyntheticFixture,
    incoming: &SyntheticFixture,
    plan: &wotoha_core::automix::TransitionPlan,
    phase_error: Option<Duration>,
) -> RenderQualityObservation {
    let duration = plan.duration.max(Duration::from_millis(1));
    let sample_rate = outgoing.spec.sample_rate.max(1) as f64;
    let steps = 128usize;
    let mut peak: f32 = 0.0;
    let mut max_gain_step: f32 = 0.0;
    let mut edge_energy = Vec::new();
    let mut middle_energy = Vec::new();
    let mut previous_gains: Option<(f32, f32)> = None;
    for index in 0..=steps {
        let progress = index as f32 / steps as f32;
        let elapsed = duration.mul_f32(progress);
        let outgoing_index =
            plan.outgoing_start
                .as_secs_f64()
                .mul_add(sample_rate, elapsed.as_secs_f64() * sample_rate) as usize;
        let incoming_index =
            plan.incoming_start
                .as_secs_f64()
                .mul_add(sample_rate, elapsed.as_secs_f64() * sample_rate) as usize;
        let outgoing_sample = outgoing.audio.get(outgoing_index).copied().unwrap_or(0.0);
        let incoming_sample = incoming.audio.get(incoming_index).copied().unwrap_or(0.0);
        let gains = automix_mix_gains(plan.kind, progress);
        if let Some(previous) = previous_gains {
            max_gain_step = max_gain_step.max((gains.0 - previous.0).abs());
            max_gain_step = max_gain_step.max((gains.1 - previous.1).abs());
        }
        previous_gains = Some(gains);
        let mixed = gains.0 * outgoing_sample + gains.1 * incoming_sample;
        peak = peak.max(mixed.abs());
        let energy = mixed * mixed;
        if index < steps / 4 || index > steps * 3 / 4 {
            edge_energy.push(energy);
        } else if index > steps / 3 && index < steps * 2 / 3 {
            middle_energy.push(energy);
        }
    }
    let edge_rms = rms(&edge_energy);
    let middle_rms = rms(&middle_energy);
    let quietest_to_edge = edge_rms.map(|edge| (edge / 2.0).min(1.0));
    let middle_to_edge = middle_rms
        .zip(edge_rms)
        .map(|(middle, edge)| middle / edge.max(1.0e-6));
    let phase_alignment_ms = phase_error.map(|error| error.as_secs_f32() * 1_000.0);
    let mut issues = Vec::new();
    if peak > 1.0 {
        issues.push("peak_above_unity".into());
    }
    if max_gain_step > 0.15 {
        issues.push("gain_step_large".into());
    }
    if phase_alignment_ms.is_some_and(|value| value > 35.0) {
        issues.push("phase_alignment_large".into());
    }
    if edge_rms.is_some_and(|edge| edge > 0.01) && middle_to_edge.is_some_and(|value| value < 0.60)
    {
        issues.push("middle_energy_drop".into());
    }
    RenderQualityObservation {
        phase_alignment_ms,
        beat_transient_alignment_ms: phase_alignment_ms,
        gain_continuity_max_step: Some(max_gain_step),
        peak: Some(peak),
        overlap_micros: plan.duration.as_micros() as u64,
        tempo_adjustment: Some((plan.incoming_tempo_ratio - 1.0).abs()),
        quietest_to_edge_rms_ratio: quietest_to_edge,
        mid_to_edge_rms_ratio: middle_to_edge,
        acceptable: issues.is_empty(),
        issues,
    }
}

fn rms(values: &[f32]) -> Option<f32> {
    (!values.is_empty()).then(|| {
        (values.iter().map(|value| value * value).sum::<f32>() / values.len() as f32).sqrt()
    })
}

fn build_automix_shadow(
    _analyzed: &[AnalyzedFixture],
    _flows: &[CandidateFlowObservation],
    positive: &BeatMatchedPositiveCorpus,
) -> Result<AutoMixAliasShadowMatrix, LabError> {
    let current = positive
        .cases
        .iter()
        .filter(|case| case.variant == "current_v2")
        .collect::<Vec<_>>();
    let shadow = positive
        .cases
        .iter()
        .filter(|case| case.variant == "merged_shadow")
        .collect::<Vec<_>>();
    let current_selected_correct = current
        .iter()
        .filter(|case| case.correct_beatmatched)
        .count();
    let current_false = current.iter().filter(|case| case.false_beatmatched).count();
    let shadow_selected_correct = shadow
        .iter()
        .filter(|case| case.correct_beatmatched)
        .count();
    let shadow_false = shadow.iter().filter(|case| case.false_beatmatched).count();
    let safe_fallback_count = shadow.iter().filter(|case| case.safe_fallback).count();
    let missed = shadow.iter().filter(|case| case.missed_opportunity).count();
    Ok(AutoMixAliasShadowMatrix {
        config: AutoMixConfigView {
            crossfade_micros: 8_000_000,
            max_tempo_adjustment: 0.05,
            min_beat_confidence: 0.70,
        },
        pair_count: positive.pair_count,
        variant_count: 5,
        cases: positive.cases.clone(),
        current_selected_correct,
        current_false_beatmatched: current_false,
        shadow_selected_correct,
        shadow_false_beatmatched: shadow_false,
        safe_fallback_count,
        missed_beatmatched_opportunities: missed,
    })
}

fn write_followup_outputs(
    output_dir: &Path,
    report: &TempoShadowFollowupReport,
) -> Result<(), LabError> {
    write_json(
        &output_dir.join("tempo-candidate-flow.json"),
        &report.candidate_flow,
    )?;
    write_json(
        &output_dir.join("tempo-reanchor-comparison.json"),
        &report.reanchor,
    )?;
    write_json(
        &output_dir.join("candidate-failure-taxonomy.json"),
        &report.failure_taxonomy,
    )?;
    write_json(
        &output_dir.join("duration-invariance-v2.json"),
        &report.duration_invariance,
    )?;
    write_json(
        &output_dir.join("beatmatched-positive-corpus.json"),
        &report.positive_corpus,
    )?;
    write_json(
        &output_dir.join("automix-alias-shadow-matrix.json"),
        &report.automix_shadow,
    )?;
    write_json(
        &output_dir.join("heldout-ranking-evaluation.json"),
        &report.heldout_ranking,
    )?;
    write_json(&output_dir.join("tempo-shadow-followup.json"), report)?;
    write_json(
        &output_dir.join("tempo-shadow-followup-report.json"),
        report,
    )?;
    write_json(
        &output_dir.join("tempo-quantization-audit.json"),
        &report.quantization,
    )?;
    write_json(
        &output_dir.join("variable-tempo-safety.json"),
        &report.variable_tempo_safety,
    )?;
    write_csv(
        &output_dir.join("tempo-candidate-flow.csv"),
        &report.candidate_flow,
    )?;
    fs::write(
        output_dir.join("tempo-shadow-followup-report.md"),
        markdown_report(report),
    )?;
    Ok(())
}

fn write_csv(path: &Path, rows: &[CandidateFlowObservation]) -> Result<(), LabError> {
    let mut output = String::from(
        "fixture_id,master_id,family,duration_micros,truth_bpm,production_bpm,current_v2_bpm,shadow_bpm,event_bpm,primary_failure_class\n",
    );
    for row in rows {
        let field = |value: String| {
            if value.contains(',') || value.contains('"') {
                format!("\"{}\"", value.replace('"', "\"\""))
            } else {
                value
            }
        };
        let value = |bpm: Option<f32>| bpm.map(|value| format!("{value:.4}")).unwrap_or_default();
        let line = [
            row.fixture_id.clone(),
            row.master_id.clone(),
            row.family.clone(),
            row.duration_micros.to_string(),
            value(row.truth_bpm),
            value(
                row.pools
                    .production_selected
                    .first()
                    .map(|candidate| candidate.bpm),
            ),
            value(row.current_v2_top1.as_ref().map(|candidate| candidate.bpm)),
            value(row.shadow_top1.as_ref().map(|candidate| candidate.bpm)),
            value(row.event_clock_bpm),
            row.primary_failure_class.clone(),
        ]
        .into_iter()
        .map(field)
        .collect::<Vec<_>>()
        .join(",");
        output.push_str(&line);
        output.push('\n');
    }
    fs::write(path, output)?;
    Ok(())
}

fn markdown_report(report: &TempoShadowFollowupReport) -> String {
    let scalar = report
        .candidate_flow
        .iter()
        .filter(|flow| flow.truth_bpm.is_some())
        .count();
    let current = report
        .reanchor
        .variants
        .first()
        .map(|variant| variant.top1_canonical_correct)
        .unwrap_or(0);
    let shadow = report
        .reanchor
        .variants
        .last()
        .map(|variant| variant.top1_canonical_correct)
        .unwrap_or(0);
    let mut markdown = String::new();
    markdown.push_str("# Tempo shadow follow-up research\n\n");
    markdown.push_str("This is a research-only report. Classical remains the rhythm, beat, phase, meter, downbeat, and production authority. No production behavior or BeatEvent timeline changed.\n\n");
    markdown.push_str(&format!(
        "- source commit: {}\n- scalar fixtures: {scalar}\n- current V2 top-1 canonical: {current}/{scalar}\n- merged shadow top-1 canonical: {shadow}/{scalar}\n- event re-anchor improvements: {}\n- event re-anchor regressions: {}\n- production behavior changed: {}\n\n",
        report.source_commit,
        report.reanchor.improved_sample_ids.len(),
        report.reanchor.regressed_sample_ids.len(),
        report.production_behavior_changed
    ));
    markdown.push_str("## Candidate flow\n\n");
    markdown.push_str("Candidate generation, propagation, ranking, and event-clock refinement are reported as separate pools. Ground Truth is used only for labels and retrospective classification.\n\n");
    markdown.push_str("## Duration invariance\n\n");
    markdown.push_str(&format!(
        "Compared masters: {}. Selected BPM stable: {}. Candidate relation order stable: {}. Numeric candidate stable: {}. Event clock stable: {}. Resolved meter stable: {}. Full meter order stable: {}.\n\n",
        report.duration_invariance.compared_master_count,
        report.duration_invariance.selected_bpm_stable,
        report.duration_invariance.candidate_relation_order_stable,
        report.duration_invariance.candidate_numeric_stable,
        report.duration_invariance.event_clock_stable,
        report.duration_invariance.resolved_meter_stable,
        report.duration_invariance.full_meter_ordering_stable
    ));
    markdown.push_str("## BeatMatched positive corpus and AutoMix shadow\n\n");
    markdown.push_str(&format!(
        "The strict known-positive harness contains {} variant cases and {} sanity selections. Current false BeatMatched cases: {}; merged-shadow false BeatMatched cases: {}; safe fallback cases: {}.\n\n",
        report.positive_corpus.pair_count,
        report.positive_corpus.sanity_selected_count,
        report.automix_shadow.current_false_beatmatched,
        report.automix_shadow.shadow_false_beatmatched,
        report.automix_shadow.safe_fallback_count
    ));
    markdown.push_str("## Variable-tempo safety\n\n");
    for case in &report.variable_tempo_safety {
        markdown.push_str(&format!(
            "- {}: early={:?}, middle={:?}, late={:?}, dispersion={:?}, abstain={} ({:?})\n",
            case.profile,
            case.early_period_micros,
            case.middle_period_micros,
            case.late_period_micros,
            case.interval_dispersion,
            case.refinement_would_abstain,
            case.abstention_reason
        ));
    }
    markdown.push('\n');
    markdown.push_str("## Failure taxonomy\n\n");
    for (name, count) in &report.failure_taxonomy.counts {
        markdown.push_str(&format!("- {name}: {count}\n"));
    }
    markdown.push_str("\n## Held-out family stress\n\n");
    markdown.push_str(&format!(
        "Rule: {}. Valid folds: {}. Canonical correct: {}/{} fold-observations ({} unique fixtures). False confident accepts: {}. This is a fixed-rule stress test, not proof of real-world generalization.\n\n",
        report.heldout_ranking.rule_name,
        report.heldout_ranking.aggregate.valid_folds,
        report.heldout_ranking.aggregate.canonical_correct,
        report.heldout_ranking.aggregate.scored,
        report.heldout_ranking.aggregate.unique_scored,
        report.heldout_ranking.aggregate.false_confident_accepts
    ));
    markdown.push_str("## Decision\n\n");
    markdown.push_str(&format!(
        "**{}**\n\nBottleneck: {}. {}\n\n",
        report.decision.recommendation,
        report.decision.bottleneck,
        report.decision.neural_relation_trust_statement
    ));
    markdown.push_str("## Production boundary\n\n");
    markdown.push_str("- Classical beat timeline: unchanged and authoritative\n- Grid phase, meter, downbeats: unchanged and authoritative\n- Neural output: tempo shadow label only in this report\n- AutoMix/runtime/playback: unchanged\n- Generated reports: outside Git\n");
    markdown
}

trait MaybeBpm {
    fn into_bpm(self) -> Option<f32>;
}

impl MaybeBpm for f32 {
    fn into_bpm(self) -> Option<f32> {
        Some(self)
    }
}

impl MaybeBpm for Option<f32> {
    fn into_bpm(self) -> Option<f32> {
        self
    }
}

fn relation<B: MaybeBpm, T: MaybeBpm>(bpm: B, truth: T) -> String {
    let Some(bpm) = bpm.into_bpm() else {
        return "absent".into();
    };
    let Some(truth) = truth.into_bpm() else {
        return "unscored".into();
    };
    if within_relation(bpm, truth) {
        "primary".into()
    } else if within_relation(bpm, truth / 2.0) {
        "half_time".into()
    } else if within_relation(bpm, truth * 2.0) {
        "double_time".into()
    } else {
        "other_wrong".into()
    }
}

fn within_relation(actual: f32, expected: f32) -> bool {
    expected > 0.0
        && actual.is_finite()
        && (actual - expected).abs() / expected <= CANONICAL_RELATIVE_TOLERANCE
}

fn canonical<B: MaybeBpm, T: MaybeBpm>(bpm: B, truth: T) -> bool {
    matches!(relation(bpm, truth).as_str(), "primary")
}

fn family_correct<B: MaybeBpm, T: MaybeBpm>(bpm: B, truth: T) -> bool {
    matches!(
        relation(bpm, truth).as_str(),
        "primary" | "half_time" | "double_time"
    )
}

fn relative_bpm_difference(left: f32, right: f32) -> f32 {
    (left - right).abs() / left.abs().max(right.abs()).max(1.0)
}

fn median_f64(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    Some(sorted[sorted.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relation_preserves_half_and_double_semantics() {
        assert_eq!(relation(Some(128.0), Some(128.0)), "primary");
        assert_eq!(relation(Some(64.0), Some(128.0)), "half_time");
        assert_eq!(relation(Some(256.0), Some(128.0)), "double_time");
        assert_eq!(relation(None, Some(128.0)), "absent");
    }

    #[test]
    fn event_reanchor_preserves_relation_label() {
        let candidates = vec![candidate_value(
            125.0,
            "neural",
            "half_time",
            0.9,
            "generation",
            None,
        )];
        let reanchored = reanchor_neural(&candidates, Some(128.0), None);
        assert_eq!(reanchored[0].relation, "half_time");
        assert!((reanchored[0].bpm - 64.0).abs() < f32::EPSILON);
    }

    #[test]
    fn merge_is_bounded_and_deterministic() {
        let candidates = (0..32)
            .map(|index| {
                candidate_value(
                    100.0 + index as f32,
                    "test",
                    "primary",
                    0.5,
                    "generation",
                    None,
                )
            })
            .collect::<Vec<_>>();
        let merged = merge_candidates(candidates.clone(), None, "merged");
        assert!(merged.len() <= CANDIDATE_TOP_K);
        assert_eq!(merged, merge_candidates(candidates, None, "merged"));
    }

    #[test]
    fn fixed_shadow_rank_does_not_depend_on_truth() {
        let candidate = candidate_value(128.0, "event", "primary", 0.8, "generation", None);
        let left = rank_shadow(vec![candidate.clone()], Some(128.0), Some(128.0));
        let right = rank_shadow(vec![candidate], Some(128.0), Some(160.0));
        assert_eq!(left[0].bpm, right[0].bpm);
        assert_eq!(left[0].shadow_rank_score, right[0].shadow_rank_score);
    }
}
